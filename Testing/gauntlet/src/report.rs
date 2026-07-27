//! Check bookkeeping and exit-code contract shared by every suite.
//!
//! A suite is a list of named checks. Each check either passes, fails, or is
//! skipped because a precondition could not be met. The distinction between
//! "failed" and "skipped" matters a lot here: a skipped check means the runner
//! could not exercise the code path at all (no D2D, no corpus, no privileges),
//! which is inconclusive rather than green - so suites report skips separately
//! and the workflow surfaces them instead of quietly counting them as success.
//!
//! Exit codes (consumed by the supervisor in main.rs and by CI):
//!   0   all checks passed (skips are reported but do not fail)
//!   1   at least one check failed
//!   10  the suite could not run at all (environment/setup failure)
//!   101 Rust panic (the default panic exit code)
//!   negative / >= 0x40000000  unhandled Windows exception, i.e. a real crash
//! The supervisor adds 124 for a watchdog-killed hang.

use std::io::Write as _;

pub const EXIT_FAIL: i32 = 1;
pub const EXIT_ENV: i32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Skip,
}

pub struct Check {
    pub name: String,
    pub outcome: Outcome,
    pub detail: String,
}

pub struct Report {
    suite: String,
    checks: Vec<Check>,
    /// Name of the case currently in flight, mirrored to a heartbeat file so the
    /// supervisor can name the exact input that was being processed if this
    /// process crashes or is killed for hanging.
    heartbeat_path: Option<std::path::PathBuf>,
}

impl Report {
    pub fn new(suite: &str) -> Self {
        Self { suite: suite.to_string(), checks: Vec::new(), heartbeat_path: None }
    }

    /// Enables crash attribution. Every subsequent `begin_case` overwrites this
    /// file with the case name, flushed immediately, so a hard crash still
    /// leaves the last-attempted case on disk.
    pub fn with_heartbeat(mut self, path: std::path::PathBuf) -> Self {
        self.heartbeat_path = Some(path);
        self
    }

    /// Records the case about to be attempted. Call this immediately before any
    /// work that could crash the process.
    pub fn begin_case(&self, case: &str) {
        if let Some(path) = &self.heartbeat_path {
            if let Ok(mut f) = std::fs::File::create(path) {
                let _ = writeln!(f, "{}\t{}", self.suite, case);
                let _ = f.flush();
            }
        }
    }

    pub fn pass(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.record(name.into(), Outcome::Pass, detail.into());
    }

    pub fn fail(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.record(name.into(), Outcome::Fail, detail.into());
    }

    pub fn skip(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.record(name.into(), Outcome::Skip, detail.into());
    }

    /// Records a pass or fail from a boolean, which keeps call sites to one line.
    pub fn check(&mut self, name: impl Into<String>, ok: bool, detail: impl Into<String>) {
        let name = name.into();
        if ok {
            self.pass(name, detail);
        } else {
            self.fail(name, detail);
        }
    }

    fn record(&mut self, name: String, outcome: Outcome, detail: String) {
        let qualified = format!("{}/{}", self.suite, name);
        let tag = match outcome {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Skip => "SKIP",
        };
        println!("  [{tag}] {qualified}: {detail}");
        let _ = std::io::stdout().flush();
        self.checks.push(Check { name: qualified, outcome, detail });
    }

    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.outcome == Outcome::Pass).count()
    }

    /// Every failed check. There is deliberately no allowlist: a check that
    /// fails fails the build, whether or not the cause is already understood.
    ///
    /// The alternative - suppressing checks for defects that are known but not
    /// yet fixed - makes a green run mean "no *new* problems", which is not a
    /// signal anybody can act on without reading the whole log every time. If a
    /// finding is not worth going red for, the honest fix is to delete the
    /// check, not to hide its result.
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| c.outcome == Outcome::Fail).collect()
    }

    pub fn skipped(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| c.outcome == Outcome::Skip).collect()
    }

    /// Prints the summary and returns the process exit code for this suite.
    pub fn finish(&self) -> i32 {
        let failures = self.failures();
        let skips = self.skipped();

        println!();
        println!(
            "SUITE-SUMMARY {}: {} passed, {} failed, {} skipped",
            self.suite,
            self.passed(),
            failures.len(),
            skips.len()
        );
        for c in &failures {
            println!("  FAILED: {} - {}", c.name, c.detail);
        }
        for c in &skips {
            println!("  SKIPPED (could not be checked): {} - {}", c.name, c.detail);
        }

        self.write_github_summary(&failures, &skips);

        if failures.is_empty() {
            println!("SUITE-RESULT {}: OK", self.suite);
            0
        } else {
            println!("SUITE-RESULT {}: FAILED", self.suite);
            EXIT_FAIL
        }
    }

    /// Appends the failing check names to the GitHub Actions run summary.
    ///
    /// Without this, a red run only says which *suite* failed, and finding out
    /// which check - and why - means opening the job and scrolling the log. The
    /// whole value of the gauntlet is being able to glance at a red run and know
    /// what it found, so the specifics belong on the summary page, not buried.
    fn write_github_summary(&self, failures: &[&Check], skips: &[&Check]) {
        let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") else { return };
        let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };

        if failures.is_empty() && skips.is_empty() {
            let _ = writeln!(f, "- **{}**: {} checks passed", self.suite, self.passed());
            return;
        }

        let _ = writeln!(
            f,
            "<details open><summary><strong>{}</strong>: {} passed, {} FAILED, {} skipped</summary>\n",
            self.suite,
            self.passed(),
            failures.len(),
            skips.len()
        );
        for c in failures {
            // The detail line carries the measured numbers, which is usually
            // enough to triage without reopening the log at all.
            let _ = writeln!(f, "- **FAILED** `{}`<br>{}", c.name, c.detail);
        }
        for c in skips {
            let _ = writeln!(f, "- _skipped_ `{}`: {}", c.name, c.detail);
        }
        let _ = writeln!(f, "\n</details>\n");
    }
}

/// Bails out of a suite that cannot run at all. Distinct from a check failure:
/// it means the environment could not host the test, not that the DLL is wrong.
pub fn env_bail(msg: &str) -> ! {
    println!("SUITE-ENVIRONMENT-FAILURE: {msg}");
    let _ = std::io::stdout().flush();
    std::process::exit(EXIT_ENV);
}

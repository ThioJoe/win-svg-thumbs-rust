//! Adversarial test gauntlet for the SVG thumbnail provider.
//!
//! # Design
//!
//! The gauntlet is a supervisor plus a set of suites. The supervisor runs each
//! suite in its own child process under a watchdog, which is what makes the
//! whole thing usable on every push:
//!
//!   * a suite that faults takes down only itself, so the remaining suites still
//!     produce results in the same CI run,
//!   * a suite that deadlocks is killed at a known timeout and reported as a
//!     hang rather than burning the job's entire time budget,
//!   * every child writes a heartbeat naming the case it is about to attempt, so
//!     a crash or a hang is attributed to one specific input instead of "somewhere
//!     in the adversarial suite",
//!   * children can cap their own committed memory with a job object, so a
//!     decompression bomb fails the test instead of the runner.
//!
//! # Usage
//!
//!   gauntlet run   [--dll PATH] [--suite NAME]... [--scale N] [--seed N]
//!   gauntlet exec  SUITE --dll PATH [--seed N] [--only CASE] [--mem-cap-mb N]
//!
//! `run` is the supervisor. `exec` is one suite in-process and is normally
//! invoked by the supervisor, but can be run directly to reproduce a failure.

mod corpus;
mod dll;
mod metrics;
mod report;
mod rng;
mod streams;
mod suites;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::HRESULT;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

use report::Report;

/// Suite definitions: name, default per-suite watchdog timeout, and whether the
/// suite needs a memory cap in the child.
struct SuiteSpec {
    name: &'static str,
    timeout: Duration,
    /// Committed-memory cap for the child process, in MiB. `None` means no cap.
    /// Suites that deliberately feed bombs and oversized inputs run capped so a
    /// genuine blowup fails cleanly rather than taking down the runner.
    mem_cap_mb: Option<u64>,
    description: &'static str,
}

const SUITES: &[SuiteSpec] = &[
    SuiteSpec {
        name: "api-misuse",
        timeout: Duration::from_secs(300),
        mem_cap_mb: None,
        description: "COM contract abuse: null pointers, wrong CLSIDs, aggregation, double \
                      initialisation, release ordering, server locks, size boundaries",
    },
    SuiteSpec {
        name: "stream-faults",
        timeout: Duration::from_secs(600),
        mem_cap_mb: Some(2048),
        description: "Hostile IStream implementations: lying Stat, partial and failing reads, \
                      over-reported byte counts, oversized payloads",
    },
    SuiteSpec {
        name: "render",
        timeout: Duration::from_secs(900),
        mem_cap_mb: None,
        description: "Rendering correctness: size sweep, determinism, alpha un-premultiplication, \
                      CSS precedence, scaling, fallback detection",
    },
    SuiteSpec {
        name: "adversarial",
        timeout: Duration::from_secs(1800),
        mem_cap_mb: Some(3072),
        description: "Synthetic malformed/hostile SVG corpus including XXE and entity expansion",
    },
    SuiteSpec {
        name: "svgz",
        timeout: Duration::from_secs(900),
        mem_cap_mb: Some(3072),
        description: "Compressed SVG corpus: truncation, corruption, concatenation, \
                      decompression bombs",
    },
    SuiteSpec {
        name: "size-limits",
        timeout: Duration::from_secs(900),
        mem_cap_mb: Some(4096),
        description: "Inputs either side of the documented 101 MiB cap",
    },
    SuiteSpec {
        name: "lifecycle",
        timeout: Duration::from_secs(900),
        mem_cap_mb: None,
        description: "Randomized COM lifecycle state machine across STA and MTA threads",
    },
    SuiteSpec {
        name: "concurrency",
        timeout: Duration::from_secs(900),
        mem_cap_mb: None,
        description: "Concurrency storm: contamination detection, threads exiting mid-render",
    },
    SuiteSpec {
        name: "churn",
        timeout: Duration::from_secs(1800),
        // Capped deliberately. Every one-shot rendering thread leaks a whole
        // D2D/D3D-WARP chain by design, so a few hundred of them is exactly the
        // workload that could drive the runner into swap. With a cap, runaway
        // retention fails the suite cleanly and reports the measured slope
        // instead of destabilising the machine.
        mem_cap_mb: Some(6144),
        description: "Thread-churn resource retention and GDI/handle accounting",
    },
    SuiteSpec {
        name: "breadth",
        timeout: Duration::from_secs(1800),
        mem_cap_mb: Some(2048),
        description: "Real-world icon corpus (fetched and cached by CI)",
    },
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");

    match mode {
        "run" => supervise(&args[2..]),
        "exec" => exec_suite(&args[2..]),
        // Isolated single-purpose probes. These deliberately do things that may
        // fault, so they always run in their own process.
        "probe-null-out" => probe_null_out(&args[2..], true, true),
        "probe-null-alpha" => probe_null_out(&args[2..], false, true),
        "list" => {
            for s in SUITES {
                println!("{:<14} {}", s.name, s.description);
            }
        }
        _ => {
            eprintln!(
                "usage:\n  \
                 gauntlet run  [--dll PATH] [--suite NAME]... [--scale N] [--seed N] [--corpus DIR]\n  \
                 gauntlet exec SUITE --dll PATH [--seed N] [--only CASE] [--scale N] [--corpus DIR]\n  \
                 gauntlet list"
            );
            std::process::exit(2);
        }
    }
}

// =====================================================================
//                          Argument parsing
// =====================================================================

struct Args {
    dll: String,
    suites: Vec<String>,
    seed: u64,
    scale: usize,
    only: Option<String>,
    corpus: PathBuf,
    work_dir: PathBuf,
    mem_cap_mb: Option<u64>,
}

fn parse_args(args: &[String]) -> Args {
    let mut dll = None;
    let mut suites = Vec::new();
    let mut seed = None;
    let mut scale = 1usize;
    let mut only = None;
    let mut corpus = None;
    let mut work_dir = None;
    let mut mem_cap_mb = None;

    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match args[i].as_str() {
            "--dll" => dll = take(&mut i),
            "--suite" => {
                if let Some(v) = take(&mut i) {
                    suites.push(v);
                }
            }
            "--seed" => seed = take(&mut i).and_then(|v| v.parse().ok()),
            "--scale" => scale = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(1),
            "--only" => only = take(&mut i),
            "--corpus" => corpus = take(&mut i).map(PathBuf::from),
            "--work-dir" => work_dir = take(&mut i).map(PathBuf::from),
            "--mem-cap-mb" => mem_cap_mb = take(&mut i).and_then(|v| v.parse().ok()),
            other if !other.starts_with("--") && suites.is_empty() => {
                // Positional suite name, so `exec render --dll x` works.
                suites.push(other.to_string());
            }
            other => eprintln!("warning: ignoring unrecognised argument {other:?}"),
        }
        i += 1;
    }

    Args {
        dll: dll.unwrap_or_else(default_dll_path),
        suites,
        seed: seed.unwrap_or_else(rng::arbitrary_seed),
        scale: scale.max(1),
        only,
        corpus: corpus.unwrap_or_else(|| PathBuf::from("svg-corpus")),
        work_dir: work_dir.unwrap_or_else(|| PathBuf::from("gauntlet-work")),
        mem_cap_mb,
    }
}

/// Filename `build.rs` gives the provider for the architecture this binary was
/// built for.
///
/// The gauntlet loads the DLL with LoadLibraryW, so the two must always be the
/// same architecture - a 64-bit harness cannot load the 32-bit provider. Keying
/// the default off this binary's own target architecture is therefore the only
/// answer that can ever be right.
const fn provider_dll_name() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "win_svg_thumbs_x64.dll"
    } else if cfg!(target_arch = "x86") {
        "win_svg_thumbs_x86.dll"
    } else if cfg!(target_arch = "aarch64") {
        "win_svg_thumbs_arm64.dll"
    } else {
        "win_svg_thumbs_x64.dll"
    }
}

fn default_dll_path() -> String {
    // The gauntlet binary is built into target/debug (or release); the DLL under
    // test is built into target/release. Resolve relative to the target dir so
    // the tool works without arguments in a normal workspace layout.
    //
    // This also lands correctly for `--target <triple>` builds: the executable
    // is then at target/<triple>/release/, so going up two levels reaches
    // target/<triple> and the rejoin finds the matching provider next to it.
    let exe = std::env::current_exe().expect("current_exe failed");
    let target = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot locate target dir");
    target
        .join("release")
        .join(provider_dll_name())
        .display()
        .to_string()
}

// =====================================================================
//                            Supervisor
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed,
    /// Unhandled Windows exception in the child.
    Crashed,
    /// Killed by the watchdog.
    Hung,
    /// The child could not run the suite (setup/environment).
    Inconclusive,
    /// The suite itself panicked. Distinct from a crash: it means a bug in the
    /// harness (or an assertion the suite raised), not a fault in the DLL.
    Panicked,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Passed => "PASS",
            Verdict::Failed => "FAIL",
            Verdict::Crashed => "CRASH",
            Verdict::Hung => "HANG",
            Verdict::Inconclusive => "INCONCLUSIVE",
            Verdict::Panicked => "PANIC",
        }
    }
}

struct SuiteResult {
    name: String,
    verdict: Verdict,
    exit_code: i32,
    duration: Duration,
    /// Case that was in flight when the child died, from the heartbeat file.
    last_case: Option<String>,
}

fn supervise(args: &[String]) {
    let parsed = parse_args(args);

    println!("=================================================================");
    println!(" SVG thumbnail provider - test gauntlet");
    println!("=================================================================");
    println!("DLL under test : {}", parsed.dll);
    println!("seed           : {}", parsed.seed);
    println!("scale          : {}", parsed.scale);
    println!("corpus         : {}", parsed.corpus.display());

    if !Path::new(&parsed.dll).exists() {
        eprintln!("FATAL: DLL not found at {}", parsed.dll);
        std::process::exit(report::EXIT_ENV);
    }
    if let Ok(meta) = std::fs::metadata(&parsed.dll) {
        println!("DLL size       : {} bytes", meta.len());
    }
    let _ = std::fs::create_dir_all(&parsed.work_dir);

    let selected: Vec<&SuiteSpec> = if parsed.suites.is_empty() {
        SUITES.iter().collect()
    } else {
        let picked: Vec<&SuiteSpec> = SUITES
            .iter()
            .filter(|s| parsed.suites.iter().any(|n| n == s.name))
            .collect();
        for requested in &parsed.suites {
            if !SUITES.iter().any(|s| s.name == requested) {
                eprintln!("FATAL: unknown suite {requested:?}. Known suites:");
                for s in SUITES {
                    eprintln!("  {}", s.name);
                }
                std::process::exit(2);
            }
        }
        picked
    };

    println!("suites         : {}", selected.iter().map(|s| s.name).collect::<Vec<_>>().join(", "));
    println!();

    // Heading for the GitHub Actions run summary. Each suite child appends its
    // own failing checks underneath, so the summary page shows exactly what the
    // gauntlet found without anyone opening a log.
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "### Gauntlet (seed `{}`)\n", parsed.seed);
        }
    }

    let exe = std::env::current_exe().expect("current_exe failed");
    let mut results = Vec::new();

    for spec in &selected {
        println!("-----------------------------------------------------------------");
        println!(">>> SUITE {} ({})", spec.name, spec.description);
        println!("-----------------------------------------------------------------");
        let _ = std::io::stdout().flush();

        let heartbeat = parsed.work_dir.join(format!("heartbeat-{}.txt", spec.name));
        let _ = std::fs::remove_file(&heartbeat);

        let mut cmd = Command::new(&exe);
        cmd.arg("exec")
            .arg(spec.name)
            .arg("--dll")
            .arg(&parsed.dll)
            .arg("--seed")
            .arg(parsed.seed.to_string())
            .arg("--scale")
            .arg(parsed.scale.to_string())
            .arg("--corpus")
            .arg(&parsed.corpus)
            .arg("--work-dir")
            .arg(&parsed.work_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(mb) = spec.mem_cap_mb {
            cmd.arg("--mem-cap-mb").arg(mb.to_string());
        }

        let start = Instant::now();
        let result = run_with_watchdog(cmd, spec.timeout);
        let duration = start.elapsed();

        let last_case = std::fs::read_to_string(&heartbeat)
            .ok()
            .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            .filter(|s| !s.is_empty());

        let (verdict, exit_code) = match result {
            WatchdogResult::Exited(code) => (classify_exit(code), code),
            WatchdogResult::TimedOut => (Verdict::Hung, 124),
            WatchdogResult::SpawnFailed(e) => {
                eprintln!("could not spawn child for suite {}: {e}", spec.name);
                (Verdict::Inconclusive, report::EXIT_ENV)
            }
        };

        println!();
        println!(
            "<<< SUITE {} -> {} (exit {}, {:.1}s)",
            spec.name,
            verdict.label(),
            exit_code,
            duration.as_secs_f64()
        );
        if matches!(verdict, Verdict::Crashed | Verdict::Hung | Verdict::Panicked) {
            match &last_case {
                Some(case) => {
                    println!(
                        "    LAST CASE IN FLIGHT: {case}");
                    println!(
                        "    reproduce with: gauntlet exec {} --dll {} --seed {} --only {}",
                        spec.name,
                        parsed.dll,
                        parsed.seed,
                        case.split('\t').next_back().unwrap_or(case)
                    );
                }
                None => println!("    (no heartbeat recorded; the suite died before its first case)"),
            }
        }
        println!();

        results.push(SuiteResult {
            name: spec.name.to_string(),
            verdict,
            exit_code,
            duration,
            last_case,
        });
    }

    print_summary(&results, parsed.seed);

    // Any crash, hang, failure or inconclusive result fails the run. An
    // inconclusive suite counts as a failure on purpose: a gauntlet that
    // silently stopped testing something is worse than one that goes red.
    let bad = results.iter().any(|r| r.verdict != Verdict::Passed);
    std::process::exit(if bad { 1 } else { 0 });
}

fn print_summary(results: &[SuiteResult], seed: u64) {
    println!("=================================================================");
    println!(" GAUNTLET SUMMARY");
    println!("=================================================================");
    for r in results {
        println!(
            "  {:<14} {:<13} exit={:<6} {:>7.1}s{}",
            r.name,
            r.verdict.label(),
            r.exit_code,
            r.duration.as_secs_f64(),
            match (&r.last_case, r.verdict) {
                (Some(c), Verdict::Crashed) | (Some(c), Verdict::Hung) =>
                    format!("  last case: {}", c.split('\t').next_back().unwrap_or(c)),
                _ => String::new(),
            }
        );
    }
    println!();

    let crashed: Vec<&str> = results.iter().filter(|r| r.verdict == Verdict::Crashed).map(|r| r.name.as_str()).collect();
    let hung: Vec<&str> = results.iter().filter(|r| r.verdict == Verdict::Hung).map(|r| r.name.as_str()).collect();
    let failed: Vec<&str> = results.iter().filter(|r| r.verdict == Verdict::Failed).map(|r| r.name.as_str()).collect();
    let inconclusive: Vec<&str> = results.iter().filter(|r| r.verdict == Verdict::Inconclusive).map(|r| r.name.as_str()).collect();
    let panicked: Vec<&str> = results.iter().filter(|r| r.verdict == Verdict::Panicked).map(|r| r.name.as_str()).collect();

    if !crashed.is_empty() {
        println!("  CRASHES (unhandled exception in the child process): {}", crashed.join(", "));
    }
    if !hung.is_empty() {
        println!("  HANGS (killed by the watchdog): {}", hung.join(", "));
    }
    if !panicked.is_empty() {
        println!(
            "  PANICS (a bug in the gauntlet itself, not necessarily in the DLL): {}",
            panicked.join(", ")
        );
    }
    if !failed.is_empty() {
        println!("  FAILED CHECKS: {}", failed.join(", "));
    }
    if !inconclusive.is_empty() {
        println!(
            "  INCONCLUSIVE (the suite could not run - this is treated as a failure so that a \
             silently skipped suite cannot look green): {}",
            inconclusive.join(", ")
        );
    }
    if crashed.is_empty() && hung.is_empty() && failed.is_empty() && inconclusive.is_empty() && panicked.is_empty() {
        println!("  All suites passed.");
    }
    println!();
    println!("  seed for this run: {seed}");
    println!("  replay any suite with: gauntlet exec <suite> --dll <path> --seed {seed}");

    // Expose the verdict to the workflow so it can build a job summary without
    // re-parsing this output.
    if let Ok(path) = std::env::var("GITHUB_OUTPUT") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "seed={seed}");
            let _ = writeln!(f, "crashed={}", crashed.join(" "));
            let _ = writeln!(f, "hung={}", hung.join(" "));
            let _ = writeln!(f, "failed={}", failed.join(" "));
            let _ = writeln!(f, "inconclusive={}", inconclusive.join(" "));
            let _ = writeln!(f, "panicked={}", panicked.join(" "));
        }
    }
}

enum WatchdogResult {
    Exited(i32),
    TimedOut,
    SpawnFailed(std::io::Error),
}

/// Runs a child to completion or kills it at `timeout`.
fn run_with_watchdog(mut cmd: Command, timeout: Duration) -> WatchdogResult {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return WatchdogResult::SpawnFailed(e),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return WatchdogResult::Exited(status.code().unwrap_or(-1));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!(
                        "    WATCHDOG: suite exceeded {:?}; killing the child process",
                        timeout
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return WatchdogResult::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return WatchdogResult::SpawnFailed(e),
        }
    }
}

/// Maps a child exit code onto a verdict.
///
/// Windows reports an unhandled exception as the exception code itself, which is
/// a large negative value when read as i32 (0xC0000005 for an access violation).
/// Rust's own panic exit code is 101, and the suite's own codes are small.
fn classify_exit(code: i32) -> Verdict {
    match code {
        0 => Verdict::Passed,
        report::EXIT_FAIL => Verdict::Failed,
        report::EXIT_ENV => Verdict::Inconclusive,
        101 => Verdict::Panicked, // Rust panic that escaped the suite
        c if c < 0 || c as u32 >= 0x4000_0000 => Verdict::Crashed,
        _ => Verdict::Failed,
    }
}

// =====================================================================
//                        Child: one suite
// =====================================================================

fn exec_suite(args: &[String]) {
    let parsed = parse_args(args);
    let Some(suite) = parsed.suites.first().cloned() else {
        eprintln!("exec requires a suite name");
        std::process::exit(2);
    };

    // Apply the memory cap before doing anything else, so there is no window in
    // which a bomb could balloon before the limit exists.
    if let Some(mb) = parsed.mem_cap_mb {
        match metrics::limit_own_memory(mb) {
            Ok(()) => println!("  (child committed-memory cap: {mb} MiB)"),
            Err(e) => println!("  (warning: could not apply a {mb} MiB memory cap: {e:?})"),
        }
    }

    // Every suite talks to an apartment-threaded in-process COM server, so the
    // main thread joins an STA exactly like a shell thread would.
    // Probe children re-exec this binary and must load the same DLL; the
    // default path only resolves in a plain workspace layout, not in CI.
    std::env::set_var("GAUNTLET_DLL", &parsed.dll);

    let com_initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();

    let dll_handle = match dll::Dll::load(&parsed.dll) {
        Ok(d) => d,
        Err(e) => report::env_bail(&format!("could not load {}: {e:?}", parsed.dll)),
    };

    let _ = std::fs::create_dir_all(&parsed.work_dir);
    let heartbeat = parsed.work_dir.join(format!("heartbeat-{suite}.txt"));
    let mut report = Report::new(&suite).with_heartbeat(heartbeat);

    let only = parsed.only.as_deref();

    match suite.as_str() {
        "api-misuse" => suites::api_misuse::run(&dll_handle, &mut report),
        "stream-faults" => suites::stream_faults::run(&dll_handle, &mut report),
        "render" => suites::render::run(&dll_handle, &mut report),
        "adversarial" => {
            suites::adversarial::run_svg(&dll_handle, &mut report, &parsed.work_dir, only)
        }
        "svgz" => suites::adversarial::run_svgz(&dll_handle, &mut report, only),
        "size-limits" => suites::adversarial::run_size_limits(&dll_handle, &mut report),
        "lifecycle" => {
            suites::lifecycle::run(&dll_handle, &mut report, parsed.seed, 20 * parsed.scale)
        }
        "concurrency" => suites::concurrency::run(
            &dll_handle,
            &mut report,
            parsed.seed,
            12,
            40 * parsed.scale,
        ),
        "churn" => suites::churn::run(&dll_handle, &mut report, parsed.scale),
        "breadth" => suites::breadth::run(
            &dll_handle,
            &mut report,
            &parsed.corpus,
            600 * parsed.scale,
        ),
        other => {
            eprintln!("unknown suite {other:?}");
            std::process::exit(2);
        }
    }

    let code = report.finish();

    if com_initialized {
        unsafe { CoUninitialize() };
    }
    std::process::exit(code);
}

// =====================================================================
//                       Isolated crash probes
// =====================================================================

/// Result of an out-of-process probe.
pub use suites::api_misuse::ProbeOutcome;

/// Spawns this executable in a probe mode and classifies how it ended.
///
/// Probes exist for checks that may legitimately fault the process. Running them
/// in a child means an access violation is data (an exit code) rather than the
/// end of the run. The probe writes the HRESULT it observed to stdout as
/// `PROBE-HRESULT=0x...`, which is parsed back here.
pub fn spawn_probe(mode: &str) -> std::io::Result<ProbeOutcome> {
    let exe = std::env::current_exe()?;
    let dll = std::env::var("GAUNTLET_DLL").unwrap_or_else(|_| default_dll_path());
    let output = Command::new(exe).arg(mode).arg("--dll").arg(dll).output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Surface the child's own output so the CI log shows what it observed.
    for line in stdout.lines() {
        println!("      [probe {mode}] {line}");
    }
    let hresult = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PROBE-HRESULT="))
        .and_then(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok())
        .map(|v| HRESULT(v as i32));

    let exit_code = output.status.code().unwrap_or(-1);
    let crashed = matches!(classify_exit(exit_code), Verdict::Crashed);

    Ok(ProbeOutcome { crashed, exit_code, hresult })
}

/// Calls `IThumbnailProvider::GetThumbnail` through the raw vtable with null
/// output pointers.
///
/// This is the one call in the whole gauntlet that is expected to be able to
/// fault the process, which is precisely why it lives in its own executable
/// mode. `null_hbmp` and `null_alpha` select which output pointer is null.
fn probe_null_out(args: &[String], null_hbmp: bool, null_alpha: bool) {
    use std::ffi::c_void;
    use windows::core::Interface;
    use windows::Win32::Graphics::Gdi::HBITMAP;
    use windows::Win32::UI::Shell::{IThumbnailProvider, WTS_ALPHATYPE, WTSAT_UNKNOWN};

    let parsed = parse_args(args);
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    let dll_handle = match dll::Dll::load(&parsed.dll) {
        Ok(d) => d,
        Err(e) => {
            println!("PROBE-SETUP-FAILED: could not load DLL: {e:?}");
            std::process::exit(report::EXIT_ENV);
        }
    };

    let provider = match dll_handle.create_provider() {
        Ok(p) => p,
        Err(e) => {
            println!("PROBE-SETUP-FAILED: CreateInstance failed: {e:?}");
            std::process::exit(report::EXIT_ENV);
        }
    };
    let stream = match dll::mem_stream(corpus::BASE_SVG.as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            println!("PROBE-SETUP-FAILED: mem stream: {e:?}");
            std::process::exit(report::EXIT_ENV);
        }
    };
    if let Err(e) = unsafe { provider.Initialize(&stream, 0) } {
        println!("PROBE-SETUP-FAILED: Initialize failed: {e:?}");
        std::process::exit(report::EXIT_ENV);
    }
    let thumb: IThumbnailProvider = match provider.cast() {
        Ok(t) => t,
        Err(e) => {
            println!("PROBE-SETUP-FAILED: QI for IThumbnailProvider failed: {e:?}");
            std::process::exit(report::EXIT_ENV);
        }
    };

    println!(
        "PROBE: calling GetThumbnail with phbmp={} pdwalpha={}",
        if null_hbmp { "NULL" } else { "valid" },
        if null_alpha { "NULL" } else { "valid" }
    );
    let _ = std::io::stdout().flush();

    let mut real_hbmp = HBITMAP(std::ptr::null_mut());
    let mut real_alpha = WTSAT_UNKNOWN;

    let hr = unsafe {
        type GetThumbnailFn = unsafe extern "system" fn(
            *mut c_void,
            u32,
            *mut HBITMAP,
            *mut WTS_ALPHATYPE,
        ) -> HRESULT;
        let raw = Interface::as_raw(&thumb);
        // IThumbnailProvider vtable: QueryInterface, AddRef, Release, GetThumbnail.
        let vtbl = *(raw as *const *const c_void);
        let entry = *(vtbl as *const *const c_void).add(3);
        let f: GetThumbnailFn = std::mem::transmute(entry);
        f(
            raw,
            64,
            if null_hbmp { std::ptr::null_mut() } else { &mut real_hbmp },
            if null_alpha { std::ptr::null_mut() } else { &mut real_alpha },
        )
    };

    println!("PROBE-HRESULT=0x{:08X}", hr.0 as u32);
    println!("PROBE: returned without faulting");
    let _ = std::io::stdout().flush();

    if !real_hbmp.is_invalid() {
        let _ = dll::take_bitmap(real_hbmp, real_alpha);
    }
    unsafe { CoUninitialize() };
    std::process::exit(0);
}

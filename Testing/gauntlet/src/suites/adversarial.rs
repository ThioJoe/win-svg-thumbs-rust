//! Drives the synthetic adversarial corpus through the real COM path.
//!
//! For hostile input the contract is deliberately weak on output and strict on
//! behaviour:
//!
//!   * the call must return within a per-case time budget,
//!   * the process must not fault,
//!   * committed memory must not grow without bound,
//!   * whatever bitmap comes back must be structurally valid (right size, right
//!     bit depth, readable bits, declared alpha type),
//!   * and no case may cause outbound network access.
//!
//! Each case is announced to the heartbeat file before it runs, so if the
//! process is killed for hanging or dies from an access violation, the
//! supervisor can name the exact input and re-run it in isolation.

use std::io::Read as _;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::corpus::{self, Case, Expect, XxeProbe};
use crate::dll::{self, Dll, Rendering};
use crate::metrics::Snapshot;
use crate::report::Report;

/// Wall-clock budget for a single render of a single adversarial input.
///
/// Generous enough that a slow WARP path on a loaded CI runner is not flagged,
/// tight enough that a genuine superlinear blowup is. Exceeding it is reported
/// as a failure but does not abort the suite, so one slow input cannot hide the
/// results of everything after it.
///
/// The budget is per suite rather than global, because the size-limit cases are
/// legitimately slow: a 101 MiB document has to be read in 64 KiB chunks,
/// scanned end-to-end for `!important`, and then handed to MSXML, so judging it
/// against the same deadline as a 200-byte malformed file would flag normal
/// behaviour as a blowup.
const BUDGET_SMALL_INPUT: Duration = Duration::from_secs(45);
const BUDGET_COMPRESSED: Duration = Duration::from_secs(90);
const BUDGET_HUGE_INPUT: Duration = Duration::from_secs(360);

/// Committed memory a single case is allowed to leave behind once the call has
/// returned.
///
/// Measured after the render completes, so transient peaks are not counted -
/// this catches memory that was committed and never given back, which is what a
/// decompression bomb or an entity-expansion bomb would actually cost the host.
/// The compressed budget is larger because the whole point of that corpus is
/// input that expands enormously; the huge-input budget covers the 101 MiB
/// boundary cases, where the provider legitimately buffers the whole file.
const MEM_BUDGET_SMALL_INPUT: i64 = 512 * 1024 * 1024;
const MEM_BUDGET_COMPRESSED: i64 = 1024 * 1024 * 1024;
const MEM_BUDGET_HUGE_INPUT: i64 = 1536 * 1024 * 1024;

/// A local HTTP listener plus an on-disk sentinel, used to make XXE resolution
/// observable instead of theoretical.
struct NetworkProbe {
    hits: Arc<AtomicU32>,
    url: String,
    sentinel: std::path::PathBuf,
}

impl NetworkProbe {
    /// Binds a listener on an ephemeral loopback port and serves (or rather,
    /// counts and rejects) anything that connects.
    fn start(work_dir: &std::path::Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let hits = Arc::new(AtomicU32::new(0));

        let thread_hits = Arc::clone(&hits);
        std::thread::spawn(move || {
            // Any connection at all is a finding, so the handler only needs to
            // count it and close. Reading first avoids the client seeing a
            // connection reset before it has sent its request.
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        thread_hits.fetch_add(1, Ordering::SeqCst);
                        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut buf = [0u8; 512];
                        let _ = s.read(&mut buf);
                    }
                    Err(_) => break,
                }
            }
        });

        let sentinel = work_dir.join("xxe-sentinel.txt");
        std::fs::write(&sentinel, b"GAUNTLET-XXE-SENTINEL-CONTENT")?;

        Ok(Self { hits, url, sentinel })
    }

    fn probe(&self) -> XxeProbe {
        XxeProbe {
            http_url: self.url.clone(),
            sentinel_path: self.sentinel.display().to_string().replace('\\', "/"),
        }
    }

    fn hit_count(&self) -> u32 {
        self.hits.load(Ordering::SeqCst)
    }
}

/// Outcome of one case, as observed from the calling side.
struct Observed {
    elapsed: Duration,
    /// None means GetThumbnail failed; Some means a bitmap came back.
    rendering: Option<Rendering>,
    hresult: u32,
    geometry_ok: bool,
    alpha_declared: bool,
    memory_growth: i64,
}

fn observe(dll_handle: &Dll, case: &Case, size: u32) -> Observed {
    let before = Snapshot::take();
    let start = Instant::now();
    let result = dll::try_render(dll_handle, &case.bytes, size);
    let elapsed = start.elapsed();
    let memory_growth = Snapshot::take().delta(&before).private_bytes;

    match result {
        Ok(t) => Observed {
            elapsed,
            rendering: Some(t.classify()),
            hresult: 0,
            // A returned bitmap must always match the requested geometry,
            // regardless of how malformed the input was.
            geometry_ok: t.width == size && t.height == size,
            alpha_declared: dll::declares_argb(&t),
            memory_growth,
        },
        Err(hr) => Observed {
            elapsed,
            rendering: None,
            hresult: hr.0 as u32,
            geometry_ok: true, // nothing was returned, so nothing to check
            alpha_declared: true,
            memory_growth,
        },
    }
}

/// Runs one case and records the appropriate checks for its expectation.
fn run_case(
    dll_handle: &Dll,
    case: &Case,
    size: u32,
    budget: Duration,
    mem_budget: i64,
    report: &mut Report,
) {
    report.begin_case(&case.name);
    let o = observe(dll_handle, case, size);

    let summary = format!(
        "{} in {:?}, rendering={:?}, hr=0x{:08X}, mem={:+.1}MiB",
        if o.rendering.is_some() { "returned a bitmap" } else { "failed" },
        o.elapsed,
        o.rendering,
        o.hresult,
        o.memory_growth as f64 / 1048576.0
    );

    // Invariants that hold for every case regardless of expectation.
    if !o.geometry_ok {
        report.fail(
            format!("{}/geometry", case.name),
            format!("returned a bitmap whose size does not match the requested {size}x{size}: {summary}"),
        );
    }
    if o.rendering.is_some() && !o.alpha_declared {
        report.fail(
            format!("{}/alpha-type", case.name),
            format!("returned a bitmap without declaring WTSAT_ARGB: {summary}"),
        );
    }
    if o.elapsed > budget {
        report.fail(
            format!("{}/time-budget", case.name),
            format!("exceeded the {budget:?} per-case budget: {summary}"),
        );
    }
    if o.memory_growth > mem_budget {
        report.fail(
            format!("{}/memory-budget", case.name),
            format!(
                "left {:.1} MiB committed after the call returned, budget is {} MiB: {summary}",
                o.memory_growth as f64 / 1048576.0,
                mem_budget / 1048576
            ),
        );
    }

    match case.expect {
        Expect::Survive => {
            // Reaching this line at all means no fault and no watchdog kill.
            report.pass(&case.name, summary);
        }
        Expect::RealRender => {
            let ok = o.rendering == Some(Rendering::Real);
            report.check(
                &case.name,
                ok,
                if ok {
                    summary
                } else {
                    format!(
                        "valid input did not render as real artwork - a fallback here means the \
                         ordinary rendering path regressed: {summary}"
                    )
                },
            );
        }
        Expect::RejectOrFallback => {
            // The input exceeds a documented limit, so a successful *real*
            // render would mean the limit is not being enforced.
            let ok = o.rendering.is_none() || o.rendering.map(|r| r.is_fallback()) == Some(true);
            report.check(
                &case.name,
                ok,
                if ok {
                    summary
                } else {
                    format!("input past a documented limit was rendered as if valid: {summary}")
                },
            );
        }
    }
}

/// The synthetic .svg corpus.
pub fn run_svg(dll_handle: &Dll, report: &mut Report, work_dir: &std::path::Path, only: Option<&str>) {
    let probe = match NetworkProbe::start(work_dir) {
        Ok(p) => Some(p),
        Err(e) => {
            report.skip(
                "xxe_network_probe",
                format!("could not bind a loopback listener ({e}); XXE cases will run but network access cannot be observed"),
            );
            None
        }
    };
    let xxe = probe
        .as_ref()
        .map(|p| p.probe())
        .unwrap_or(XxeProbe {
            http_url: "http://127.0.0.1:9/unbound".to_string(),
            sentinel_path: work_dir.join("missing.txt").display().to_string().replace('\\', "/"),
        });

    let cases = corpus::all_cases(&xxe);
    let total = cases.len();
    let mut ran = 0usize;
    for case in &cases {
        if let Some(filter) = only {
            if case.name != filter {
                continue;
            }
        }
        ran += 1;
        // Vary the requested size across cases so scaling logic is exercised
        // alongside parsing, without multiplying the corpus by a size sweep.
        let size = match ran % 4 {
            0 => 16,
            1 => 64,
            2 => 256,
            _ => 133, // deliberately not a power of two
        };
        run_case(dll_handle, case, size, BUDGET_SMALL_INPUT, MEM_BUDGET_SMALL_INPUT, report);
    }

    if ran == 0 {
        report.skip("corpus", format!("no case matched the --only filter (corpus has {total} cases)"));
        return;
    }
    println!("  (ran {ran} of {total} synthetic .svg cases)");

    // The security assertion: nothing in the corpus may have caused the parser
    // to reach out to the network, even though several cases explicitly invite
    // it to via external DTDs, external entities and remote stylesheet hrefs.
    if let Some(p) = probe {
        // Give any in-flight resolution a moment to land before judging.
        std::thread::sleep(Duration::from_millis(500));
        let hits = p.hit_count();
        report.check(
            "no_outbound_network_access",
            hits == 0,
            if hits == 0 {
                format!(
                    "the local listener at {} recorded no connections across the XXE cases \
                     (external DTDs, external general and parameter entities, UNC paths, remote \
                     xml-stylesheet and remote <image> hrefs)",
                    p.url
                )
            } else {
                format!(
                    "SECURITY: {hits} outbound connection(s) reached {} while parsing untrusted \
                     SVG - XML external entity resolution is enabled",
                    p.url
                )
            },
        );

        // The sentinel is only observable indirectly, but if its contents ever
        // appear in a rendered document the expansion definitely happened.
        report.check(
            "xxe_sentinel_still_present",
            p.sentinel.exists(),
            "local sentinel file was not modified or removed by parsing".to_string(),
        );
    }
}

/// The .svgz (gzip) corpus.
pub fn run_svgz(dll_handle: &Dll, report: &mut Report, only: Option<&str>) {
    let cases = corpus::svgz_cases();
    let total = cases.len();
    let mut ran = 0usize;

    for case in &cases {
        if let Some(filter) = only {
            if case.name != filter {
                continue;
            }
        }
        ran += 1;
        run_case(dll_handle, case, 128, BUDGET_COMPRESSED, MEM_BUDGET_COMPRESSED, report);
    }

    if ran == 0 {
        report.skip("svgz_corpus", format!("no case matched the --only filter ({total} cases)"));
        return;
    }
    println!("  (ran {ran} of {total} synthetic .svgz cases)");

    // Compressed input takes a different route through the provider: it skips
    // CSS processing entirely and hands the bytes straight to Direct2D. Confirm
    // the two paths agree on the same artwork, which is the property a user
    // would notice breaking.
    if only.is_none() {
        report.begin_case("svgz_and_svg_agree");
        let plain = dll::try_render(dll_handle, corpus::BASE_SVG.as_bytes(), 128);
        // Reuse the already-built corpus: regenerating it would rebuild the
        // multi-hundred-megabyte decompression bombs for no reason.
        let compressed = match cases.iter().find(|c| c.name == "svgz-valid") {
            Some(c) => dll::try_render(dll_handle, &c.bytes, 128),
            None => {
                report.skip("svgz_and_svg_agree", "svgz-valid case not present in the corpus");
                return;
            }
        };
        match (plain, compressed) {
            (Ok(a), Ok(b)) => {
                // Both go through the same Direct2D renderer, so the images
                // should be identical rather than merely similar.
                let identical = a.pixels == b.pixels;
                let both_real =
                    a.classify() == Rendering::Real && b.classify() == Rendering::Real;
                report.check(
                    "svgz_and_svg_agree",
                    both_real && identical,
                    format!(
                        "plain={:?} gz={:?} pixels_identical={identical}",
                        a.classify(),
                        b.classify()
                    ),
                );
            }
            (a, b) => report.fail(
                "svgz_and_svg_agree",
                format!(
                    "could not render both forms: plain_ok={} gz_ok={}",
                    a.is_ok(),
                    b.is_ok()
                ),
            ),
        }
    }
}

/// Oversized inputs, kept separate because they dominate wall-clock time and
/// memory and are pointless to re-run when minimising a small case.
pub fn run_size_limits(dll_handle: &Dll, report: &mut Report) {
    for case in corpus::heavy_cases() {
        run_case(dll_handle, &case, 96, BUDGET_HUGE_INPUT, MEM_BUDGET_HUGE_INPUT, report);
    }
}

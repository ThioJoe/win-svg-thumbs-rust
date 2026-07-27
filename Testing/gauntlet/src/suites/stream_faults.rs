//! Fault injection through the provider's only input: the `IStream`.
//!
//! The provider's read loop has three separate defences - a `Stat`-based fast
//! reject, a running size cap, and a fallback path for when `Stat` fails - and
//! none of them are reachable through a normal `SHCreateMemStream`, which always
//! tells the truth and always fills the buffer. Each case here breaks one
//! assumption so a specific defensive branch actually executes.
//!
//! Everything is judged on behaviour, not on a specific HRESULT: the provider is
//! free to reject, degrade, or fall back, but it must never fault, spin, or
//! commit memory proportional to a number the stream merely claimed.

use windows::core::Interface;
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, WTSAT_UNKNOWN};

use crate::corpus::BASE_SVG;
use crate::dll::{self, Dll};
use crate::metrics::Snapshot;
use crate::report::Report;
use crate::streams::{HostileStream, ReadMode, StatMode};

/// Runs one full provider round-trip against a hostile stream and reports what
/// happened, without ever treating a failure as fatal.
struct Attempt {
    initialize_ok: bool,
    initialize_hr: u32,
    render_ok: bool,
    render_hr: u32,
    rendering: Option<crate::dll::Rendering>,
}

fn attempt(dll_handle: &Dll, data: Vec<u8>, stat: StatMode, read: ReadMode, size: u32) -> Attempt {
    let provider: IInitializeWithStream = match dll_handle.create_provider() {
        Ok(p) => p,
        Err(e) => {
            return Attempt {
                initialize_ok: false,
                initialize_hr: e.code().0 as u32,
                render_ok: false,
                render_hr: 0,
                rendering: None,
            }
        }
    };
    let stream = HostileStream::new(data, stat, read);
    let init = unsafe { provider.Initialize(&stream, 0) };
    let initialize_hr = init.as_ref().err().map(|e| e.code().0 as u32).unwrap_or(0);

    let mut render_ok = false;
    let mut render_hr = 0u32;
    let mut rendering = None;
    if let Ok(thumb) = provider.cast::<IThumbnailProvider>() {
        let mut hbmp = HBITMAP(std::ptr::null_mut());
        let mut alpha = WTSAT_UNKNOWN;
        let hr = unsafe { thumb.GetThumbnail(size, &mut hbmp, &mut alpha) };
        render_hr = hr.as_ref().err().map(|e| e.code().0 as u32).unwrap_or(0);
        render_ok = hr.is_ok();
        if !hbmp.is_invalid() {
            if let Ok(t) = dll::take_bitmap(hbmp, alpha) {
                rendering = Some(t.classify());
            }
        }
    }
    Attempt {
        initialize_ok: init.is_ok(),
        initialize_hr,
        render_ok,
        render_hr,
        rendering,
    }
}

pub fn run(dll_handle: &Dll, report: &mut Report) {
    let svg = BASE_SVG.as_bytes().to_vec();

    // ---- Stat behaviour ----
    //
    // Each of these changes only what the stream *claims* its size is; the bytes
    // delivered are always the same valid SVG. A correct implementation renders
    // it in every case except the one where the claimed size exceeds the
    // documented cap.

    report.begin_case("stat_truthful_renders");
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::Truthful, 64);
    report.check(
        "stat_truthful_renders",
        a.initialize_ok && a.render_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!(
            "control case: init={} render={} rendering={:?}",
            a.initialize_ok, a.render_ok, a.rendering
        ),
    );

    report.begin_case("stat_failure_falls_back_to_chunked_read");
    // With Stat failing, the provider has no size hint at all and must rely
    // purely on its read loop. The same bytes must still produce the same image.
    let a = attempt(dll_handle, svg.clone(), StatMode::Fail, ReadMode::Truthful, 64);
    report.check(
        "stat_failure_falls_back_to_chunked_read",
        a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!(
            "init={} (hr=0x{:08X}) rendering={:?} - a failing Stat must not prevent reading",
            a.initialize_ok, a.initialize_hr, a.rendering
        ),
    );

    report.begin_case("stat_claims_u64_max_does_not_preallocate");
    // The danger here is a `Vec::with_capacity` driven by the claimed size. Watch
    // committed memory across the call: a provider that trusted this number
    // would either abort on allocation failure or commit gigabytes.
    let before = Snapshot::take();
    let a = attempt(dll_handle, svg.clone(), StatMode::Huge, ReadMode::Truthful, 64);
    let growth = Snapshot::take().delta(&before).private_bytes;
    report.check(
        "stat_claims_u64_max_does_not_preallocate",
        growth < 256 * 1024 * 1024,
        format!(
            "committed {:+.1} MiB while the stream claimed u64::MAX bytes (init={}, rendering={:?}); \
             budget is 256 MiB",
            growth as f64 / 1048576.0,
            a.initialize_ok,
            a.rendering
        ),
    );

    report.begin_case("stat_claims_zero_still_reads_data");
    // A zero-size claim must not short-circuit the read: the provider only uses
    // the value as a capacity hint, so the real bytes should still arrive.
    let a = attempt(dll_handle, svg.clone(), StatMode::Zero, ReadMode::Truthful, 64);
    report.check(
        "stat_claims_zero_still_reads_data",
        a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!(
            "init={} rendering={:?} - a zero size claim must be a hint, not a truncation",
            a.initialize_ok, a.rendering
        ),
    );

    report.begin_case("stat_over_cap_is_rejected_without_reading");
    // Claiming more than the documented 101 MiB ceiling must be rejected up
    // front, even though the actual payload is a few hundred bytes.
    let a = attempt(dll_handle, svg.clone(), StatMode::OverCap, ReadMode::Truthful, 64);
    report.check(
        "stat_over_cap_is_rejected_without_reading",
        !a.initialize_ok,
        format!(
            "init={} hr=0x{:08X} - a stream claiming 200 MiB must be rejected by the size check",
            a.initialize_ok, a.initialize_hr
        ),
    );

    report.begin_case("stat_under_reports_actual_size");
    // Stat says 1 byte, the stream delivers a full document. The pre-allocated
    // buffer is far too small, so this exercises buffer growth mid-read.
    let a = attempt(dll_handle, svg.clone(), StatMode::UnderReport, ReadMode::Truthful, 64);
    report.check(
        "stat_under_reports_actual_size",
        a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!("init={} rendering={:?}", a.initialize_ok, a.rendering),
    );

    // ---- Read behaviour ----

    report.begin_case("single_byte_reads_reassemble_correctly");
    // The legal-but-pathological case. If the loop mishandled chunk boundaries
    // the document would be corrupted and would fall back instead of rendering.
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::OneByte, 64);
    report.check(
        "single_byte_reads_reassemble_correctly",
        a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!("init={} rendering={:?}", a.initialize_ok, a.rendering),
    );

    for chunk in [3u32, 7, 4096] {
        let name = format!("short_reads_{chunk}_bytes");
        report.begin_case(&name);
        let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::Short(chunk), 64);
        report.check(
            name,
            a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
            format!("init={} rendering={:?}", a.initialize_ok, a.rendering),
        );
    }

    report.begin_case("read_failure_midway_degrades_cleanly");
    // Half a document arrives, then the stream errors. The provider keeps what
    // it has, which is malformed XML, and must degrade to a fallback rather than
    // crash on the truncated input.
    let half = svg.len() / 2;
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::FailAfter(half), 64);
    report.check(
        "read_failure_midway_degrades_cleanly",
        a.render_ok || a.render_hr != 0,
        format!(
            "init={} render={} rendering={:?} - truncated input must produce a result or a clean error",
            a.initialize_ok, a.render_ok, a.rendering
        ),
    );

    report.begin_case("read_failure_before_any_data");
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::FailAfter(0), 64);
    report.check(
        "read_failure_before_any_data",
        a.render_ok || a.render_hr != 0,
        format!(
            "init={} (hr=0x{:08X}) render={} rendering={:?}",
            a.initialize_ok, a.initialize_hr, a.render_ok, a.rendering
        ),
    );

    report.begin_case("zero_byte_reads_terminate_the_loop");
    // A stream that forever reports success with zero bytes read would hang any
    // loop that only terminated on error. This case completing at all is the
    // assertion; the supervisor's watchdog catches the failure mode.
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::ZeroForever, 64);
    // Reaching this line proves it did not spin (a hang is caught by the
    // supervisor's watchdog), but that alone is a check that can never fail.
    // The substantive assertion is that no data arrived, so the provider must
    // not have produced real artwork from it.
    report.check(
        "zero_byte_reads_terminate_the_loop",
        a.rendering != Some(crate::dll::Rendering::Real),
        format!(
            "returned without spinning; init={} render={} rendering={:?} - a stream that never \
             delivers a byte must not yield a rendered document",
            a.initialize_ok, a.render_ok, a.rendering
        ),
    );

    report.begin_case("over_reported_read_count_is_not_trusted");
    // The stream claims to have written several times more bytes than were
    // requested. A caller that trusted the count would copy out of bounds; the
    // provider must instead bail out (its slice bound catches this, and the FFI
    // guard converts the resulting panic into an HRESULT).
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::OverReport, 64);
    report.check(
        "over_reported_read_count_is_not_trusted",
        !a.initialize_ok || a.render_ok || a.render_hr != 0,
        format!(
            "survived a stream claiming more bytes read than requested: init={} (hr=0x{:08X}) \
             render={} rendering={:?}",
            a.initialize_ok, a.initialize_hr, a.render_ok, a.rendering
        ),
    );

    report.begin_case("read_that_never_sets_count_terminates");
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::NeverSetCount, 64);
    // The provider's byte counter is never updated, so from its point of view
    // nothing was read. As above, terminating is necessary but not sufficient:
    // it must also not treat the unread buffer as a document.
    report.check(
        "read_that_never_sets_count_terminates",
        a.rendering != Some(crate::dll::Rendering::Real),
        format!(
            "returned; init={} rendering={:?} - a stream that never reports a byte count must \
             not yield a rendered document",
            a.initialize_ok, a.rendering
        ),
    );

    report.begin_case("s_false_partial_read_is_treated_as_success");
    // S_FALSE from ISequentialStream::Read means "fewer bytes than requested",
    // which is a success code. Treating it as an error would truncate perfectly
    // good files - a subtle correctness bug rather than a crash.
    let a = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::SFalse, 64);
    report.check(
        "s_false_partial_read_is_treated_as_success",
        a.initialize_ok && a.rendering == Some(crate::dll::Rendering::Real),
        format!(
            "init={} rendering={:?} - S_FALSE is a success HRESULT and must not truncate the read",
            a.initialize_ok, a.rendering
        ),
    );

    // ---- Size cap enforced through the read loop ----

    report.begin_case("oversized_stream_is_capped_during_read");
    // Stat lies about the size being small, so the fast reject cannot fire and
    // only the in-loop cap can stop the transfer. Feed it 130 MiB and watch
    // committed memory: the loop must abort near the 101 MiB limit rather than
    // buffering everything.
    let big = vec![b'x'; 130 * 1024 * 1024];
    let before = Snapshot::take();
    let a = attempt(dll_handle, big, StatMode::UnderReport, ReadMode::Truthful, 64);
    let growth = Snapshot::take().delta(&before).private_bytes;
    report.check(
        "oversized_stream_is_capped_during_read",
        !a.initialize_ok && growth < 160 * 1024 * 1024,
        format!(
            "init={} (hr=0x{:08X}), committed {:+.1} MiB feeding 130 MiB through a stream that \
             claimed to be tiny - the in-loop cap must stop it",
            a.initialize_ok,
            a.initialize_hr,
            growth as f64 / 1048576.0
        ),
    );

    // ---- Leak behaviour on the error paths ----
    //
    // These two checks are deliberately separate. The first isolates one hostile
    // mode that is known to leak, so the finding has a name and a measured cost;
    // the second sweeps every *other* mode, so a new leak somewhere else cannot
    // hide behind the known one.

    report.begin_case("over_reported_read_leaks_thread_graphics_cache");
    // A stream that over-reports its byte count makes the provider index past
    // its own 64 KiB chunk buffer, which panics. The panic is caught, but
    // ffi_guard's recovery removes the thread's cached D2D/D3D-WARP resources
    // from TLS without dropping them - deliberately, since their state is
    // unknown after a panic. The next render rebuilds the chain, so every
    // triggered panic abandons a complete device chain's worth of handles.
    //
    // Warm the cache first so the resources being abandoned actually exist,
    // then measure the per-iteration cost across enough iterations that a
    // single-digit accounting wobble cannot explain the result.
    let _ = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::Truthful, 64);
    let before = Snapshot::take();
    const OVER_REPORT_ITERATIONS: i64 = 25;
    for _ in 0..OVER_REPORT_ITERATIONS {
        // Trigger the panic, then render normally so the cache is rebuilt and
        // the next iteration has something to abandon again.
        let _ = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::OverReport, 64);
        let _ = attempt(dll_handle, svg.clone(), StatMode::Truthful, ReadMode::Truthful, 64);
    }
    let d = Snapshot::take().delta(&before);
    let handles_per_iteration = d.handles as f64 / OVER_REPORT_ITERATIONS as f64;
    report.check(
        "over_reported_read_leaks_thread_graphics_cache",
        d.handles <= OVER_REPORT_ITERATIONS,
        format!(
            "after {OVER_REPORT_ITERATIONS} over-reported reads (each followed by a normal \
             render): {} = {handles_per_iteration:.1} handles leaked per triggered panic. \
             A stream that repeatedly over-reports therefore grows the host's handle count \
             without bound.",
            d.describe()
        ),
    );

    report.begin_case("repeated_hostile_streams_do_not_leak");
    // Every hostile mode except the one isolated above. A slow leak on any of
    // these error paths is invisible in a single iteration and obvious after a
    // couple of hundred.
    let before = Snapshot::take();
    for i in 0..200 {
        let (stat, read) = match i % 7 {
            0 => (StatMode::Fail, ReadMode::Truthful),
            1 => (StatMode::Huge, ReadMode::OneByte),
            2 => (StatMode::Zero, ReadMode::Short(5)),
            3 => (StatMode::OverCap, ReadMode::Truthful),
            4 => (StatMode::UnderReport, ReadMode::FailAfter(40)),
            5 => (StatMode::Truthful, ReadMode::ZeroForever),
            _ => (StatMode::Truthful, ReadMode::SFalse),
        };
        let _ = attempt(dll_handle, svg.clone(), stat, read, 64);
    }
    let d = Snapshot::take().delta(&before);
    report.check(
        "repeated_hostile_streams_do_not_leak",
        d.gdi_objects <= 8 && d.handles <= 32,
        format!("after 200 hostile-stream round-trips (excluding over-report): {}", d.describe()),
    );
}

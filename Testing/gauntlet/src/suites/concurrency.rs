//! Concurrency storm.
//!
//! The real host renders thumbnails on a pool of shell/surrogate threads, all
//! hitting the same in-process server at once. The provider's per-thread D2D
//! cache means each thread builds its own device chain, while the DLL-wide
//! reference counter, the registry-read `Once` and the module-pin flag are
//! shared. This suite puts many threads through that mix simultaneously and
//! checks the two properties that matter: nobody corrupts anybody else's output,
//! and the shared accounting still balances at the end.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};

use crate::corpus::{self, BASE_SVG};
use crate::dll::{self, Dll, Rendering};
use crate::metrics::Snapshot;
use crate::report::Report;
use crate::rng::Rng;

pub fn run(dll_handle: &Dll, report: &mut Report, seed: u64, threads: usize, per_thread: usize) {
    println!("  concurrency seed = {seed}, {threads} threads x {per_thread} renders");

    identical_work_agrees(dll_handle, report, threads, per_thread);
    mixed_workload_storm(dll_handle, report, seed, threads, per_thread);
    threads_exiting_during_active_renders(dll_handle, report);
    accounting_balances(dll_handle, report);
}

// ---------------------------------------------------------------

/// Every thread renders the same document at the same size. Because rendering
/// is deterministic, all of them must produce byte-identical output; any
/// divergence is direct evidence of cross-thread contamination in the shared
/// device state or in the DOM rewriting.
fn identical_work_agrees(dll_handle: &Dll, report: &mut Report, threads: usize, per_thread: usize) {
    report.begin_case("concurrent_identical_renders_agree");

    // Establish the expected image on this thread first.
    let expected = match dll::try_render(dll_handle, BASE_SVG.as_bytes(), 128) {
        Ok(t) => Arc::new(t.pixels),
        Err(hr) => {
            report.fail(
                "concurrent_identical_renders_agree",
                format!("baseline render failed: hr=0x{:08X}", hr.0 as u32),
            );
            return;
        }
    };

    let barrier = Arc::new(Barrier::new(threads));
    let mismatches = Arc::new(AtomicU32::new(0));
    let failures = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();

    for t in 0..threads {
        let dll_handle = *dll_handle;
        let barrier = Arc::clone(&barrier);
        let expected = Arc::clone(&expected);
        let mismatches = Arc::clone(&mismatches);
        let failures = Arc::clone(&failures);
        handles.push(std::thread::spawn(move || {
            // Alternate apartment models: the shell uses STAs, but an
            // in-process server can legitimately be called from an MTA.
            let model = if t % 3 == 2 { COINIT_MULTITHREADED } else { COINIT_APARTMENTTHREADED };
            let com_ok = unsafe { CoInitializeEx(None, model) }.is_ok();
            barrier.wait(); // maximise overlap
            for _ in 0..per_thread {
                match dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 128) {
                    Ok(got) => {
                        if got.pixels != *expected {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if com_ok {
                unsafe { CoUninitialize() };
            }
        }));
    }
    let mut panics = 0;
    for h in handles {
        if h.join().is_err() {
            panics += 1;
        }
    }

    report.check(
        "concurrent_identical_renders_agree",
        mismatches.load(Ordering::Relaxed) == 0
            && failures.load(Ordering::Relaxed) == 0
            && panics == 0,
        format!(
            "{} threads x {} renders: {} pixel mismatches (cross-thread contamination), \
             {} render failures, {} panics",
            threads,
            per_thread,
            mismatches.load(Ordering::Relaxed),
            failures.load(Ordering::Relaxed),
            panics
        ),
    );
}

// ---------------------------------------------------------------

/// A deliberately messy storm: different documents, different sizes, valid and
/// invalid input, all at once. Each thread verifies its own results locally, so
/// a thread that received another thread's image is detected immediately.
fn mixed_workload_storm(
    dll_handle: &Dll,
    report: &mut Report,
    seed: u64,
    threads: usize,
    per_thread: usize,
) {
    report.begin_case("mixed_workload_storm");

    // Each document is a solid fill in a distinct colour, so a thread can check
    // the centre pixel against the colour it asked for. Cross-contamination
    // therefore fails locally and loudly instead of being averaged away.
    let palette: Vec<(u8, u8, u8)> = vec![
        (0xE8, 0x11, 0x23),
        (0x00, 0x78, 0xD7),
        (0x10, 0x89, 0x3E),
        (0xFF, 0xB9, 0x00),
        (0x88, 0x17, 0x98),
        (0x00, 0xB7, 0xC3),
    ];

    let wrong_colour = Arc::new(AtomicU32::new(0));
    let hard_failures = Arc::new(AtomicU32::new(0));
    let total_renders = Arc::new(AtomicU64::new(0));
    let max_latency_us = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(threads));

    let mut handles = Vec::new();
    for t in 0..threads {
        let dll_handle = *dll_handle;
        let palette = palette.clone();
        let barrier = Arc::clone(&barrier);
        let wrong_colour = Arc::clone(&wrong_colour);
        let hard_failures = Arc::clone(&hard_failures);
        let total_renders = Arc::clone(&total_renders);
        let max_latency_us = Arc::clone(&max_latency_us);
        let thread_seed = seed.wrapping_add(t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);

        handles.push(std::thread::spawn(move || {
            let model = if t % 3 == 2 { COINIT_MULTITHREADED } else { COINIT_APARTMENTTHREADED };
            let com_ok = unsafe { CoInitializeEx(None, model) }.is_ok();
            let mut rng = Rng::new(thread_seed);
            barrier.wait();

            for _ in 0..per_thread {
                // One in five renders feeds deliberately broken input, so the
                // error path runs concurrently with the success path. That mix
                // is what would expose a device context left in a bad state by
                // a failing render on another thread.
                if rng.chance(20) {
                    let junk: Vec<u8> = (0..rng.range(0, 400))
                        .map(|_| (rng.next_u64() & 0xFF) as u8)
                        .collect();
                    let _ = dll::try_render(&dll_handle, &junk, rng.range(1, 512));
                    total_renders.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let (r, g, b) = *rng.pick(&palette);
                let svg = format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#{r:02x}{g:02x}{b:02x}"/></svg>"##
                );
                let size = *rng.pick(&[16u32, 32, 48, 64, 96, 128, 256]);

                let start = Instant::now();
                let result = dll::try_render(&dll_handle, svg.as_bytes(), size);
                let us = start.elapsed().as_micros() as u64;
                max_latency_us.fetch_max(us, Ordering::Relaxed);
                total_renders.fetch_add(1, Ordering::Relaxed);

                match result {
                    Ok(thumb) => {
                        let p = thumb.pixel(size / 2, size / 2);
                        // BGRA out, RGB in.
                        let ok = (p[0] as i32 - b as i32).abs() <= 24
                            && (p[1] as i32 - g as i32).abs() <= 24
                            && (p[2] as i32 - r as i32).abs() <= 24
                            && p[3] > 200;
                        if !ok {
                            wrong_colour.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        hard_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if com_ok {
                unsafe { CoUninitialize() };
            }
        }));
    }

    let mut panics = 0;
    for h in handles {
        if h.join().is_err() {
            panics += 1;
        }
    }

    let wrong = wrong_colour.load(Ordering::Relaxed);
    let failed = hard_failures.load(Ordering::Relaxed);
    let total = total_renders.load(Ordering::Relaxed);
    report.check(
        "mixed_workload_storm",
        wrong == 0 && failed == 0 && panics == 0,
        format!(
            "{total} renders across {threads} threads: {wrong} returned the wrong colour \
             (cross-thread contamination), {failed} valid renders failed, {panics} threads panicked, \
             worst latency {:.1} ms",
            max_latency_us.load(Ordering::Relaxed) as f64 / 1000.0
        ),
    );
}

// ---------------------------------------------------------------

/// Threads that exit while other threads are actively rendering.
///
/// This is the concurrent form of the scenario that caused the original crash:
/// a thread carrying a live TLS D2D/WARP cache terminates while the DLL stays
/// loaded and other threads keep using their own caches. The fix leaks the
/// exiting thread's cache on purpose; this checks that doing so repeatedly,
/// under load, neither crashes nor disturbs the threads still working.
fn threads_exiting_during_active_renders(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("threads_exit_while_others_render");

    let keep_going = Arc::new(AtomicBool::new(true));
    let steady_failures = Arc::new(AtomicU32::new(0));
    let steady_renders = Arc::new(AtomicU64::new(0));

    // Long-lived workers that render continuously throughout.
    let mut steady = Vec::new();
    for _ in 0..4 {
        let dll_handle = *dll_handle;
        let keep_going = Arc::clone(&keep_going);
        let steady_failures = Arc::clone(&steady_failures);
        let steady_renders = Arc::clone(&steady_renders);
        steady.push(std::thread::spawn(move || {
            let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            while keep_going.load(Ordering::Relaxed) {
                match dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 96) {
                    Ok(t) => {
                        if t.classify() != Rendering::Real {
                            steady_failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        steady_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
                steady_renders.fetch_add(1, Ordering::Relaxed);
            }
            if com_ok {
                unsafe { CoUninitialize() };
            }
        }));
    }

    // Meanwhile, spin up and tear down short-lived threads that each build a
    // TLS cache and then exit.
    let mut churned = 0u32;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && churned < 200 {
        let mut batch = Vec::new();
        for _ in 0..8 {
            let dll_handle = *dll_handle;
            batch.push(std::thread::spawn(move || {
                let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
                let ok = dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 64).is_ok();
                if com_ok {
                    unsafe { CoUninitialize() };
                }
                ok
                // Thread exits here with its TLS D2D cache still populated.
            }));
        }
        for h in batch {
            let _ = h.join();
            churned += 1;
        }
    }

    keep_going.store(false, Ordering::Relaxed);
    let mut panics = 0;
    for h in steady {
        if h.join().is_err() {
            panics += 1;
        }
    }

    let failures = steady_failures.load(Ordering::Relaxed);
    report.check(
        "threads_exit_while_others_render",
        failures == 0 && panics == 0,
        format!(
            "{churned} short-lived rendering threads created and exited while 4 steady threads \
             completed {} renders: {failures} steady-render failures, {panics} panics",
            steady_renders.load(Ordering::Relaxed)
        ),
    );
}

// ---------------------------------------------------------------

fn accounting_balances(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("reference_count_balanced_after_storm");
    // Every object created above has been dropped. If the DLL's reference count
    // did not come back to zero, some path is leaking a reference under
    // concurrency - which would silently pin the DLL in the surrogate forever.
    report.check(
        "reference_count_balanced_after_storm",
        dll_handle.unload_allowed(),
        format!(
            "DllCanUnloadNow=0x{:08X} after the concurrency storm (expected S_OK)",
            dll_handle.can_unload().0 as u32
        ),
    );

    report.begin_case("concurrent_gdi_objects_released");
    // Bitmaps are created on worker threads and deleted by the gauntlet. A
    // per-render GDI leak would be invisible in a single render and obvious
    // after a few thousand.
    let before = Snapshot::take();
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let dll_handle = *dll_handle;
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            barrier.wait();
            for _ in 0..100 {
                let _ = dll::try_render(&dll_handle, corpus::BASE_SVG.as_bytes(), 64);
            }
            if com_ok {
                unsafe { CoUninitialize() };
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let d = Snapshot::take().delta(&before);
    report.check(
        "concurrent_gdi_objects_released",
        d.gdi_objects <= 16,
        format!("after 800 concurrent renders: {}", d.describe()),
    );
}

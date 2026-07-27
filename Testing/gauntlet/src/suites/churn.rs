//! Thread churn and resource retention.
//!
//! This suite exists to measure a design decision rather than to find a crash.
//!
//! src/lib.rs wraps the per-thread D2D cache in `ManuallyDrop` so that a thread
//! exiting never tears down its Direct2D-on-WARP chain: doing so would run under
//! the loader lock and is what caused the original dllhost.exe crash. The code
//! comment justifies leaking with "threads that render thumbnails are
//! shell/surrogate pool threads that in practice live until process exit, so
//! nothing meaningful accumulates".
//!
//! That is an assumption about the host's threading behaviour, not a property of
//! the code, and it is exactly the kind of assumption that quietly stops holding
//! - a future Windows build could recycle surrogate threads far more
//! aggressively. So rather than trusting it, this suite measures two things:
//!
//!   * **Stable pool**: a fixed set of threads doing a lot of work. Retention
//!     here must be flat, because no thread ever exits. This is the shape the
//!     design assumes, and a leak here would be a plain bug.
//!
//!   * **Thread churn**: many one-shot rendering threads. Every one of them
//!     leaks its cache by design, so growth is *expected*; what is measured is
//!     the slope in bytes retained per completed rendering thread, so the cost
//!     is a documented number instead of an unknown.
//!
//! The thresholds below are intentionally generous. Their job is to catch an
//! order-of-magnitude regression - a cache that suddenly retains ten times as
//! much, or a stable pool that starts leaking at all - not to police normal
//! allocator variance.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

use crate::corpus::BASE_SVG;
use crate::dll::{self, Dll};
use crate::metrics::Snapshot;
use crate::report::Report;

/// Per-thread retention budget. A D2D device context, a D2D device, a factory
/// and the underlying D3D11 WARP device is a heavyweight chain; several MiB per
/// thread is plausible. This ceiling flags an order-of-magnitude regression.
const RETENTION_BUDGET_PER_THREAD: f64 = 12.0 * 1024.0 * 1024.0;

/// Growth allowed across the whole stable-pool phase. No thread exits, so the
/// caches are reused and growth should be near zero; this only absorbs
/// allocator noise and lazily-loaded system resources.
const STABLE_POOL_GROWTH_BUDGET: i64 = 96 * 1024 * 1024;

pub fn run(dll_handle: &Dll, report: &mut Report, scale: usize) {
    stable_pool(dll_handle, report, scale);
    thread_churn(dll_handle, report, scale);
    gdi_and_handle_retention(dll_handle, report, scale);
}

// ---------------------------------------------------------------
//                        Stable pool
// ---------------------------------------------------------------

fn stable_pool(dll_handle: &Dll, report: &mut Report, scale: usize) {
    let threads = 8usize;
    let per_thread = 250 * scale;
    report.begin_case("stable_pool_retention");

    // Warm up first: the first render on each thread creates the D2D chain and
    // pins the module, which is a one-off cost that must not be counted as
    // growth. Measuring from after the warm-up is what makes a real leak visible.
    let warmup = Arc::new(Barrier::new(threads + 1));
    let start_work = Arc::new(Barrier::new(threads + 1));
    let done = Arc::new(Barrier::new(threads + 1));
    let failures = Arc::new(AtomicU32::new(0));
    let latencies_us = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));

    let mut handles = Vec::new();
    for _ in 0..threads {
        let dll_handle = *dll_handle;
        let warmup = Arc::clone(&warmup);
        let start_work = Arc::clone(&start_work);
        let done = Arc::clone(&done);
        let failures = Arc::clone(&failures);
        let latencies_us = Arc::clone(&latencies_us);
        handles.push(std::thread::spawn(move || {
            let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();

            // Warm-up: build this thread's cache.
            for _ in 0..5 {
                if dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 96).is_err() {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            warmup.wait();
            start_work.wait();

            let mut local = Vec::with_capacity(per_thread);
            for _ in 0..per_thread {
                let t0 = Instant::now();
                if dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 96).is_err() {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                local.push(t0.elapsed().as_micros() as u64);
            }
            if let Ok(mut all) = latencies_us.lock() {
                all.extend_from_slice(&local);
            }

            done.wait();
            // Threads stay alive until after the final measurement, which is the
            // whole point of this phase.
            if com_ok {
                unsafe { CoUninitialize() };
            }
        }));
    }

    warmup.wait();
    let baseline = Snapshot::take();
    start_work.wait();
    done.wait();
    let after = Snapshot::take();

    let d = after.delta(&baseline);
    let total = threads * per_thread;

    // Latency percentiles, reported as regression evidence rather than enforced:
    // a shared CI runner is far too noisy for a hard latency threshold to mean
    // anything, but a 10x shift is still visible in the logs.
    let (p50, p95, p99) = percentiles(&latencies_us.lock().map(|v| v.clone()).unwrap_or_default());

    report.check(
        "stable_pool_retention",
        d.private_bytes < STABLE_POOL_GROWTH_BUDGET && failures.load(Ordering::Relaxed) == 0,
        format!(
            "{threads} threads x {per_thread} renders ({total} total, no thread exited): {} | \
             latency p50={:.1}ms p95={:.1}ms p99={:.1}ms | {} render failures | budget {} MiB",
            d.describe(),
            p50 as f64 / 1000.0,
            p95 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            failures.load(Ordering::Relaxed),
            STABLE_POOL_GROWTH_BUDGET / 1048576
        ),
    );

    for h in handles {
        let _ = h.join();
    }

    report.begin_case("stable_pool_thread_count_returns_to_baseline");
    let final_snapshot = Snapshot::take();
    report.check(
        "stable_pool_thread_count_returns_to_baseline",
        final_snapshot.threads <= baseline.threads + 2,
        format!(
            "threads before={} after joining={} (worker threads must actually terminate)",
            baseline.threads, final_snapshot.threads
        ),
    );
}

// ---------------------------------------------------------------
//                        Thread churn
// ---------------------------------------------------------------

fn thread_churn(dll_handle: &Dll, report: &mut Report, scale: usize) {
    // Stages chosen so the slope can be fitted across two orders of magnitude.
    // Each stage's threads render and then exit, leaking their cache by design.
    let stages: Vec<usize> = if scale > 1 {
        vec![1, 8, 64, 256, 1024]
    } else {
        vec![1, 8, 64, 256]
    };

    report.begin_case("thread_churn_retention");

    // Warm up on this thread so process-wide one-off costs (module loads, the
    // registry read, the module pin) are already paid before measuring.
    for _ in 0..5 {
        let _ = dll::try_render(dll_handle, BASE_SVG.as_bytes(), 64);
    }

    let baseline = Snapshot::take();
    println!("      churn baseline: {}", baseline.describe());

    let mut completed_threads = 0usize;
    let mut samples: Vec<(usize, i64)> = Vec::new();
    let mut render_failures = 0u32;

    for stage in &stages {
        // Bounded concurrency: the point is total threads *completed*, not how
        // many run at once, and 1024 simultaneous WARP devices would exhaust the
        // runner for reasons that have nothing to do with retention.
        let mut remaining = *stage;
        while remaining > 0 {
            let batch = remaining.min(16);
            let mut handles = Vec::new();
            for _ in 0..batch {
                let dll_handle = *dll_handle;
                handles.push(std::thread::spawn(move || {
                    let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
                    let ok = dll::try_render(&dll_handle, BASE_SVG.as_bytes(), 64).is_ok();
                    if com_ok {
                        unsafe { CoUninitialize() };
                    }
                    ok
                    // Thread exits here holding a live, deliberately leaked
                    // D2D/WARP cache in its TLS.
                }));
            }
            for h in handles {
                match h.join() {
                    Ok(true) => {}
                    _ => render_failures += 1,
                }
            }
            remaining -= batch;
            completed_threads += batch;
        }

        let now = Snapshot::take();
        let d = now.delta(&baseline);
        samples.push((completed_threads, d.private_bytes));
        println!(
            "      after {completed_threads:>5} completed rendering threads: {} ({:.2} MiB/thread)",
            d.describe(),
            d.private_bytes as f64 / completed_threads as f64 / 1048576.0
        );
    }

    // Slope from the largest sample, which is the least sensitive to fixed
    // start-up costs that a small sample would misattribute to per-thread cost.
    let (threads_done, retained) = *samples.last().expect("at least one stage");
    let per_thread = retained as f64 / threads_done as f64;

    report.check(
        "thread_churn_retention",
        per_thread < RETENTION_BUDGET_PER_THREAD && render_failures == 0,
        format!(
            "{threads_done} one-shot rendering threads retained {:.1} MiB total = {:.2} MiB per \
             thread (budget {:.0} MiB/thread); {render_failures} render failures. Retention is \
             BY DESIGN - the TLS D2D cache is deliberately leaked when a rendering thread exits, \
             because destroying it would run under the loader lock. This check exists to keep that \
             cost bounded and visible, not to demand zero.",
            retained as f64 / 1048576.0,
            per_thread / 1048576.0,
            RETENTION_BUDGET_PER_THREAD / 1048576.0
        ),
    );

    // A second, independent signal: retention should be roughly linear in the
    // number of threads. Super-linear growth would mean each thread costs more
    // than the last, which points at something worse than a fixed-size leak.
    report.begin_case("thread_churn_growth_is_not_superlinear");
    if samples.len() >= 2 {
        let (n_small, r_small) = samples[samples.len() / 2];
        let (n_big, r_big) = *samples.last().unwrap();
        let slope_small = r_small as f64 / n_small as f64;
        let slope_big = r_big as f64 / n_big as f64;
        // Allow a wide margin: fixed costs inflate the small-sample slope, so
        // the later slope being *lower* is normal and fine.
        let ratio = if slope_small > 0.0 { slope_big / slope_small } else { 0.0 };
        report.check(
            "thread_churn_growth_is_not_superlinear",
            ratio < 4.0,
            format!(
                "per-thread retention at {n_small} threads = {:.2} MiB, at {n_big} threads = \
                 {:.2} MiB (ratio {ratio:.2}, budget 4.0)",
                slope_small / 1048576.0,
                slope_big / 1048576.0
            ),
        );
    } else {
        report.skip("thread_churn_growth_is_not_superlinear", "not enough stages to fit a slope");
    }

    // Threads must actually be gone. If the count keeps climbing, something is
    // keeping exited threads' stacks alive, which would be a far more serious
    // leak than the intended cache retention.
    report.begin_case("churned_threads_actually_terminate");
    let now = Snapshot::take();
    report.check(
        "churned_threads_actually_terminate",
        now.threads <= baseline.threads + 4,
        format!(
            "thread count baseline={} after {} one-shot threads={}",
            baseline.threads, completed_threads, now.threads
        ),
    );

    // The DLL pins itself in memory on first render and is never unmapped, so
    // the module count must be stable regardless of churn.
    report.begin_case("module_count_stable_across_churn");
    report.check(
        "module_count_stable_across_churn",
        (now.modules as i64 - baseline.modules as i64).abs() <= 4,
        format!(
            "modules baseline={} after churn={} (the DLL self-pins, so this must not climb)",
            baseline.modules, now.modules
        ),
    );
}

// ---------------------------------------------------------------
//                  GDI / handle retention
// ---------------------------------------------------------------

fn gdi_and_handle_retention(dll_handle: &Dll, report: &mut Report, scale: usize) {
    report.begin_case("gdi_objects_do_not_accumulate");
    // Every render allocates an HBITMAP via CreateDIBSection and hands ownership
    // to the caller. The gauntlet deletes each one, so the GDI count must be
    // flat. GDI objects are a per-process quota (10,000 by default), so a leak
    // here would eventually break the whole surrogate, not just this provider.
    let renders = 400 * scale;
    let before = Snapshot::take();
    let mut failures = 0u32;
    for i in 0..renders {
        // Alternate valid and invalid input so the error path - which allocates
        // a fallback bitmap and may abandon a partially built one - is covered
        // as heavily as the success path.
        let input: &[u8] = if i % 3 == 0 { b"not an svg" } else { BASE_SVG.as_bytes() };
        if dll::try_render(dll_handle, input, 64).is_err() && i % 3 != 0 {
            failures += 1;
        }
    }
    let d = Snapshot::take().delta(&before);
    report.check(
        "gdi_objects_do_not_accumulate",
        d.gdi_objects <= 16 && d.handles <= 64 && failures == 0,
        format!(
            "after {renders} renders (one third deliberately invalid): {} | {failures} unexpected \
             failures",
            d.describe()
        ),
    );

    report.begin_case("repeated_failed_renders_do_not_leak");
    // Failure paths are the classic place for a leaked bitmap, because the
    // success path's cleanup is the one that gets tested by hand.
    let before = Snapshot::take();
    for _ in 0..(300 * scale) {
        let _ = dll::try_render(dll_handle, b"<svg", 64);
        let _ = dll::try_render(dll_handle, &[], 64);
        let _ = dll::try_render(dll_handle, BASE_SVG.as_bytes(), 0); // rejected size
    }
    let d = Snapshot::take().delta(&before);
    report.check(
        "repeated_failed_renders_do_not_leak",
        d.gdi_objects <= 16 && d.handles <= 64,
        format!("after {} failing renders: {}", 900 * scale, d.describe()),
    );
}

/// p50 / p95 / p99 of a latency sample, in microseconds.
fn percentiles(values: &[u64]) -> (u64, u64, u64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let at = |p: f64| -> u64 {
        let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    (at(0.50), at(0.95), at(0.99))
}

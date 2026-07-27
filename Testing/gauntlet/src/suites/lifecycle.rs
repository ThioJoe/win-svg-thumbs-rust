//! Randomized COM lifecycle state machine.
//!
//! The suites elsewhere exercise fixed sequences. This one generates random but
//! reproducible orderings of the operations a COM host can legally perform, on
//! the theory that the dangerous states are the ones nobody thought to write a
//! test for: a provider released before its factory, a `LockServer` held across
//! a render, `DllCanUnloadNow` polled from a thread that never rendered, an
//! object created on one thread and released on another.
//!
//! Every run prints its seed. A failure is reproduced exactly with
//! `gauntlet exec lifecycle --seed <n>`, and the operation transcript is printed
//! alongside the failure so the offending sequence is visible without a rerun.

use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IClassFactory, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, WTSAT_UNKNOWN};

use crate::corpus::BASE_SVG;
use crate::dll::{self, Dll};
use crate::metrics::Snapshot;
use crate::report::Report;
use crate::rng::Rng;

/// One operation in a generated sequence.
#[derive(Debug, Clone, Copy)]
enum Op {
    CreateFactory,
    CreateProvider,
    InitializeProvider,
    RenderProvider,
    QueryUnsupported,
    ReleaseFactory,
    ReleaseProvider,
    LockServer(bool),
    PollCanUnload,
    Sleep(u32),
}

/// Live objects held by one simulated client.
struct World {
    factories: Vec<IClassFactory>,
    providers: Vec<(IInitializeWithStream, bool)>, // (object, initialized)
    outstanding_locks: i32,
}

impl World {
    fn new() -> Self {
        Self { factories: Vec::new(), providers: Vec::new(), outstanding_locks: 0 }
    }

    /// Releases everything and balances any locks still held, so the DLL's
    /// reference count returns to zero between sequences.
    ///
    /// A sequence can legally release every factory while a server lock is still
    /// outstanding, which leaves this with a lock to balance and no factory to
    /// balance it through. `LockServer` maps onto the DLL-global reference
    /// counter rather than onto the individual factory, so any factory instance
    /// can release it - create one if the sequence left none behind. Skipping
    /// the unlock instead would strand a reference and make every later
    /// "reference count returned to zero" assertion fail for the wrong reason.
    fn wind_down(&mut self, dll_handle: &Dll) {
        if self.outstanding_locks > 0 {
            let factory = match self.factories.first() {
                Some(f) => Some(f.clone()),
                None => dll_handle.class_factory().ok(),
            };
            if let Some(f) = factory {
                while self.outstanding_locks > 0 {
                    let _ = unsafe { f.LockServer(false) };
                    self.outstanding_locks -= 1;
                }
            }
        }
        self.providers.clear();
        self.factories.clear();
    }
}

fn apply(dll_handle: &Dll, world: &mut World, op: Op, rng: &mut Rng) -> Result<(), String> {
    match op {
        Op::CreateFactory => {
            // Cap the population so a long sequence cannot exhaust memory.
            if world.factories.len() < 8 {
                let f = dll_handle
                    .class_factory()
                    .map_err(|e| format!("class_factory failed: {e:?}"))?;
                world.factories.push(f);
            }
        }
        Op::CreateProvider => {
            if world.providers.len() < 16 {
                // Prefer creating through an existing factory, which is what a
                // real host does; fall back to a fresh one.
                let provider = if let Some(f) = world.factories.first() {
                    unsafe { f.CreateInstance(None::<&windows::core::IUnknown>) }
                        .map_err(|e| format!("CreateInstance failed: {e:?}"))?
                } else {
                    dll_handle
                        .create_provider()
                        .map_err(|e| format!("create_provider failed: {e:?}"))?
                };
                world.providers.push((provider, false));
            }
        }
        Op::InitializeProvider => {
            if !world.providers.is_empty() {
                let idx = rng.below(world.providers.len());
                let stream = dll::mem_stream(BASE_SVG.as_bytes())
                    .map_err(|e| format!("mem_stream failed: {e:?}"))?;
                let (provider, initialized) = &mut world.providers[idx];
                let hr = unsafe { provider.Initialize(&stream, 0) };
                // Re-initialising must fail; initialising a fresh object must
                // succeed. Anything else is a contract violation.
                match (*initialized, hr.is_ok()) {
                    (false, true) => *initialized = true,
                    (true, false) => {}
                    (false, false) => {
                        return Err(format!(
                            "Initialize failed on a fresh provider: 0x{:08X}",
                            hr.err().map(|e| e.code().0 as u32).unwrap_or(0)
                        ))
                    }
                    (true, true) => {
                        return Err("Initialize succeeded twice on the same provider".to_string())
                    }
                }
            }
        }
        Op::RenderProvider => {
            if !world.providers.is_empty() {
                let idx = rng.below(world.providers.len());
                let (provider, initialized) = &world.providers[idx];
                let initialized = *initialized;
                if let Ok(thumb) = provider.cast::<IThumbnailProvider>() {
                    let size = *rng.pick(&[1u32, 16, 32, 64, 128, 256, 512, 4096, 4097, 0]);
                    let mut hbmp = HBITMAP(std::ptr::null_mut());
                    let mut alpha = WTSAT_UNKNOWN;
                    let hr = unsafe { thumb.GetThumbnail(size, &mut hbmp, &mut alpha) };
                    if !hbmp.is_invalid() {
                        let _ = dll::take_bitmap(hbmp, alpha);
                    }
                    // An uninitialised provider must never render successfully.
                    if !initialized && hr.is_ok() {
                        return Err(
                            "GetThumbnail succeeded on a provider that was never initialised"
                                .to_string(),
                        );
                    }
                }
            }
        }
        Op::QueryUnsupported => {
            if let Some((provider, _)) = world.providers.first() {
                let unsupported: windows::core::Result<windows::Win32::System::Com::IPersist> =
                    provider.cast();
                if unsupported.is_ok() {
                    return Err("QI succeeded for an interface the provider does not implement".to_string());
                }
            }
        }
        Op::ReleaseFactory => {
            if !world.factories.is_empty() {
                // Release a random one, not necessarily the newest: out-of-order
                // release is the interesting case.
                let idx = rng.below(world.factories.len());
                world.factories.remove(idx);
            }
        }
        Op::ReleaseProvider => {
            if !world.providers.is_empty() {
                let idx = rng.below(world.providers.len());
                world.providers.remove(idx);
            }
        }
        Op::LockServer(lock) => {
            if let Some(f) = world.factories.first() {
                let hr = unsafe { f.LockServer(lock) };
                if hr.is_err() {
                    return Err(format!("LockServer({lock}) failed: {hr:?}"));
                }
                if lock {
                    world.outstanding_locks += 1;
                } else if world.outstanding_locks > 0 {
                    world.outstanding_locks -= 1;
                }
            }
        }
        Op::PollCanUnload => {
            let hr = dll_handle.can_unload();
            // The invariant that actually matters: while this client holds live
            // objects or locks, the DLL must never claim it is safe to unload.
            let holding = !world.factories.is_empty()
                || !world.providers.is_empty()
                || world.outstanding_locks > 0;
            if holding && hr == S_OK {
                return Err(format!(
                    "DllCanUnloadNow returned S_OK while {} factories, {} providers and {} locks \
                     were outstanding - COM would unmap the DLL under live objects",
                    world.factories.len(),
                    world.providers.len(),
                    world.outstanding_locks
                ));
            }
        }
        Op::Sleep(ms) => std::thread::sleep(std::time::Duration::from_millis(ms as u64)),
    }
    Ok(())
}

/// Picks the next operation, restricted to sequences a *correct* COM client
/// could actually produce.
///
/// The one restriction that matters is `LockServer(false)`: it is only legal
/// when a matching `LockServer(true)` is outstanding. Emitting it unbalanced
/// would be a bug in the client, not the server, and it corrupts the DLL's
/// global reference count in a way that makes every later assertion in the
/// sequence meaningless. That specific abuse is worth testing, but as a
/// deliberate, deterministic check rather than as random noise here - see
/// `api_misuse::unbalanced_unlock_corrupts_reference_count`.
fn random_op(rng: &mut Rng, outstanding_locks: i32) -> Op {
    match rng.below(100) {
        0..=9 => Op::CreateFactory,
        10..=29 => Op::CreateProvider,
        30..=47 => Op::InitializeProvider,
        48..=67 => Op::RenderProvider,
        68..=71 => Op::QueryUnsupported,
        72..=78 => Op::ReleaseFactory,
        79..=87 => Op::ReleaseProvider,
        88..=91 => Op::LockServer(true),
        92..=94 => {
            if outstanding_locks > 0 {
                Op::LockServer(false)
            } else {
                // Nothing to unlock, so do something else legal instead.
                Op::PollCanUnload
            }
        }
        95..=98 => Op::PollCanUnload,
        _ => Op::Sleep(1),
    }
}

/// Runs one random sequence on the current thread, returning the transcript on
/// failure so the exact ordering is visible in CI output.
fn run_sequence(dll_handle: &Dll, seed: u64, length: usize) -> Result<(), (Vec<Op>, String)> {
    let mut rng = Rng::new(seed);
    let mut world = World::new();
    let mut transcript = Vec::with_capacity(length);

    for _ in 0..length {
        let op = random_op(&mut rng, world.outstanding_locks);
        transcript.push(op);
        if let Err(e) = apply(dll_handle, &mut world, op, &mut rng) {
            world.wind_down(dll_handle);
            return Err((transcript, e));
        }
    }
    world.wind_down(dll_handle);
    Ok(())
}

pub fn run(dll_handle: &Dll, report: &mut Report, seed: u64, sequences: usize) {
    println!("  lifecycle seed = {seed} (replay with: gauntlet exec lifecycle --seed {seed})");

    // ---- Single-threaded STA sequences ----
    report.begin_case("sta_random_sequences");
    let mut failures = Vec::new();
    for i in 0..sequences {
        let seq_seed = seed.wrapping_add(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        report.begin_case(&format!("sta_sequence_{i}"));
        if let Err((transcript, why)) = run_sequence(dll_handle, seq_seed, 60) {
            failures.push(format!("sequence {i} (seed {seq_seed}): {why}\n      ops: {transcript:?}"));
            if failures.len() >= 3 {
                break;
            }
        }
    }
    report.check(
        "sta_random_sequences",
        failures.is_empty(),
        if failures.is_empty() {
            format!("{sequences} random STA lifecycle sequences held every COM invariant")
        } else {
            format!("{} sequence(s) violated a COM invariant:\n      {}", failures.len(), failures.join("\n      "))
        },
    );

    // ---- Concurrent apartments ----
    //
    // The provider is registered with ThreadingModel=Apartment, so the shell
    // only ever calls it on an STA. But an in-process server can be reached from
    // an MTA thread too, and the DLL's globals (the reference counter, the
    // registry-read Once, the module-pin flag) are shared across every
    // apartment. Running both concurrently is what exercises that sharing.
    report.begin_case("mixed_apartment_sequences");
    let mut handles = Vec::new();
    for t in 0..6u64 {
        let dll_handle = *dll_handle;
        let thread_seed = seed.wrapping_add(0x1000).wrapping_add(t.wrapping_mul(0x9E37_79B9));
        handles.push(std::thread::spawn(move || {
            // Half the threads join an MTA instead of creating an STA.
            let mta = t % 2 == 1;
            let model = if mta { COINIT_MULTITHREADED } else { COINIT_APARTMENTTHREADED };
            let com_ok = unsafe { CoInitializeEx(None, model) }.is_ok();
            let result = run_sequence(&dll_handle, thread_seed, 40);
            if com_ok {
                unsafe { CoUninitialize() };
            }
            (mta, result)
        }));
    }
    let mut concurrent_failures = Vec::new();
    for h in handles {
        match h.join() {
            Ok((mta, Err((transcript, why)))) => concurrent_failures.push(format!(
                "{} thread: {why}\n      ops: {transcript:?}",
                if mta { "MTA" } else { "STA" }
            )),
            Ok((_, Ok(()))) => {}
            Err(_) => concurrent_failures.push("a lifecycle thread panicked".to_string()),
        }
    }
    report.check(
        "mixed_apartment_sequences",
        concurrent_failures.is_empty(),
        if concurrent_failures.is_empty() {
            "6 concurrent STA/MTA lifecycle threads held every COM invariant".to_string()
        } else {
            format!("{}", concurrent_failures.join("\n      "))
        },
    );

    // ---- Cross-thread object release ----
    //
    // Creating an object on one thread and releasing it on another is legal for
    // an in-process server and is what happens when the shell hands work between
    // pool threads. It is also the pattern most likely to expose thread-affine
    // state that was assumed to be thread-local.
    report.begin_case("object_released_on_a_different_thread");
    let created = std::thread::spawn({
        let dll_handle = *dll_handle;
        move || {
            let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
            let provider = dll_handle.create_provider();
            let boxed = provider.map(|p| {
                // A failure to build the stream is not interesting here; the
                // point of this case is the cross-thread handoff, and the
                // receiving thread reports whether the object still works.
                if let Ok(stream) = dll::mem_stream(BASE_SVG.as_bytes()) {
                    let _ = unsafe { p.Initialize(&stream, 0) };
                }
                // Hand the raw pointer across the thread boundary; the receiving
                // thread takes ownership of the reference.
                Interface::into_raw(p) as usize
            });
            if com_ok {
                unsafe { CoUninitialize() };
            }
            boxed
        }
    })
    .join();

    match created {
        Ok(Ok(raw)) => {
            let provider = unsafe {
                IInitializeWithStream::from_raw(raw as *mut std::ffi::c_void)
            };
            // Render it here, on a thread that never created it.
            let rendered = provider
                .cast::<IThumbnailProvider>()
                .ok()
                .map(|t| {
                    let mut hbmp = HBITMAP(std::ptr::null_mut());
                    let mut alpha = WTSAT_UNKNOWN;
                    let hr = unsafe { t.GetThumbnail(64, &mut hbmp, &mut alpha) };
                    if !hbmp.is_invalid() {
                        let _ = dll::take_bitmap(hbmp, alpha);
                    }
                    hr.is_ok()
                })
                .unwrap_or(false);
            drop(provider); // released on the wrong thread, deliberately
            report.check(
                "object_released_on_a_different_thread",
                rendered,
                format!(
                    "provider created and initialised on thread A, rendered and released on thread B \
                     (render_ok={rendered})"
                ),
            );
        }
        Ok(Err(e)) => report.fail("object_released_on_a_different_thread", format!("creation failed: {e:?}")),
        Err(_) => report.fail("object_released_on_a_different_thread", "creating thread panicked"),
    }

    // ---- Reference accounting returns to zero ----
    report.begin_case("reference_count_returns_to_zero");
    // After every sequence above has wound down, the DLL must agree that
    // nothing is outstanding. If it does not, either the provider leaked a
    // reference or LockServer accounting drifted - both of which stop the
    // surrogate from ever unloading the DLL.
    report.check(
        "reference_count_returns_to_zero",
        dll_handle.unload_allowed(),
        format!(
            "DllCanUnloadNow=0x{:08X} after all lifecycle sequences completed (expected S_OK)",
            dll_handle.can_unload().0 as u32
        ),
    );

    report.begin_case("no_handle_growth_across_sequences");
    // A slow leak of GDI objects or kernel handles across many object lifetimes
    // is invisible in any single sequence.
    let before = Snapshot::take();
    for i in 0..40 {
        let _ = run_sequence(dll_handle, seed.wrapping_add(0x5000 + i), 30);
    }
    let d = Snapshot::take().delta(&before);
    report.check(
        "no_handle_growth_across_sequences",
        d.gdi_objects <= 16 && d.handles <= 64,
        format!("after 40 further lifecycle sequences: {}", d.describe()),
    );
}

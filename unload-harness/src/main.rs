//! Standalone crash-repro harness for the "DLL unloaded / thread exited while a
//! thread's TLS still caches live D2D-on-WARP graphics state" bug.
//!
//! It deliberately does NOT link against the win_svg_thumbs crate. It loads the
//! built DLL with LoadLibrary and talks to it through DllGetClassObject /
//! IClassFactory / IInitializeWithStream / IThumbnailProvider, exactly like the
//! COM runtime in Explorer's thumbnail surrogate (dllhost.exe) does.
//!
//! Modes (first CLI arg):
//!
//! * `control` - N renders on one worker thread. The DLL is never unloaded and
//!   no thread that holds a TLS cache ever exits; the process ends with
//!   TerminateProcess so no teardown path runs at all. This must always pass;
//!   it proves that rendering itself is stable in this environment.
//!
//! * `threadexit` - N iterations; each renders on a fresh worker thread which
//!   then exits normally while the DLL stays loaded. Rust runs the thread's TLS
//!   destructors from a PE TLS callback during DLL_THREAD_DETACH, i.e. under
//!   the loader lock, so the DLL's cached D2D/D3D-WARP chain is torn down there.
//!
//! * `processexit` - renders on the MAIN thread (so the exiting thread itself
//!   owns a TLS cache: Windows runs TLS destructors at process exit only for
//!   the thread that calls ExitProcess) and also on a parked worker (whose
//!   destructors are expected NOT to run), then exits via ExitProcess
//!   (std::process::exit) with the DLL still loaded. Exercises the
//!   DLL_PROCESS_DETACH path at process termination.
//!
//! * `freelibrary` - N iterations; each renders on a worker which then parks
//!   with its COM STA still alive (like an idle dllhost STA thread), while the
//!   main thread checks DllCanUnloadNow and calls FreeLibrary. This mimics what
//!   CoFreeUnusedLibraries does in the surrogate during idle periods and is the
//!   theorized crash site: the module (and its d2d1/d3d11/dxgi imports) can be
//!   unmapped while the parked worker still owns a live WARP device chain.
//!
//! Exit codes are classified so the CI wrapper can distinguish outcomes:
//!   0   = survived (all iterations completed, rendering verified pixel-exact)
//!   2   = usage error
//!   3   = unload precondition failed (DllCanUnloadNow was not S_OK)
//!   10  = harness/environment failure (e.g. the DLL fell back to a non-D2D
//!         bitmap, so the hazardous state was never created - inconclusive)
//!   12  = FreeLibrary itself failed (no unload was exercised - inconclusive)
//!   101 = Rust panic (harness failure)
//!   negative NTSTATUS = a real crash (unhandled exception)
//! A loader-lock deadlock shows up as a hang, which the CI wrapper kills and
//! reports distinctly (124).

use std::ffi::c_void;
use std::io::Write as _;
use std::sync::mpsc;

use windows::core::*;
use windows::Win32::Foundation::{FreeLibrary, HMODULE, S_OK};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IClassFactory, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, SHCreateMemStream, WTSAT_UNKNOWN};

/// Must match CLSID_SVG_THUMBNAIL_PROVIDER in src/lib.rs.
const CLSID_SVG_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0xa884a812_58fd_47d5_bda6_4fab4fabdcd9);

const TEST_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="4" y="4" width="56" height="56" fill="#2d89ef"/><circle cx="32" cy="32" r="14" fill="#ffb900"/></svg>"##;

const THUMB_SIZE: u32 = 256;

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
type DllCanUnloadNowFn = unsafe extern "system" fn() -> HRESULT;

/// Print a line and flush immediately so the last words before a crash or a
/// kill are preserved in the redirected log file.
fn say(msg: &str) {
    println!("{msg}");
    let _ = std::io::stdout().flush();
}

/// Harness/environment failure: the hazardous state could not be created, so
/// the run is inconclusive rather than a survival or a crash. Exit code 10 is
/// classified separately from crashes by the CI wrapper.
fn fail_env(msg: &str) -> ! {
    say(&format!("HARNESS-INCONCLUSIVE: {msg}"));
    std::process::exit(10);
}

/// Verifies the returned bitmap actually contains the rendered TEST_SVG, i.e.
/// the DLL's real D2D-on-WARP pipeline ran and populated this thread's TLS
/// cache. Guards against a vacuous pass: on D2D failure the DLL silently
/// returns a fallback bitmap (red-X SVG or plain GDI black square) and no
/// hazardous graphics state would exist at all.
///
/// Sample points are chosen to be orientation-proof (same expectation whether
/// the DIB is stored top-down or bottom-up):
///   (128,128) center of the yellow circle  -> #ffb900, opaque
///   ( 32, 32) inside the blue rect         -> #2d89ef, opaque
///   (  4,  4) outside the rect             -> transparent
fn validate_rendered_pixels(hbmp: HBITMAP) {
    unsafe {
        let mut bmp = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut BITMAP as *mut c_void),
        );
        if got == 0 || bmp.bmBits.is_null() {
            fail_env("GetObjectW failed or bitmap has no accessible bits");
        }
        let w = bmp.bmWidth as usize;
        let h = bmp.bmHeight.unsigned_abs() as usize;
        if w != THUMB_SIZE as usize || h != THUMB_SIZE as usize {
            fail_env(&format!("unexpected bitmap size {w}x{h}"));
        }
        let stride = bmp.bmWidthBytes as usize;
        let px = |x: usize, y: usize| -> [u8; 4] {
            let p = (bmp.bmBits as *const u8).add(y * stride + x * 4);
            [*p, *p.add(1), *p.add(2), *p.add(3)] // B, G, R, A
        };
        let near = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 16;

        let center = px(128, 128); // yellow circle #ffb900
        let rect = px(32, 32); // blue rect #2d89ef
        let corner = px(4, 4); // outside the rect: transparent

        let center_ok =
            near(center[0], 0x00) && near(center[1], 0xB9) && near(center[2], 0xFF) && center[3] >= 250;
        let rect_ok =
            near(rect[0], 0xEF) && near(rect[1], 0x89) && near(rect[2], 0x2D) && rect[3] >= 250;
        let corner_ok = corner[3] <= 10;

        if !(center_ok && rect_ok && corner_ok) {
            fail_env(&format!(
                "thumbnail pixels do not match the test SVG (D2D path did not run; \
                 fallback bitmap suspected). center(BGRA)={center:?} rect={rect:?} corner={corner:?}"
            ));
        }
    }
}

fn load_dll(path: &str) -> (HMODULE, DllGetClassObjectFn, DllCanUnloadNowFn) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let module = LoadLibraryW(PCWSTR(wide.as_ptr()))
            .unwrap_or_else(|e| panic!("LoadLibraryW({path}) failed: {e:?}"));
        let gco = GetProcAddress(module, s!("DllGetClassObject"))
            .expect("DllGetClassObject export not found");
        let cun = GetProcAddress(module, s!("DllCanUnloadNow"))
            .expect("DllCanUnloadNow export not found");
        (
            module,
            std::mem::transmute::<unsafe extern "system" fn() -> isize, DllGetClassObjectFn>(gco),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, DllCanUnloadNowFn>(cun),
        )
    }
}

/// Renders one thumbnail on the current thread through the DLL's COM
/// interfaces, populating this thread's TLS D2D/WARP cache inside the DLL.
/// All COM objects are created and released on this thread, so afterwards the
/// DLL's own reference counting (DLL_REFERENCES) is back to zero.
fn render_one(get_class_object: DllGetClassObjectFn) {
    unsafe {
        let mut factory_ptr: *mut c_void = std::ptr::null_mut();
        let hr = get_class_object(
            &CLSID_SVG_THUMBNAIL_PROVIDER,
            &IClassFactory::IID,
            &mut factory_ptr,
        );
        assert!(hr.is_ok(), "DllGetClassObject failed: {hr:?}");
        let factory = IClassFactory::from_raw(factory_ptr);

        let init: IInitializeWithStream = factory
            .CreateInstance(None::<&IUnknown>)
            .expect("IClassFactory::CreateInstance failed");

        let stream = SHCreateMemStream(Some(TEST_SVG)).expect("SHCreateMemStream failed");
        init.Initialize(&stream, 0).expect("IInitializeWithStream::Initialize failed");

        let thumb: IThumbnailProvider = init.cast().expect("QI for IThumbnailProvider failed");
        let mut hbmp = HBITMAP(std::ptr::null_mut());
        let mut alpha = WTSAT_UNKNOWN;
        thumb
            .GetThumbnail(THUMB_SIZE, &mut hbmp, &mut alpha)
            .expect("IThumbnailProvider::GetThumbnail failed");
        assert!(!hbmp.is_invalid(), "GetThumbnail returned a null HBITMAP");
        validate_rendered_pixels(hbmp);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        // factory / init / thumb are dropped (Released) here, on this thread.
    }
}

/// N renders on a single worker; no unload, no caching thread ever exits.
/// Ends with TerminateProcess so not even process-exit teardown runs.
fn mode_control(dll_path: &str, iterations: u32) {
    let (_module, gco, _cun) = load_dll(dll_path);
    let (done_tx, done_rx) = mpsc::channel::<u32>();
    let _worker = std::thread::spawn(move || {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .expect("CoInitializeEx failed");
        }
        for i in 1..=iterations {
            render_one(gco);
            done_tx.send(i).expect("main thread went away");
        }
        // Keep the thread (and its TLS cache) alive until the process dies.
        std::thread::park();
    });

    for _ in 1..=iterations {
        let i = done_rx.recv().expect("worker thread died");
        say(&format!("iter {i}: rendered ok"));
    }
    say("control complete; hard-exiting via TerminateProcess (no teardown paths exercised)");
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), 0);
    }
    unreachable!("TerminateProcess returned");
}

/// N iterations of render-on-a-thread-that-then-exits, with the DLL loaded the
/// whole time. The TLS destructors for the DLL's cached D2D/WARP chain run at
/// DLL_THREAD_DETACH, under the loader lock, on the exiting worker.
fn mode_threadexit(dll_path: &str, iterations: u32) {
    let (_module, gco, _cun) = load_dll(dll_path);
    for i in 1..=iterations {
        let worker = std::thread::spawn(move || {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .expect("CoInitializeEx failed");
            }
            render_one(gco);
            unsafe { CoUninitialize() };
            say(&format!(
                "iter {i}: worker rendered; exiting thread now (TLS destructors run under loader lock)"
            ));
        });
        worker.join().expect("worker panicked");
        say(&format!("iter {i}: worker joined ok"));
    }
}

/// Let the process exit normally (ExitProcess) with the DLL still loaded while
/// TLS caches are live on two kinds of threads:
///   - the MAIN thread, which renders here itself: Windows runs TLS destructors
///     at process exit only for the thread that initiates termination, so this
///     is the one thread whose destructor actually fires (under the loader
///     lock, after all other threads have already been terminated);
///   - a parked worker, whose destructors are expected to be skipped entirely.
fn mode_processexit(dll_path: &str) {
    let (_module, gco, _cun) = load_dll(dll_path);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let _worker = std::thread::spawn(move || {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .expect("CoInitializeEx failed");
        }
        render_one(gco);
        done_tx.send(()).expect("main thread went away");
        std::thread::park();
    });
    done_rx.recv().expect("worker thread died");
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .expect("CoInitializeEx failed on main thread");
    }
    render_one(gco);
    say("rendered on main thread and parked worker; exiting via ExitProcess (main thread's TLS destructor runs during process teardown)...");
    std::process::exit(0);
}

/// The theorized crash site: render on a worker that stays parked with its STA
/// alive, then unload the DLL from the main thread, exactly like the surrogate
/// does after DllCanUnloadNow says S_OK during an idle period.
fn mode_freelibrary(dll_path: &str, iterations: u32) {
    for i in 1..=iterations {
        let (module, gco, cun) = load_dll(dll_path);

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let (unpark_tx, unpark_rx) = mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .expect("CoInitializeEx failed");
            }
            render_one(gco);
            done_tx.send(()).expect("main thread went away");
            // Park with the STA still alive and the TLS cache still populated,
            // like an idle dllhost STA thread.
            let _ = unpark_rx.recv();
            unsafe { CoUninitialize() };
        });

        done_rx.recv().expect("worker thread died before signaling");
        let hr = unsafe { cun() };
        say(&format!(
            "iter {i}: rendered on parked worker; DllCanUnloadNow = 0x{:08X} ({})",
            hr.0 as u32,
            if hr == S_OK {
                "S_OK - COM accounting says unload is safe"
            } else {
                "NOT S_OK - unload would be blocked"
            }
        ));
        // Realism gate: a correct COM host (CoFreeUnusedLibraries) only unloads
        // after DllCanUnloadNow returns S_OK. If the DLL ever says otherwise,
        // fail distinctly instead of forcing an invalid unload, so this variant
        // can never produce a misleading "reproduction".
        if hr != S_OK {
            say(&format!(
                "iter {i}: DllCanUnloadNow blocked the unload; a real host would stop here. \
                 Exiting with code 3 (premise not met) instead of forcing an invalid FreeLibrary."
            ));
            std::process::exit(3);
        }
        say(&format!("iter {i}: calling FreeLibrary (theorized crash site)..."));
        let free_result = unsafe { FreeLibrary(module) };
        if let Err(e) = free_result {
            // Without a successful FreeLibrary no unload was exercised at all;
            // report that distinctly instead of pretending the iteration passed.
            say(&format!(
                "iter {i}: FreeLibrary FAILED ({e:?}) - unload was not exercised, exiting with code 12"
            ));
            std::process::exit(12);
        }
        // FreeLibrary success only means the reference count was decremented.
        // Record whether the module is actually still mapped: with the unfixed
        // DLL it should be unmapped here (refcounts are balanced 1:1); with the
        // fixed DLL it stays mapped because the DLL pins itself on first render.
        let base_name: Vec<u16> = std::path::Path::new(dll_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let still_mapped = unsafe { GetModuleHandleW(PCWSTR(base_name.as_ptr())) }.is_ok();
        say(&format!(
            "iter {i}: FreeLibrary succeeded; module {} after unload request",
            if still_mapped {
                "STILL MAPPED (self-pinned)"
            } else {
                "UNMAPPED"
            }
        ));
        unpark_tx.send(()).expect("worker went away");
        worker.join().expect("worker panicked");
        say(&format!("iter {i}: ok"));
    }
}

fn default_dll_path() -> String {
    // The DLL is built into target/release; the harness itself may live in
    // target/debug, so resolve relative to the target/ directory.
    let exe = std::env::current_exe().expect("current_exe failed");
    let target = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot locate target dir");
    target
        .join("release")
        .join("win_svg_thumbs_x64.dll")
        .display()
        .to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let iterations: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
    let dll_path = args.get(3).cloned().unwrap_or_else(default_dll_path);

    say(&format!(
        "unload_harness: mode={mode} iterations={iterations} dll={dll_path}"
    ));

    match mode {
        "control" => mode_control(&dll_path, iterations),
        "threadexit" => mode_threadexit(&dll_path, iterations),
        "processexit" => mode_processexit(&dll_path),
        "freelibrary" => mode_freelibrary(&dll_path, iterations),
        _ => {
            eprintln!(
                "usage: unload_harness <control|threadexit|processexit|freelibrary> [iterations] [dll_path]"
            );
            std::process::exit(2);
        }
    }
    say("ALL ITERATIONS COMPLETED OK");
}

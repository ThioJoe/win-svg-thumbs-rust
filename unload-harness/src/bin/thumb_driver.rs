//! Real-shell thumbnail activity driver for the long-haul COM Surrogate repro.
//!
//! Unlike unload_harness (which loads the DLL in-process), this tool requests
//! thumbnails through the genuine Windows Shell path - IShellItemImageFactory
//! and IThumbnailCache - so the registered SVG thumbnail provider is activated
//! the way Explorer activates it: out of process, inside the thumbnail
//! extraction COM Surrogate (dllhost.exe), with the provider's real COM object
//! lifetime, STA threads, TLS graphics caches and idle unload behavior.
//!
//! One invocation = one activity burst, then exit. The orchestrator alternates
//! bursts with genuine idle periods so the surrogate performs its natural
//! cleanup (thread wind-down, CoFreeUnusedLibraries, process exit). Exiting
//! between bursts also matters: it is exactly what Explorer's clients do, and
//! it drops every reference the driver holds on surrogate objects.
//!
//! Per burst:
//!   1. copy a rotating slice of the SVG corpus into a fresh burst directory
//!      (fresh paths defeat the thumbnail cache and mimic new files arriving),
//!   2. enumerate the directory and request a thumbnail for every file,
//!   3. rename every file and request thumbnails again at a different size,
//!   4. on even bursts use IShellItemImageFactory::GetImage(SIIGBF_THUMBNAILONLY),
//!      on odd bursts use IThumbnailCache::GetThumbnail(WTS_EXTRACT|WTS_FORCEEXTRACTION).
//!
//! The driver never loads the provider DLL itself and verifies that the shell
//! did not secretly load it in-process (isolation check).
//!
//! Exit codes:
//!   0  = burst completed, at least one thumbnail was produced
//!   20 = every thumbnail request failed (environment cannot exercise the path)
//!   21 = provider DLL found loaded IN THIS PROCESS (process isolation is not
//!        in effect; results would not test the dllhost.exe path)
//!   2  = usage / setup error

use std::io::Write as _;
use std::path::{Path, PathBuf};

use windows::core::*;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IBindCtx, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, IThumbnailCache, ISharedBitmap,
    LocalThumbnailCache, SHCreateItemFromParsingName, SIIGBF, SIIGBF_BIGGERSIZEOK,
    SIIGBF_THUMBNAILONLY, WTS_EXTRACT, WTS_FLAGS, WTS_FORCEEXTRACTION,
};

fn say(msg: &str) {
    println!("{msg}");
    let _ = std::io::stdout().flush();
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

enum Mode {
    ImageFactory,
    ThumbCache,
}

/// Requests one thumbnail through IShellItemImageFactory (the path used by
/// file-open dialogs and most shell views). SIIGBF_THUMBNAILONLY guarantees a
/// real thumbnail-provider extraction rather than an icon fallback.
fn request_via_image_factory(path: &str, size: i32) -> std::result::Result<(), HRESULT> {
    unsafe {
        let w = wide(path);
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None::<&IBindCtx>)
                .map_err(|e| e.code())?;
        let flags = SIIGBF(SIIGBF_THUMBNAILONLY.0 | SIIGBF_BIGGERSIZEOK.0);
        let hbmp = factory
            .GetImage(SIZE { cx: size, cy: size }, flags)
            .map_err(|e| e.code())?;
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        Ok(())
    }
}

/// Requests one thumbnail through the shell's local thumbnail cache object -
/// the same machinery Explorer uses - with WTS_FORCEEXTRACTION so a fresh
/// extraction happens even if a cache entry exists.
fn request_via_thumb_cache(cache: &IThumbnailCache, path: &str, size: u32) -> std::result::Result<(), HRESULT> {
    unsafe {
        let w = wide(path);
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None::<&IBindCtx>)
                .map_err(|e| e.code())?;
        let mut shared: Option<ISharedBitmap> = None;
        cache
            .GetThumbnail(
                &item,
                size,
                WTS_FLAGS(WTS_EXTRACT.0 | WTS_FORCEEXTRACTION.0),
                Some(&mut shared),
                None,
                None,
            )
            .map_err(|e| e.code())?;
        // `shared` (the ISharedBitmap) is released on drop.
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: thumb_driver <svg_source_dir> <work_dir> <burst_index> [files_per_burst=120]");
        std::process::exit(2);
    }
    let source_dir = PathBuf::from(&args[1]);
    let work_dir = PathBuf::from(&args[2]);
    let burst_index: usize = args[3].parse().expect("burst_index must be a number");
    let files_per_burst: usize = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    let mode = if burst_index % 2 == 0 { Mode::ImageFactory } else { Mode::ThumbCache };
    let mode_name = match mode {
        Mode::ImageFactory => "IShellItemImageFactory",
        Mode::ThumbCache => "IThumbnailCache(FORCEEXTRACTION)",
    };
    say(&format!(
        "thumb_driver: burst {burst_index} mode={mode_name} files={files_per_burst} source={}",
        source_dir.display()
    ));

    // Collect the corpus (sorted for determinism) and take a rotating slice so
    // different bursts touch different files.
    let mut corpus: Vec<PathBuf> = std::fs::read_dir(&source_dir)
        .expect("cannot read svg source dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |e| e.eq_ignore_ascii_case("svg")))
        .collect();
    corpus.sort();
    if corpus.is_empty() {
        eprintln!("no .svg files in {}", source_dir.display());
        std::process::exit(2);
    }
    let start = (burst_index * files_per_burst) % corpus.len();
    let slice: Vec<PathBuf> = corpus
        .iter()
        .cycle()
        .skip(start)
        .take(files_per_burst.min(corpus.len()))
        .cloned()
        .collect();

    // Fresh burst directory: new paths mean new thumbnail-cache identities.
    let burst_dir = work_dir.join(format!("burst_{burst_index:04}"));
    let _ = std::fs::remove_dir_all(&burst_dir);
    std::fs::create_dir_all(&burst_dir).expect("cannot create burst dir");
    for f in &slice {
        let dest = burst_dir.join(f.file_name().unwrap());
        std::fs::copy(f, &dest).expect("copy failed");
    }

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .expect("CoInitializeEx failed");
    }

    let cache: Option<IThumbnailCache> = match mode {
        Mode::ThumbCache => Some(unsafe {
            CoCreateInstance(&LocalThumbnailCache, None, CLSCTX_INPROC_SERVER)
                .expect("CoCreateInstance(LocalThumbnailCache) failed")
        }),
        Mode::ImageFactory => None,
    };

    let mut ok: u64 = 0;
    let mut fail: u64 = 0;
    let mut first_errors: Vec<String> = Vec::new();

    let mut request = |path: &Path, size: i32, ok: &mut u64, fail: &mut u64, errs: &mut Vec<String>| {
        let p = path.to_string_lossy().into_owned();
        let res = match &cache {
            None => request_via_image_factory(&p, size),
            Some(c) => request_via_thumb_cache(c, &p, size as u32),
        };
        match res {
            Ok(()) => *ok += 1,
            Err(hr) => {
                *fail += 1;
                if errs.len() < 5 {
                    errs.push(format!("{} (size {size}): 0x{:08X}", path.file_name().unwrap().to_string_lossy(), hr.0 as u32));
                }
            }
        }
    };

    // Pass 1: enumerate the burst directory like a shell view would and request
    // a small thumbnail for every file.
    let mut files: Vec<PathBuf> = std::fs::read_dir(&burst_dir)
        .expect("cannot enumerate burst dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    files.sort();
    for f in &files {
        request(f, 96, &mut ok, &mut fail, &mut first_errors);
    }

    // Rename every file (a new shell item identity, like user file management)
    // and request thumbnails again at a larger size.
    let mut renamed: Vec<PathBuf> = Vec::new();
    for f in &files {
        let stem = f.file_stem().unwrap().to_string_lossy().into_owned();
        let new_path = f.with_file_name(format!("{stem}_r{burst_index}.svg"));
        if std::fs::rename(f, &new_path).is_ok() {
            renamed.push(new_path);
        }
    }
    for f in &renamed {
        request(f, 256, &mut ok, &mut fail, &mut first_errors);
    }

    unsafe { CoUninitialize() };

    for e in &first_errors {
        say(&format!("thumb_driver: sample failure: {e}"));
    }

    // Isolation check: the provider must have run in dllhost.exe, never here.
    let dll_in_proc =
        unsafe { GetModuleHandleW(PCWSTR(wide("win_svg_thumbs_x64.dll").as_ptr())) }.is_ok();
    if dll_in_proc {
        say("thumb_driver: DRIVER-INPROC-CONTAMINATION - provider DLL is loaded in the driver process; the shell did not isolate extraction into dllhost.exe");
        std::process::exit(21);
    }

    if ok == 0 {
        say(&format!(
            "thumb_driver: DRIVER-ALL-FAILED - 0 of {} thumbnail requests succeeded",
            ok + fail
        ));
        std::process::exit(20);
    }
    say(&format!(
        "thumb_driver: DRIVER-BURST-OK burst={burst_index} mode={mode_name} ok={ok} fail={fail}"
    ));
}

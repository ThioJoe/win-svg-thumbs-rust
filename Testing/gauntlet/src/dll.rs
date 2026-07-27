//! Loading and driving the provider DLL through its real COM surface.
//!
//! Like `Testing/unload-harness`, the gauntlet deliberately does NOT link against the
//! `win_svg_thumbs` crate. It loads the built DLL with `LoadLibraryW` and goes
//! in through `DllGetClassObject` / `IClassFactory` / `IInitializeWithStream` /
//! `IThumbnailProvider`, which is exactly how Explorer's thumbnail surrogate
//! activates it. That means these tests exercise the shipping binary - including
//! its exports, its self-pinning behaviour and its DLL-global reference counting
//! - rather than a statically linked copy of the source.

use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::{
    E_FAIL, E_OUTOFMEMORY, E_UNEXPECTED, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{IClassFactory, IStream};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_UNKNOWN};

/// Must match CLSID_SVG_THUMBNAIL_PROVIDER in src/lib.rs.
pub const CLSID_SVG_THUMBNAIL_PROVIDER: GUID =
    GUID::from_u128(0xa884a812_58fd_47d5_bda6_4fab4fabdcd9);

pub type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
pub type DllCanUnloadNowFn = unsafe extern "system" fn() -> HRESULT;

/// A loaded provider DLL plus its resolved exports.
#[derive(Clone, Copy)]
pub struct Dll {
    pub module: HMODULE,
    pub get_class_object: DllGetClassObjectFn,
    pub can_unload_now: DllCanUnloadNowFn,
}

// The raw HMODULE and function pointers are process-wide and immutable once
// resolved, so a `Dll` handle can be shared across the worker threads that the
// concurrency and churn suites spin up.
unsafe impl Send for Dll {}
unsafe impl Sync for Dll {}

impl Dll {
    pub fn load(path: &str) -> Result<Self> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let module = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
            let gco = GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| {
                Error::new(E_FAIL, "DllGetClassObject export not found")
            })?;
            let cun = GetProcAddress(module, s!("DllCanUnloadNow"))
                .ok_or_else(|| Error::new(E_FAIL, "DllCanUnloadNow export not found"))?;
            Ok(Self {
                module,
                get_class_object: std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    DllGetClassObjectFn,
                >(gco),
                can_unload_now: std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    DllCanUnloadNowFn,
                >(cun),
            })
        }
    }

    pub fn can_unload(&self) -> HRESULT {
        unsafe { (self.can_unload_now)() }
    }

    /// True when the DLL's own COM accounting says no objects or server locks
    /// are outstanding.
    pub fn unload_allowed(&self) -> bool {
        self.can_unload() == S_OK
    }

    pub fn unload_blocked(&self) -> bool {
        self.can_unload() == S_FALSE
    }

    /// Asks the DLL for a class factory, the same call COM makes.
    pub fn class_factory(&self) -> Result<IClassFactory> {
        unsafe {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            (self.get_class_object)(
                &CLSID_SVG_THUMBNAIL_PROVIDER,
                &IClassFactory::IID,
                &mut ptr,
            )
            .ok()?;
            if ptr.is_null() {
                return Err(Error::new(E_UNEXPECTED, "DllGetClassObject returned S_OK and null"));
            }
            Ok(IClassFactory::from_raw(ptr))
        }
    }

    /// Raw form used by the API-misuse suite so it can pass deliberately bad
    /// arguments (null pointers, wrong CLSIDs) that the typed wrapper forbids.
    pub fn get_class_object_raw(
        &self,
        clsid: *const GUID,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        unsafe { (self.get_class_object)(clsid, iid, out) }
    }

    /// Creates an uninitialised provider object.
    pub fn create_provider(&self) -> Result<IInitializeWithStream> {
        let factory = self.class_factory()?;
        unsafe { factory.CreateInstance(None::<&IUnknown>) }
    }
}

/// Wraps SHCreateMemStream so callers get a Result instead of an Option.
pub fn mem_stream(bytes: &[u8]) -> Result<IStream> {
    // SHCreateMemStream returns null on allocation failure. An empty slice is
    // legal and produces a valid zero-length stream, which several suites rely
    // on, so the empty case must not be special-cased away here.
    unsafe {
        windows::Win32::UI::Shell::SHCreateMemStream(Some(bytes))
            .ok_or_else(|| Error::new(E_OUTOFMEMORY, "SHCreateMemStream returned null"))
    }
}

/// A thumbnail read back out of the HBITMAP the provider returned.
///
/// The GDI object is deleted as soon as the pixels are copied, so suites can
/// hold thousands of these without leaking handles.
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    /// BGRA, top-down, exactly `width * height * 4` bytes.
    pub pixels: Vec<u8>,
    pub alpha: WTS_ALPHATYPE,
}

impl Thumb {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2], self.pixels[i + 3]]
    }

    pub fn is_fully_transparent(&self) -> bool {
        self.pixels.chunks_exact(4).all(|p| p[3] == 0)
    }

    pub fn is_uniform(&self) -> bool {
        match self.pixels.chunks_exact(4).next() {
            None => true,
            Some(first) => self.pixels.chunks_exact(4).all(|p| p == first),
        }
    }

    /// Fraction of pixels with any opacity at all.
    pub fn coverage(&self) -> f64 {
        let total = (self.width * self.height) as f64;
        if total == 0.0 {
            return 0.0;
        }
        let opaque = self.pixels.chunks_exact(4).filter(|p| p[3] > 0).count();
        opaque as f64 / total
    }

    /// Classifies what the provider actually produced.
    ///
    /// The provider silently substitutes a fallback thumbnail when rendering
    /// fails, so a suite that only checked "GetThumbnail returned S_OK" would
    /// pass even if every single render had failed. Detecting the two fallback
    /// shapes is what makes "valid fixtures must never fall back" enforceable.
    pub fn classify(&self) -> Rendering {
        if self.pixels.is_empty() {
            return Rendering::Empty;
        }
        // Last-resort fallback: CreateDIBSection filled with solid opaque black.
        if self.pixels.chunks_exact(4).all(|p| p == [0x00, 0x00, 0x00, 0xFF]) {
            return Rendering::BlackSquareFallback;
        }
        if self.is_fully_transparent() {
            return Rendering::Transparent;
        }
        // Red-X fallback: a thin pair of red diagonals on transparent. Every
        // visible pixel is red-dominant and coverage is very low.
        let mut visible = 0usize;
        let mut red = 0usize;
        for p in self.pixels.chunks_exact(4) {
            if p[3] > 16 {
                visible += 1;
                // BGRA: strongly red means high R, low G and B.
                if p[2] > 150 && p[1] < 90 && p[0] < 90 {
                    red += 1;
                }
            }
        }
        if visible > 0 && red * 100 >= visible * 95 && self.coverage() < 0.25 {
            return Rendering::RedXFallback;
        }
        Rendering::Real
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rendering {
    /// Genuine artwork.
    Real,
    /// The provider's red-X "broken file" SVG.
    RedXFallback,
    /// The provider's last-resort solid black bitmap.
    BlackSquareFallback,
    /// Rendered, but nothing is visible.
    Transparent,
    Empty,
}

impl Rendering {
    pub fn is_fallback(self) -> bool {
        matches!(self, Rendering::RedXFallback | Rendering::BlackSquareFallback)
    }
}

/// Copies an HBITMAP's pixels into a `Thumb` and deletes the GDI object.
///
/// Takes ownership of `hbmp`: the handle is always deleted, including on the
/// error paths, so a malformed bitmap cannot leak a GDI object.
pub fn take_bitmap(hbmp: HBITMAP, alpha: WTS_ALPHATYPE) -> Result<Thumb> {
    struct Owned(HBITMAP);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(self.0 .0));
                }
            }
        }
    }
    let owned = Owned(hbmp);

    unsafe {
        let mut bmp = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(owned.0 .0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut BITMAP as *mut c_void),
        );
        if got == 0 {
            return Err(Error::new(E_FAIL, "GetObjectW failed on the returned HBITMAP"));
        }
        if bmp.bmBits.is_null() {
            return Err(Error::new(
                E_FAIL,
                "returned HBITMAP is not a DIB section (no accessible bits)",
            ));
        }
        if bmp.bmBitsPixel != 32 {
            return Err(Error::new(
                E_FAIL,
                format!("expected a 32bpp bitmap, got {}bpp", bmp.bmBitsPixel),
            ));
        }
        let width = bmp.bmWidth.unsigned_abs();
        let height = bmp.bmHeight.unsigned_abs();
        let stride = bmp.bmWidthBytes as usize;
        let row_bytes = (width as usize) * 4;
        if width == 0 || height == 0 || stride < row_bytes {
            return Err(Error::new(
                E_FAIL,
                format!("implausible bitmap geometry {width}x{height} stride {stride}"),
            ));
        }

        // GetObjectW reports the DIB's own orientation. The provider requests a
        // top-down DIB (negative biHeight), but normalise either way so pixel
        // assertions in the suites are orientation-independent.
        let top_down = bmp.bmHeight < 0;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        let base = bmp.bmBits as *const u8;
        for y in 0..height as usize {
            let src_row = if top_down { y } else { height as usize - 1 - y };
            let src = base.add(src_row * stride);
            std::ptr::copy_nonoverlapping(src, pixels.as_mut_ptr().add(y * row_bytes), row_bytes);
        }
        Ok(Thumb { width, height, pixels, alpha })
    }
}

/// One full provider round-trip: create, initialise from bytes, get a thumbnail.
///
/// This is the exact sequence the shell performs per file, so every suite that
/// only needs "render this input" goes through here.
pub fn render(dll: &Dll, svg: &[u8], size: u32) -> Result<Thumb> {
    let provider = dll.create_provider()?;
    let stream = mem_stream(svg)?;
    unsafe { provider.Initialize(&stream, 0)? };
    let thumb: IThumbnailProvider = provider.cast()?;
    let mut hbmp = HBITMAP(std::ptr::null_mut());
    let mut alpha = WTSAT_UNKNOWN;
    unsafe { thumb.GetThumbnail(size, &mut hbmp, &mut alpha)? };
    if hbmp.is_invalid() {
        return Err(Error::new(E_UNEXPECTED, "GetThumbnail returned S_OK with a null HBITMAP"));
    }
    take_bitmap(hbmp, alpha)
}

/// Like `render`, but reports the HRESULT rather than treating failure as an
/// error - used by suites where a failing render is an acceptable outcome and
/// only a crash or hang is not.
pub fn try_render(dll: &Dll, svg: &[u8], size: u32) -> std::result::Result<Thumb, HRESULT> {
    render(dll, svg, size).map_err(|e| e.code())
}

/// True when the provider declared the bitmap as having a real alpha channel,
/// which is the only correct answer for a 32bpp BGRA thumbnail it produced.
pub fn declares_argb(t: &Thumb) -> bool {
    t.alpha == WTSAT_ARGB
}

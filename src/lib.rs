use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsStr,
    // fs::OpenOptions,
    io::Write,
    mem::ManuallyDrop,
    os::windows::prelude::OsStrExt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{
            AtomicPtr, 
            AtomicU32, 
            Ordering
        }, 
        Arc,
        Mutex,
        OnceLock,
        Once
    },
    path::PathBuf,
};

use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{
            self,
            Direct2D::{
                *, 
                Common::*
            },
            Direct3D,
            Direct3D11,
            Dxgi,
            Gdi,
        },
        System::{
            self,
            Com,
            Variant::*,
            Registry::{
                *,
                RegCreateKeyExW,
                RegSetValueExW,
            },
            SystemInformation::GetLocalTime
        },
        UI::Shell::{
            self,
            SHGetKnownFolderPath,
            FOLDERID_Desktop
        },
        Data::Xml::MsXml,
        Data::Xml::MsXml::*,
        Globalization::{GetTimeFormatEx, TIME_FORMAT_FLAGS},
    },
};

// Global states and definitions
const WRITE_FLAGS: REG_SAM_FLAGS = KEY_WRITE; // Works for both 32-bit and 64-bit.
static INIT_REGISTRY: Once = Once::new(); // Track if registry settings were already loaded

// =================================================================
//                  FFI Panic Safety Macro
// =================================================================

/// Format a panic payload for debug logging without allocating when logging is off
/// (caller should only invoke this inside debug_log! / after checking the flag).
fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
/// What a caught panic should do with the calling thread's cached Direct2D/D3D chain.
#[derive(Clone, Copy, PartialEq)]
enum GraphicsCacheOnPanic {
    /// The entry point renders, so a panic may have interrupted D2D work and left the
    /// cached device chain in an unknown state. Abandon it.
    Discard,
    /// The entry point never touches Direct2D, so no panic inside it can have corrupted
    /// the cache. Discarding it here would abandon a whole device chain (permanently -
    /// see `discard_graphics_cache_after_panic`) for nothing.
    Keep,
}

/// Drops this thread's graphics cache out of TLS after a caught panic, when the entry
/// point that panicked could plausibly have touched it.
///
/// The slot is cleared without releasing what was in it: the resources are `ManuallyDrop`,
/// so this deliberately leaks a device chain (~1.47 MiB and ~24 kernel handles) rather
/// than tearing down D2D/D3D objects whose state is unknown. That trade is only worth
/// making when the panic could actually have corrupted them, hence the policy argument.
fn discard_graphics_cache_after_panic(policy: GraphicsCacheOnPanic) {
    if policy == GraphicsCacheOnPanic::Discard {
        RESOURCES.with(|resources| {
            if let Ok(mut res) = resources.try_borrow_mut() {
                res.take();
            }
        });
    }
}

/// Macro to wrap FFI functions with panic protection.
/// This eliminates the boilerplate code for catch_unwind and error handling.
///
/// The first argument is the `GraphicsCacheOnPanic` variant for this entry point:
/// `Discard` for anything that can reach `get_d2d_resources`, `Keep` for everything else.
macro_rules! ffi_guard {
    // For functions that return Result<T>
    ($cache_policy:ident, Result<$ret_type:ty>, $body:expr) => {{
        let result = catch_unwind(AssertUnwindSafe(|| $body));
        match result {
            Ok(value) => value,
            Err(payload) => {
                discard_graphics_cache_after_panic(GraphicsCacheOnPanic::$cache_policy);
                debug_log!( "A PANIC occurred in FFI function: {}", panic_payload_message(payload) );
                Err(E_FAIL.into())
            }
        }
    }};

    // For functions that return HRESULT directly
    ($cache_policy:ident, HRESULT, $body:expr) => {{
        let result = catch_unwind(AssertUnwindSafe(|| $body));
        match result {
            Ok(hr) => hr,
            Err(payload) => {
                discard_graphics_cache_after_panic(GraphicsCacheOnPanic::$cache_policy);
                debug_log!( "A PANIC occurred in FFI function: {}", panic_payload_message(payload) );
                E_FAIL
            }
        }
    }};

    // For functions that return BOOL
    ($cache_policy:ident, BOOL, $body:expr) => {{
        let result = catch_unwind(AssertUnwindSafe(|| $body));
        match result {
            Ok(success) => success.into(),
            Err(payload) => {
                discard_graphics_cache_after_panic(GraphicsCacheOnPanic::$cache_policy);
                debug_log!( "A PANIC occurred in FFI function: {}", panic_payload_message(payload) );
                false.into()
            }
        }
    }};
}

// =================================================================
//                  Debug Logging Macro
// =================================================================

/// Macro to avoid eager string formatting when debug logging is disabled.
/// Format arguments are only evaluated if the debug flag is turned on.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if ENABLE_DEBUG_LOGGING.load(std::sync::atomic::Ordering::Relaxed) {
            log_message(&format!($($arg)*));
        }
    }
}

// RAII wrapper for HBITMAP - automatically calls DeleteObject when dropped
struct HBitmapGuard(Gdi::HBITMAP);

impl HBitmapGuard {
    // Create a new guard. Takes ownership of the handle.
    fn new(handle: Gdi::HBITMAP) -> Self {
        Self(handle)
    }

    // Release ownership of the handle (e.g., when transferring to the Shell).
    fn release(mut self) -> Gdi::HBITMAP {
        let handle = self.0;
        // Set the internal handle to null so Drop doesn't delete it.
        self.0 = Gdi::HBITMAP(std::ptr::null_mut());
        handle
    }
}

impl Drop for HBitmapGuard {
    fn drop(&mut self) {
        // Only delete if the handle is not null/invalid (i.e., it hasn't been released).
        if !self.0.is_invalid() && !self.0.0.is_null() {
            let success = unsafe { Graphics::Gdi::DeleteObject(Gdi::HGDIOBJ(self.0.0)) };
            // In a Drop implementation, we can't return an error, so just ignore failure
            // In production, you might want to log this for debugging
            let _ = success;
        }
    }
}

// RAII Guard to automatically free memory from SHGetKnownFolderPath
struct CoTaskMemFreeGuard(PWSTR);

impl Drop for CoTaskMemFreeGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // This is safe because the pointer was allocated by the COM task allocator
            unsafe { Com::CoTaskMemFree(Some(self.0 .0 as *const std::ffi::c_void)) };
        }
    }
}

// RAII wrapper for COM initialization - automatically calls CoUninitialize when dropped
struct ComGuard {
    initialized_by_us: bool,
}

impl ComGuard {
    fn new() -> Result<Self> {
        let hr: HRESULT = unsafe { Com::CoInitializeEx(None, Com::COINIT_APARTMENTTHREADED) };
        match hr {
            S_OK => {
                // We successfully initialized COM, so we're responsible for cleanup
                Ok(Self { initialized_by_us: true })
            }
            S_FALSE => {
                // COM was already initialized by someone else, don't clean up
                Ok(Self { initialized_by_us: false })
            }
            RPC_E_CHANGED_MODE => { 
                // COM was already initialized as MTA on this thread, don't clean up
                Ok(Self { initialized_by_us: false })
            }
            _ => {
                // Real error
                Err(Error::new(hr, "Failed to initialize COM"))
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized_by_us {
            unsafe { Com::CoUninitialize() };
        }
    }
}

// --- Thread-local storage for COM objects that cannot be shared between threads ---
struct ThreadResources {
    // D2D resources must be declared first so they are dropped first
    d2d_factory: Option<ID2D1Factory1>,
    d2d_device: Option<ID2D1Device>,
    d2d_context: Option<ID2D1DeviceContext5>,
    poisoned: bool,
    
    // Important: ComGuard must be the last field. This ensures it is dropped last, calling CoUninitialize only after all other COM objects have been released.
    _com_guard: ComGuard,
}

// SAFETY-CRITICAL: The cache is wrapped in ManuallyDrop so that a thread's cached
// D2D/D3D-WARP chain is never torn down by Rust's TLS destructor machinery. TLS
// destructors run inside a PE TLS callback during DLL_THREAD_DETACH and
// DLL_PROCESS_DETACH, i.e. while the OS loader lock is held. Releasing the last
// reference to a WARP-backed D3D11 device there can deadlock (device teardown waits on
// its worker threads, which themselves need the loader lock to exit) or crash the host
// process. Instead:
//   - Resources are explicitly dropped (ManuallyDrop::into_inner) only when they are
//     intentionally replaced during normal rendering (device-lost/poisoned path), which
//     runs on the owning thread outside the loader lock.
//   - When a thread exits, its cached resources are deliberately leaked (no destructor
//     is even registered, since the TLS payload no longer needs drop). Threads that
//     render thumbnails are shell/surrogate pool threads that in practice live until
//     process exit, so nothing meaningful accumulates.
//   - After a caught panic, ffi_guard clears the slot without dropping - leaking is the
//     safest option when the resources are in an unknown state. Only entry points that
//     can actually reach this cache do so (GraphicsCacheOnPanic::Discard); a panic in,
//     say, the stream-read loop cannot have touched it, and abandoning a device chain
//     there would cost ~1.47 MiB and ~24 handles for nothing.
//   - pin_module_in_memory() below guarantees the DLL (and its d2d1/d3d11/dxgi imports)
//     is never unmapped while leaked or live caches exist.
thread_local! {
    static RESOURCES: RefCell<Option<ManuallyDrop<ThreadResources>>> = RefCell::new(None);
}

// Tracks whether the DLL is already pinned; pinning is idempotent, this only avoids a redundant syscall.
static MODULE_PINNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pins this DLL in memory for the lifetime of the process. Called right before the
/// first per-thread D2D cache is created (a normal call context, never DllMain, so
/// briefly taking the loader lock inside GetModuleHandleExW is safe).
///
/// Why: DllCanUnloadNow only counts live COM objects and server locks, so once the
/// shell releases its last thumbnail object, COM may FreeLibrary this DLL even though
/// per-thread caches still own a live D2D-on-WARP device chain. Unmapping the DLL then
/// pulls d2d1/d3d11/dxgi/d3d10warp out from under that live state - and from under
/// WARP's running worker threads - which crashes or deadlocks the host (observed as
/// dllhost.exe crashes during idle periods). Once any thread has cached graphics
/// state, staying loaded is the only safe option; the surrogate process still exits on
/// idle, which is when the DLL actually gets released.
fn pin_module_in_memory() -> Result<()> {
    if MODULE_PINNED.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut module = HMODULE::default();
    unsafe {
        System::LibraryLoader::GetModuleHandleExW(
            System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_PIN
                | System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(pin_module_in_memory as *const () as *const u16),
            &mut module,
        )?;
    }
    MODULE_PINNED.store(true, Ordering::Release);
    debug_log!("pin_module_in_memory: DLL pinned in memory for process lifetime");
    Ok(())
}
/// Initializes and retrieves the thread-local Direct2D and WIC resources.
/// This function ensures that the heavyweight factory and device objects are created only once per thread.
fn get_d2d_resources() -> Result<(ID2D1Factory1, ID2D1Device, ID2D1DeviceContext5)> {
    RESOURCES.with(|resources| -> Result<(ID2D1Factory1, ID2D1Device, ID2D1DeviceContext5)> {
        let mut resources_ref = resources.borrow_mut();

        let needs_recreation = match resources_ref.as_ref() {
            None => true,
            Some(res) => res.poisoned,
        };
        
        // If resources are poisoned or don't exist, recreate them
        if needs_recreation {
            debug_log!("get_d2d_resources: Creating new D2D resources");

            // This thread is about to hold cached graphics state, so make sure the DLL
            // can never be unmapped while that state exists (see pin_module_in_memory).
            pin_module_in_memory()?;

            // If poisoned resources exist, release them for real before recreating: we
            // are on the owning thread in a normal call context (not under the loader
            // lock), so this is the one place where full teardown is safe.
            if let Some(old_resources) = resources_ref.take() {
                debug_log!("get_d2d_resources: Releasing poisoned resources before recreation");
                drop(ManuallyDrop::into_inner(old_resources));
            }

            // Initialize COM and create all resources
            let com_guard = ComGuard::new()?;
            
            // log_message("get_d2d_resources: Creating D2D factory");
            let options = D2D1_FACTORY_OPTIONS {
                debugLevel: D2D1_DEBUG_LEVEL_NONE,
            };
            let d2d_factory: ID2D1Factory1 = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))? };
            
            // Local function to create D3D11 device with specified driver type
            let create_d3d_device = |driver_type: Direct3D::D3D_DRIVER_TYPE| -> Result<Direct3D11::ID3D11Device> {
                let mut device: Option<Direct3D11::ID3D11Device> = None;
                unsafe {
                    Direct3D11::D3D11CreateDevice(
                        None,
                        driver_type,
                        HMODULE::default(),
                        Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT, // Required for D2D interop
                        None,
                        Direct3D11::D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        None,
                    )?;
                }
                device.ok_or_else(|| Error::new(E_FAIL, "Failed to create D3D11 device"))
            };

            // Create the D3D11 Device - use registry setting to determine hardware vs WARP
            let d3d_device: Direct3D11::ID3D11Device;
            let use_hardware = USE_HARDWARE_ACCELERATION.load(Ordering::Relaxed);
            
            if use_hardware {
                // log_message("get_d2d_resources: Attempting hardware acceleration (D3D_DRIVER_TYPE_HARDWARE)");
                // Try hardware first if enabled in registry, fallback to WARP if it fails
                match create_d3d_device(Direct3D::D3D_DRIVER_TYPE_HARDWARE) {
                    Ok(device) => {
                        debug_log!("get_d2d_resources: Hardware acceleration succeeded");
                        d3d_device = device;
                    },
                    Err(e) => {
                        debug_log!("get_d2d_resources: Hardware acceleration failed with error: {:?}, falling back to WARP", e);
                        d3d_device = create_d3d_device(Direct3D::D3D_DRIVER_TYPE_WARP)?;
                    }
                }
            } else {
                debug_log!("get_d2d_resources: Using WARP (software rendering) as configured");
                // Default to WARP (software rendering) for stability
                d3d_device = create_d3d_device(Direct3D::D3D_DRIVER_TYPE_WARP)?;
            }
            let dxgi_device: Dxgi::IDXGIDevice = d3d_device.cast()?;

            // log_message("get_d2d_resources: Creating D2D device and context");
            // Create the D2D Device from the D3D11 device
            let d2d_device: ID2D1Device = unsafe { d2d_factory.CreateDevice(&dxgi_device)? };
            
            // Create the D2D Device Context
            let dc: ID2D1DeviceContext = unsafe { d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)? };
            let d2d_context: ID2D1DeviceContext5 = dc.cast()?;
            
            debug_log!("get_d2d_resources: Successfully created all D2D resources");
            // Store all resources in the unified structure (ManuallyDrop: see RESOURCES)
            *resources_ref = Some(ManuallyDrop::new(ThreadResources {
                d2d_factory: Some(d2d_factory.clone()),
                d2d_device: Some(d2d_device.clone()),
                d2d_context: Some(d2d_context.clone()),
                poisoned: false,
                _com_guard: com_guard,
            }));
            
            Ok((d2d_factory, d2d_device, d2d_context))
        } else {
            debug_log!("get_d2d_resources: Reusing existing D2D resources");
            // Resources exist and are not poisoned, cleanly extract and clone them
            if let Some(resources) = resources_ref.as_ref() {
                if let (Some(factory), Some(device), Some(context)) = (
                    &resources.d2d_factory,
                    &resources.d2d_device,
                    &resources.d2d_context,
                ) {
                    return Ok((factory.clone(), device.clone(), context.clone()));
                }
            }
            
            // If we somehow get here, our state invariants were violated.
            // Return an error instead of panicking the host process.
            Err(Error::new(E_UNEXPECTED, "D2D resources were corrupted or partially initialized"))
        }
    })
}

// RAII wrapper for D2D bitmap mapping - automatically unmaps when dropped
struct BitmapMapGuard<'a> {
    bitmap: &'a ID2D1Bitmap1,
    mapped: bool,
}

impl<'a> BitmapMapGuard<'a> {
    fn new(bitmap: &'a ID2D1Bitmap1) -> Result<(Self, D2D1_MAPPED_RECT)> {
        let mapped_rect = unsafe { bitmap.Map(D2D1_MAP_OPTIONS_READ)? };
        Ok((Self { bitmap, mapped: true }, mapped_rect))
    }
}

impl<'a> Drop for BitmapMapGuard<'a> {
    fn drop(&mut self) {
        if self.mapped {
            unsafe { let _ = self.bitmap.Unmap(); }
        }
    }
}

// RAII wrapper for D2D BeginDraw/EndDraw - automatically calls EndDraw when dropped
struct D2D1DrawGuard<'a> {
    context: &'a ID2D1DeviceContext5,
    ended: bool,
}

impl<'a> D2D1DrawGuard<'a> {
    fn new(context: &'a ID2D1DeviceContext5) -> Self {
        unsafe { context.BeginDraw() };
        Self { context, ended: false }
    }

    // Explicitly end drawing and return the result to allow proper error handling
    fn end(mut self) -> Result<()> {
        self.ended = true;
        unsafe { self.context.EndDraw(None, None) }
    }
}

impl<'a> Drop for D2D1DrawGuard<'a> {
    fn drop(&mut self) {
        // If dropped without explicit end (e.g. via ? operator), ensure we clean up state
        if !self.ended {
            let _ = unsafe { self.context.EndDraw(None, None) };
        }
    }
}

// RAII wrapper for VARIANT - automatically calls VariantClear when dropped
struct VariantGuard(VARIANT);

impl Drop for VariantGuard {
    fn drop(&mut self) {
        // This is safe to call even on a default/zeroed VARIANT
        unsafe { let _ = VariantClear(&mut self.0); }
    }
}

impl VariantGuard {
    /// Attempts to extract a String from the VARIANT if it contains a BSTR.
    ///
    /// Returns:
    /// - `Ok(Some(String))` if the variant is a `VT_BSTR`.
    /// - `Ok(None)` if the variant is `VT_EMPTY` or `VT_NULL`.
    /// - `Err` if the variant is any other type.
    pub fn try_as_string(&self) -> Result<Option<String>> {
        // This entire operation is unsafe because we are manually interpreting a C-style union.
            // Access the variant type tag `vt` directly. It is already a `VARENUM` type, so no casting or construction is needed.
        match unsafe { self.0.Anonymous.Anonymous.vt } {
            VT_BSTR => {
                // It's a BSTR. The `bstrVal` field is valid.
                let bstr = unsafe { &self.0.Anonymous.Anonymous.Anonymous.bstrVal };
                if bstr.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(bstr.to_string()))
                }
            }

            VT_EMPTY | VT_NULL => {
                // The attribute exists but is empty. This is a valid, non-error state. We represent this as `None`.
                Ok(None)
            }

            _ => {
                // The variant holds a different type (e.g., a number). This is an unexpected state for a 'style' attribute. We return an error to indicate this.
                Err(Error::new(E_INVALIDARG, "Variant was not a string type."))
            }
        }
    }
}

impl std::ops::Deref for VariantGuard {
    type Target = VARIANT;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for VariantGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Parses CSS text content and returns a HashMap of selectors to their concatenated style properties.
fn parse_css_rules(css_content: &str) -> HashMap<String, String> {
    // Helper to find the matching closing brace, aware of strings and nested braces.
    // `s` is the full string, `start_pos` is the byte index of the opening brace '{'.
    // Returns the byte index of the matching '}'.
    fn find_matching_brace(s: &str, start_pos: usize) -> Option<usize> {
        let mut level = 1;
        let mut in_string: Option<char> = None;
        let mut is_escaped = false;
        let start_slice = match start_pos.checked_add(1) {
            Some(val) if val <= s.len() => &s[val..],
            _ => return None,
        };
        
        let mut chars = start_slice.char_indices();
        while let Some((i, ch)) = chars.next() {

            if is_escaped {
                is_escaped = false;
                continue;
            }

            if ch == '\\' {
                is_escaped = true;
                continue;
            }

            if let Some(quote) = in_string {
                if ch == quote {
                    in_string = None;
                }
            } else {
                match ch {
                    '\'' | '"' => in_string = Some(ch),
                    '{' => level += 1,
                    '}' => {
                        level -= 1;
                        if level == 0 {
                            return start_pos.checked_add(1).and_then(|v| v.checked_add(i));
                        }
                    },
                    _ => {}
                }
            }
        }
        None
    }

    // SECURITY: Use a HashMap to store styles during parsing. This provides O(1) amortized
    // lookup time and prevents a Denial of Service attack where a malicious CSS with thousands
    // of rules for the same class name would cause O(N^2) behavior in a Vec-based approach.
    let mut style_map: HashMap<String, String> = HashMap::new();
    
    // Clean the input string: remove leading/trailing whitespace and control characters.
    let cleaned_content = remove_css_comments(css_content.trim());
    
    // Use an iterative approach with a heap-allocated stack to prevent stack overflow DoS.
    let mut work_stack: Vec<&str> = vec![&cleaned_content];
    const MAX_DEPTH: usize = 256; // Defense-in-depth against extreme nesting causing memory exhaustion.

    while let Some(current_css) = work_stack.pop() {
        let mut cursor = 0;
        while cursor < current_css.len() {
            // Find the next rule block, which starts with '{'
            let open_brace_pos = match current_css[cursor..].find('{') {
                Some(pos) => cursor + pos,
                None => break, // No more rules in this block
            };

            let selectors_part = &current_css[cursor..open_brace_pos];

            // Find the matching closing brace to define the rule's scope
            let close_brace_pos = match find_matching_brace(current_css, open_brace_pos) {
                Some(pos) => pos,
                None => break, // Unmatched brace, stop parsing this block
            };

            let properties_part = &current_css[open_brace_pos + 1 .. close_brace_pos];

            // Check if it's an at-rule (e.g., @media, @keyframes)
            if selectors_part.trim().starts_with('@') {
                // It's a nested block. Instead of recursing, push its contents onto the
                // work stack to be processed iteratively.
                if work_stack.len() < MAX_DEPTH {
                    work_stack.push(properties_part);
                }
            } else {
                // It's a standard rule; process its selectors.
                for selector in selectors_part.split(',') {
                    let selector = selector.trim();
                    
                    if !selector.is_empty() {
                        let normalized_properties = normalize_css_properties(properties_part);

                        // Use HashMap::entry for efficient O(1) amortized lookup and insertion.
                        style_map
                            .entry(selector.to_string())
                            .or_default()
                            .push_str(&normalized_properties);
                    }
                }
            }

            // Move cursor to the position after the current rule block
            cursor = close_brace_pos + 1;
        }
    }
    
    style_map
}

/// Removes CSS comments from the input string.
fn remove_css_comments(css: &str) -> String {
    let mut result = String::new();
    let mut chars = css.chars().peekable();
    
    while let Some(ch) = chars.next() {
        if ch == '/' && chars.peek() == Some(&'*') {
            // Start of comment, consume until */
            chars.next(); // consume '*'
            
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next(); // consume '/'
                    break;
                }
            }
        } else {
            result.push(ch);
        }
    }
    
    result
}

/// Normalizes CSS properties to ensure they're properly formatted for inline styles.
fn normalize_css_properties(properties: &str) -> String {
    let mut result = String::new();
    
    // Split by semicolon and clean up each property
    for prop in properties.split(';') {
        let prop = prop.trim();
        if !prop.is_empty() {
            result.push_str(prop);
            if !prop.ends_with(';') {
                result.push(';');
            }
        }
    }
    
    result
}

/// The CSS token Direct2D refuses to honour, so it has to come out of both <style>
/// content and inline style attributes before the document is handed to the renderer.
const CSS_IMPORTANT: &str = "!important";

/// Removes every occurrence of `needle` from `haystack`, ignoring ASCII case.
///
/// CSS keywords are case-insensitive, so `!IMPORTANT` has to be stripped exactly like
/// `!important`, and `str::replace` is case-sensitive. Returns `None` when there is
/// nothing to remove, so callers skip both the allocation and the DOM write in the common
/// case where a style carries no `!important` at all.
///
/// `needle` must be non-empty ASCII (as `CSS_IMPORTANT` is): every match then begins and
/// ends on a UTF-8 character boundary, so slicing `haystack` at those offsets is valid.
fn remove_ignore_ascii_case(haystack: &str, needle: &str) -> Option<String> {
    debug_assert!(needle.is_ascii() && !needle.is_empty());

    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut result: Option<String> = None;
    let mut copied_up_to = 0;
    let mut i = 0;

    while i + needle_bytes.len() <= bytes.len() {
        if bytes[i..i + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes) {
            let out = result.get_or_insert_with(|| String::with_capacity(haystack.len()));
            out.push_str(&haystack[copied_up_to..i]);
            i += needle_bytes.len();
            copied_up_to = i;
        } else {
            i += 1;
        }
    }

    if let Some(out) = result.as_mut() {
        out.push_str(&haystack[copied_up_to..]);
    }
    result
}

/// Processes an uncompressed SVG in a single MSXML DOM pass:
/// 1. Extracts CSS from <style> tags
/// 2. Strips !important from both <style> content and inline style attributes
/// 3. Parses extracted CSS into rules and applies them as inline styles on matching elements
/// 4. Serializes the modified DOM back to bytes exactly once
///
/// Returns the processed SVG data ready for Direct2D rendering.
fn process_svg_styles(svg_data: &[u8]) -> Result<Vec<u8>> {
    // First, do a quick check on the entire SVG data to see if !important exists anywhere, to know whether further processing is needed for that.
    // If so we'll want to remove !important from inline styles as well, because apparently Direct2D won't render any attributes with it.
    let found_important = svg_data
        .windows(CSS_IMPORTANT.len())
        .any(|w| w.eq_ignore_ascii_case(CSS_IMPORTANT.as_bytes()));

    // MSXML is a COM library, so COM must be initialized on the current thread.
    let _com_guard = ComGuard::new()?;

    // Create an instance of the MSXML6 DOM Document object.
    let dom: MsXml::IXMLDOMDocument2 = unsafe { Com::CoCreateInstance(&DOMDocument60, None, Com::CLSCTX_INPROC_SERVER)? };

    // Load the SVG data from an in-memory stream.
    let stream: Com::IStream = unsafe { Shell::SHCreateMemStream(Some(svg_data)) }
        .ok_or_else(|| Error::new(E_FAIL, "Failed to create memory stream for SVG processing"))?;

    let stream_unknown: IUnknown = stream.cast()?;
    let stream_variant = VariantGuard(VARIANT::from(stream_unknown));

    // The MSXML parser will read the SVG data directly from our in-memory stream.
    let success = unsafe { dom.load(&stream_variant)? };
    if success != VARIANT_TRUE {
        debug_log!("process_svg_styles: MSXML failed to parse SVG, returning original data");
        // If loading fails, it might not be a valid XML/SVG. Instead of failing the entire render,
        // we'll treat this as "no processing needed" and return the original data.
        return Ok(svg_data.to_vec());
    }

    // --- Phase 1: Extract CSS from <style> elements ---

    // Use a namespace-agnostic XPath query to find all <style> elements. This is necessary because
    // most SVGs define a default namespace (xmlns="..."), which would cause a simple "//style" query to fail.
    let style_nodes: IXMLDOMNodeList = unsafe { dom.selectNodes(&BSTR::from("//*[local-name()='style']"))? };

    let mut combined_css = String::new();
    for i in 0..unsafe { style_nodes.length()? } {
        if let Ok(node) = unsafe { style_nodes.get_item(i) } {
            // The .text property of a node gets the concatenated text content of the node and its children.
            // For a <style> element, this is exactly the CSS code inside it.
            if let Ok(css_bstr) = unsafe { node.text() } {
                let css_text = css_bstr.to_string();
                // Strip "!important" declarations from CSS content - not needed for SVGs and can cause rendering issues
                let cleaned_css = match remove_ignore_ascii_case(&css_text, CSS_IMPORTANT) {
                    Some(cleaned) => {
                        // Update the original node with the cleaned CSS to prevent issues during SVG processing
                        debug_log!("process_svg_styles: Cleaned !important from <style> element");
                        let _ = unsafe { node.Settext(&BSTR::from(cleaned.as_str())) };
                        cleaned
                    }
                    None => css_text,
                };

                combined_css.push_str(&cleaned_css);
                combined_css.push('\n'); // Add a newline for separation.
            }
        }
    }

    // --- Phase 2: Strip !important from inline style attributes ---

    // If we found !important anywhere in the SVG, also check for it in inline style attributes.
    // This is an expensive operation, so we only do it when we see !important anywhere in the data.
    if found_important {
        let bstr_style = BSTR::from("style");

        // Find all elements that have a style attribute
        let styled_elements: IXMLDOMNodeList = unsafe { dom.selectNodes(&BSTR::from("//*[@style]"))? };

        for i in 0..unsafe { styled_elements.length()? } {
            if let Ok(node) = unsafe { styled_elements.get_item(i) } {
                if let Ok(element) = node.cast::<IXMLDOMElement>() {
                    if let Ok(style_variant_raw) = unsafe { element.getAttribute(&bstr_style) } {
                        let style_variant = VariantGuard(style_variant_raw);
                        if let Ok(Some(raw_style)) = style_variant.try_as_string() {
                            // Only rewrite the attribute if it actually carries !important
                            if let Some(cleaned_style) = remove_ignore_ascii_case(&raw_style, CSS_IMPORTANT) {
                                let variant_value = VariantGuard(VARIANT::from(BSTR::from(cleaned_style)));

                                let _ = unsafe { element.setAttribute(&bstr_style, &variant_value) };
                            }
                        }
                    }
                }
            }
        }
    }

    // --- Phase 3: Parse CSS rules and apply them as inline styles ---

    // If no CSS was found in <style> tags, we may still have modified !important above.
    // Check if we need to serialize based on whether any modifications were made.
    let has_css = !combined_css.trim().is_empty();

    if has_css {
        let style_map = parse_css_rules(&combined_css);

        let bstr_style = BSTR::from("style");
        let bstr_class = BSTR::from("class");

        // Single-pass approach: traverse the DOM once and use O(1) HashMap lookups
        // to apply styles, instead of running an XPath query per CSS rule.
        let all_elements: IXMLDOMNodeList = unsafe { dom.selectNodes(&BSTR::from("//*"))? };
        for i in 0..unsafe { all_elements.length()? } {
            if let Ok(element) = unsafe { all_elements.get_item(i)?.cast::<IXMLDOMElement>() } {
                let mut new_styles = String::new();

                // 1. Check tag name (e.g., "path", "rect") for matching CSS rules.
                if let Ok(tag_name) = unsafe { element.nodeName() } {
                    let tag_str = tag_name.to_string();
                    // local-name matching: strip any namespace prefix.
                    let local_name = tag_str.rsplit_once(':').map_or(tag_str.as_str(), |(_, l)| l);
                    if let Some(css) = style_map.get(local_name) {
                        new_styles.push_str(css);
                    }
                }

                // 2. Check class names (e.g., ".st0") for matching CSS rules.
                if let Ok(class_attr_raw) = unsafe { element.getAttribute(&bstr_class) } {
                    if let Ok(Some(classes)) = VariantGuard(class_attr_raw).try_as_string() {
                        for class_name in classes.split_whitespace() {
                            let key = format!(".{}", class_name);
                            if let Some(css) = style_map.get(&key) {
                                new_styles.push_str(css);
                            }
                        }
                    }
                }

                // If we found matching CSS rules, combine with any existing inline style and apply.
                if !new_styles.is_empty() {
                    let mut existing_style = String::new();
                    if let Ok(style_variant_raw) = unsafe { element.getAttribute(&bstr_style) } {
                        if let Ok(Some(style_string)) = VariantGuard(style_variant_raw).try_as_string() {
                            existing_style = style_string;
                            if !existing_style.is_empty() && !existing_style.ends_with(';') {
                                existing_style.push(';');
                            }
                        }
                    }

                    // Prepend CSS rules so existing inline styles take precedence (standard CSS behavior).
                    let final_style = format!("{}{}", new_styles, existing_style);
                    let variant_value = VariantGuard(VARIANT::from(BSTR::from(final_style)));
                    let _ = unsafe { element.setAttribute(&bstr_style, &variant_value) };
                }
            }
        }
    }

    // --- Phase 4: Serialize the DOM back to bytes (only once!) ---

    // Only serialize if we actually modified anything (had CSS to apply, or had !important to strip).
    if has_css || found_important {
        let modified_xml_bstr = unsafe { dom.xml()? };
        let modified_xml_string = modified_xml_bstr.to_string();

        debug_log!("process_svg_styles: Successfully processed SVG, returning {} bytes", modified_xml_string.len());

        Ok(modified_xml_string.into_bytes())
    } else {
        // Nothing was modified, return original data without serialization overhead.
        Ok(svg_data.to_vec())
    }
}

pub fn render_svg_to_hbitmap(svg_data: &[u8], requested_width: u32, requested_height: u32) -> Result<Gdi::HBITMAP> {
    debug_log!("render_svg_to_hbitmap: Starting render for {}x{} size, {} bytes of data", requested_width, requested_height, svg_data.len());
    
    // Encapsulate main rendering logic in a helper closure.
    // This makes it easier to catch any error, check if it's D2DERR_RECREATE_TARGET, poison the resources if needed, and then return the original error.
    let result = (|| -> Result<Gdi::HBITMAP> {
        // Early validation - avoid work for invalid sizes
        if requested_width == 0 || requested_height == 0 || requested_width > 4096 || requested_height > 4096 {
            debug_log!("render_svg_to_hbitmap: Invalid dimensions: {}x{}", requested_width, requested_height);
            return Err(Error::new(E_INVALIDARG, "Invalid bitmap dimensions"));
        }

        // log_message("render_svg_to_hbitmap: Getting D2D resources");
        // 1. Get resources (now includes cached device context)
        let (_d2d_factory, _d2d_device, d2d_context) = get_d2d_resources()?;
        
        // log_message("render_svg_to_hbitmap: Creating render target bitmap");
        // 2. Create the D2D RENDER TARGET bitmap (GPU-only)
        let bitmap_props_rt = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            ..Default::default()
        };
        let render_target_bitmap: ID2D1Bitmap1 = unsafe { d2d_context.CreateBitmap(D2D_SIZE_U { width: requested_width, height: requested_height }, None, 0, &bitmap_props_rt) }?;
        
        // 3. Set target and draw the SVG
        unsafe { d2d_context.SetTarget(&render_target_bitmap) };
        
        let draw_guard = D2D1DrawGuard::new(&d2d_context);
        
        // Clear to transparent black
        unsafe { d2d_context.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 })) };

        // Check for GZIP magic number (0x1F 0x8B) to detect SVGZ files
        let is_compressed = svg_data.len() >= 2 && svg_data[0] == 0x1F && svg_data[1] == 0x8B;
           
        let processed_svg_data: Vec<u8>;
        // Skip CSS processing for compressed SVGZ files - Direct2D can handle them directly
        if is_compressed {
            // log_message("render_svg_to_hbitmap: Detected SVGZ (compressed) file, skipping CSS processing");
            processed_svg_data = svg_data.to_vec();
        } else {
            // Single-pass MSXML processing: extracts CSS, strips !important, applies inline styles, serializes once.
            processed_svg_data = process_svg_styles(svg_data)?;
        }

        // log_message("render_svg_to_hbitmap: Creating SVG document from processed data");
        // Load the (potentially processed) svg data into a memory stream.
        let stream: Com::IStream = unsafe { Shell::SHCreateMemStream(Some(&processed_svg_data)) }.ok_or_else(|| Error::new(E_FAIL, "Failed to create memory stream"))?;
        
        // Create the SVG document from the stream of processed SVG data.
        let svg_doc: ID2D1SvgDocument = unsafe { d2d_context.CreateSvgDocument(
            &stream,
            D2D_SIZE_F { 
                width: requested_width as f32, 
                height: requested_height as f32
            }
        ) }?;

        // Get the root <svg> element from the document, so we can get or change the top level attributes such as width, height, viewbox, etc.
        if let Ok(root_element) = unsafe { svg_doc.GetRoot() } {
            // Apparently if there are no width and height attributes, DrawSvgDocument will automatically scale it to the viewbox
            // So we can just remove them from before drawing, and it will autoscale and fill the thumbnail.
            //      IMPORTANT: ViewBox is not the same as ViewPort (which is actually just the height/width attributes).
            // HOWEVER, if there is no viewbox, it could cause issues with scaling. So if there is no viewbox but there are original width and height attributes,
            //      we can set the viewbox to "0 0 width height" to make it more likely to scale correctly.
            // Also apparently even though we apparently set the width and height of the viewport when creating the SVG document, it retains the original width and height attributes when using GetAttributeValue3
            unsafe {
                // // DEBUG - Maybe useful later: Get the width and height attributes from the root element
                // let mut width_buffer = [0u16; 32]; // Buffer for width string
                // let mut height_buffer = [0u16; 32]; // Buffer for height string
                // let width_result = root_element.GetAttributeValue3(&BSTR::from("width"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &mut width_buffer);
                // let height_result = root_element.GetAttributeValue3(&BSTR::from("height"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &mut height_buffer);
                // // Print the width and height attributes if they exist
                // if width_result.is_ok() {
                //     let width_str = String::from_utf16_lossy(&width_buffer).trim_end_matches('\0').to_string();
                //     if !width_str.is_empty() { println!("SVG Width: {}", width_str); }
                // }
                // if height_result.is_ok() {
                //     let height_str = String::from_utf16_lossy(&height_buffer).trim_end_matches('\0').to_string();
                //     if !height_str.is_empty() { println!("SVG Height: {}", height_str); }
                // }

                // If there is no viewbox, but there is a width and height, set the viewbox to "0 0 width height" before removing the attributes.
                let mut viewbox_buffer = [0u16; 64]; // Buffer for viewBox string
                if root_element.GetAttributeValue3(&BSTR::from("viewBox"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &mut viewbox_buffer).is_err() {
                    let mut width_buffer = [0u16; 32]; // Buffer for width string
                    let mut height_buffer = [0u16; 32]; // Buffer for height string
                    let width_result = root_element.GetAttributeValue3(&BSTR::from("width"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &mut width_buffer);
                    let height_result = root_element.GetAttributeValue3(&BSTR::from("height"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &mut height_buffer);

                    if width_result.is_ok() && height_result.is_ok() {
                        let width_str = String::from_utf16_lossy(&width_buffer).trim_end_matches('\0').to_string();
                        let height_str = String::from_utf16_lossy(&height_buffer).trim_end_matches('\0').to_string();
                        let _ = root_element.SetAttributeValue3(&BSTR::from("viewBox"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &BSTR::from(format!("0 0 {} {}", width_str, height_str)));
                    }
                }

                // Remove width, height and viewBox attributes if they exist
                let _ = root_element.RemoveAttribute(w!("height"));
                let _ = root_element.RemoveAttribute(w!("width"));
                // let _ = root_element.RemoveAttribute(w!("viewBox"));
                
                // DEBUG - Maybe useful later: How to set height, width and viewBox attributes on the root element
                // let _ = root_element.SetAttributeValue3(&BSTR::from("height"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &BSTR::from(height.to_string()));
                // let _ = root_element.SetAttributeValue3(&BSTR::from("width"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &BSTR::from(width.to_string()));
                // let _ = root_element.SetAttributeValue3(&BSTR::from("viewBox"), D2D1_SVG_ATTRIBUTE_STRING_TYPE_SVG, &BSTR::from(format!("0 0 {} {}", width, height)));
            }
        }
        
        unsafe { d2d_context.DrawSvgDocument(&svg_doc) };
        
        // Explicitly end the draw batch to bubble up errors (like D2DERR_RECREATE_TARGET)
        draw_guard.end()?;
        
        // Clear target after drawing
        unsafe { d2d_context.SetTarget(None) };

        // 4. Create the CPU-readable STAGING bitmap
        let bitmap_props_staging = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        let staging_bitmap: ID2D1Bitmap1 = unsafe { d2d_context.CreateBitmap(D2D_SIZE_U { width: requested_width, height: requested_height }, None, 0, &bitmap_props_staging) }?;
        
        // 5. Copy from render target to staging bitmap (GPU -> CPU accessible D2D memory)
        // This copies the pixel data but it's still in D2D's memory space
        unsafe { staging_bitmap.CopyFromBitmap(None, &render_target_bitmap, None) }?;
        
        // 6. Map the staging bitmap to get a pointer to the pixel data using RAII guard
        let (map_guard, mapped_rect) = BitmapMapGuard::new(&staging_bitmap)?;

        let dest_stride: usize = (requested_width * 4) as usize;
        let source_stride: usize = mapped_rect.pitch as usize;

        // A successful map must provide a valid buffer with enough bytes for
        // one complete BGRA row before it is used to construct a Rust slice.
        if mapped_rect.bits.is_null() || source_stride < dest_stride {
            debug_log!(
                "render_svg_to_hbitmap: Invalid mapped bitmap (pitch={}, required={}, bits_null={})",
                source_stride,
                dest_stride,
                mapped_rect.bits.is_null()
            );
            return Err(Error::new(
                E_FAIL,
                "Mapped D2D bitmap has a null buffer or insufficient row pitch",
            ));
        }
        
        // 7. Create the final GDI HBITMAP
        // This creates a separate GDI bitmap with its own memory buffer
        let bmi = Gdi::BITMAPINFO { bmiHeader: Gdi::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<Gdi::BITMAPINFOHEADER>() as u32, biWidth: requested_width as i32, biHeight: -(requested_height as i32),
            biPlanes: 1, biBitCount: 32, biCompression: Gdi::BI_RGB.0 as u32, ..Default::default()
        }, ..Default::default() };

        let mut dib_data: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbitmap_handle: Gdi::HBITMAP = unsafe {
            Gdi::CreateDIBSection(None, &bmi, Gdi::DIB_RGB_COLORS, &mut dib_data, None, 0)
        }?;

        // CreateDIBSection succeeded, so it must also have returned a writable pixel buffer.
        // Treat an inconsistent result as an error rather than returning an uninitialized bitmap as a successful thumbnail.
        let hbitmap_guard = HBitmapGuard::new(hbitmap_handle); 

        // The returned HBITMAP was already validated by the Result and `?`.
        // Verify the separate output buffer before constructing a Rust slice from it.
        if dib_data.is_null() {
            return Err(Error::new( E_FAIL, "CreateDIBSection returned a null pixel buffer", ));
        }
        
        // 8. Copy pixels from the mapped D2D buffer to the GDI HBITMAP buffer
        // SECURITY LOGIC: Always promote pitch * height to u64 before casting to usize.
        // This prevents integer overflow if a malicious or buggy driver returns a huge pitch.
        // Without this, a wrapped value could create a dangerously small slice, leading to a heap buffer overflow when copying rows below.
        // Do not remove this check: it is critical for safe memory access.
        let source_buffer_size_64 = (mapped_rect.pitch as u64) * (requested_height as u64);

        // On 32-bit systems, usize is 32 bits. Ensure the calculated size fits.
        if source_buffer_size_64 > usize::MAX as u64 {
            // Defensive: If this ever triggers, the driver is returning a bogus pitch, or there is something deeply wrong with the D2D bitmap.
            return Err(Error::new(E_FAIL, "Calculated source buffer size exceeds addressable memory."));
        }
        let source_buffer_size = source_buffer_size_64 as usize;

        // Create safe slices from the raw pointers.
        let source_data: &[u8] = unsafe { 
            std::slice::from_raw_parts(mapped_rect.bits, source_buffer_size) 
        };
        let dest_data: &mut [u8] = unsafe { 
            std::slice::from_raw_parts_mut(dib_data.cast::<u8>(), (requested_width * requested_height * 4) as usize) 
        };
        // Copy pixels from source to dest, un-premultiplying alpha on the CPU.
        // No pre-initialization needed: the loop below writes every byte of dest_data
        // (dest stride == width*4, matching the 32-bpp GDI DIB section with no padding).
        // This replaces the previous GPU-based D2D UnPremultiply effect.

        for y in 0..requested_height as usize {
            let src_start: usize = y * source_stride;
            let dest_start: usize = y * dest_stride;

            let src_slice = &source_data[src_start .. src_start + dest_stride];
            let dest_slice = &mut dest_data[dest_start .. dest_start + dest_stride];

            // Copy pixels and un-premultiply simultaneously
            for i in (0..dest_stride).step_by(4) {
                let a = src_slice[i + 3] as u32;
                if a == 0 || a == 255 {
                    dest_slice[i..i+4].copy_from_slice(&src_slice[i..i+4]);
                } else {
                    // Un-premultiply: Color = (Premultiplied * 255 + Rounding) / Alpha
                    dest_slice[i]   = (((src_slice[i] as u32 * 255) + (a / 2)) / a).min(255) as u8;   // B
                    dest_slice[i+1] = (((src_slice[i+1] as u32 * 255) + (a / 2)) / a).min(255) as u8; // G
                    dest_slice[i+2] = (((src_slice[i+2] as u32 * 255) + (a / 2)) / a).min(255) as u8; // R
                    dest_slice[i+3] = a as u8;                                                        // A
                }
            }
        }

        // The map_guard will automatically unmap the bitmap when it goes out of scope
        drop(map_guard);
        
        debug_log!("render_svg_to_hbitmap: Successfully completed rendering");
        Ok(hbitmap_guard.release())
    })();

    // Device-loss errors mean the cached D2D/D3D chain can no longer be used.
    // Mark it poisoned so the next render recreates it on this thread, outside the loader lock.
    if let Err(e) = &result {
        if is_graphics_device_lost(e.code()) {
            debug_log!( "render_svg_to_hbitmap: Graphics device lost ({:?}), marking resources as poisoned for recreation", e.code() );
            mark_thread_resources_poisoned();
        } else {
            debug_log!("render_svg_to_hbitmap: Error occurred: {:?}", e);
        }
    }

    result
}

fn mark_thread_resources_poisoned() {
    RESOURCES.with(|resources| {
        match resources.try_borrow_mut() {
            Ok(mut resources_ref) => {
                if let Some(resources) = resources_ref.as_mut() {
                    resources.poisoned = true;
                }
            }
            Err(_) => {
                debug_log!( "mark_thread_resources_poisoned: Could not borrow thread resources" );
            }
        }
    });
}

/// Returns true when the cached D2D/D3D device must be discarded and recreated.
fn is_graphics_device_lost(code: HRESULT) -> bool {
    code == D2DERR_RECREATE_TARGET
        || code == Dxgi::DXGI_ERROR_DEVICE_REMOVED
        || code == Dxgi::DXGI_ERROR_DEVICE_HUNG
        || code == Dxgi::DXGI_ERROR_DEVICE_RESET
        || code == Dxgi::DXGI_ERROR_DRIVER_INTERNAL_ERROR
}
// =================================================================
//                 COM Thumbnail Provider Object
// =================================================================

#[implement(Shell::PropertiesSystem::IInitializeWithStream, Shell::IThumbnailProvider)]
struct ThumbnailProvider {
    svg_data: Mutex<Option<Arc<[u8]>>>,
}

impl ThumbnailProvider {
    fn new() -> Self {
        dll_add_ref();
        debug_log!("ThumbnailProvider: Created new instance");
        Self {
            svg_data: Mutex::new(None),
        }
    }
}

impl Drop for ThumbnailProvider {
    fn drop(&mut self) {
        debug_log!("ThumbnailProvider: Dropping instance");
        dll_release();
    }
}

impl Shell::PropertiesSystem::IInitializeWithStream_Impl for ThumbnailProvider_Impl {
    #[allow(non_snake_case)]
    fn Initialize(&self, pstream: Ref<'_, Com::IStream>, _grfmode: u32) -> Result<()> {
        ffi_guard!(Keep, Result<()>, {          
            // log_message("Initialize: Starting SVG data loading");
            
            // Guard against repeated initialization calls
            if self.svg_data.lock().map_err(|_| Error::new(E_FAIL, "Mutex was poisoned"))?.is_some() {
                debug_log!("Initialize: Error - Already initialized");
                return Err(Error::from(HRESULT::from_win32(ERROR_ALREADY_INITIALIZED.0)));
            }

            match &*pstream {
                Some(stream) => {
                    // 101 MiB max file size.
                    const MAX_SIZE: u64 = 101 * 1024 * 1024;
                    pub const ERROR_FILE_TOO_LARGE: WIN32_ERROR = WIN32_ERROR(223u32);

                    // Fast Fail Check: Ask the stream for its size for a quick rejection.
                    // If the size check fails continue to read the stream in chunks, there is another safety net below.
                    let mut statstg = Default::default();
                    if unsafe { stream.Stat(&mut statstg, Com::STATFLAG_NONAME) }.is_ok() {
                        let stream_size = statstg.cbSize;
                        // log_message(&format!("Initialize: Stream reports size: {} bytes", stream_size));
                        if stream_size > 0 && stream_size > MAX_SIZE {
                            debug_log!("Initialize: Error - File too large: {} bytes (max: {} bytes)", stream_size, MAX_SIZE);
                            return Err(Error::from(HRESULT::from_win32(ERROR_FILE_TOO_LARGE.0)));
                        }
                    } else {
                        debug_log!("Initialize: Warning - Could not get stream size, will read with safety checks");
                    }

                    // Pre-allocate buffer if we got a valid stream size, otherwise let it grow.
                    let seq_stream: Com::ISequentialStream = stream.cast()?;
                    let capacity = if statstg.cbSize > 0 && statstg.cbSize <= MAX_SIZE {
                        statstg.cbSize as usize
                    } else {
                        0
                    };
                    let mut buffer: Vec<u8> = Vec::with_capacity(capacity);
                    let mut chunk: Vec<u8> = vec![0u8; 65536];
                    
                    loop {
                        let mut bytes_read: u32 = 0;
                        let hr: HRESULT = unsafe {
                            seq_stream.Read(
                                chunk.as_mut_ptr() as *mut core::ffi::c_void,
                                chunk.len() as u32,
                                Some(&mut bytes_read)
                            )
                        };
                        
                        if hr.is_err() || bytes_read == 0 {
                            if hr.is_err() {
                                debug_log!("Initialize: Stream read error: {:?}", hr);
                            }
                            break;
                        }

                        // A stream cannot have written more than the buffer it was handed, so a
                        // larger count is a broken (or hostile) ISequentialStream implementation.
                        // Trusting it would index past `chunk` and panic; clamping instead would
                        // append bytes the stream never actually wrote. Neither is acceptable for
                        // data that is about to be parsed, so reject the stream outright.
                        let bytes_read = bytes_read as usize;
                        if bytes_read > chunk.len() {
                            debug_log!("Initialize: Error - Stream reported {} bytes read into a {}-byte buffer", bytes_read, chunk.len());
                            return Err(Error::new(E_UNEXPECTED, "Stream reported reading more bytes than were requested"));
                        }

                        // Extra file size safety net protects memory usage in case statstg failed or returned a wrong size.
                        if buffer.len() + bytes_read > (MAX_SIZE as usize) {
                            debug_log!("Initialize: Error - File too large during read: {} bytes (max: {} bytes)", buffer.len() + bytes_read, MAX_SIZE);
                            return Err(Error::from(HRESULT::from_win32(ERROR_FILE_TOO_LARGE.0)));
                        }

                        buffer.extend_from_slice(&chunk[..bytes_read]);
                    }
                    
                    // log_message(&format!("Initialize: Successfully loaded {} bytes of SVG data", buffer.len()));
                    
                    // Convert to Arc<[u8]> to save memory overhead
                    *self.svg_data.lock().map_err(|_| Error::new(E_FAIL, "Mutex was poisoned"))? = Some(Arc::from(buffer.into_boxed_slice()));
                    
                    // log_message("Initialize: Succeeded.");
                    Ok(())
                }
                None => {
                    // This case handles if Windows passes a null stream.
                    debug_log!("Initialize: Error - Stream was null.");
                    Err(E_INVALIDARG.into())
                }
            }
        })
    }
}

impl Shell::IThumbnailProvider_Impl for ThumbnailProvider_Impl {
    #[allow(non_snake_case)]
    fn GetThumbnail(&self, cx: u32, phbmp: *mut Gdi::HBITMAP, pdwalpha: *mut Shell::WTS_ALPHATYPE) -> Result<()> {
        // The only entry point that renders, so the only one where a caught panic can
        // have left this thread's cached D2D/D3D chain in an unusable state.
        ffi_guard!(Discard, Result<()>, {
            // log_message(&format!("GetThumbnail: Entered with size: {}x{}", cx, cx));
			
			// Both output pointers are dereferenced unconditionally below, so they must be
            // validated first. The Shell is expected to supply valid pointers, but this is a public
            // COM vtable entry: any in-process caller can reach it, and dereferencing a null
            // here would take down the host process instead of returning a clean E_POINTER.
            if phbmp.is_null() || pdwalpha.is_null() {
                debug_log!("GetThumbnail: Error - Null output pointer (phbmp_null={}, pdwalpha_null={})", phbmp.is_null(), pdwalpha.is_null());
                return Err(Error::new(E_POINTER, "Null output pointer passed to GetThumbnail"));
            }
			
            // Initialize output parameters to safe defaults (COM contract requirement)
            // pdwalpha is set to UNKNOWN for all failure cases, only changed to ARGB on success
            unsafe {
                *phbmp = Gdi::HBITMAP(std::ptr::null_mut());
                *pdwalpha = Shell::WTSAT_UNKNOWN;
            }

            // Clone the Arc (cheap pointer copy) and release the mutex before rendering to prevent deadlocks
            let svg_data = {
                let data_guard = self.svg_data.lock().map_err(|_| Error::new(E_FAIL, "Mutex was poisoned"))?;
                
                match data_guard.as_ref() {
                    Some(data) => {
                        // log_message(&format!("GetThumbnail: SVG data is {} bytes.", data.len()));
                        Arc::clone(data) // Clone the Arc (cheap pointer copy)
                    }
                    None => {
                        debug_log!("GetThumbnail: Error - SVG data was not initialized.");
                        return Err(Error::new(E_UNEXPECTED, "SVG data not initialized"));
                    }
                }
            }; // Mutex lock is released here

            // First attempt. On device-lost, resources are marked poisoned; retry once so a transient DXGI/D2D reset does not immediately fall back.
            let mut render_result = render_svg_to_hbitmap(&svg_data[..], cx, cx);
            if let Err(ref e) = render_result {
                if is_graphics_device_lost(e.code()) {
                    debug_log!( "GetThumbnail: retrying once after graphics device loss ({:?})", e.code() );
                    render_result = render_svg_to_hbitmap(&svg_data[..], cx, cx);
                }
            }

            match render_result {
                Ok(hbitmap) => {
                    // log_message("GetThumbnail: render_svg_to_hbitmap succeeded.");
                    unsafe {
                        *phbmp = hbitmap;
                        *pdwalpha = Shell::WTSAT_ARGB;
                    }
                    // log_message("GetThumbnail: Succeeded.");
                    Ok(())
                }
                Err(e) => {
                    debug_log!("GetThumbnail: render_svg_to_hbitmap failed with error: {:?}", e);
                    
                    // Instead of returning an error, create a fallback thumbnail
                    match create_fallback_thumbnail(cx) {
                        Ok(fallback_hbitmap) => {
                            debug_log!("GetThumbnail: Created fallback thumbnail for invalid SVG.");
                            unsafe {
                                *phbmp = fallback_hbitmap;
                                *pdwalpha = Shell::WTSAT_ARGB;
                            }
                            Ok(())
                        }
                        Err(fallback_err) => {
                            debug_log!("GetThumbnail: Failed to create fallback thumbnail: {:?}", fallback_err);
                            Err(e) // Only return error if we can't even create a fallback
                        }
                    }
                }
            }
        })
    }
}

/// Creates a simple fallback thumbnail for invalid SVG files
fn create_fallback_thumbnail(size: u32) -> Result<Gdi::HBITMAP> {
    // log_message(&format!("create_fallback_thumbnail: Creating fallback thumbnail of size {}x{}", size, size));
    
    // Use a hardcoded "broken file" SVG with red X pattern
    const FALLBACK_SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\"><g><line stroke-width=\"2\" stroke=\"#ff0000\" y2=\"256\" x2=\"0\" y1=\"0\" x1=\"256\" fill=\"none\"/><line stroke-width=\"2\" y2=\"256\" x2=\"256\" y1=\"0\" x1=\"0\" stroke=\"#ff0000\" fill=\"none\"/></g></svg>";
    
    // Try to render the fallback SVG using our normal rendering pipeline
    match render_svg_to_hbitmap(FALLBACK_SVG, size, size) {
        Ok(hbitmap) => {
            debug_log!("create_fallback_thumbnail: Successfully created SVG-based fallback");
            Ok(hbitmap)
        },
        Err(e) => {
            debug_log!("create_fallback_thumbnail: SVG fallback failed: {:?}, creating bitmap fallback", e);
            // If even the fallback SVG fails to render, create a simple black square as last resort
            let bmi = Gdi::BITMAPINFO {
                bmiHeader: Gdi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<Gdi::BITMAPINFOHEADER>() as u32,
                    biWidth: size as i32,
                    biHeight: -(size as i32), // Negative for top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: Gdi::BI_RGB.0 as u32,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut dib_data: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbitmap_handle: Gdi::HBITMAP = unsafe {
                Gdi::CreateDIBSection(None, &bmi, Gdi::DIB_RGB_COLORS, &mut dib_data, None, 0)
            }?;

            let hbitmap_guard = HBitmapGuard::new(hbitmap_handle);

            if dib_data.is_null() {
                return Err(Error::new(
                    E_FAIL,
                    "CreateDIBSection returned a null fallback pixel buffer",
                ));
            }

            // Fill with solid black (BGRA format: 0xFF000000)
            let pixel_count = (size * size) as usize;
            let buffer: &mut [u32] = unsafe {
                std::slice::from_raw_parts_mut(dib_data as *mut u32, pixel_count)
            };
            
            // Solid black with full alpha
            buffer.fill(0xFF000000);

            debug_log!("create_fallback_thumbnail: Successfully created bitmap-based fallback");
            Ok(hbitmap_guard.release())
        }
    }
}

// =================================================================
//                      COM Class Factory
// =================================================================

#[implement(Com::IClassFactory)]
struct ClassFactory;

impl ClassFactory {
    fn new() -> Self {
        dll_add_ref();
        debug_log!("ClassFactory: Created new instance");
        Self {}
    }
}

impl Drop for ClassFactory {
    fn drop(&mut self) {
        debug_log!("ClassFactory: Dropping instance");
        dll_release();
    }
}

impl Com::IClassFactory_Impl for ClassFactory_Impl {
    #[allow(non_snake_case)]
    fn CreateInstance(&self, punkouter: Ref<'_, IUnknown>, riid: *const GUID, ppvobject: *mut *mut std::ffi::c_void) -> Result<()> {
        ffi_guard!(Keep, Result<()>, {           
            // log_message(&format!("ClassFactory::CreateInstance: Entered. Requesting interface: {:?}", unsafe { *riid }));

            // Safety checks for null pointers
            if riid.is_null() || ppvobject.is_null() {
                debug_log!("ClassFactory::CreateInstance: Error - Null pointer passed");
                return Err(Error::new(E_POINTER, "Null pointer passed to CreateInstance"));
            }

            // We do not support aggregation.
            if !punkouter.is_null() {
                debug_log!("ClassFactory::CreateInstance: Error - Aggregation not supported.");
                return Err(Error::new(CLASS_E_NOAGGREGATION, "Aggregation not supported"));
            }
            
            debug_log!("ClassFactory::CreateInstance: Creating ThumbnailProvider instance");
            
            // Create an instance of our ThumbnailProvider
            let thumbnail_provider: IUnknown = ThumbnailProvider::new().into();
            
            // Query for the interface requested by the caller and return it.
            let hr: HRESULT = unsafe { thumbnail_provider.query(&*riid, ppvobject) };
         
            if hr.is_ok() {
                Ok(())
            } else {
                debug_log!("ClassFactory::CreateInstance: Error - Exiting with HRESULT: {:?}", hr);
                Err(Error::new(hr, "Failed to query interface"))
            }
        })
    }

    #[allow(non_snake_case)]
    fn LockServer(&self, flock: BOOL) -> Result<()> {
        ffi_guard!(Keep, Result<()>, {
            // Server locks are counted separately from live objects (see SERVER_LOCKS):
            // a client that unlocks without a matching lock must not be able to consume a
            // reference that belongs to a living provider.
            if flock.as_bool() {
                let locks = SERVER_LOCKS.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                debug_log!("ClassFactory::LockServer: Server locked. Outstanding locks: {}", locks);
            } else {
                let locks = SERVER_LOCKS.fetch_sub(1, Ordering::Release).saturating_sub(1);
                if locks < 0 {
                    debug_log!("ClassFactory::LockServer: Unmatched unlock, lock balance now {}", locks);
                } else {
                    debug_log!("ClassFactory::LockServer: Server unlocked. Outstanding locks: {}", locks);
                }
            }
            Ok(())
        })
    }
}

// =================================================================
//                      DLL Global State & Exports
// =================================================================

// A global reference counter for the DLL itself: one reference per live COM object.
static DLL_REFERENCES: AtomicU32 = AtomicU32::new(0);
// Outstanding IClassFactory::LockServer(TRUE) locks, counted separately from live objects.
//
// A client that calls LockServer(FALSE) without a matching lock is violating the COM
// contract, but folding that unmatched unlock into DLL_REFERENCES would let it consume a
// reference belonging to a live provider - and then DllCanUnloadNow reports S_OK while
// that provider is still in use, which entitles COM to FreeLibrary the DLL out from under
// it. Keeping the two counters apart confines the damage to the lock ledger.
//
// Signed on purpose: an over-release goes negative rather than being clamped, so a client
// that unlocks once too often and then locks again ends up balanced instead of leaving a
// lock outstanding that nothing can ever release. Any balance <= 0 means "no locks held".
static SERVER_LOCKS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
// A global handle to the DLL module instance - using Option for safer null checking
static MODULE_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
// Global flag for hardware acceleration preference (defaults to false = WARP)
static USE_HARDWARE_ACCELERATION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Global flag for whether to enable debug logging
static ENABLE_DEBUG_LOGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// A global OnceLock for the log file path, initialized only once
static LOG_FILE_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn dll_add_ref() {
    let new_count = DLL_REFERENCES.fetch_add(1, Ordering::Relaxed) + 1;
    debug_log!("DLL reference added. New count: {}", new_count);
}
fn dll_release() {
    // Saturating: the count must never wrap to a huge value, which would keep the DLL
    // mapped for the life of the process, nor below zero, which would understate the
    // number of live objects.
    let old_count = DLL_REFERENCES
        .fetch_update(Ordering::Release, Ordering::Relaxed, |count| Some(count.saturating_sub(1)))
        .unwrap_or(0); // The update closure never returns None, so this is always Ok.
    debug_log!("DLL reference released. New count: {}", old_count.saturating_sub(1));
}

/// Generic function to read registry values from HKEY_CLASSES_ROOT\.svg
/// Returns the value as a u32 if it exists and is a valid DWORD, otherwise returns None
fn read_svg_registry_dword(value_name: &str) -> Option<u32> {
    let mut svg_key: HKEY = HKEY::default();
    let result = unsafe { 
        RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            w!(".svg"),
            Some(0),
            KEY_READ,
            &mut svg_key,
        )
    };
    
    if result.is_ok() {
        let svg_key_guard = RegistryKeyGuard(svg_key);
        
        let mut value: u32 = 0;
        let mut value_size = std::mem::size_of::<u32>() as u32;
        let mut value_type = REG_DWORD;
        
        // Convert the value name to a wide string
        let wide_name = to_pcwstr(value_name);
        
        let query_result = unsafe {
            RegQueryValueExW(
                svg_key_guard.0,
                PCWSTR(wide_name.as_ptr()),
                None,
                Some(&mut value_type),
                Some(&mut value as *mut u32 as *mut u8),
                Some(&mut value_size),
            )
        };
        
        // Only return the value if it exists, is a DWORD, and has the expected size
        if query_result.is_ok() && value_type == REG_DWORD && value_size == std::mem::size_of::<u32>() as u32 {
            return Some(value);
        } else if !query_result.is_ok() {
            debug_log!("Registry read failed for '{}': {:?}", value_name, query_result);
        }
    } // Registry key automatically closed here by RegistryKeyGuard
    
    None // Return
}

// Checks the registry for the hardware acceleration preference.
// Only called once during DLL initialization.
fn check_hardware_acceleration_registry() {
    // log_message("Checking registry for hardware acceleration preference...");
    
    // Default to WARP (software rendering) for stability
    let use_hardware = match read_svg_registry_dword("win_svg_thumbs_use_hardware") {
        Some(1) => {
            debug_log!("Registry: Hardware acceleration ENABLED");
            true  // Only enable hardware if value exists and equals 1
        },
        Some(value) => {
            debug_log!("Registry: Hardware acceleration disabled (value: {})", value);
            false
        },
        None => {
            debug_log!("Registry: Hardware acceleration preference not found, defaulting to WARP (software)");
            false       // Default to WARP for any other case (missing, 0, or other values)
        }
    };
    
    USE_HARDWARE_ACCELERATION.store(use_hardware, Ordering::Relaxed);
}

// Checks registry for setting for whether to enable debug logging
fn check_debug_logging_registry() {
    // Note: We can't log here initially since logging might not be enabled yet
    let enable_debug = match read_svg_registry_dword("win_svg_thumbs_enable_debug_log") {
        Some(1) => true,  // Only enable debug logging if value exists and equals 1
        _ => false,       // Default to disabled for any other case (missing, 0, or other values)
    };
    
    ENABLE_DEBUG_LOGGING.store(enable_debug, Ordering::Relaxed);
    
    // Now we can log since the flag is set
    if enable_debug {
        debug_log!("Debug logging ENABLED via registry");
    }
}

// This is our thumbnail provider's unique Class ID (CLSID).
// Use a new GUID for your own projects!
const CLSID_SVG_THUMBNAIL_PROVIDER: GUID = GUID::from_u128(0xa884a812_58fd_47d5_bda6_4fab4fabdcd9);

#[no_mangle]
#[allow(non_snake_case)]
extern "system" fn DllMain(hinst_dll: HMODULE, fdw_reason: u32, _lpv_reserved: *const std::ffi::c_void) -> BOOL {
    // DllMain runs under the loader lock, so it must stay minimal: no COM, no
    // shell calls, no file I/O or logging, nothing that can panic or touch TLS.
    // Storing the module handle in an atomic is the only work that is both safe
    // and needed here (DllRegisterServer uses it to resolve the DLL path).
    if fdw_reason == System::SystemServices::DLL_PROCESS_ATTACH {
        MODULE_HANDLE.store(hinst_dll.0 as *mut _, Ordering::Release);
    }
    true.into()
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut std::ffi::c_void) -> HRESULT {
    ffi_guard!(Keep, HRESULT, {
        // Guarantee registry reads happen exactly once per process, completely off the loader lock.
        INIT_REGISTRY.call_once(|| {
            check_debug_logging_registry();
            check_hardware_acceleration_registry();
        });
        
        debug_log!("DllGetClassObject: Entered");
        
        // Safety checks for null pointers
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            debug_log!("DllGetClassObject: Error - Null pointer passed");
            return E_POINTER;
        }

        // Check if the caller is asking for our specific class.
        if unsafe { *rclsid } != CLSID_SVG_THUMBNAIL_PROVIDER {
            debug_log!("DllGetClassObject: Error - CLSID mismatch. Requested: {:?}, Expected: {:?}", unsafe { *rclsid }, CLSID_SVG_THUMBNAIL_PROVIDER);
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        
        debug_log!("DllGetClassObject: Creating class factory for SVG Thumbnail Provider");
        
        // Create our class factory.
        let factory: Com::IClassFactory = ClassFactory::new().into();
        
        // Query for the interface the caller wants (usually IClassFactory) and return it.
        let hr: HRESULT = unsafe { factory.query(riid, ppv) };
        
        // The factory variable will automatically drop here, releasing our local reference.
        // The caller retains their reference from the query() call.

        // log_message(&format!("DllGetClassObject: Exiting with HRESULT: {:?}", hr));
        // Log only if it's an error
        if hr.is_err() {
            debug_log!("DllGetClassObject: Error - Exiting with HRESULT: {:?}", hr);
        } else {
            // log_message("DllGetClassObject: Succeeded.");
        }
        
        hr
    })
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    ffi_guard!(Keep, HRESULT, {
        let ref_count = DLL_REFERENCES.load(Ordering::Acquire);
        let lock_count = SERVER_LOCKS.load(Ordering::Acquire);

        if ref_count == 0 && lock_count <= 0 {
            debug_log!("DllCanUnloadNow: Returning S_OK - DLL can be unloaded");
            S_OK
        } else {
            debug_log!("DllCanUnloadNow: Returning S_FALSE - DLL still has {} active references and {} server locks", ref_count, lock_count);
            S_FALSE
        }
    })
}


// =================================================================
//                      DLL Registration
// =================================================================

/// Helper to convert a Rust string slice to a null-terminated UTF-16 wide string.
fn to_pcwstr(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn create_registry_keys() -> Result<()> {
    debug_log!("create_registry_keys: Starting registry key creation");
    
    let clsid_string = format!("{{{CLSID_SVG_THUMBNAIL_PROVIDER:?}}}");
    let dll_path = get_dll_path()?;
    debug_log!("create_registry_keys: Using CLSID: {} and DLL path: {}", clsid_string, dll_path);

    // Create CLSID\{our-clsid}
    // log_message("create_registry_keys: Creating CLSID root key");
    let clsid_root_key = RegistryKeyGuard::create_root_key(HKEY_CLASSES_ROOT, &w!("CLSID"))?;

    debug_log!("create_registry_keys: Creating CLSID subkey and setting description");
    let clsid_key = clsid_root_key.create_subkey(&PCWSTR(to_pcwstr(&clsid_string).as_ptr()))?;
    clsid_key.set_string_value("", "SVG Thumbnail Provider (Rust)")?;

    // Create CLSID\{our-clsid}\InprocServer32
    debug_log!("create_registry_keys: Creating InprocServer32 key");
    let inproc_key = clsid_key.create_subkey(&w!("InprocServer32"))?;
    inproc_key.set_string_value("", &dll_path)?;
    inproc_key.set_string_value("ThreadingModel", "Apartment")?;

    // Associate with .svg files
    debug_log!("create_registry_keys: Associating with .svg files");
    let svg_root_key = RegistryKeyGuard(HKEY_CLASSES_ROOT).create_subkey(&w!(".svg"))?;
    let svg_shellex_key = svg_root_key.create_subkey(&w!("shellex"))?;
    let svg_handler_key = svg_shellex_key.create_subkey(&w!("{E357FCCD-A995-4576-B01F-234630154E96}"))?;
    svg_handler_key.set_string_value("", &clsid_string)?;

    // Associate with .svgz files
    debug_log!("create_registry_keys: Associating with .svgz files");
    let svgz_root_key = RegistryKeyGuard(HKEY_CLASSES_ROOT).create_subkey(&w!(".svgz"))?;
    let svgz_shellex_key = svgz_root_key.create_subkey(&w!("shellex"))?;
    let svgz_handler_key = svgz_shellex_key.create_subkey(&w!("{E357FCCD-A995-4576-B01F-234630154E96}"))?;
    svgz_handler_key.set_string_value("", &clsid_string)?;

    // log_message("create_registry_keys: Notifying shell of association changes");
    unsafe { Shell::SHChangeNotify(Shell::SHCNE_ASSOCCHANGED, Shell::SHCNF_IDLIST, None, None) };

    // log_message("create_registry_keys: Successfully completed registry key creation");
    Ok(())
}

fn get_dll_path() -> Result<String> {
    let handle_ptr: *mut std::ffi::c_void = MODULE_HANDLE.load(Ordering::Acquire);

    // Check for null pointer to avoid potential crashes
    if handle_ptr.is_null() {
        return Err(Error::new(E_FAIL, "MODULE_HANDLE is null; DLL not loaded?"));
    }

    let handle: HMODULE = HMODULE(handle_ptr);
    let mut path = vec![0u16; MAX_PATH as usize];
    let len: u32 = unsafe { System::LibraryLoader::GetModuleFileNameW(Some(handle), &mut path) };

    // If the returned length is zero, it's an error
    if len == 0 {
        return Err(Error::new(E_FAIL, "GetModuleFileNameW failed (returned 0)"));
    }

    // If the returned length is equal to the buffer size, truncation may have occurred
    if (len as usize) >= path.len() {
        return Err(Error::new(E_FAIL, "DLL path is too long (truncated); registration aborted"));
    }

    // Additional safety check - ensure we don't go beyond the buffer
    let len = std::cmp::min(len as usize, path.len());
    Ok(String::from_utf16_lossy(&path[..len]))
}

// RAII wrapper for registry keys - automatically closes when dropped
struct RegistryKeyGuard(HKEY);

impl Drop for RegistryKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe { let _ = RegCloseKey(self.0); }
        }
    }
}

impl RegistryKeyGuard {
    fn create_subkey(&self, name: &PCWSTR) -> Result<RegistryKeyGuard> {
        let mut key = HKEY::default();
        let mut disposition = REG_CREATE_KEY_DISPOSITION(0);
        unsafe {
            RegCreateKeyExW(
                self.0,
                *name,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                WRITE_FLAGS,
                None,
                &mut key,
                Some(&mut disposition as *mut _)
            ).ok()?;
        }
        if key.is_invalid() {
            return Err(Error::new(E_FAIL, "RegCreateKeyExW returned null handle"));
        }

        Ok(RegistryKeyGuard(key))
    }

    fn create_root_key(hive: HKEY, name: &PCWSTR) -> Result<RegistryKeyGuard> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                hive,
                *name,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                WRITE_FLAGS,
                None,
                &mut key,
                None
            ).ok()?;
        }
        Ok(RegistryKeyGuard(key))
    }

    /// Sets a REG_SZ (string) value for this registry key.
    /// The `name` can be an empty string to set the (Default) value.
    fn set_string_value(&self, name: &str, value: &str) -> Result<()> {
        // Convert name and value to null-terminated wide strings (Vec<u16>)
        let wide_name = to_pcwstr(name);
        let wide_value = to_pcwstr(value);

        // The size for RegSetValueExW must be in bytes, including the null terminator.
        // to_pcwstr already adds the null terminator, so wide_value.len() is correct.
        let value_size_bytes = (wide_value.len() * std::mem::size_of::<u16>()) as u32;

        unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(wide_name.as_ptr()),
                None,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide_value.as_ptr() as *const u8,
                    value_size_bytes as usize,
                )),
            ).ok()?;
        }
        Ok(())
    }
}

fn delete_registry_keys() -> Result<()> {
    debug_log!("delete_registry_keys: Starting registry key deletion");
    
    let clsid_string = format!("{{{CLSID_SVG_THUMBNAIL_PROVIDER:?}}}");
    debug_log!("delete_registry_keys: Deleting keys for CLSID: {}", clsid_string);
    // Track if we encountered any real errors (not just "not found")
    let mut first_real_error: Option<Error> = None;
    
    // Helper closure for robust key deletion
    let mut delete_key_with_error_tracking = |key_path: PCWSTR| {
        let result = unsafe { RegDeleteKeyExW(HKEY_CLASSES_ROOT, key_path, WRITE_FLAGS.0, Some(0)) };
        if result == ERROR_SUCCESS || result == ERROR_FILE_NOT_FOUND {
            // Success or key already gone - both fine for uninstall
        } else {
            // Real error (access denied, etc.) - remember the first one we see
            if first_real_error.is_none() {
                first_real_error = Some(Error::new(result.into(), "Registry key deletion failed"));
            }
        }
    };
    
    // Try to delete all keys, tracking errors but not stopping
    let inproc_path = to_pcwstr(&format!("CLSID\\{}\\InprocServer32", clsid_string));
    delete_key_with_error_tracking(PCWSTR(inproc_path.as_ptr()));

    let clsid_path = to_pcwstr(&format!("CLSID\\{}", clsid_string));
    delete_key_with_error_tracking(PCWSTR(clsid_path.as_ptr()));
    
    delete_key_with_error_tracking(w!(".svg\\shellex\\{E357FCCD-A995-4576-B01F-234630154E96}"));
    delete_key_with_error_tracking(w!(".svgz\\shellex\\{E357FCCD-A995-4576-B01F-234630154E96}"));

    // Always notify of association changes, even if some deletions failed
    unsafe { Shell::SHChangeNotify(Shell::SHCNE_ASSOCCHANGED, Shell::SHCNF_IDLIST, None, None) };

    // Now propagate the first real error we encountered, if any
    match first_real_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}


#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    ffi_guard!(Keep, HRESULT, {
        // log_message("DllRegisterServer: Starting registration");
        match create_registry_keys() {
            Ok(_) => {
                debug_log!("DllRegisterServer: Registration succeeded");
                S_OK
            },
            Err(e) => {
                debug_log!("DllRegisterServer: Registration failed: {:?}", e);
                E_FAIL
            },
        }
    })
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    ffi_guard!(Keep, HRESULT, {
        // log_message("DllUnregisterServer: Starting unregistration");
        match delete_registry_keys() {
            Ok(_) => {
                debug_log!("DllUnregisterServer: Unregistration succeeded");
                S_OK
            },
            Err(e) => {
                debug_log!("DllUnregisterServer: Unregistration failed: {:?}", e);
                E_FAIL
            },
        }
    })
}

#[no_mangle]
// Simple function that only notifies the shell of file association changes.
pub extern "system" fn notify_shell_change() -> HRESULT {
    ffi_guard!(Keep, HRESULT, {
        // Notify the shell that file associations have changed
        unsafe { Shell::SHChangeNotify(Shell::SHCNE_ASSOCCHANGED, Shell::SHCNF_IDLIST, None, None) };
        S_OK
    })
}

// =================================================================

// -------------- Logger ----------------
fn log_message(message: &str) {
    if !ENABLE_DEBUG_LOGGING.load(Ordering::Relaxed) {
        return;
    }

    // get_or_init will only execute the closure ONCE, the very first time it's called.
    // All subsequent calls will return the cached value instantly.
    let log_path_option = LOG_FILE_PATH.get_or_init(|| {
        let known_folder_flags = Shell::KNOWN_FOLDER_FLAG::default(); // Use default flags, no special options
        let desktop_path_pwstr = match unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, known_folder_flags, None) } {
            Ok(path) => path,
            Err(_) => return None, // Initialization failed, cache 'None'
        };

        let desktop_path_guard = CoTaskMemFreeGuard(desktop_path_pwstr);
        
        let mut path = match unsafe { desktop_path_guard.0.to_string() } {
            Ok(s) => PathBuf::from(s),
            Err(_) => return None, // Conversion failed, cache 'None'
        };
        
        path.push("win_svg_thumb_debug_log.txt");
        Some(path) // Success! Cache the full path.
        // --- End of one-time execution block ---
    });

    // Now, use the cached path.
    // If initialization failed, log_path_option will be &None, and we'll do nothing.
    if let Some(log_path) = log_path_option {
        match std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
            Ok(mut file) => {
                let pid = std::process::id();
                let tid = std::thread::current().id();
                let time_str = get_formatted_time_string_win_api();

                let _ = writeln!(file, "[PID: {} | TID: {:?}] [{}] {}", pid, tid, time_str, message);
            }
            Err(_) => {
                // Opening the file failed.
            }
        }
    }
}

fn get_formatted_time_string_win_api() -> String {
    let system_time = unsafe { GetLocalTime() };
    let mut time_buffer = [0u16; 64]; // Buffer for formatted time string
    let chars_written = unsafe { // Returns the number of characters put into the buffer. If 0 it failed
        GetTimeFormatEx(
            None, // Null is LOCALE_NAME_USER_DEFAULT
            TIME_FORMAT_FLAGS::default(), // Default time format flags
            Some(&system_time),
            None, // Use default format
            Some(&mut time_buffer),
        )
    };

    if chars_written > 0 {
        return String::from_utf16_lossy(&time_buffer[..chars_written as usize - 1]) // -1 to remove null terminator
    } else {
        // Fallback if formatting fails
         return format!("{:02}:{:02}:{:02}.{:03}",
            system_time.wHour,
            system_time.wMinute,
            system_time.wSecond,
            system_time.wMilliseconds)
    }
}

// fn serialize_svg_doc(svg_doc: &ID2D1SvgDocument) {
//     //TESTING: Serialize the SVG document to inspect its contents with ID2D1SvgDocument::Serialize
//     // Create a memory stream to capture the serialized SVG data
//     if let Some(memory_stream) = unsafe { Shell::SHCreateMemStream(Some(&[])) } {
//         // Serialize the SVG document
//         let serialize_result = unsafe { svg_doc.Serialize(&memory_stream, None) };
        
//         if serialize_result.is_ok() {
//             // Seek back to the beginning of the stream
//             let mut new_position = 0u64;
//             let seek_result = unsafe { 
//                 memory_stream.Seek(0, Com::STREAM_SEEK_SET, Some(&mut new_position))
//             };
            
//             if seek_result.is_ok() {
//                 // Read the entire stream content
//                 let mut buffer = Vec::new();
//                 let mut chunk = vec![0u8; 4096];
                
//                 loop {
//                     let mut bytes_read = 0u32;
//                     let read_result = unsafe { 
//                         memory_stream.Read(
//                             chunk.as_mut_ptr() as *mut std::ffi::c_void, 
//                             chunk.len() as u32, 
//                             Some(&mut bytes_read)
//                         ) 
//                     };
                    
//                     if read_result.is_err() || bytes_read == 0 {
//                         break;
//                     }
                    
//                     buffer.extend_from_slice(&chunk[..bytes_read as usize]);
//                 }
                
//                 // Convert to string for debugging
//                 let serialized_svg = String::from_utf8_lossy(&buffer);
//                 print!("{}", serialized_svg);
//             } else {
//                 print!("[DEBUG] Failed to seek to beginning of stream");
//             }
//         } else {
//             print!("[DEBUG] Failed to serialize SVG document");
//         }
//     }
// }
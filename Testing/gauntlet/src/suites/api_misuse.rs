//! COM contract abuse.
//!
//! Explorer is a well-behaved caller, so nothing in normal operation ever
//! exercises the provider's error paths. But an in-process COM server is
//! reachable by anything running in the host process, and the shell itself
//! changes its calling patterns between Windows releases. Every check here is a
//! documented COM requirement that the provider must satisfy without faulting
//! the host.

use std::ffi::c_void;

use windows::core::*;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOINTERFACE, E_POINTER, S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::{IClassFactory, IPersist};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, WTS_ALPHATYPE, WTSAT_UNKNOWN};

use crate::corpus::BASE_SVG;
use crate::dll::{self, Dll, CLSID_SVG_THUMBNAIL_PROVIDER};
use crate::report::Report;

pub fn run(dll_handle: &Dll, report: &mut Report) {
    identity(dll_handle, report);
    class_object_contract(dll_handle, report);
    factory_contract(dll_handle, report);
    initialize_contract(dll_handle, report);
    thumbnail_contract(dll_handle, report);
    lock_server_contract(dll_handle, report);
    release_ordering(dll_handle, report);
    size_boundaries(dll_handle, report);
    // Last: these deliberately leave the DLL's lock ledger inconsistent.
    unbalanced_unlock_reference_count(dll_handle, report);
    server_lock_ledger(dll_handle, report);
}

// ---------------------------------------------------------------
//                      Module identity
// ---------------------------------------------------------------

/// Confirms the gauntlet is exercising exactly one copy of the intended DLL.
///
/// Every other check in every other suite is meaningless if the process ended up
/// with a different build mapped (a stale copy on the search path) or with the
/// module loaded twice under different paths, so this runs first.
fn identity(dll_handle: &Dll, report: &mut Report) {
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

    report.begin_case("module_identity");
    unsafe {
        let mut path = vec![0u16; 1024];
        let len = GetModuleFileNameW(Some(dll_handle.module), &mut path);
        let loaded_path = String::from_utf16_lossy(&path[..len as usize]);

        // Look the module up by base name: if a different copy were already
        // mapped, this would resolve to that one instead of ours.
        let base = std::path::Path::new(&loaded_path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let wide: Vec<u16> = base.encode_utf16().chain(std::iter::once(0)).collect();
        let by_name = GetModuleHandleW(PCWSTR(wide.as_ptr())).ok();

        let same = by_name.map(|h| h.0 == dll_handle.module.0).unwrap_or(false);
        report.check(
            "module_identity",
            len > 0 && same,
            format!(
                "loaded {loaded_path}; lookup by base name resolves to the same module={same}"
            ),
        );
    }
}

// ---------------------------------------------------------------
//                      DllGetClassObject
// ---------------------------------------------------------------

fn class_object_contract(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("dll_get_class_object_null_args");
    // COM requires E_POINTER (or at minimum a clean failure) for null
    // out-parameters. A crash here would take the host down at activation time.
    let mut out: *mut c_void = std::ptr::null_mut();
    let hr_all_null = dll_handle.get_class_object_raw(
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null_mut(),
    );
    let hr_null_clsid = dll_handle.get_class_object_raw(
        std::ptr::null(),
        &IClassFactory::IID,
        &mut out,
    );
    let hr_null_iid = dll_handle.get_class_object_raw(
        &CLSID_SVG_THUMBNAIL_PROVIDER,
        std::ptr::null(),
        &mut out,
    );
    let hr_null_ppv = dll_handle.get_class_object_raw(
        &CLSID_SVG_THUMBNAIL_PROVIDER,
        &IClassFactory::IID,
        std::ptr::null_mut(),
    );
    report.check(
        "dll_get_class_object_null_args",
        [hr_all_null, hr_null_clsid, hr_null_iid, hr_null_ppv]
            .iter()
            .all(|hr| *hr == E_POINTER),
        format!(
            "all-null=0x{:08X} null-clsid=0x{:08X} null-iid=0x{:08X} null-ppv=0x{:08X} (expected E_POINTER for each)",
            hr_all_null.0 as u32, hr_null_clsid.0 as u32, hr_null_iid.0 as u32, hr_null_ppv.0 as u32
        ),
    );

    report.begin_case("dll_get_class_object_unknown_clsid");
    let other = GUID::from_u128(0x00000000_1111_2222_3333_444455556666);
    let mut out: *mut c_void = std::ptr::null_mut();
    let hr = dll_handle.get_class_object_raw(&other, &IClassFactory::IID, &mut out);
    report.check(
        "dll_get_class_object_unknown_clsid",
        hr == CLASS_E_CLASSNOTAVAILABLE && out.is_null(),
        format!("hr=0x{:08X} out_null={} (expected CLASS_E_CLASSNOTAVAILABLE)", hr.0 as u32, out.is_null()),
    );

    report.begin_case("dll_get_class_object_unsupported_iid");
    // Asking the class object for an interface it does not implement must fail
    // cleanly and must not write a bogus pointer to the out-parameter.
    let mut out: *mut c_void = std::ptr::null_mut();
    let hr = dll_handle.get_class_object_raw(
        &CLSID_SVG_THUMBNAIL_PROVIDER,
        &IPersist::IID,
        &mut out,
    );
    report.check(
        "dll_get_class_object_unsupported_iid",
        hr.is_err() && out.is_null(),
        format!("hr=0x{:08X} out_null={} (expected a failure with a null out-pointer)", hr.0 as u32, out.is_null()),
    );
}

// ---------------------------------------------------------------
//                        IClassFactory
// ---------------------------------------------------------------

fn factory_contract(dll_handle: &Dll, report: &mut Report) {
    let factory = match dll_handle.class_factory() {
        Ok(f) => f,
        Err(e) => {
            report.fail("factory_available", format!("could not obtain a class factory: {e:?}"));
            return;
        }
    };

    report.begin_case("create_instance_rejects_aggregation");
    // The provider does not support aggregation, so it must return
    // CLASS_E_NOAGGREGATION when handed a non-null outer unknown.
    let Ok(outer) = factory.cast::<IUnknown>() else {
        report.fail("create_instance_rejects_aggregation", "could not QI the factory for IUnknown");
        return;
    };
    let result: Result<IInitializeWithStream> = unsafe { factory.CreateInstance(&outer) };
    let hr = result.as_ref().err().map(|e| e.code()).unwrap_or(S_OK);
    report.check(
        "create_instance_rejects_aggregation",
        hr == CLASS_E_NOAGGREGATION,
        format!("hr=0x{:08X} (expected CLASS_E_NOAGGREGATION)", hr.0 as u32),
    );

    report.begin_case("create_instance_null_ppv");
    // Raw call so a null out-pointer can actually be passed.
    let hr = unsafe {
        type CreateInstanceFn = unsafe extern "system" fn(
            *mut c_void,
            *mut c_void,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT;
        let raw = Interface::as_raw(&factory);
        // IClassFactory vtable: QueryInterface, AddRef, Release, CreateInstance, LockServer.
        let vtbl = *(raw as *const *const c_void);
        let entry = *(vtbl as *const *const c_void).add(3);
        let f: CreateInstanceFn = std::mem::transmute(entry);
        f(raw, std::ptr::null_mut(), &IInitializeWithStream::IID, std::ptr::null_mut())
    };
    report.check(
        "create_instance_null_ppv",
        hr == E_POINTER,
        format!("hr=0x{:08X} (expected E_POINTER)", hr.0 as u32),
    );

    report.begin_case("create_instance_unsupported_iid");
    let result: Result<IPersist> = unsafe { factory.CreateInstance(None::<&IUnknown>) };
    let hr = result.as_ref().err().map(|e| e.code()).unwrap_or(S_OK);
    report.check(
        "create_instance_unsupported_iid",
        hr == E_NOINTERFACE,
        format!("hr=0x{:08X} (expected E_NOINTERFACE)", hr.0 as u32),
    );

    report.begin_case("provider_supports_required_interfaces");
    match dll_handle.create_provider() {
        Ok(provider) => {
            let has_thumb = provider.cast::<IThumbnailProvider>().is_ok();
            let has_init = provider.cast::<IInitializeWithStream>().is_ok();
            let has_unknown = provider.cast::<IUnknown>().is_ok();
            let rejects_other = provider.cast::<IPersist>().is_err();
            report.check(
                "provider_supports_required_interfaces",
                has_thumb && has_init && has_unknown && rejects_other,
                format!(
                    "IThumbnailProvider={has_thumb} IInitializeWithStream={has_init} \
                     IUnknown={has_unknown} rejects-IPersist={rejects_other}"
                ),
            );

            report.begin_case("query_interface_is_reflexive_and_symmetric");
            // COM identity rules: QI must be reachable in both directions and
            // IUnknown must be identical whichever interface it is asked from.
            let thumb: IThumbnailProvider = provider.cast().expect("checked above");
            let back: Result<IInitializeWithStream> = thumb.cast();
            let unk_a: IUnknown = provider.cast().expect("checked above");
            let unk_b: Result<IUnknown> = thumb.cast();
            let same_identity = match (&unk_b, &unk_a) {
                (Ok(b), a) => Interface::as_raw(b) == Interface::as_raw(a),
                _ => false,
            };
            report.check(
                "query_interface_is_reflexive_and_symmetric",
                back.is_ok() && same_identity,
                format!(
                    "round-trip QI ok={} IUnknown identity stable={same_identity}",
                    back.is_ok()
                ),
            );
        }
        Err(e) => report.fail("provider_supports_required_interfaces", format!("CreateInstance failed: {e:?}")),
    }
}

// ---------------------------------------------------------------
//                   IInitializeWithStream
// ---------------------------------------------------------------

fn initialize_contract(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("initialize_twice_is_rejected");
    // Re-initialising an already-initialised handler must fail. If it silently
    // succeeded, the shell could swap the file contents underneath a pending
    // thumbnail request.
    match dll_handle.create_provider() {
        Ok(provider) => {
            let (Ok(stream), Ok(stream2)) = (
                dll::mem_stream(BASE_SVG.as_bytes()),
                dll::mem_stream(BASE_SVG.as_bytes()),
            ) else {
                report.skip("initialize_twice_is_rejected", "SHCreateMemStream failed");
                return;
            };
            let first = unsafe { provider.Initialize(&stream, 0) };
            let second = unsafe { provider.Initialize(&stream2, 0) };
            report.check(
                "initialize_twice_is_rejected",
                first.is_ok() && second.is_err(),
                format!(
                    "first={:?} second=0x{:08X} (expected the second call to fail)",
                    first.is_ok(),
                    second.as_ref().err().map(|e| e.code().0 as u32).unwrap_or(0)
                ),
            );
        }
        Err(e) => report.fail("initialize_twice_is_rejected", format!("CreateInstance failed: {e:?}")),
    }

    report.begin_case("initialize_null_stream");
    // Raw vtable call so a genuine null interface pointer reaches the provider.
    match dll_handle.create_provider() {
        Ok(provider) => {
            let hr = unsafe {
                type InitFn =
                    unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> HRESULT;
                let raw = Interface::as_raw(&provider);
                let vtbl = *(raw as *const *const c_void);
                let entry = *(vtbl as *const *const c_void).add(3);
                let f: InitFn = std::mem::transmute(entry);
                f(raw, std::ptr::null_mut(), 0)
            };
            report.check(
                "initialize_null_stream",
                hr.is_err(),
                format!("hr=0x{:08X} (expected a clean failure, not a fault)", hr.0 as u32),
            );
        }
        Err(e) => report.fail("initialize_null_stream", format!("CreateInstance failed: {e:?}")),
    }

    report.begin_case("initialize_empty_stream");
    // A zero-length file is a real thing the shell will hand over. Whether
    // Initialize accepts it is the provider's choice, but the outcome must not
    // be a rendered document: there is nothing to render.
    match dll_handle.create_provider() {
        Ok(provider) => {
            let Ok(stream) = dll::mem_stream(&[]) else {
                report.skip("initialize_empty_stream", "SHCreateMemStream rejected an empty buffer");
                return;
            };
            let init = unsafe { provider.Initialize(&stream, 0) };
            let rendering = provider.cast::<IThumbnailProvider>().ok().and_then(|t| {
                let mut hbmp = HBITMAP(std::ptr::null_mut());
                let mut alpha = WTSAT_UNKNOWN;
                let hr = unsafe { t.GetThumbnail(64, &mut hbmp, &mut alpha) };
                let out = if hbmp.is_invalid() {
                    None
                } else {
                    dll::take_bitmap(hbmp, alpha).ok().map(|b| b.classify())
                };
                let _ = hr;
                out
            });
            report.check(
                "initialize_empty_stream",
                rendering != Some(crate::dll::Rendering::Real),
                format!(
                    "Initialize hr={:?}, resulting thumbnail={rendering:?} - an empty file must \
                     fail or fall back, never produce real artwork",
                    init.as_ref().err().map(|e| format!("0x{:08X}", e.code().0 as u32))
                ),
            );
        }
        Err(e) => report.fail("initialize_empty_stream", format!("CreateInstance failed: {e:?}")),
    }
}

// ---------------------------------------------------------------
//                     IThumbnailProvider
// ---------------------------------------------------------------

fn thumbnail_contract(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("get_thumbnail_before_initialize");
    // Calling GetThumbnail on an uninitialised object must fail rather than
    // render whatever happens to be in the object's state.
    match dll_handle.create_provider() {
        Ok(provider) => match provider.cast::<IThumbnailProvider>() {
            Ok(thumb) => {
                let mut hbmp = HBITMAP(std::ptr::null_mut());
                let mut alpha = WTSAT_UNKNOWN;
                let hr = unsafe { thumb.GetThumbnail(64, &mut hbmp, &mut alpha) };
                let produced_bitmap = !hbmp.is_invalid();
                if produced_bitmap {
                    let _ = dll::take_bitmap(hbmp, alpha);
                }
                report.check(
                    "get_thumbnail_before_initialize",
                    hr.is_err() && !produced_bitmap && alpha == WTSAT_UNKNOWN,
                    format!(
                        "hr={:?} produced_bitmap={produced_bitmap} alpha_left_unknown={}",
                        hr.as_ref().err().map(|e| format!("0x{:08X}", e.code().0 as u32)),
                        alpha == WTSAT_UNKNOWN
                    ),
                );
            }
            Err(e) => report.fail("get_thumbnail_before_initialize", format!("QI failed: {e:?}")),
        },
        Err(e) => report.fail("get_thumbnail_before_initialize", format!("CreateInstance failed: {e:?}")),
    }

    // ---- The null-output-pointer probe. ----
    //
    // This calls the raw vtable entry with null out-parameters. It is listed in
    // report::KNOWN_ISSUES because the current implementation writes through
    // both pointers before validating them, so this check is expected to be
    // recorded as a known issue rather than to break the build.
    //
    // It runs in a dedicated child process (see main.rs `probe-null-out`), so an
    // access violation is observed as a child exit code instead of destroying
    // the rest of the gauntlet run.
    report.begin_case("get_thumbnail_null_out_pointers");
    match crate::spawn_probe("probe-null-out") {
        Ok(ProbeOutcome { crashed, exit_code, hresult }) => {
            report.check(
                "get_thumbnail_null_out_pointers",
                !crashed && hresult == Some(E_POINTER),
                if crashed {
                    format!(
                        "child process FAULTED (exit code 0x{:08X}) calling GetThumbnail with null \
                         phbmp/pdwalpha - the raw vtable entry dereferences both output pointers \
                         before checking them",
                        exit_code as u32
                    )
                } else {
                    format!(
                        "child returned hr={:?} (expected E_POINTER)",
                        hresult.map(|h| format!("0x{:08X}", h.0 as u32))
                    )
                },
            );
        }
        Err(e) => report.skip("get_thumbnail_null_out_pointers", format!("could not spawn probe child: {e}")),
    }

    report.begin_case("get_thumbnail_partial_null_out_pointer");
    match crate::spawn_probe("probe-null-alpha") {
        Ok(ProbeOutcome { crashed, exit_code, hresult }) => {
            report.check(
                "get_thumbnail_partial_null_out_pointer",
                !crashed,
                if crashed {
                    format!(
                        "child process FAULTED (exit 0x{:08X}) with a valid phbmp but null pdwalpha",
                        exit_code as u32
                    )
                } else {
                    format!("child returned hr={:?} without faulting", hresult.map(|h| format!("0x{:08X}", h.0 as u32)))
                },
            );
        }
        Err(e) => report.skip("get_thumbnail_partial_null_out_pointer", format!("could not spawn probe child: {e}")),
    }

    report.begin_case("failed_render_still_sets_output_parameters");
    // On any failure path the provider must leave *phbmp null and *pdwalpha at
    // WTSAT_UNKNOWN, so a caller that ignores the HRESULT cannot use a stale
    // handle. Pre-poison both so "left untouched" is distinguishable from
    // "correctly initialised".
    match dll_handle.create_provider() {
        Ok(provider) => {
            let Ok(stream) = dll::mem_stream(b"not an svg") else {
                report.skip("failed_render_still_sets_output_parameters", "SHCreateMemStream failed");
                return;
            };
            let _ = unsafe { provider.Initialize(&stream, 0) };
            match provider.cast::<IThumbnailProvider>() {
                Ok(thumb) => {
                    let poison = HBITMAP(0xDEAD_BEEF_usize as *mut c_void);
                    let mut hbmp = poison;
                    let mut alpha = WTS_ALPHATYPE(0x7FFF_FFFF);
                    let hr = unsafe { thumb.GetThumbnail(0, &mut hbmp, &mut alpha) };
                    let overwritten = hbmp.0 != poison.0 || alpha != WTS_ALPHATYPE(0x7FFF_FFFF);
                    let safe_on_failure = hr.is_err() && hbmp.is_invalid() && alpha == WTSAT_UNKNOWN;
                    if hr.is_ok() && !hbmp.is_invalid() {
                        let _ = dll::take_bitmap(hbmp, alpha);
                    }
                    report.check(
                        "failed_render_still_sets_output_parameters",
                        overwritten && (hr.is_ok() || safe_on_failure),
                        format!(
                            "hr={:?} outputs_overwritten={overwritten} null_on_failure={}",
                            hr.as_ref().err().map(|e| format!("0x{:08X}", e.code().0 as u32)),
                            hbmp.is_invalid()
                        ),
                    );
                }
                Err(e) => report.fail("failed_render_still_sets_output_parameters", format!("QI failed: {e:?}")),
            }
        }
        Err(e) => report.fail("failed_render_still_sets_output_parameters", format!("CreateInstance failed: {e:?}")),
    }
}

pub struct ProbeOutcome {
    pub crashed: bool,
    pub exit_code: i32,
    pub hresult: Option<HRESULT>,
}

// ---------------------------------------------------------------
//                      Server lifetime
// ---------------------------------------------------------------

fn lock_server_contract(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("lock_server_blocks_unload");
    // LockServer(TRUE) must keep DllCanUnloadNow saying S_FALSE until it is
    // balanced; this is the only thing preventing COM from unloading a DLL that
    // a client still intends to use.
    let factory = match dll_handle.class_factory() {
        Ok(f) => f,
        Err(e) => {
            report.fail("lock_server_blocks_unload", format!("no class factory: {e:?}"));
            return;
        }
    };
    let baseline_blocked = dll_handle.unload_blocked(); // the factory itself holds a ref
    let locked = unsafe { factory.LockServer(true) };
    let while_locked = dll_handle.can_unload();
    let unlocked = unsafe { factory.LockServer(false) };
    drop(factory);
    let after_release = dll_handle.can_unload();

    report.check(
        "lock_server_blocks_unload",
        locked.is_ok() && unlocked.is_ok() && while_locked == S_FALSE && baseline_blocked,
        format!(
            "lock={:?} unlock={:?} while_locked=0x{:08X} after_all_released=0x{:08X}",
            locked.is_ok(),
            unlocked.is_ok(),
            while_locked.0 as u32,
            after_release.0 as u32
        ),
    );

    report.begin_case("unload_allowed_once_everything_released");
    // With no factories, no providers and no locks outstanding, the DLL must
    // finally agree that it can be unloaded. If it never does, COM keeps it
    // mapped forever and the surrogate's idle cleanup silently stops working.
    report.check(
        "unload_allowed_once_everything_released",
        dll_handle.unload_allowed(),
        format!(
            "DllCanUnloadNow=0x{:08X} (expected S_OK with nothing outstanding)",
            dll_handle.can_unload().0 as u32
        ),
    );

    report.begin_case("unbalanced_unlock_does_not_report_unloadable");
    // An over-released server lock must never reach the live-object counter.
    // Reporting S_OK while objects are alive would be catastrophic, because COM
    // would unmap the DLL under a live provider.
    let (Ok(factory), Ok(provider)) = (dll_handle.class_factory(), dll_handle.create_provider())
    else {
        report.skip("unbalanced_unlock_does_not_report_unloadable", "could not create COM objects");
        return;
    };
    let _ = unsafe { factory.LockServer(false) }; // unbalanced release
    let hr = dll_handle.can_unload();
    report.check(
        "unbalanced_unlock_does_not_report_unloadable",
        hr != S_OK,
        format!(
            "DllCanUnloadNow=0x{:08X} after an unbalanced LockServer(FALSE) while a provider and \
             factory are still alive (S_OK here would let COM unload a DLL still in use)",
            hr.0 as u32
        ),
    );
    // Nothing to re-balance: the unmatched unlock is dropped rather than banked, so
    // the ledger is already where it started. A LockServer(TRUE) here would not undo
    // anything - it would strand a lock nothing ever releases, and every later
    // "unload allowed once everything is released" check would fail on it.
    drop(provider);
    drop(factory);
}

/// Demonstrates what the unbalanced unlock actually costs, once the extra
/// objects that were masking it are gone.
///
/// The check above shows the *immediate* state is still safe, because enough
/// live objects remain to keep the count above zero. This one carries the
/// corruption forward: unlock without locking, then release everything except a
/// single live provider, and ask whether the DLL still believes it is in use.
///
/// It runs last, and rebuilds the counter afterwards, because it deliberately
/// leaves the DLL's global reference count inconsistent with reality.
fn unbalanced_unlock_reference_count(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("unbalanced_unlock_corrupts_reference_count");

    let (Ok(factory), Ok(survivor)) = (dll_handle.class_factory(), dll_handle.create_provider())
    else {
        report.skip("unbalanced_unlock_corrupts_reference_count", "could not create COM objects");
        return;
    };

    // One unmatched unlock: legal to attempt, illegal per the COM contract, and
    // Explorer never does it. The question is only what it costs if anything does.
    let _ = unsafe { factory.LockServer(false) };
    drop(factory);

    // `survivor` is still alive and still owns a DLL reference, so a correct
    // count can only be >= 1 and DllCanUnloadNow must say S_FALSE.
    let hr = dll_handle.can_unload();
    let safe = hr != S_OK;
    report.check(
        "unbalanced_unlock_corrupts_reference_count",
        safe,
        format!(
            "DllCanUnloadNow=0x{:08X} with one live provider outstanding after a single unmatched \
             LockServer(FALSE). S_OK here means the DLL's reference count no longer reflects its \
             live objects, and COM would be entitled to unmap the DLL while that provider is \
             still in use.",
            hr.0 as u32
        ),
    );

    // Nothing to restore: the unmatched unlock leaves neither the object count nor
    // the lock ledger inconsistent, so a compensating LockServer(TRUE) would only
    // strand a lock for server_lock_ledger to clean up.
    drop(survivor);
}

fn release_ordering(dll_handle: &Dll, report: &mut Report) {
    report.begin_case("provider_outlives_its_factory");
    // COM object lifetimes are independent: a provider created from a factory
    // must keep working after that factory is released.
    //
    // Scoped so the provider is definitely released before the next check reads
    // DllCanUnloadNow - otherwise this object would still be holding a DLL
    // reference and the next check would misreport a leak.
    {
        let factory = match dll_handle.class_factory() {
            Ok(f) => f,
            Err(e) => {
                report.fail("provider_outlives_its_factory", format!("no class factory: {e:?}"));
                return;
            }
        };
        let provider: Result<IInitializeWithStream> =
            unsafe { factory.CreateInstance(None::<&IUnknown>) };
        let provider = match provider {
            Ok(p) => p,
            Err(e) => {
                report.fail("provider_outlives_its_factory", format!("CreateInstance failed: {e:?}"));
                return;
            }
        };
        drop(factory); // release the factory first, deliberately out of order

        let Ok(stream) = dll::mem_stream(BASE_SVG.as_bytes()) else {
            report.skip("provider_outlives_its_factory", "SHCreateMemStream failed");
            return;
        };
        let init_ok = unsafe { provider.Initialize(&stream, 0) }.is_ok();
        let rendered = provider.cast::<IThumbnailProvider>().ok().map(|t| {
            let mut hbmp = HBITMAP(std::ptr::null_mut());
            let mut alpha = WTSAT_UNKNOWN;
            let hr = unsafe { t.GetThumbnail(64, &mut hbmp, &mut alpha) };
            let ok = hr.is_ok() && !hbmp.is_invalid();
            if !hbmp.is_invalid() {
                let _ = dll::take_bitmap(hbmp, alpha);
            }
            ok
        });
        report.check(
            "provider_outlives_its_factory",
            init_ok && rendered == Some(true),
            format!("initialize_ok={init_ok} render_ok={rendered:?} after the factory was released"),
        );
    }

    report.begin_case("many_live_providers_from_one_factory");
    // One factory, many simultaneously live providers, released in reverse
    // order. Each holds its own DLL reference, so this also exercises the
    // reference counter across a range rather than just 0 and 1.
    let Ok(factory) = dll_handle.class_factory() else {
        report.skip("many_live_providers_from_one_factory", "could not obtain a class factory");
        return;
    };
    let mut providers: Vec<IInitializeWithStream> = Vec::new();
    for _ in 0..64 {
        match unsafe { factory.CreateInstance(None::<&IUnknown>) } {
            Ok(p) => providers.push(p),
            Err(e) => {
                report.fail("many_live_providers_from_one_factory", format!("CreateInstance failed: {e:?}"));
                return;
            }
        }
    }
    let blocked_with_many = dll_handle.unload_blocked();
    while providers.pop().is_some() {}
    drop(factory);
    report.check(
        "many_live_providers_from_one_factory",
        blocked_with_many && dll_handle.unload_allowed(),
        format!(
            "unload blocked with 64 providers alive={blocked_with_many}, allowed after all released={}",
            dll_handle.unload_allowed()
        ),
    );
}

// ---------------------------------------------------------------
//                      Thumbnail sizes
// ---------------------------------------------------------------

fn size_boundaries(dll_handle: &Dll, report: &mut Report) {
    // The renderer rejects anything outside 1..=4096. Probe both sides of every
    // boundary plus the extremes, where a width * height * 4 computation could
    // overflow. Sizes that are rejected must fall back or fail cleanly; sizes
    // that are accepted must produce a bitmap of exactly the requested size.
    for size in [0u32, 1, 2, 15, 16, 4095, 4096, 4097, 8192, 65535, u32::MAX / 2, u32::MAX] {
        let name = format!("size_{size}");
        report.begin_case(&name);
        let result = dll::try_render(dll_handle, BASE_SVG.as_bytes(), size);
        let detail = match &result {
            Ok(t) => format!(
                "returned a {}x{} bitmap ({:?})",
                t.width,
                t.height,
                t.classify()
            ),
            Err(hr) => format!("failed with 0x{:08X}", hr.0 as u32),
        };

        // Two properties, both safety-relevant and neither of them a guess about
        // policy:
        //   * an in-range size must succeed - the shell asks for these,
        //   * ANY returned bitmap must have exactly the requested geometry,
        //     because the caller sizes its own buffers from what it asked for.
        // Out-of-range sizes are free to fail or to fall back; what they must
        // not do is hand back a bitmap of a different size than requested.
        let ok = match (&result, size) {
            (Ok(t), s) => t.width == s && t.height == s,
            (Err(_), s) => !(1..=4096).contains(&s),
        };
        report.check(name, ok, detail);
    }
}

// ---------------------------------------------------------------
//                     Server lock ledger
// ---------------------------------------------------------------

/// Properties of the server-lock counter that became load-bearing when locks
/// stopped sharing the live-object counter.
///
/// While `LockServer(TRUE)` incremented the same counter as live objects, "a
/// lock keeps the DLL loaded" was covered for free by the object count. Now that
/// the two are separate, it is a distinct property with nothing else testing it,
/// and the interaction between an over-release and a later legitimate lock is a
/// new behaviour that did not exist before.
fn server_lock_ledger(dll_handle: &Dll, report: &mut Report) {
    /// Drives the lock ledger back to "no locks held" so a later check starts
    /// from a known state. Bounded, because the whole point of these checks is
    /// that the counter may not be where we think it is.
    fn rebalance(dll_handle: &Dll) {
        for _ in 0..8 {
            if dll_handle.unload_allowed() {
                return;
            }
            match dll_handle.class_factory() {
                Ok(f) => {
                    let _ = unsafe { f.LockServer(false) };
                }
                Err(_) => return,
            }
        }
    }

    // ---- A lock on its own must block unload. ----
    report.begin_case("server_lock_alone_blocks_unload");
    match dll_handle.class_factory() {
        Ok(factory) => {
            let locked = unsafe { factory.LockServer(true) }.is_ok();
            // Release the factory, so the only thing left holding the DLL is the
            // lock itself. Before locks had their own counter this was
            // indistinguishable from the factory's own reference.
            drop(factory);
            let hr = dll_handle.can_unload();
            report.check(
                "server_lock_alone_blocks_unload",
                locked && hr != S_OK,
                format!(
                    "LockServer(TRUE)={locked}, then every object released: \
                     DllCanUnloadNow=0x{:08X}. A server lock must keep the DLL loaded on its \
                     own - COM may unmap it the moment this reports S_OK.",
                    hr.0 as u32
                ),
            );
            // Balance our own lock before moving on.
            if let Ok(f) = dll_handle.class_factory() {
                let _ = unsafe { f.LockServer(false) };
            }
        }
        Err(e) => report.fail("server_lock_alone_blocks_unload", format!("no class factory: {e:?}")),
    }
    rebalance(dll_handle);

    // ---- One client's over-release must not cancel another's lock. ----
    report.begin_case("unmatched_unlock_does_not_cancel_a_later_lock");
    // LockServer is process-global: every client shares one ledger. So the
    // question is not just "can a client corrupt its own accounting" but "can a
    // buggy client silently revoke a lock that a *different*, correct client is
    // relying on".
    //
    // Client A over-releases.
    if let Ok(a) = dll_handle.class_factory() {
        let _ = unsafe { a.LockServer(false) };
        drop(a);
    }
    // Client B then takes a lock it is entitled to, and releases its factory.
    let b_locked = match dll_handle.class_factory() {
        Ok(b) => {
            let ok = unsafe { b.LockServer(true) }.is_ok();
            drop(b);
            ok
        }
        Err(_) => false,
    };
    let hr = dll_handle.can_unload();
    report.check(
        "unmatched_unlock_does_not_cancel_a_later_lock",
        b_locked && hr != S_OK,
        format!(
            "after an unmatched LockServer(FALSE) from one client, a second client's \
             LockServer(TRUE)={b_locked} left DllCanUnloadNow=0x{:08X}. The lock ledger is \
             process-global, so a signed counter that goes negative on an over-release lets one \
             buggy client silently consume a lock that a correct client is holding: the -1 and \
             the +1 cancel, the ledger reads 0, and COM may unload the DLL while client B still \
             believes it is locked. Saturating the decrement at zero avoids this - its worst \
             case is a DLL that stays mapped, which for an idle-exiting surrogate is harmless.",
            hr.0 as u32
        ),
    );
    rebalance(dll_handle);
}

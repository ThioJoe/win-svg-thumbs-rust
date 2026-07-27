//! Process resource sampling for the retention suites.
//!
//! v1.11.0 deliberately never destroys a thread's cached Direct2D/D3D-WARP
//! resources: TLS destructors would run under the loader lock, which is what
//! caused the original dllhost.exe crash, so the cache is leaked on purpose when
//! a rendering thread exits. The comment in src/lib.rs justifies that with "in
//! practice these threads live until process exit, so nothing meaningful
//! accumulates".
//!
//! That is an assumption about the *host*, not a property of the code, and this
//! module exists to measure it rather than trust it. The churn suite samples
//! these counters across a controlled number of completed rendering threads and
//! reports the retention slope in bytes per thread.

use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModules, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount, GetGuiResources, GR_GDIOBJECTS,
    GR_USEROBJECTS,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    /// Private committed bytes - the number that actually matters for a leak,
    /// since working set can be trimmed by the OS at any time.
    pub private_bytes: u64,
    pub working_set: u64,
    pub handles: u32,
    pub gdi_objects: u32,
    pub user_objects: u32,
    pub threads: u32,
    pub modules: u32,
}

impl Snapshot {
    pub fn take() -> Self {
        unsafe {
            let process = GetCurrentProcess();

            let mut counters = PROCESS_MEMORY_COUNTERS_EX {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                ..Default::default()
            };
            // GetProcessMemoryInfo is declared against the base struct; passing
            // the EX layout with its own cb is the documented way to get
            // PrivateUsage back.
            let mem_ok = GetProcessMemoryInfo(
                process,
                &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
                counters.cb,
            )
            .is_ok();

            let mut handles: u32 = 0;
            let _ = GetProcessHandleCount(process, &mut handles);

            Self {
                private_bytes: if mem_ok { counters.PrivateUsage as u64 } else { 0 },
                working_set: if mem_ok { counters.WorkingSetSize as u64 } else { 0 },
                handles,
                gdi_objects: GetGuiResources(process, GR_GDIOBJECTS),
                user_objects: GetGuiResources(process, GR_USEROBJECTS),
                threads: count_threads(),
                modules: count_modules(),
            }
        }
    }

    /// Signed delta against an earlier snapshot.
    pub fn delta(&self, base: &Snapshot) -> Delta {
        Delta {
            private_bytes: self.private_bytes as i64 - base.private_bytes as i64,
            working_set: self.working_set as i64 - base.working_set as i64,
            handles: self.handles as i64 - base.handles as i64,
            gdi_objects: self.gdi_objects as i64 - base.gdi_objects as i64,
            user_objects: self.user_objects as i64 - base.user_objects as i64,
            threads: self.threads as i64 - base.threads as i64,
            modules: self.modules as i64 - base.modules as i64,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "private={:.1}MiB ws={:.1}MiB handles={} gdi={} user={} threads={} modules={}",
            self.private_bytes as f64 / 1048576.0,
            self.working_set as f64 / 1048576.0,
            self.handles,
            self.gdi_objects,
            self.user_objects,
            self.threads,
            self.modules
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Delta {
    pub private_bytes: i64,
    pub working_set: i64,
    pub handles: i64,
    pub gdi_objects: i64,
    pub user_objects: i64,
    pub threads: i64,
    pub modules: i64,
}

impl Delta {
    pub fn describe(&self) -> String {
        format!(
            "private={:+.2}MiB ws={:+.2}MiB handles={:+} gdi={:+} user={:+} threads={:+} modules={:+}",
            self.private_bytes as f64 / 1048576.0,
            self.working_set as f64 / 1048576.0,
            self.handles,
            self.gdi_objects,
            self.user_objects,
            self.threads,
            self.modules
        )
    }
}

/// Counts threads belonging to this process via a ToolHelp snapshot.
///
/// Returns 0 if the snapshot cannot be taken; callers treat 0 as "unknown"
/// rather than as a meaningful measurement.
fn count_threads() -> u32 {
    unsafe {
        let pid = GetCurrentProcessId();
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            Ok(h) if !h.is_invalid() => h,
            _ => return 0,
        };
        let mut count = 0u32;
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    count += 1;
                }
                entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
        count
    }
}

/// Counts modules loaded in this process.
///
/// A rising module count across churn would mean the DLL (or its D2D/D3D
/// dependencies) is being mapped repeatedly without being released.
fn count_modules() -> u32 {
    unsafe {
        let mut modules = vec![windows::Win32::Foundation::HMODULE::default(); 1024];
        let mut needed: u32 = 0;
        let byte_len = (modules.len() * std::mem::size_of::<windows::Win32::Foundation::HMODULE>())
            as u32;
        if EnumProcessModules(GetCurrentProcess(), modules.as_mut_ptr(), byte_len, &mut needed)
            .is_err()
        {
            return 0;
        }
        needed / std::mem::size_of::<windows::Win32::Foundation::HMODULE>() as u32
    }
}

/// Caps this process's committed memory using a job object.
///
/// Applied by the child process to itself at startup, before any test work, so
/// there is no window in which a decompression bomb or entity-expansion bomb
/// could balloon before the limit takes effect. When the limit is hit the
/// allocation fails inside the child, which surfaces as a clean suite failure
/// instead of the CI runner being driven into swap.
pub fn limit_own_memory(mb: u64) -> windows::core::Result<()> {
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    unsafe {
        let job = CreateJobObjectW(None, None)?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = (mb * 1024 * 1024) as usize;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        AssignProcessToJobObject(job, GetCurrentProcess())?;
        // The job handle is intentionally never closed: closing it while this
        // process is the only member would terminate the job (and us) on some
        // Windows versions. It is reclaimed when the process exits.
        let _ = job;
        Ok(())
    }
}


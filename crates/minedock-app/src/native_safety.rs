//! Native process ownership helpers. These are deliberately app-side.

use minedock_core::{MineDockError, Result};
use std::path::Path;
use std::process::{Child, Command};

/// Fail closed before a large download/provision/backup when the native
/// filesystem can report free space. Non-Windows test hosts do not have the
/// same Win32 volume semantics, so the portable adapter leaves the check to
/// the filesystem operation itself.
pub fn ensure_free_space(path: &Path, required_bytes: u64) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available = 0_u64;
        let result = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(MineDockError::Persistence(format!(
                "could not determine free disk space for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        if available < required_bytes {
            return Err(MineDockError::Persistence(format!(
                "not enough free disk space: need {required_bytes} bytes, have {available}"
            )));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (path, required_bytes);
    }
    Ok(())
}

#[cfg(not(windows))]
#[derive(Debug)]
pub struct KillOnDropJob;

#[cfg(not(windows))]
impl KillOnDropJob {
    pub fn attach(_child: &Child) -> Result<Self> {
        Ok(Self)
    }

    pub fn terminate(&self, child: &mut Child) -> Result<()> {
        child
            .kill()
            .map_err(|error| MineDockError::ProcessControl(error.to_string()))
    }

    /// Non-Windows process creation is already resumed by `Command::spawn`.
    /// Keeping this seam symmetric lets the Windows adapter make the
    /// suspended-create/assign/resume sequence explicit without putting OS
    /// mechanics in core.
    pub fn configure_suspended(_command: &mut Command) {}

    pub fn resume(&self, _child: &Child) -> Result<()> {
        Ok(())
    }

    pub fn disabled_for_test() -> Self {
        Self
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct KillOnDropJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
// Windows process/job handles are kernel-owned, transferable values. The
// adapter keeps the handle behind its process-lifetime owner and never shares
// mutable access across threads.
unsafe impl Send for KillOnDropJob {}

#[cfg(windows)]
impl KillOnDropJob {
    pub fn configure_suspended(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        command.creation_flags(CREATE_SUSPENDED);
    }

    pub fn attach(child: &Child) -> Result<Self> {
        use std::ptr::null;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(MineDockError::ProcessStart(
                "could not create Windows process job object".into(),
            ));
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            unsafe { CloseHandle(handle) };
            return Err(MineDockError::ProcessStart(
                "could not configure Windows process job object".into(),
            ));
        }
        let process: HANDLE = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                child.id(),
            )
        };
        if process.is_null() {
            unsafe { CloseHandle(handle) };
            return Err(MineDockError::ProcessStart(
                "could not open spawned process for job assignment".into(),
            ));
        }
        let assigned = unsafe { AssignProcessToJobObject(handle, process) };
        unsafe { CloseHandle(process) };
        if assigned == 0 {
            unsafe { CloseHandle(handle) };
            return Err(MineDockError::ProcessStart(
                "could not assign spawned process to job object".into(),
            ));
        }
        Ok(Self { handle })
    }

    pub fn terminate(&self, child: &mut Child) -> Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        let result = unsafe { TerminateJobObject(self.handle, 1) };
        if result == 0 {
            child
                .kill()
                .map_err(|error| MineDockError::ProcessControl(error.to_string()))?;
        }
        Ok(())
    }

    /// Resume only after the suspended child has been assigned to the
    /// kill-on-close job. `std::process::Child` intentionally hides the
    /// primary thread handle, so the Windows Toolhelp snapshot is used while
    /// the process is still suspended. At creation time there is exactly one
    /// process thread and no child code can run before this call.
    pub fn resume(&self, child: &Child) -> Result<()> {
        use std::io;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot.is_null() || snapshot == (-1_isize as HANDLE) {
            return Err(MineDockError::ProcessStart(format!(
                "could not enumerate suspended process threads: {}",
                io::Error::last_os_error()
            )));
        }
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut found = false;
        let mut first_error = None;
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while has_entry {
            if entry.th32OwnerProcessID == child.id() {
                found = true;
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    first_error = Some(io::Error::last_os_error());
                    break;
                }
                let resumed = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if resumed == u32::MAX {
                    first_error = Some(io::Error::last_os_error());
                }
                break;
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
        if let Some(error) = first_error {
            return Err(MineDockError::ProcessStart(format!(
                "could not resume suspended process: {error}"
            )));
        }
        if !found {
            return Err(MineDockError::ProcessStart(
                "suspended process primary thread was not found".into(),
            ));
        }
        Ok(())
    }

    pub fn disabled_for_test() -> Self {
        Self {
            handle: std::ptr::null_mut(),
        }
    }
}

#[cfg(windows)]
impl Drop for KillOnDropJob {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
        }
    }
}

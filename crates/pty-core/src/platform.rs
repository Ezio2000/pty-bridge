use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{Child, MasterPty};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessLocator {
    Unix {
        process_id: i32,
        process_group: Option<i32>,
    },
    WindowsJob {
        name: String,
    },
}

pub(crate) struct Process {
    locator: ProcessLocator,
    #[cfg(windows)]
    handle: isize,
}

impl Process {
    #[cfg(unix)]
    pub fn new(child: &dyn Child, master: &dyn MasterPty) -> Result<Self> {
        Ok(Self {
            locator: ProcessLocator::Unix {
                process_id: child
                    .process_id()
                    .and_then(|id| i32::try_from(id).ok())
                    .ok_or_else(|| anyhow!("PTY child has no process id"))?,
                process_group: master.process_group_leader(),
            },
        })
    }

    #[cfg(windows)]
    pub fn new(child: &dyn Child, _master: &dyn MasterPty) -> Result<Self> {
        use std::{ffi::c_void, mem::size_of, ptr::null};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };
        let name = format!("Local\\pty-core-{}", uuid::Uuid::new_v4());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateJobObjectW(null(), wide.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let setup = (|| -> Result<()> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let process = child
                .as_raw_handle()
                .ok_or_else(|| anyhow!("PTY child has no process handle"))?
                as HANDLE;
            if unsafe { AssignProcessToJobObject(handle, process) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        if let Err(error) = setup {
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self {
            locator: ProcessLocator::WindowsJob { name },
            handle: handle as isize,
        })
    }

    pub fn locator(&self) -> ProcessLocator {
        self.locator.clone()
    }

    #[cfg(unix)]
    pub fn signal(&self, force: bool) -> Result<()> {
        signal_unix(&self.locator, force)
    }

    #[cfg(windows)]
    pub fn signal(&self, _force: bool) -> Result<()> {
        use windows_sys::Win32::{Foundation::HANDLE, System::JobObjects::TerminateJobObject};
        if unsafe { TerminateJobObject(self.handle as HANDLE, 1) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Process {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe { CloseHandle(self.handle as HANDLE) };
    }
}

#[cfg(unix)]
fn signal_unix(locator: &ProcessLocator, force: bool) -> Result<()> {
    let ProcessLocator::Unix {
        process_id,
        process_group,
    } = locator
    else {
        bail!("wrong platform locator");
    };
    let target = match process_group {
        Some(group) if *group > 0 => -*group,
        _ if *process_id > 0 => *process_id,
        _ => bail!("invalid process locator"),
    };
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    if unsafe { libc::kill(target, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).context("signal PTY process tree")
    }
}

/// Used by an external owner after the process hosting the core has disappeared.
#[cfg(unix)]
pub fn terminate_tree(locator: &ProcessLocator) -> Result<()> {
    signal_unix(locator, true)
}

#[cfg(windows)]
pub fn terminate_tree(locator: &ProcessLocator) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND},
        System::{
            JobObjects::{OpenJobObjectW, TerminateJobObject},
            SystemServices::JOB_OBJECT_TERMINATE,
        },
    };
    let ProcessLocator::WindowsJob { name } = locator else {
        bail!("wrong platform locator");
    };
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE, 0, wide.as_ptr()) };
    if job.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(error).context("open PTY job")
        };
    }
    let result = unsafe { TerminateJobObject(job, 1) };
    let error = std::io::Error::last_os_error();
    unsafe { CloseHandle(job) };
    if result == 0 {
        Err(error).context("terminate PTY job")
    } else {
        Ok(())
    }
}

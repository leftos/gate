//! What stopping a process tree needs: another gate's job opened by its name, and a process opened to be stopped.

use std::io;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::OpenJobObjectW;
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

use crate::{job, own_handle, start_of, wait_millis, wide};

/// A job another process created, opened by its name with the access to list its processes, and to end them when it
/// was opened to be stopped. Dropping it only closes this handle.
pub struct OpenedJob {
    handle: OwnedHandle,
    name: String,
}

impl OpenedJob {
    /// Opens the job `name` (such as `Local\gate-job-<pid>`) with `JOB_OBJECT_QUERY` and `JOB_OBJECT_TERMINATE`
    /// access, to list and end its processes; `None` when no job has that name.
    ///
    /// # Errors
    ///
    /// The system's error when the job exists but cannot be opened.
    pub fn open(name: &str) -> io::Result<Option<OpenedJob>> {
        Self::open_with(name, JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE)
    }

    /// Opens the job `name` with `JOB_OBJECT_QUERY` access alone, to list its processes: [`OpenedJob::terminate`]
    /// fails on it. `None` when no job has that name.
    ///
    /// # Errors
    ///
    /// The system's error when the job exists but cannot be opened.
    pub fn open_to_list(name: &str) -> io::Result<Option<OpenedJob>> {
        Self::open_with(name, JOB_OBJECT_QUERY)
    }

    /// Opens the job `name` with `access`; `None` when no job has that name.
    fn open_with(name: &str, access: u32) -> io::Result<Option<OpenedJob>> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call. A success is a fresh handle nothing else owns.
        match unsafe { own_handle(OpenJobObjectW(access, 0, wide_name.as_ptr())) } {
            Ok(handle) => Ok(Some(OpenedJob {
                handle,
                name: name.to_owned(),
            })),
            // `OpenJobObjectW` reports a name no job has as `ERROR_FILE_NOT_FOUND`.
            Err(error) if is_error(&error, ERROR_FILE_NOT_FOUND) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The name it was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The ids of every process in the job now, those in the jobs nested inside it included.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        job::process_ids(&self.handle)
    }

    /// Ends every process in the job, and in the jobs nested inside it, with `exit_code`.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        job::terminate(&self.handle, exit_code)
    }
}

/// A process opened to be stopped: with the access to read its creation time, to wait for it and to terminate it.
pub struct StoppableProcess {
    handle: OwnedHandle,
}

impl StoppableProcess {
    /// Opens the process `pid` with `PROCESS_TERMINATE`, `PROCESS_QUERY_LIMITED_INFORMATION` and `SYNCHRONIZE`
    /// access; `None` when no process has that pid. A process that has exited but whose handle another process still
    /// holds opens, and [`StoppableProcess::wait`] finds it ended.
    ///
    /// # Errors
    ///
    /// The system's error when the process exists but cannot be opened, such as one this process may not terminate.
    pub fn open(pid: u32) -> io::Result<Option<StoppableProcess>> {
        let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        match unsafe { own_handle(OpenProcess(access, 0, pid)) } {
            Ok(handle) => Ok(Some(StoppableProcess { handle })),
            // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
            Err(error) if is_error(&error, ERROR_INVALID_PARAMETER) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Its creation time, as the process table gives start times (whole microseconds), so the two compare equal for
    /// one process.
    ///
    /// # Errors
    ///
    /// The system's error when its times cannot be read, or when the time is outside a [`Timestamp`]'s range.
    pub fn created(&self) -> io::Result<Timestamp> {
        // SAFETY: the handle is open for as long as `self` lives, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
        unsafe { start_of(self.handle.raw()) }
    }

    /// Waits at most `timeout` for the process to end: true when it has ended, false when it still runs.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        // SAFETY: the handle is open for the whole call, with the `SYNCHRONIZE` access a wait asks for.
        match unsafe { WaitForSingleObject(self.handle.raw(), wait_millis(timeout)) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Ends the process with `exit_code`.
    ///
    /// # Errors
    ///
    /// The system's error when it cannot be terminated, as when it is ending already.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the handle is open for the whole call, with the `PROCESS_TERMINATE` access the call asks for.
        let terminated = unsafe { TerminateProcess(self.handle.raw(), exit_code) };
        if terminated == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}

/// Whether `error` is the Win32 error `code`.
pub(crate) fn is_error(error: &io::Error, code: u32) -> bool {
    error.raw_os_error() == i32::try_from(code).ok()
}

//! The gate's Win32 layer: named mutexes and the gate process's own start time. It holds no policy: the names, the
//! waits and what an abandoned mutex means to the gate are the caller's. Windows only; elsewhere the crate is empty.

#![cfg(windows)]

use std::io;
use std::time::Duration;

use jiff::Timestamp;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetProcessTimes, INFINITE, ReleaseMutex, WaitForSingleObject};

/// A handle to a named mutex, created unowned when no process has it open. A Windows mutex is owned by a thread, so
/// neither the handle nor its guard leaves the thread that opened it.
#[derive(Debug)]
pub struct NamedMutex {
    handle: HANDLE,
    name: String,
}

/// Why a mutex was not taken.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another thread held it for the whole wait.
    #[error("not free in time")]
    Timeout,
    /// The wait itself failed.
    #[error("the wait failed: {0}")]
    Failed(io::Error),
}

/// An owned mutex, released when dropped.
#[derive(Debug)]
pub struct MutexGuard<'a> {
    mutex: &'a NamedMutex,
    abandoned: bool,
}

impl NamedMutex {
    /// Opens the mutex `name` (such as `Local\gate-journal`), creating it unowned when it does not exist.
    ///
    /// # Errors
    ///
    /// The system's error when the mutex cannot be created or opened.
    pub fn open(name: &str) -> io::Result<NamedMutex> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; null security attributes are allowed.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(NamedMutex {
            handle,
            name: name.to_owned(),
        })
    }

    /// The name it was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Takes the mutex, waiting at most `timeout`. A mutex whose owner thread ended without releasing it comes back
    /// owned, marked [`MutexGuard::abandoned`].
    ///
    /// # Errors
    ///
    /// [`AcquireError::Timeout`] when it was not free within `timeout`, [`AcquireError::Failed`] when the wait failed.
    pub fn acquire(&self, timeout: Duration) -> Result<MutexGuard<'_>, AcquireError> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).min(INFINITE - 1);
        // SAFETY: the handle is open for as long as `self` lives.
        let result = unsafe { WaitForSingleObject(self.handle, millis) };
        let abandoned = match result {
            WAIT_OBJECT_0 => false,
            WAIT_ABANDONED => true,
            WAIT_TIMEOUT => return Err(AcquireError::Timeout),
            _ => return Err(AcquireError::Failed(io::Error::last_os_error())),
        };
        Ok(MutexGuard { mutex: self, abandoned })
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned by `self` alone. A close that fails leaves nothing to do.
        unsafe { CloseHandle(self.handle) };
    }
}

impl MutexGuard<'_> {
    /// True when the mutex was taken abandoned: its last owner's thread ended while it held it.
    #[must_use]
    pub fn abandoned(&self) -> bool {
        self.abandoned
    }
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex through the open handle. A release that fails leaves it to be abandoned
        // when the thread ends, which the next taker handles.
        unsafe { ReleaseMutex(self.mutex.handle) };
    }
}

/// This process's start time, as the process table gives start times (whole microseconds).
///
/// # Errors
///
/// The system's error when the process's times cannot be read, or when the time is outside a [`Timestamp`]'s range.
pub fn process_start() -> io::Result<Timestamp> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the pseudo-handle of the current process needs no closing, and every pointer is to a writable FILETIME.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    i64::try_from(ticks)
        .ok()
        .and_then(proc_table::filetime_to_timestamp)
        .ok_or_else(|| io::Error::other("the process start time is out of range"))
}

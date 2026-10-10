//! Named mutexes: one owner at a time across handles, an abandoned mutex taken as owned, and a probe that sees
//! whether any handle to a name is open.

use std::io;
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{CreateMutexW, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};

use crate::{own_handle, wait_millis, wide};

/// A handle to a named mutex, created unowned when no process has it open. A Windows mutex is owned by a thread, so
/// neither the handle nor its guard leaves the thread that opened it.
pub struct NamedMutex {
    handle: OwnedHandle,
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

/// An owned mutex, released when dropped. It borrows the handle it was taken through, which the caller keeps open.
pub struct MutexGuard<'a> {
    mutex: &'a NamedMutex,
    abandoned: bool,
}

/// A mutex owned by the guard itself, released when dropped. Its handle stays open for as long as the guard lives,
/// which is what a caller that holds a slot for a whole run needs.
pub struct OwnedMutexGuard {
    mutex: NamedMutex,
    abandoned: bool,
}

impl NamedMutex {
    /// Opens the mutex `name` (such as `Local\gate-journal`), creating it unowned when it does not exist.
    ///
    /// # Errors
    ///
    /// The system's error when the mutex cannot be created or opened.
    pub fn open(name: &str) -> io::Result<NamedMutex> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; null security attributes are allowed. A success
        // is a fresh handle nothing else owns.
        let handle = unsafe { own_handle(CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr())) }?;
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
        let abandoned = wait(self.handle.raw(), timeout)?;
        Ok(MutexGuard { mutex: self, abandoned })
    }

    /// Takes the mutex and its handle together, waiting at most `timeout`, so the guard owns what it holds. A mutex
    /// whose owner thread ended without releasing it comes back owned, marked [`OwnedMutexGuard::abandoned`]. A try
    /// that fails drops the handle at once: the create, try, drop of a gate that is only trying a slot.
    ///
    /// # Errors
    ///
    /// [`AcquireError::Timeout`] when it was not free within `timeout`, [`AcquireError::Failed`] when the wait failed.
    pub fn acquire_owned(self, timeout: Duration) -> Result<OwnedMutexGuard, AcquireError> {
        let abandoned = wait(self.handle.raw(), timeout)?;
        Ok(OwnedMutexGuard { mutex: self, abandoned })
    }
}

/// Waits on an open handle for at most `timeout`, mapping the result to whether the object came back abandoned.
fn wait(handle: HANDLE, timeout: Duration) -> Result<bool, AcquireError> {
    // SAFETY: the handle is open for the whole call.
    let result = unsafe { WaitForSingleObject(handle, wait_millis(timeout)) };
    match result {
        WAIT_OBJECT_0 => Ok(false),
        WAIT_ABANDONED => Ok(true),
        WAIT_TIMEOUT => Err(AcquireError::Timeout),
        _ => Err(AcquireError::Failed(io::Error::last_os_error())),
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
        unsafe { ReleaseMutex(self.mutex.handle.raw()) };
    }
}

impl OwnedMutexGuard {
    /// True when the mutex was taken abandoned: its last owner's thread ended while it held it.
    #[must_use]
    pub fn abandoned(&self) -> bool {
        self.abandoned
    }

    /// The name the mutex was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        self.mutex.name()
    }
}

impl Drop for OwnedMutexGuard {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex through the open handle. A release that fails leaves it to be abandoned
        // when the thread ends, which the next taker handles. The fields drop after this body in declaration order,
        // `mutex` first, so the release always runs while its `OwnedHandle` still has the handle open.
        unsafe { ReleaseMutex(self.mutex.handle.raw()) };
    }
}

/// True while a mutex of this name exists. A Windows mutex lives as long as any process holds a handle to it, and a
/// gate holds one only while it tries or holds the slot, so a `true` may be a gate's own instant of trying. A name
/// this process may not open counts as existing too.
///
/// # Errors
///
/// The system's error when the open fails for any reason other than the mutex's absence or an access it is refused.
pub fn mutex_exists(name: &str) -> io::Result<bool> {
    let wide_name = wide(name);
    // SAFETY: `wide_name` is NUL-terminated and outlives the call. Only `SYNCHRONIZE` is asked for and the handle is
    // never waited on, so the mutex is not taken. A success is a fresh handle nothing else owns, which the guard
    // closes at once.
    match unsafe { own_handle(OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide_name.as_ptr())) } {
        Ok(_open) => Ok(true),
        // An access this process is refused proves the name exists: an elevated gate made it with a DACL this process
        // cannot open. Only elevation produces it, so it has no test.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(true),
        // `OpenMutexW` reports a name no process holds as `ERROR_FILE_NOT_FOUND`.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

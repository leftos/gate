//! The gate's Win32 layer: named mutexes and events, and the start time of the gate process or of another. It holds no policy: the
//! names, the waits and what an abandoned mutex means to the gate are the caller's. Windows only; elsewhere the crate
//! is empty.

#![cfg(windows)]

mod event;
mod mutex;

pub use event::{EventWait, NamedEvent};
pub use mutex::{AcquireError, MutexGuard, NamedMutex, OwnedMutexGuard, mutex_exists};

use std::io;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The guard of a handle a Win32 call just returned, or the error the call left behind.
///
/// A null handle is a call that failed, and its error is read here before the guard is made.
///
/// # Safety
///
/// `handle` is open and this guard alone owns it from now on: nothing else closes it and no other guard is made
/// from it.
pub(crate) unsafe fn own_handle(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the caller guarantees the handle is open and unowned, and it is not null.
    unsafe { OwnedHandle::new(handle) }.ok_or_else(|| io::Error::other("the handle a Win32 call returned was refused"))
}

/// A wait of `timeout` in milliseconds, saturating and capped below `INFINITE`, so that no wait of this crate can be
/// asked for an endless one by accident.
pub(crate) fn wait_millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).min(INFINITE - 1)
}

/// The name as a NUL-terminated UTF-16 buffer, as a Win32 call that takes a name wants it.
pub(crate) fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(Some(0)).collect()
}

/// This process's start time, as the process table gives start times (whole microseconds).
///
/// # Errors
///
/// The system's error when the process's times cannot be read, or when the time is outside a [`Timestamp`]'s range.
pub fn process_start() -> io::Result<Timestamp> {
    // SAFETY: the pseudo-handle of the current process is always open and needs no closing.
    unsafe { start_of(GetCurrentProcess()) }
}

/// The start time of the running process `pid`, as [`process_start`] gives it; `None` when no process has that pid or
/// the process has exited.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::PermissionDenied`] when this process may not query that one, else the system's
/// error when the process cannot be opened or its times read, or when the time is outside a [`Timestamp`]'s range.
pub fn process_start_of(pid: u32) -> io::Result<Option<Timestamp>> {
    // SAFETY: a success is a fresh handle nothing else owns.
    let process = match unsafe { own_handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)) } {
        Ok(process) => process,
        // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
        Err(error) if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut code = 0;
    // SAFETY: the handle is open for the whole call, with the query access the call asks for, and `code` is writable.
    if unsafe { GetExitCodeProcess(process.raw(), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if code != STILL_ACTIVE.cast_unsigned() {
        return Ok(None);
    }
    // SAFETY: the handle is open for the whole call, with the query access `GetProcessTimes` asks for.
    unsafe { start_of(process.raw()) }.map(Some)
}

/// The creation time of the process behind `process`.
///
/// # Safety
///
/// `process` is open for the whole call, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
unsafe fn start_of(process: HANDLE) -> io::Result<Timestamp> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the caller guarantees the handle is open with query access, and every pointer is to a writable FILETIME.
    let ok = unsafe { GetProcessTimes(process, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    i64::try_from(ticks)
        .ok()
        .and_then(proc_table::filetime_to_timestamp)
        .ok_or_else(|| io::Error::other("the process start time is out of range"))
}

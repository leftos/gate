//! The gate's Win32 layer: named mutexes and events, job objects, commands created suspended, the start time of the
//! gate process or of another, and the Windows message of an error code. It holds no policy: the names, the waits, the
//! command's environment and priority, and what an abandoned mutex means to the gate are the caller's. Windows only;
//! elsewhere the crate is empty.

#![cfg(windows)]

mod event;
mod job;
mod launch;
mod mutex;

pub use event::{EventWait, NamedEvent};
pub use job::Job;
pub use launch::{EnvironmentBlock, Launch, LaunchError, PriorityClass, Running, Suspended, same_variable_name};
pub use mutex::{AcquireError, MutexGuard, NamedMutex, OwnedMutexGuard, mutex_exists};

use std::io;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, LocalFree, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_ALLOCATE_BUFFER, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetExitCodeProcess, GetPriorityClass, GetProcessTimes, IDLE_PRIORITY_CLASS, INFINITE,
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
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

/// The system's message for the Win32 error `code`, as .NET's `Win32Exception.Message` gives it: inserts such as `%1`
/// left as written, trailing whitespace and line breaks removed, the closing period kept, and `Unknown error (0x<code>)`
/// for a code the system has no message for. It carries no `(os error N)` suffix, so a caller can print it in a line
/// of its own.
#[must_use]
pub fn win32_message(code: u32) -> String {
    let mut buffer: *mut u16 = std::ptr::null_mut();
    let flags = FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS;
    // SAFETY: with `FORMAT_MESSAGE_ALLOCATE_BUFFER` the buffer argument is the address of a pointer the system sets to a
    // buffer it allocates; a null source and no arguments are what a system message with inserts ignored takes.
    let length = unsafe { FormatMessageW(flags, std::ptr::null(), code, 0, (&raw mut buffer).cast(), 0, std::ptr::null()) };
    if length == 0 || buffer.is_null() {
        return format!("Unknown error (0x{code:x})");
    }
    // SAFETY: the system wrote `length` UTF-16 units into the buffer it allocated, which stays alive until freed below.
    let text = unsafe { std::slice::from_raw_parts(buffer, length as usize) };
    let message = String::from_utf16_lossy(text);
    // SAFETY: the buffer was allocated by `FormatMessageW` with `LocalAlloc`, is no longer read, and is freed once.
    unsafe { LocalFree(buffer.cast()) };
    message.trim_end_matches(|c: char| c <= ' ').to_owned()
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

/// Whether this process runs in the below-normal or the idle priority class.
///
/// # Errors
///
/// The system's error when the class cannot be read.
pub fn runs_below_normal() -> io::Result<bool> {
    // SAFETY: the pseudo-handle of the current process is always open, with the query access the call asks for.
    let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
    if class == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(class == BELOW_NORMAL_PRIORITY_CLASS || class == IDLE_PRIORITY_CLASS)
}

/// The system folder (`System32`), as `GetSystemDirectoryW` gives it: where cmd.exe is, whatever the environment says.
///
/// # Errors
///
/// The system's error when the folder cannot be read.
pub fn system_directory() -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0_u16; 260];
    loop {
        let size = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: `buffer` is writable for `size` units, and the call writes at most that many.
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), size) } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(std::ffi::OsString::from_wide(&buffer).into());
        }
        // Too small: `length` is the size the folder needs, its terminating NUL included.
        buffer.resize(length, 0);
    }
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

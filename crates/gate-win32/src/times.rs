//! The CPU times a watch samples: the system's, and those of one process with the time it was created, all in the
//! 100 ns ticks of a `FILETIME`.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, STILL_ACTIVE};
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::own_handle;

/// The system's CPU times since it started, summed over every processor, in 100 ns ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemTimes {
    /// The ticks the processors spent idle.
    pub idle: u64,
    /// The ticks they spent in kernel mode, idle time included.
    pub kernel: u64,
    /// The ticks they spent in user mode.
    pub user: u64,
}

/// One process's creation time and the CPU time it has used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessTimes {
    /// When it was created, in 100 ns ticks since 1601 as a `FILETIME` counts them.
    pub created: u64,
    /// Its user and kernel time, in 100 ns ticks.
    pub cpu: u64,
}

/// The system's idle, kernel and user times, as `GetSystemTimes` gives them.
///
/// # Errors
///
/// The system's error when the times cannot be read.
pub fn system_times() -> io::Result<SystemTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: every pointer is to a writable FILETIME that outlives the call.
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SystemTimes {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
    })
}

/// The time now as the system clock that stamps a process's creation reads it, in 100 ns ticks since 1601, so a
/// process created after this call has a creation time at or after it.
#[must_use]
pub fn file_time_now() -> u64 {
    let mut now = FILETIME::default();
    // SAFETY: the pointer is to a writable FILETIME that outlives the call, which cannot fail.
    unsafe { GetSystemTimeAsFileTime(&raw mut now) };
    ticks(now)
}

/// The creation time and the CPU time of the running process `pid`; `None` when no process has that pid or the process
/// has exited, which a process that left a job between its listing and this call has.
///
/// # Errors
///
/// The system's error when the process exists but cannot be opened or its times read.
pub fn process_times(pid: u32) -> io::Result<Option<ProcessTimes>> {
    // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
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
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the handle is open for the whole call with query access, and every pointer is to a writable FILETIME.
    let read = unsafe { GetProcessTimes(process.raw(), &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(ProcessTimes {
        created: ticks(created),
        cpu: ticks(kernel) + ticks(user),
    }))
}

/// A `FILETIME`'s two halves as one count of 100 ns ticks.
fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

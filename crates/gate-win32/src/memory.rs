//! A running process's working set: how many bytes of memory it is using, as `GetProcessMemoryInfo` reports it, which
//! the diagnostics block shows for the processes of a killed job.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, STILL_ACTIVE};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::own_handle;

/// The working set of the running process `pid`, in bytes; `None` when no process has that pid or the process has
/// exited, whose handle another process may still hold.
///
/// # Errors
///
/// The system's error when the process exists but cannot be opened or its memory counted.
pub fn working_set(pid: u32) -> io::Result<Option<u64>> {
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
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = u32::try_from(std::mem::size_of_val(&counters)).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the whole call with the query access the call asks for, and `counters` is a
    // writable `PROCESS_MEMORY_COUNTERS` of `size` bytes that outlives the call.
    if unsafe { GetProcessMemoryInfo(process.raw(), &raw mut counters, size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = u64::try_from(counters.WorkingSetSize).map_err(io::Error::other)?;
    Ok(Some(bytes))
}

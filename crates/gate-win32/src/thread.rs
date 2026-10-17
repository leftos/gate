//! Cancelling the synchronous I/O a thread waits on.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
use windows_sys::Win32::System::IO::CancelSynchronousIo;

/// Whether the synchronous I/O `thread` was waiting on has been cancelled, as `CancelSynchronousIo` tells: `Ok(true)`
/// when one was pending and has been cancelled, `Ok(false)` when none was pending, and the system's error otherwise.
/// A cancelled read fails with `ERROR_OPERATION_ABORTED`.
///
/// # Errors
///
/// The system's error when the call fails for any reason but finding nothing pending.
pub fn cancel_synchronous_io(thread: &JoinHandle<()>) -> io::Result<bool> {
    // SAFETY: the join handle keeps its thread's handle open, with the access the call asks for, for the whole call.
    let cancelled = unsafe { CancelSynchronousIo(thread.as_raw_handle().cast()) };
    if cancelled != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    // Nothing pending is the answer rather than a failure: the thread is not waiting on I/O.
    if error.raw_os_error() == i32::try_from(ERROR_NOT_FOUND).ok() {
        return Ok(false);
    }
    Err(error)
}

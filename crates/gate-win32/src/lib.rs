//! The gate's Win32 layer: named mutexes and events, job objects and their accounting, commands created suspended and
//! the UTF-8 console a command can be created on, the restricted token a command can be created with, the start time
//! of the gate process or of another, the system's, a process's and the WSL VM's CPU times, the console interrupt, the
//! system's boot time, what a drive letter stands for, a rename that never replaces its target, a name opened inside an
//! open folder, a file opened again through its reparse point's filter, an open file's volume serial and file id
//! ([`file_id`]), whether two open files are one, whether an open file is on a network volume ([`on_network_volume`]),
//! the synchronous I/O a thread waits on cancelled ([`cancel_synchronous_io`]), and the Windows message of an error
//! code. And what a run on the interactive desktop needs: a named pipe for one
//! user's local client, handles copied into and out of another process and the checks on a process taken that way, a
//! command created on the interactive desktop, the console session's state, and a registered scheduled task started in
//! a given session once its definition is checked ([`run_task`]). It holds no policy but two: [`run_task`] starts only
//! a task defined as the desktop helper's must be, and a
//! [`DesktopLaunch`] always creates its command in the normal priority class on `WinSta0\Default`. The names, the
//! waits, the environment and priority of any other command, and what an abandoned mutex means to the gate are the
//! caller's. Windows only; elsewhere the crate is empty.

#![cfg(windows)]

mod adopt;
mod console;
mod create;
mod detached;
mod device;
mod event;
mod file;
mod interrupt;
mod job;
mod launch;
mod memory;
mod mutex;
mod pipe;
mod piped;
mod priority;
mod security;
mod session;
mod task;
#[cfg(test)]
mod test_support;
mod thread;
mod times;
mod token;
mod tree;

pub use adopt::{
    EndWait, QueryOnlyProcess, RemoteProcess, check_received_output, creation_time_of, end_process, image_path_of, is_handle_value, parent_pid_of,
    pid_of_process, pid_of_thread, same_object, session_of, thread_is_suspended, user_of,
};
pub use console::{Donor, Utf8Console};
pub use create::spawn;
pub use detached::{Detached, DetachedLaunch};
pub use device::device_target;
pub use event::{EventWait, NamedEvent};
pub use interrupt::Interrupt;
pub use job::{Accounting, Job};
pub use launch::{
    AdoptError, EndReport, EnvironmentBlock, Launch, LaunchError, LaunchOutput, PriorityClass, Released, Running, StdioPipes, Suspended, Waited,
    same_variable_name,
};
pub use memory::working_set;
pub use mutex::{AcquireError, MutexGuard, NamedMutex, OwnedMutexGuard, PrivateMutex, PrivateName, mutex_exists};
pub use pipe::{ConnectWait, PipeClient, PipeOwner, PipeRead, PipeServer, PipeWrite};
pub use piped::{DesktopLaunch, OutputPipes, PipedConsole, PipedFed, PipedLaunch, PipedSuspended, breakaway_refused, output_pipes};
pub use session::{ConnectState, LockState, SessionState, active_console_session, input_desktop_name, session_state};
pub use task::{StartedTask, TaskArgument, TaskError, TaskStart, run_task};
pub use times::{ProcessTimes, SystemTimes, file_time_now, process_times, system_times, wsl_vm_cpu};
pub use token::{Account, Grant, NetworkGroup, RestrictedToken, TokenFacts, current_account, current_user_sid, token_facts};

/// Refuses an open file that is not a plain disk file: one that is not on a disk, is a reparse point (a link or a
/// junction, when opened with `FILE_FLAG_OPEN_REPARSE_POINT`), or has more than one name.
///
/// # Errors
///
/// A refusal, an error with no system error code ([`io::Error::raw_os_error`] is `None`) saying which of those it is;
/// else the system's error, its code kept, when the file cannot be inspected.
pub fn check_plain_file(file: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    launch::check_plain_file(file.as_raw_handle())
}
pub use file::{
    Make, Share, delete_opened, delete_tree_opened, file_id, make_folder_beneath, move_no_replace, names_beneath, on_network_volume, open_beneath,
    open_beneath_as, rename_beneath, rename_beneath_no_replace, reopen_through_filter, same_file,
};
pub use security::{Descriptor, inherit_security, is_protected_folder, protected_descriptor, security_text, without_auto_inherit};
pub use thread::cancel_synchronous_io;
pub use tree::{Membership, OpenedJob, StoppableProcess, WaitableProcess};

pub use priority::{Priority, priority_of, set_priority};

use std::io;
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, LocalFree, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_ALLOCATE_BUFFER, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetExitCodeProcess, GetPriorityClass, GetProcessTimes, IDLE_PRIORITY_CLASS, INFINITE,
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
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

/// Makes a DLL this process loads by name after this call come from the system folder (`System32`) alone
/// (`SetDefaultDllDirectories` with `LOAD_LIBRARY_SEARCH_SYSTEM32`), never from the program's folder, the working
/// folder or `PATH`, so a DLL planted in a folder this process runs from is not loaded by name after it. DLLs loaded
/// before the call, those the program imports among them, are not affected.
///
/// # Errors
///
/// The system's error when the search order cannot be set.
pub fn set_default_dll_directories_system32() -> io::Result<()> {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};

    // SAFETY: the call takes only a flag.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

/// When the system last started: the time now less the milliseconds since the start (`GetTickCount64`, which counts
/// time asleep and hibernated too). Two readings in one boot differ only by the drift between the two clocks.
///
/// # Errors
///
/// An error when that time is before the earliest time a [`SystemTime`] holds.
pub fn boot_time() -> io::Result<SystemTime> {
    // SAFETY: the call takes no argument and cannot fail.
    let since_start = Duration::from_millis(unsafe { GetTickCount64() });
    SystemTime::now()
        .checked_sub(since_start)
        .ok_or_else(|| io::Error::other("the boot time is before the earliest time the system clock holds"))
}

/// The start time of the running process `pid`, as [`process_start`] gives it; `None` when no process has that pid or
/// the process has exited.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::PermissionDenied`] when this process may not query that one, else the system's
/// error when the process cannot be opened or its times read, or when the time is outside a [`Timestamp`]'s range.
pub fn process_start_of(pid: u32) -> io::Result<Option<Timestamp>> {
    let Some(process) = open_running(pid)? else {
        return Ok(None);
    };
    // SAFETY: the handle is open for the whole call, with the query access `GetProcessTimes` asks for.
    unsafe { start_of(process.raw()) }.map(Some)
}

/// How a process opened by [`open_running`] is judged to have exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judge {
    /// A wait of no time on the handle, which has `PROCESS_SYNCHRONIZE` access. The exit code cannot say it: a process
    /// that exited with code 259 (`STILL_ACTIVE`) reads as running to `GetExitCodeProcess` for as long as any handle
    /// keeps its process object.
    Wait,
    /// The exit code, on a handle that has query access alone.
    ExitCode,
}

/// The running process `pid`, opened with `PROCESS_QUERY_LIMITED_INFORMATION` access; `None` when no process has that
/// pid or the process has exited.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::PermissionDenied`] when this process may not query that one, else the system's
/// error when the process cannot be opened or whether it has exited cannot be read.
pub(crate) fn open_running(pid: u32) -> io::Result<Option<OwnedHandle>> {
    let open = |access| {
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        unsafe { own_handle(OpenProcess(access, 0, pid)) }
    };
    let opened = choose_handle(open(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE), || {
        open(PROCESS_QUERY_LIMITED_INFORMATION)
    })?;
    let Some((process, judge)) = opened else {
        return Ok(None);
    };
    let exited = match judge {
        Judge::Wait => event::signalled(&process)?,
        Judge::ExitCode => exit_code_says_exited(&process)?,
    };
    Ok((!exited).then_some(process))
}

/// The handle to judge a process by, and how: the `waitable` open (query and `PROCESS_SYNCHRONIZE` access) when it
/// succeeded, else the `query_only` open when the waitable one was refused; `None` when either open finds no process.
///
/// The waitable open can be refused where query access alone is granted, as to a process of another integrity level
/// or a protected one. Such a process is still judged, by its exit code, so it is never taken for gone while it runs;
/// only one that exited with code 259 (`STILL_ACTIVE`) then reads as running.
fn choose_handle<H>(waitable: io::Result<H>, query_only: impl FnOnce() -> io::Result<H>) -> io::Result<Option<(H, Judge)>> {
    match waitable {
        Ok(handle) => Ok(Some((handle, Judge::Wait))),
        // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
        Err(error) if tree::is_error(&error, ERROR_INVALID_PARAMETER) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => match query_only() {
            Ok(handle) => Ok(Some((handle, Judge::ExitCode))),
            Err(error) if tree::is_error(&error, ERROR_INVALID_PARAMETER) => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    }
}

/// Whether the process behind `process` has exited, as its exit code says: any code but 259 (`STILL_ACTIVE`). A
/// process that exited with 259 reads as running, so this is used only where a wait is refused.
///
/// # Errors
///
/// The system's error when the exit code cannot be read.
fn exit_code_says_exited(process: &OwnedHandle) -> io::Result<bool> {
    let mut code = 0;
    // SAFETY: the guard keeps the handle open for the whole call, with query access, and `code` is writable.
    if unsafe { GetExitCodeProcess(process.raw(), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code != STILL_ACTIVE.cast_unsigned())
}

/// The creation time of the process behind `process`.
///
/// # Safety
///
/// `process` is open for the whole call, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
unsafe fn start_of(process: HANDLE) -> io::Result<Timestamp> {
    // SAFETY: the caller guarantees the handle is open with query access.
    let (created, _) = unsafe { start_and_exit_of(process) }?;
    timestamp_of(created).ok_or_else(|| io::Error::other("the process start time is out of range"))
}

/// The exit time of the process behind `process`, as [`start_of`] gives its start (whole microseconds).
///
/// # Safety
///
/// `process` is open for the whole call, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
unsafe fn exit_of(process: HANDLE) -> io::Result<Timestamp> {
    // SAFETY: the caller guarantees the handle is open with query access.
    let (_, exited) = unsafe { start_and_exit_of(process) }?;
    // A process that still runs has no exit time: its FILETIME is zero.
    if exited == 0 {
        return Err(io::Error::other("the process has not exited"));
    }
    timestamp_of(exited).ok_or_else(|| io::Error::other("the process exit time is out of range"))
}

/// The creation time and the exit time of the process behind `process`, as FILETIME ticks; the exit time is zero
/// while it runs.
///
/// # Safety
///
/// `process` is open for the whole call, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
unsafe fn start_and_exit_of(process: HANDLE) -> io::Result<(u64, u64)> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the caller guarantees the handle is open with query access, and every pointer is to a writable FILETIME.
    let ok = unsafe { GetProcessTimes(process, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let ticks = |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok((ticks(created), ticks(exited)))
}

/// FILETIME `ticks` as the process table's timestamp; `None` when outside its range.
fn timestamp_of(ticks: u64) -> Option<Timestamp> {
    i64::try_from(ticks).ok().and_then(proc_table::filetime_to_timestamp)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose open result is not the one it gave must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::{fs, io};

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY};

    use super::{Judge, choose_handle};

    /// The error an `OpenProcess` refused with `code` leaves.
    fn refused(code: u32) -> io::Error {
        io::Error::from_raw_os_error(i32::try_from(code).expect("a Win32 error code fits an i32"))
    }

    /// A query-only open that succeeds and marks `tried` when it is made.
    fn query_only(tried: &Cell<bool>) -> impl FnOnce() -> io::Result<&'static str> + '_ {
        || {
            tried.set(true);
            Ok("query only")
        }
    }

    #[test]
    fn the_default_dll_directories_are_set_to_the_system_folder() {
        super::set_default_dll_directories_system32().expect("the search order is set");
    }

    #[test]
    fn a_waitable_handle_is_judged_by_a_wait() {
        let tried = Cell::new(false);
        let chosen = choose_handle(Ok("waitable"), query_only(&tried)).expect("an open that succeeded is no error");

        assert_eq!(chosen, Some(("waitable", Judge::Wait)));
        assert!(!tried.get(), "no second open is made");
    }

    #[test]
    fn a_pid_no_process_has_is_none_without_a_second_open() {
        let tried = Cell::new(false);
        let chosen = choose_handle(Err(refused(ERROR_INVALID_PARAMETER)), query_only(&tried)).expect("a pid no process has is no error");

        assert_eq!(chosen, None);
        assert!(!tried.get(), "no second open is made");
    }

    #[test]
    fn a_refused_wait_falls_back_to_the_exit_code_on_a_query_only_handle() {
        let tried = Cell::new(false);
        let chosen = choose_handle(Err(refused(ERROR_ACCESS_DENIED)), query_only(&tried)).expect("the query-only open succeeded");

        assert_eq!(chosen, Some(("query only", Judge::ExitCode)));
        assert!(tried.get(), "the query-only open is made");
    }

    #[test]
    fn a_process_gone_between_the_two_opens_is_none() {
        let chosen = choose_handle::<&str>(Err(refused(ERROR_ACCESS_DENIED)), || Err(refused(ERROR_INVALID_PARAMETER)))
            .expect("a pid no process has is no error");

        assert_eq!(chosen, None);
    }

    #[test]
    fn a_process_that_refuses_both_opens_is_permission_denied() {
        let error =
            choose_handle::<&str>(Err(refused(ERROR_ACCESS_DENIED)), || Err(refused(ERROR_ACCESS_DENIED))).expect_err("both opens were refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[test]
    fn a_handle_that_cannot_be_inspected_is_the_systems_error_with_its_code() {
        let error = crate::launch::check_plain_file(std::ptr::null_mut()).expect_err("a null handle is no open file");

        assert_eq!(error.raw_os_error(), refused(ERROR_INVALID_HANDLE).raw_os_error(), "{error}");
    }

    #[test]
    fn a_file_with_two_names_is_refused_with_no_system_error_code() {
        let folder = crate::test_support::temp_dir().join(format!("gate-win32-plain-{}", std::process::id()));
        // A folder left by an earlier run of the same pid is cleared first.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder is made");
        let first = folder.join("first");
        fs::write(&first, b"one file").expect("the file is written");
        fs::hard_link(&first, folder.join("second")).expect("the file gets a second name");
        let file = fs::File::open(&first).expect("the file opens");

        let error = super::check_plain_file(&file).expect_err("a file with two names is refused");

        drop(file);
        let _ = fs::remove_dir_all(&folder);
        assert_eq!(error.raw_os_error(), None, "{error}");
    }

    #[test]
    fn another_open_error_is_returned_without_a_second_open() {
        let tried = Cell::new(false);
        let error = choose_handle(Err(refused(ERROR_NOT_ENOUGH_MEMORY)), query_only(&tried)).expect_err("the open failed");

        assert_eq!(error.raw_os_error(), refused(ERROR_NOT_ENOUGH_MEMORY).raw_os_error(), "{error}");
        assert!(!tried.get(), "no second open is made");
    }
}

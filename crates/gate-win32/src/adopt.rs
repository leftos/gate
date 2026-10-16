//! Taking a process that another process created: copying handles into and out of that process's handle table, and
//! the checks that say what a copied process or thread handle stands for — its process id, its parent, when it was
//! created, its session, its image, and whether its thread is still suspended. A process that passes them becomes a
//! [`crate::Suspended`] through [`crate::Suspended::adopt`].

use std::io;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle};
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Wdk::Foundation::{NtQueryObject, ObjectBasicInformation};
use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INSUFFICIENT_BUFFER, GetHandleInformation, HANDLE,
    RtlNtStatusToDosError, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, FileNameInfo, GetFileInformationByHandleEx, GetFileType};
use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PIPE_SERVER_END};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessId, GetProcessIdOfThread, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, ResumeThread, SuspendThread, TerminateProcess,
    WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION;

use crate::{own_handle, start_of, wait_millis};

/// The longest image path read, in UTF-16 units: the longest path Windows has.
const LONGEST_PATH: usize = 32_768;

/// Another process, opened to copy handles into and out of its handle table (`PROCESS_DUP_HANDLE`), to query it and to
/// wait for it.
pub struct RemoteProcess {
    handle: OwnedHandle,
}

impl RemoteProcess {
    /// Opens the process `pid` with `PROCESS_DUP_HANDLE`, `PROCESS_QUERY_LIMITED_INFORMATION` and `SYNCHRONIZE`.
    ///
    /// # Errors
    ///
    /// The system's error when the process cannot be opened: `ERROR_INVALID_PARAMETER` when no process has the pid.
    pub fn open(pid: u32) -> io::Result<RemoteProcess> {
        let access = PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        let handle = unsafe { own_handle(OpenProcess(access, 0, pid)) }?;
        Ok(RemoteProcess { handle })
    }

    /// Copies `handle` into the process, with the same access and never inheritable, and returns the copy's value in the
    /// process's handle table. The copy is the process's own: this process never closes it.
    ///
    /// # Errors
    ///
    /// The system's error when the handle cannot be copied.
    pub fn push(&self, handle: BorrowedHandle<'_>) -> io::Result<usize> {
        let mut copy: HANDLE = std::ptr::null_mut();
        // SAFETY: this process's pseudo-handle is always open, `handle` is borrowed open for the whole call, the
        // target is open with `PROCESS_DUP_HANDLE`, and `copy` is writable. The copy's value names a handle in the
        // target's table, which is never used as a handle here.
        let copied = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle.as_raw_handle(),
                self.handle.raw(),
                &raw mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if copied == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(copy.addr())
    }

    /// Takes the handle `value` out of the process: a copy with the same access becomes this process's, and the
    /// process's own handle is closed (`DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE`), whether or not the copy is
    /// made. The copy is not inheritable. A value no handle table entry can have is refused before any copy is tried:
    /// 0, one that is not a multiple of 4 (the system ignores a value's two low bits, so it would name another
    /// handle), and the top eight values, among them the pseudo-handles of the process itself, its thread and its
    /// tokens.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] naming a value refused as above; the system's error when the
    /// copy cannot be made: `ERROR_INVALID_HANDLE` when the process holds no handle of that value.
    pub fn pull(&self, value: usize) -> io::Result<std::os::windows::io::OwnedHandle> {
        if !is_handle_value(value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("0x{value:x} is not a handle value another process's table can hold"),
            ));
        }
        let mut copy: HANDLE = std::ptr::null_mut();
        let options = DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE;
        // SAFETY: the source is open with `PROCESS_DUP_HANDLE`, the value is only looked up in its table (a value it
        // does not hold makes the call fail), this process's pseudo-handle is always open, and `copy` is writable.
        let copied = unsafe {
            DuplicateHandle(
                self.handle.raw(),
                std::ptr::without_provenance_mut(value),
                GetCurrentProcess(),
                &raw mut copy,
                0,
                0,
                options,
            )
        };
        if copied == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a success is a fresh handle in this process that nothing else owns.
        Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(copy) })
    }

    /// The process's own handle, for a wait or a query.
    #[must_use]
    pub fn handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: the handle is open for as long as `self` lives, which the borrow cannot outlive.
        unsafe { BorrowedHandle::borrow_raw(self.handle.raw()) }
    }
}

/// The highest value a handle table entry can have: handle values stay below 2^31, so a value with the high bit of a
/// 32-bit value set, or above it, is a kernel handle or a pseudo-handle, never one a process's table holds.
const HIGHEST_HANDLE_VALUE: usize = 0x7FFF_FFFC;

/// Whether `value` is one a process's handle table entry can have, as [`RemoteProcess::pull`] requires: not 0, a
/// multiple of 4 (the system ignores a value's two low bits, so another value would name another handle), and no
/// higher than `0x7FFF_FFFC`, so neither a kernel handle nor a pseudo-handle (those of a process itself, its thread and
/// its tokens are the top eight values).
#[must_use]
pub fn is_handle_value(value: usize) -> bool {
    value != 0 && value.is_multiple_of(4) && value <= HIGHEST_HANDLE_VALUE
}

/// The stdout or stderr handle another process placed in this process's handle table and sent as the number `value`,
/// borrowed once it passes six checks, in this order: it is a handle value ([`is_handle_value`]); it names an open
/// handle (`GetHandleInformation`); it names a pipe (`GetFileType` is `FILE_TYPE_PIPE`); it is not a pipe's server end
/// (`GetNamedPipeInfo` without `PIPE_SERVER_END`; an anonymous pipe's reading end is its server end); it is not the
/// same kernel object as `control`, this process's own control pipe (`CompareObjectHandles`); and its pipe's name
/// (`GetFileInformationByHandleEx` with `FileNameInfo`) is not the control pipe's. So a lying sender cannot make the
/// command write into the control channel, or hold an anonymous pipe's reading end or a named pipe's server end.
///
/// # Safety
///
/// The checks only look `value` up in this process's table, so a value naming no handle, or a closed one, is refused
/// without harm. For an `Ok`, the caller guarantees that nothing closes `value`, in this process or another, while the
/// returned borrow is used: the helper uses it only until it has reported its child, after which the box may close it.
///
/// # Errors
///
/// An error of kind [`io::ErrorKind::InvalidInput`] naming the check `value` failed: not a handle value, naming no open
/// handle, not naming a pipe, being a pipe's server end, naming the control pipe, or naming a pipe whose name cannot be
/// read; the system's error when the control pipe's own name cannot be read. The error never describes the object the
/// value names.
pub unsafe fn check_received_output<'a>(value: usize, control: BorrowedHandle<'_>) -> io::Result<BorrowedHandle<'a>> {
    let refused = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("the received output handle 0x{value:x} {why}"));
    if !is_handle_value(value) {
        return Err(refused("is not a value a handle table can hold"));
    }
    let raw: HANDLE = std::ptr::without_provenance_mut(value);
    let mut flags = 0;
    // SAFETY: `GetHandleInformation` only looks the value up in this process's table, failing for a value naming no
    // open handle, and writes only to `flags`, which is writable.
    if unsafe { GetHandleInformation(raw, &raw mut flags) } == 0 {
        return Err(refused("names no open handle"));
    }
    // SAFETY: the value names an open handle of this process, as `GetHandleInformation` found, which stays open for
    // the call: the caller guarantees nothing closes it meanwhile.
    let borrowed = unsafe { BorrowedHandle::borrow_raw(raw) };
    if let Some(why) = pipe_problem(borrowed, control)? {
        return Err(refused(why));
    }
    // SAFETY: the value names an open pipe handle of this process, and the caller guarantees that it stays open and
    // unowned while the borrow is used.
    Ok(unsafe { BorrowedHandle::borrow_raw(raw) })
}

/// What makes the open handle `handle` no output pipe for a command, against the control pipe `control`, or `None`.
fn pipe_problem(handle: BorrowedHandle<'_>, control: BorrowedHandle<'_>) -> io::Result<Option<&'static str>> {
    let raw = handle.as_raw_handle();
    // SAFETY: the handle is borrowed open for the whole call.
    if unsafe { GetFileType(raw) } != FILE_TYPE_PIPE {
        return Ok(Some("does not name a pipe"));
    }
    let mut flags = 0;
    // SAFETY: the handle is borrowed open for the whole call, `flags` is writable and the other outputs are not asked for.
    let read = unsafe { GetNamedPipeInfo(raw, &raw mut flags, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    if read == 0 {
        return Ok(Some("does not name a pipe"));
    }
    if flags & PIPE_SERVER_END != 0 {
        return Ok(Some("is a pipe's server end"));
    }
    if same_object(handle, control) {
        return Ok(Some("names this process's control pipe"));
    }
    let control_name = pipe_name(control)?;
    Ok(match pipe_name(handle) {
        Ok(name) if name == control_name => Some("names this process's control pipe"),
        Ok(_) => None,
        Err(_) => Some("names a pipe whose name cannot be read"),
    })
}

/// The name of the pipe behind `handle`, as `GetFileInformationByHandleEx` with `FileNameInfo` gives it.
fn pipe_name(handle: BorrowedHandle<'_>) -> io::Result<Vec<u16>> {
    // A `FILE_NAME_INFO`: a `u32` byte count, then the name; `u32` units keep it aligned. A pipe name is at most 256
    // characters, so 1024 units hold any with room to spare.
    let mut buffer = vec![0_u32; 1 + 1024 / 2];
    let size = u32::try_from(buffer.len() * 4).map_err(io::Error::other)?;
    // SAFETY: the handle is borrowed open for the whole call, and `buffer` is writable and aligned for `size` bytes.
    if unsafe { GetFileInformationByHandleEx(handle.as_raw_handle(), FileNameInfo, buffer.as_mut_ptr().cast(), size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = buffer.first().copied().unwrap_or(0) as usize;
    let name_bytes: Vec<u8> = buffer.iter().skip(1).flat_map(|word| word.to_ne_bytes()).collect();
    let units: Vec<u16> = name_bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_ne_bytes(pair)).collect();
    units
        .get(..bytes / 2)
        .map(<[u16]>::to_vec)
        .ok_or_else(|| io::Error::other("the system reported a pipe name longer than its buffer"))
}

/// Whether `first` and `second` stand for one kernel object (`CompareObjectHandles`), whatever their values.
#[must_use]
pub fn same_object(first: BorrowedHandle<'_>, second: BorrowedHandle<'_>) -> bool {
    // SAFETY: both handles are borrowed open for the whole call; the call only compares the objects behind them.
    unsafe { CompareObjectHandles(first.as_raw_handle(), second.as_raw_handle()) != 0 }
}

/// Ends the process behind `process` with `exit_code` (`TerminateProcess`), then waits at most `bound` for its end:
/// true once it has ended, false when it still had not by then. A process that had ended already is no error. It needs
/// `PROCESS_TERMINATE` and `SYNCHRONIZE` access, as the handle `CreateProcessW` returns has.
///
/// # Errors
///
/// The system's error when the wait fails, or when the process could not be terminated and has not ended.
pub fn end_process(process: BorrowedHandle<'_>, exit_code: u32, bound: Duration) -> io::Result<bool> {
    // SAFETY: the handle is borrowed open for the whole call; one without the access only makes the call fail.
    let terminated = unsafe { TerminateProcess(process.as_raw_handle(), exit_code) } != 0;
    let refused = (!terminated).then(io::Error::last_os_error);
    // SAFETY: as above; the wait is bounded below `INFINITE`.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), wait_millis(bound)) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => refused.map_or(Ok(false), Err),
        _ => Err(io::Error::last_os_error()),
    }
}

/// The access rights `handle` was granted when it was opened or copied (`NtQueryObject` with
/// `ObjectBasicInformation`).
///
/// # Errors
///
/// The system's error, from the call's status, when the rights cannot be read.
pub(crate) fn granted_access(handle: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut info = PUBLIC_OBJECT_BASIC_INFORMATION::default();
    let size = u32::try_from(size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>()).map_err(io::Error::other)?;
    // SAFETY: the handle is borrowed open for the whole call, `info` is writable for `size` bytes, and the return
    // length, which is optional, is not asked for.
    let status = unsafe {
        NtQueryObject(
            handle.as_raw_handle(),
            ObjectBasicInformation,
            (&raw mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        // SAFETY: the call reads nothing but the status it is given.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code.cast_signed()));
    }
    Ok(info.GrantedAccess)
}

/// The id of the process behind `process` (`GetProcessId`), which needs `PROCESS_QUERY_LIMITED_INFORMATION` access.
///
/// # Errors
///
/// The system's error when the id cannot be read.
pub fn pid_of_process(process: BorrowedHandle<'_>) -> io::Result<u32> {
    // SAFETY: the handle is borrowed open for the whole call; one without the access only makes the call fail.
    match unsafe { GetProcessId(process.as_raw_handle()) } {
        0 => Err(io::Error::last_os_error()),
        pid => Ok(pid),
    }
}

/// The id of the process the thread behind `thread` belongs to (`GetProcessIdOfThread`), which needs
/// `THREAD_QUERY_LIMITED_INFORMATION` access.
///
/// # Errors
///
/// The system's error when the id cannot be read.
pub fn pid_of_thread(thread: BorrowedHandle<'_>) -> io::Result<u32> {
    // SAFETY: the handle is borrowed open for the whole call; one without the access only makes the call fail.
    match unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } {
        0 => Err(io::Error::last_os_error()),
        pid => Ok(pid),
    }
}

/// The id of the process that created the process behind `process`, as the process's own record holds it, so it
/// belongs to this process whatever has become of that pid since.
///
/// # Errors
///
/// An error naming the status when the process's basic information cannot be read.
pub fn parent_pid_of(process: BorrowedHandle<'_>) -> io::Result<u32> {
    // SAFETY: the handle is borrowed open for the whole call, and the guard made from it is never dropped, so it never
    // closes a handle it does not own.
    let guard = ManuallyDrop::new(unsafe { own_handle(process.as_raw_handle()) }?);
    proc_table::win32::parent_pid_of(&guard)
}

/// When the process behind `process` was created, as the process table gives start times (whole microseconds).
///
/// # Errors
///
/// The system's error when its times cannot be read, or when the time is outside a [`Timestamp`]'s range.
pub fn creation_time_of(process: BorrowedHandle<'_>) -> io::Result<Timestamp> {
    // SAFETY: the handle is borrowed open for the whole call; one without query access only makes the call fail.
    unsafe { start_of(process.as_raw_handle()) }
}

/// The id of the session the process behind `process` runs in (`ProcessIdToSessionId` of its pid, which the open
/// handle keeps from being reused).
///
/// # Errors
///
/// The system's error when the process's id or session cannot be read.
pub fn session_of(process: BorrowedHandle<'_>) -> io::Result<u32> {
    let pid = pid_of_process(process)?;
    let mut session = 0;
    // SAFETY: `ProcessIdToSessionId` writes only to `session`, which is writable.
    if unsafe { ProcessIdToSessionId(pid, &raw mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}

/// The full path of the program file the process behind `process` runs (`QueryFullProcessImageNameW`, in its Win32
/// form), which needs `PROCESS_QUERY_LIMITED_INFORMATION` access.
///
/// # Errors
///
/// The system's error when the path cannot be read.
pub fn image_path_of(process: BorrowedHandle<'_>) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 260];
    loop {
        let mut size = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: the handle is borrowed open for the whole call, `buffer` is writable for `size` units and `size` is
        // writable.
        let read = unsafe { QueryFullProcessImageNameW(process.as_raw_handle(), PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &raw mut size) };
        if read != 0 {
            buffer.truncate(size as usize);
            return Ok(std::ffi::OsString::from_wide(&buffer).into());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed()) || buffer.len() >= LONGEST_PATH {
            return Err(error);
        }
        buffer.resize((buffer.len() * 2).min(LONGEST_PATH), 0);
    }
}

/// The user of the process behind `process`, as its token names it (`OpenProcessToken` with `TOKEN_QUERY`, then
/// `TokenUser`): the SID's string form (`S-1-5-21-…`). It needs `PROCESS_QUERY_LIMITED_INFORMATION` access.
///
/// # Errors
///
/// The system's error when the process's token cannot be opened or read.
pub fn user_of(process: BorrowedHandle<'_>) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the handle is borrowed open for the whole call, and `token` is writable; a handle without the access
    // only makes the call fail.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a success returns a fresh token handle nothing else owns.
    let token = unsafe { own_handle(token) }?;
    crate::token::user_sid_of(&token)
}

/// Whether the thread behind `thread` is suspended exactly once, as a thread created suspended and not touched since
/// is: it is suspended once more (`SuspendThread`), which says how many times it was before, and that suspension is
/// undone at once (`ResumeThread`). A thread suspended twice or more is not: one resume would leave it suspended. It
/// needs `THREAD_SUSPEND_RESUME` access.
///
/// # Errors
///
/// The system's error when the thread cannot be suspended, or when the suspension cannot be undone, which leaves the
/// thread suspended once more than before.
pub fn thread_is_suspended(thread: BorrowedHandle<'_>) -> io::Result<bool> {
    // SAFETY: the handle is borrowed open for the whole call; one without the access only makes the call fail.
    let before = unsafe { SuspendThread(thread.as_raw_handle()) };
    if before == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; this undoes the suspension just made.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("the thread was suspended to look and not resumed: {error}"),
        ));
    }
    Ok(before == 1)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose process cannot be made or read must fail loudly")]
mod tests {
    use std::io;
    use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle};
    use std::sync::{MutexGuard, PoisonError};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::DuplicateHandle;
    use windows_sys::Win32::Foundation::{CompareObjectHandles, ERROR_INVALID_HANDLE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, CreatePipe, PIPE_TYPE_BYTE};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, ResumeThread,
        SuspendThread, THREAD_QUERY_LIMITED_INFORMATION,
    };

    use super::{
        RemoteProcess, check_received_output, creation_time_of, end_process, image_path_of, is_handle_value, parent_pid_of, pid_of_process,
        pid_of_thread, session_of, thread_is_suspended, user_of,
    };
    use crate::{EnvironmentBlock, Job, Launch, LaunchError, LaunchOutput, StoppableProcess, Suspended, system_directory};

    /// The longest a test waits for a process to end: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// The lock every test of this crate that starts a process holds for its whole run (see `create.rs`).
    fn one_at_a_time() -> MutexGuard<'static, ()> {
        crate::create::tests::ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The report an adopted child is given: a child a drop could not end fails the test that dropped it.
    #[expect(clippy::panic, reason = "a child a test's drop could not end must fail that test loudly")]
    fn unended() -> crate::EndReport {
        Box::new(|pid, error| panic!("the adopted child {pid} could not be ended: {error}"))
    }

    /// A fresh unset manual-reset event.
    fn event() -> std::os::windows::io::OwnedHandle {
        // SAFETY: null security attributes and a null name are allowed. A success is a fresh handle nothing else owns.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null(), "the event is made: {}", io::Error::last_os_error());
        // SAFETY: as above.
        unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(event) }
    }

    /// cmd running `command`, created suspended, its stdin and stdout pipes held by the [`Suspended`].
    fn suspended(command: &str) -> Suspended {
        let system = system_directory().expect("the system folder is read");
        Launch {
            application: system.join("cmd.exe"),
            command_line: format!("cmd /d /c {command}").encode_utf16().collect(),
            directory: system,
            environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
            output: LaunchOutput::Piped,
            priority: None,
        }
        .start_suspended()
        .expect("the command is created")
    }

    /// `handle`, borrowed.
    fn borrow(handle: &proc_table::win32::OwnedHandle) -> BorrowedHandle<'_> {
        // SAFETY: the guard keeps the handle open for as long as the borrow lives.
        unsafe { BorrowedHandle::borrow_raw(handle.raw()) }
    }

    /// Copies of the process and thread handles of `suspended`, which is then forgotten without being ended.
    fn handles_of(mut suspended: Suspended) -> (std::os::windows::io::OwnedHandle, std::os::windows::io::OwnedHandle) {
        let process = suspended.process.take().expect("the process is held");
        let copies = (
            borrow(&process).try_clone_to_owned().expect("the process handle is copied"),
            borrow(&suspended.thread).try_clone_to_owned().expect("the thread handle is copied"),
        );
        drop((process, suspended));
        copies
    }

    /// Whether two handles stand for one object.
    fn same_object(first: BorrowedHandle<'_>, second: BorrowedHandle<'_>) -> bool {
        // SAFETY: both handles are borrowed open for the whole call.
        unsafe { CompareObjectHandles(first.as_raw_handle(), second.as_raw_handle()) != 0 }
    }

    #[test]
    fn handles_pulled_from_another_process_are_closed_there() {
        let _alone = one_at_a_time();
        let holder = suspended("exit 0");
        let remote = RemoteProcess::open(holder.pid()).expect("the holder opens");
        let event = event();

        let value = remote.push(event.as_handle()).expect("the event is pushed");
        let pulled = remote.pull(value).expect("the event is pulled back");

        assert!(same_object(event.as_handle(), pulled.as_handle()), "the pulled handle is the event");
        let again = remote.pull(value).expect_err("the holder's copy was closed");
        assert_eq!(again.raw_os_error(), Some(ERROR_INVALID_HANDLE.cast_signed()), "{again}");
    }

    #[test]
    fn pull_refuses_pseudo_and_misaligned_handle_values() {
        let _alone = one_at_a_time();
        let holder = suspended("exit 0");
        let remote = RemoteProcess::open(holder.pid()).expect("the holder opens");
        let event = event();
        let value = remote.push(event.as_handle()).expect("the event is pushed");
        // The system ignores a value's two low bits, so `value + 1` would name the event; the top values are the
        // pseudo-handles of the holder itself, its thread and its tokens.
        let impossible = [
            0,
            value + 1,
            value + 2,
            value + 3,
            usize::MAX,
            usize::MAX - 1,
            usize::MAX - 3,
            usize::MAX - 7,
        ];

        for candidate in impossible {
            let refused = remote.pull(candidate).expect_err("an impossible handle value is refused");
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "0x{candidate:x}: {refused}");
        }

        let pulled = remote.pull(value).expect("the holder's copy was left alone by the refusals");
        assert!(same_object(event.as_handle(), pulled.as_handle()), "the pulled handle is the event");
    }

    #[test]
    fn an_adopted_suspended_child_passes_its_checks() {
        let _alone = one_at_a_time();
        let holder = suspended("exit 0");
        let remote = RemoteProcess::open(holder.pid()).expect("the holder opens");
        let child = suspended("exit 0");
        let pid = child.pid();
        let (process, thread) = handles_of(child);
        let process_value = remote.push(process.as_handle()).expect("the process is pushed");
        let thread_value = remote.push(thread.as_handle()).expect("the thread is pushed");
        drop((process, thread));

        let process = remote.pull(process_value).expect("the process is pulled");
        let thread = remote.pull(thread_value).expect("the thread is pulled");

        assert_eq!(pid_of_process(process.as_handle()).expect("the pid is read"), pid);
        assert_eq!(pid_of_thread(thread.as_handle()).expect("the thread's pid is read"), pid);
        assert_eq!(parent_pid_of(process.as_handle()).expect("the parent is read"), std::process::id());
        let holder_created = creation_time_of(remote.handle()).expect("the holder's start is read");
        assert!(creation_time_of(process.as_handle()).expect("the start is read") >= holder_created);
        // SAFETY: this process's pseudo-handle is always open.
        let this_process = unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) };
        assert_eq!(
            session_of(process.as_handle()).expect("the session is read"),
            session_of(this_process).expect("ours is read")
        );
        let image = image_path_of(process.as_handle()).expect("the image is read");
        let cmd = system_directory().expect("the system folder is read").join("cmd.exe");
        assert!(
            image.to_string_lossy().eq_ignore_ascii_case(&cmd.to_string_lossy()),
            "{image:?} is {cmd:?}"
        );
        assert!(thread_is_suspended(thread.as_handle()).expect("the thread is read"));
        assert!(
            thread_is_suspended(thread.as_handle()).expect("the thread is read again"),
            "the look left it suspended"
        );
        let job = Job::anonymous_kill_on_close().expect("the job is made");
        assert!(!job.holds_process(process.as_handle()).expect("the membership is read"));

        let adopted = Suspended::adopt(process, thread, pid, unended()).expect("the child is adopted");
        adopted.assign(&job).expect("the adopted child is put in the job");

        assert_eq!(adopted.pid(), pid);
        assert!(adopted.in_job(&job).expect("the membership is read"));
    }

    #[test]
    fn a_child_that_is_not_suspended_fails_the_suspended_check() {
        let _alone = one_at_a_time();
        // `set /p` waits on stdin, whose write end the `Suspended` keeps open, so the child runs until it is ended.
        let child = suspended("set /p line=");
        // SAFETY: the thread handle is open, with the `THREAD_SUSPEND_RESUME` access a created thread's handle has.
        assert_ne!(unsafe { ResumeThread(child.thread.raw()) }, u32::MAX, "the child is let run");

        assert!(!thread_is_suspended(borrow(&child.thread)).expect("the thread is read"));
        assert!(
            !thread_is_suspended(borrow(&child.thread)).expect("the thread is read again"),
            "the look left it running"
        );
    }

    #[test]
    fn a_child_suspended_twice_is_refused() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let watch = StoppableProcess::open(child.pid()).expect("the child opens").expect("the child exists");
        // SAFETY: the thread handle is open, with the `THREAD_SUSPEND_RESUME` access a created thread's handle has.
        assert_ne!(
            unsafe { SuspendThread(child.thread.raw()) },
            u32::MAX,
            "the child is suspended a second time"
        );

        assert!(
            !thread_is_suspended(borrow(&child.thread)).expect("the thread is read"),
            "a thread suspended twice is not suspended exactly once"
        );
        let resumed = child.resume();

        assert!(
            matches!(resumed, Err(LaunchError::Setup { .. })),
            "a resume that leaves the thread suspended fails"
        );
        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the child ended");
        assert_eq!(watch.exit_code().expect("the exit code is read"), 1, "ended unstarted");
    }

    #[test]
    fn a_job_reports_only_its_own_members() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let own = Job::anonymous_kill_on_close().expect("the job is made");
        let other = Job::anonymous_kill_on_close().expect("the other job is made");

        child.assign(&own).expect("the child is put in its job");

        let process = borrow(child.process.as_ref().expect("the process is held"));
        assert!(own.holds_process(process).expect("the membership is read"));
        assert!(!other.holds_process(process).expect("the other membership is read"));
        assert!(child.in_job(&own).expect("the membership is read"));
        assert!(!child.in_job(&other).expect("the other membership is read"));
    }

    #[test]
    fn an_empty_job_takes_a_process_from_a_job_that_refuses_breakaway() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let refusing = Job::anonymous_kill_on_close().expect("the job is made");
        child.assign(&refusing).expect("the child is put in the job that refuses breakaway");
        let empty = Job::anonymous_kill_on_close().expect("the empty job is made");

        child.assign(&empty).expect("the empty job takes the child");

        assert!(child.in_job(&empty).expect("the membership is read"));
        assert!(child.in_job(&refusing).expect("the first membership is read"));
    }

    #[test]
    fn a_dropped_adopted_suspended_child_is_ended_unstarted() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let pid = child.pid();
        let watch = StoppableProcess::open(pid).expect("the child opens").expect("the child exists");
        let (process, thread) = handles_of(child);
        let adopted = Suspended::adopt(process, thread, pid, unended()).expect("the child is adopted");

        drop(adopted);

        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the child ended");
        assert_eq!(
            watch.exit_code().expect("the exit code is read"),
            1,
            "ended unstarted, not by its own exit 0"
        );
    }

    #[test]
    fn adopt_refuses_a_thread_of_another_process() {
        let _alone = one_at_a_time();
        let (first, second) = (suspended("exit 0"), suspended("exit 0"));
        let (first_pid, second_pid) = (first.pid(), second.pid());
        let (first_process, first_thread) = handles_of(first);
        let (second_process, second_thread) = handles_of(second);

        let refused = Suspended::adopt(first_process, second_thread, first_pid, unended())
            .err()
            .expect("a thread of another process is refused");

        assert_eq!(refused.why.kind(), io::ErrorKind::InvalidInput, "{refused}");
        let other_process = Suspended::adopt(second_process, first_thread, first_pid, unended())
            .err()
            .expect("a process of another pid is refused");
        assert_eq!(other_process.why.kind(), io::ErrorKind::InvalidInput, "{other_process}");
        // Both children are adopted as they are, so dropping them ends them.
        drop(Suspended::adopt(refused.process, other_process.thread, first_pid, unended()).expect("the first child is adopted"));
        drop(Suspended::adopt(other_process.process, refused.thread, second_pid, unended()).expect("the second child is adopted"));
    }

    /// A copy of `handle` with `access` alone.
    fn narrowed(handle: BorrowedHandle<'_>, access: u32) -> std::os::windows::io::OwnedHandle {
        let mut copy: HANDLE = std::ptr::null_mut();
        // SAFETY: this process's pseudo-handle is always open, `handle` is borrowed open for the whole call, and `copy`
        // is writable.
        let copied = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle.as_raw_handle(),
                GetCurrentProcess(),
                &raw mut copy,
                access,
                0,
                0,
            )
        };
        assert_ne!(copied, 0, "the handle is copied: {}", io::Error::last_os_error());
        // SAFETY: a success is a fresh handle nothing else owns.
        unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(copy) }
    }

    #[test]
    fn adopt_refuses_a_handle_without_the_access_it_needs() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let pid = child.pid();
        let (process, thread) = handles_of(child);
        let query_process = narrowed(process.as_handle(), PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE);
        let query_thread = narrowed(thread.as_handle(), THREAD_QUERY_LIMITED_INFORMATION);

        let no_terminate = Suspended::adopt(query_process, thread, pid, unended())
            .err()
            .expect("a process handle that cannot end the process is refused");
        let no_resume = Suspended::adopt(process, query_thread, pid, unended())
            .err()
            .expect("a thread handle that cannot resume the thread is refused");

        assert_eq!(no_terminate.why.kind(), io::ErrorKind::InvalidInput, "{no_terminate}");
        assert!(no_terminate.why.to_string().contains("process handle"), "{no_terminate}");
        assert_eq!(no_resume.why.kind(), io::ErrorKind::InvalidInput, "{no_resume}");
        assert!(no_resume.why.to_string().contains("thread handle"), "{no_resume}");
        // The full handles are adopted as they are, so dropping them ends the child.
        drop(Suspended::adopt(no_resume.process, no_terminate.thread, pid, unended()).expect("the child is adopted with full handles"));
    }

    #[test]
    fn a_refused_adopt_gives_back_its_handles_and_ends_nothing() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let pid = child.pid();
        let watch = StoppableProcess::open(pid).expect("the child opens").expect("the child exists");
        let (process, thread) = handles_of(child);

        let refused = Suspended::adopt(process, thread, pid + 4, unended())
            .err()
            .expect("a pid the handles are not of is refused");

        assert!(!watch.wait(Duration::ZERO).expect("the wait succeeds"), "the refused child was not ended");
        assert_eq!(pid_of_process(refused.process.as_handle()).expect("the given-back process is read"), pid);
        assert!(
            thread_is_suspended(refused.thread.as_handle()).expect("the given-back thread is read"),
            "the given-back thread is still suspended once"
        );
        drop(Suspended::adopt(refused.process, refused.thread, pid, unended()).expect("the child is adopted after all"));
        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the adopted child ended once dropped");
    }

    #[test]
    fn is_handle_value_accepts_only_values_a_handle_table_can_hold() {
        for value in [4, 8, 0x1000, 0x7FFF_FFFC] {
            assert!(is_handle_value(value), "0x{value:x} is a handle value");
        }
        for value in [
            0,
            1,
            2,
            3,
            5,
            6,
            7,
            0x8000_0000,
            usize::MAX - 11,
            usize::MAX - 7,
            usize::MAX - 3,
            usize::MAX,
        ] {
            assert!(!is_handle_value(value), "0x{value:x} is not a handle value");
        }
    }

    /// A fresh anonymous pipe: its read end, then its write end.
    fn pipe() -> (std::os::windows::io::OwnedHandle, std::os::windows::io::OwnedHandle) {
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: both ends are writable, no security attributes are passed, and 0 asks for the default size.
        let made = unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) };
        assert_ne!(made, 0, "the pipe is made: {}", io::Error::last_os_error());
        // SAFETY: a success gives two fresh handles nothing else owns.
        unsafe {
            (
                std::os::windows::io::OwnedHandle::from_raw_handle(read),
                std::os::windows::io::OwnedHandle::from_raw_handle(write),
            )
        }
    }

    /// Asserts that `check_received_output` refuses `value` against `control` for a reason that says `why`.
    fn assert_output_refused(value: usize, control: BorrowedHandle<'_>, why: &str) {
        // SAFETY: a refused value is never borrowed, so nothing about its handle's life is relied on.
        let refused = unsafe { check_received_output(value, control) }.expect_err("the value is refused");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "0x{value:x}: {refused}");
        assert!(refused.to_string().contains(why), "0x{value:x} is refused for {why:?}: {refused}");
    }

    #[test]
    fn check_received_output_takes_only_a_pipe_other_than_the_control_pipe() {
        // The helper's control pipe is the client end of a named pipe, as here.
        let sid = crate::current_user_sid().expect("the user is read");
        let server = crate::PipeServer::create("gate-win32-adopt-test", &sid).expect("the control pipe is made");
        let client = crate::PipeClient::open(server.name(), &sid).expect("the control pipe opens");
        let control = client.handle();
        let (read, write) = pipe();

        // SAFETY: `write` owns the value and outlives the borrow, closing nothing meanwhile.
        let taken = unsafe { check_received_output(write.as_raw_handle().addr(), control.as_handle()) }.expect("a pipe's write end is taken");

        assert!(same_object(taken, write.as_handle()), "the borrow is the pipe's write end");
        let file = std::fs::File::open(std::env::current_exe().expect("the test's path is read")).expect("the test's file opens");
        assert_output_refused(file.as_raw_handle().addr(), control.as_handle(), "does not name a pipe");
        let event = event();
        assert_output_refused(event.as_raw_handle().addr(), control.as_handle(), "does not name a pipe");
        assert_output_refused(control.as_raw_handle().addr(), control.as_handle(), "control pipe");
        let copy = control.try_clone_to_owned().expect("the control handle is copied");
        assert_output_refused(copy.as_raw_handle().addr(), control.as_handle(), "control pipe");
        assert_output_refused(server.raw_for_tests().addr(), control.as_handle(), "is a pipe's server end");
        assert_output_refused(read.as_raw_handle().addr(), control.as_handle(), "is a pipe's server end");
        assert_output_refused(3, control.as_handle(), "is not a value");
        let (closed_read, closed_write) = pipe();
        let closed = closed_write.as_raw_handle().addr();
        drop((closed_read, closed_write));
        assert_output_refused(closed, control.as_handle(), "names no open handle");
    }

    #[test]
    fn check_received_output_refuses_another_client_of_the_control_pipe_s_name() {
        // A pipe with two instances takes two clients: two objects of one name, which only the name check tells apart.
        let name: Vec<u16> = format!(r"\\.\pipe\gate-win32-adopt-twin-{}", std::process::id())
            .encode_utf16()
            .chain([0])
            .collect();
        let instance = || {
            // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes are passed. A failure
            // returns `INVALID_HANDLE_VALUE`, checked before the handle is owned.
            let made = unsafe { CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, 2, 4096, 4096, 0, std::ptr::null()) };
            assert_ne!(made, INVALID_HANDLE_VALUE, "an instance is made: {}", io::Error::last_os_error());
            // SAFETY: as above.
            unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(made) }
        };
        let client = || {
            // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes or template are passed. A
            // failure returns `INVALID_HANDLE_VALUE`, checked before the handle is owned.
            let opened = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(opened, INVALID_HANDLE_VALUE, "a client opens: {}", io::Error::last_os_error());
            // SAFETY: as above.
            unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(opened) }
        };
        let _instances = (instance(), instance());
        let (control, other) = (client(), client());

        assert!(!same_object(control.as_handle(), other.as_handle()), "the two clients are two objects");
        assert_output_refused(other.as_raw_handle().addr(), control.as_handle(), "names this process's control pipe");
    }

    #[test]
    fn end_process_ends_a_process_and_finds_an_ended_one_ended() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let (process, thread) = handles_of(child);

        let first = end_process(process.as_handle(), 1, HANG_GUARD).expect("the process is ended");
        let again = end_process(process.as_handle(), 1, HANG_GUARD).expect("an ended process is no error");

        assert!(first, "the process ended");
        assert!(again, "the ended process is found ended");
        drop(thread);
    }

    #[test]
    fn end_process_fails_when_the_terminate_is_refused_and_the_process_lives() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        let opened = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, child.pid()) };
        assert!(!opened.is_null(), "the child opens: {}", io::Error::last_os_error());
        // SAFETY: as above.
        let wait_only = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(opened) };

        let refused = end_process(wait_only.as_handle(), 1, Duration::ZERO).expect_err("a handle without PROCESS_TERMINATE cannot end it");

        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
        drop(child);
    }

    #[test]
    fn end_process_fails_when_the_wait_fails() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");
        let watch = StoppableProcess::open(child.pid()).expect("the child opens").expect("the child exists");
        // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
        let opened = unsafe { OpenProcess(PROCESS_TERMINATE, 0, child.pid()) };
        assert!(!opened.is_null(), "the child opens: {}", io::Error::last_os_error());
        // SAFETY: as above.
        let terminate_only = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(opened) };

        let failed = end_process(terminate_only.as_handle(), 1, HANG_GUARD).expect_err("a handle without SYNCHRONIZE cannot be waited on");

        assert_eq!(failed.kind(), io::ErrorKind::PermissionDenied, "{failed}");
        assert!(
            watch.wait(HANG_GUARD).expect("the wait succeeds"),
            "the terminate went through before the wait failed"
        );
        drop(child);
    }

    #[test]
    fn user_of_a_child_is_the_test_s_own_user() {
        let _alone = one_at_a_time();
        let child = suspended("exit 0");

        let user = user_of(borrow(child.process.as_ref().expect("the process is held"))).expect("the child's user is read");

        assert_eq!(user, crate::token_facts().expect("this process's token is read").user);
    }
}

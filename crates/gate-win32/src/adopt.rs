//! Taking a process that another process created: copying handles into and out of that process's handle table, and
//! the checks that say what a copied process or thread handle stands for — its process id, its parent, when it was
//! created, its session, its image, and whether its thread is still suspended. A process that passes them becomes a
//! [`crate::Suspended`] through [`crate::Suspended::adopt`].

use std::io;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle};
use std::path::PathBuf;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessId, GetProcessIdOfThread, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, ResumeThread, SuspendThread,
};

use crate::{own_handle, start_of};

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

    /// Copies `handle` into the process, with the same access, inheritable when `inheritable` says so, and returns the
    /// copy's value in the process's handle table. The copy is the process's own: this process never closes it.
    ///
    /// # Errors
    ///
    /// The system's error when the handle cannot be copied.
    pub fn push(&self, handle: BorrowedHandle<'_>, inheritable: bool) -> io::Result<usize> {
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
                i32::from(inheritable),
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
    /// made. The copy is not inheritable.
    ///
    /// # Errors
    ///
    /// The system's error when the copy cannot be made: `ERROR_INVALID_HANDLE` when the process holds no handle of
    /// that value.
    pub fn pull(&self, value: usize) -> io::Result<std::os::windows::io::OwnedHandle> {
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

/// Whether the thread behind `thread` is suspended: it is suspended once more (`SuspendThread`), which says how many
/// times it was before, and that suspension is undone at once (`ResumeThread`). It needs `THREAD_SUSPEND_RESUME`
/// access.
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
    Ok(before >= 1)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose process cannot be made or read must fail loudly")]
mod tests {
    use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CompareObjectHandles, ERROR_INVALID_HANDLE};
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, ResumeThread};

    use super::{RemoteProcess, creation_time_of, image_path_of, parent_pid_of, pid_of_process, pid_of_thread, session_of, thread_is_suspended};
    use crate::{EnvironmentBlock, Job, Launch, LaunchOutput, StoppableProcess, Suspended, system_directory};

    /// The longest a test waits for a process to end: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

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
        let holder = suspended("exit 0");
        let remote = RemoteProcess::open(holder.pid()).expect("the holder opens");
        // SAFETY: null security attributes and a null name are allowed. A success is a fresh handle nothing else owns.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!event.is_null(), "the event is made");
        // SAFETY: as above.
        let event = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(event) };

        let value = remote.push(event.as_handle(), false).expect("the event is pushed");
        let pulled = remote.pull(value).expect("the event is pulled back");

        assert!(same_object(event.as_handle(), pulled.as_handle()), "the pulled handle is the event");
        let again = remote.pull(value).expect_err("the holder's copy was closed");
        assert_eq!(again.raw_os_error(), Some(ERROR_INVALID_HANDLE.cast_signed()), "{again}");
    }

    #[test]
    fn an_adopted_suspended_child_passes_its_checks() {
        let holder = suspended("exit 0");
        let remote = RemoteProcess::open(holder.pid()).expect("the holder opens");
        let child = suspended("exit 0");
        let pid = child.pid();
        let (process, thread) = handles_of(child);
        let process_value = remote.push(process.as_handle(), false).expect("the process is pushed");
        let thread_value = remote.push(thread.as_handle(), false).expect("the thread is pushed");
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

        let adopted = Suspended::adopt(process, thread).expect("the child is adopted");
        adopted.assign(&job).expect("the adopted child is put in the job");

        assert_eq!(adopted.pid(), pid);
        assert!(adopted.in_job(&job).expect("the membership is read"));
    }

    #[test]
    fn a_child_that_is_not_suspended_fails_the_suspended_check() {
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
    fn a_job_reports_only_its_own_members() {
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
        let child = suspended("exit 0");
        let watch = StoppableProcess::open(child.pid()).expect("the child opens").expect("the child exists");
        let (process, thread) = handles_of(child);
        let adopted = Suspended::adopt(process, thread).expect("the child is adopted");

        drop(adopted);

        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the child ended");
        assert_eq!(
            watch.exit_code().expect("the exit code is read"),
            1,
            "ended unstarted, not by its own exit 0"
        );
    }
}

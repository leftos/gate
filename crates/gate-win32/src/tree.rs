//! What stopping a process tree or waiting for one process needs: another gate's job opened by its name, which also
//! answers whether a process is in it, a process opened to be stopped, and one opened to wait for its end.

use std::io;
use std::time::Duration;

use jiff::Timestamp;
use proc_table::win32::OwnedHandle;
use windows_sys::Wdk::System::Threading::NtTerminateProcess;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, LocalFree, NTSTATUS, RtlNtStatusToDosError,
    STATUS_OBJECT_TYPE_MISMATCH, STATUS_PROCESS_IS_TERMINATING, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::JobObjects::OpenJobObjectW;
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    WaitForSingleObject,
};

use crate::token::sid_string;
use crate::{Job, exit_of, job, own_handle, start_of, wait_millis, wide};

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The NT status the last system call made on this thread left in its thread block, which the Win32 error
    /// `GetLastError` gives was converted from; neither `windows-sys` nor `windows` binds it.
    fn RtlGetLastNtStatus() -> NTSTATUS;
}

/// What [`OpenedJob::open_or_not_a_job`] found at a name.
pub enum JobOpened {
    /// A job, opened to list and end its processes.
    Job(OpenedJob),
    /// No object has the name.
    Absent,
    /// An object that is no job (an event, a mutex) holds the name.
    NotAJob,
}

/// A job another process created, opened by its name with the access to list its processes, and to end them when it
/// was opened to be stopped. Dropping it only closes this handle.
pub struct OpenedJob {
    handle: OwnedHandle,
    name: String,
}

impl OpenedJob {
    /// Opens the job `name` (such as `Local\gate-job-<pid>`) with `JOB_OBJECT_QUERY` and `JOB_OBJECT_TERMINATE`
    /// access, to list and end its processes; `None` when no job has that name.
    ///
    /// # Errors
    ///
    /// The system's error when the job exists but cannot be opened.
    pub fn open(name: &str) -> io::Result<Option<OpenedJob>> {
        Self::open_with(name, JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE)
    }

    /// Opens the job `name` with `JOB_OBJECT_QUERY` access alone, to list its processes: [`OpenedJob::terminate`]
    /// fails on it. `None` when no job has that name.
    ///
    /// # Errors
    ///
    /// The system's error when the job exists but cannot be opened.
    pub fn open_to_list(name: &str) -> io::Result<Option<OpenedJob>> {
        Self::open_with(name, JOB_OBJECT_QUERY)
    }

    /// The owner of the job `name`, its SID in its string form (`S-1-5-21-…`), read through a handle opened with
    /// `READ_CONTROL` alone, which the system grants a job's owner whatever its DACL says; `None` when no job has that
    /// name.
    ///
    /// # Errors
    ///
    /// The system's error when the job exists but its security cannot be read, such as access denied to a process that
    /// does not own it and that its DACL does not let read it.
    pub fn owner_of(name: &str) -> io::Result<Option<String>> {
        let Some(opened) = Self::open_with(name, READ_CONTROL)? else {
            return Ok(None);
        };
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the handle is open for the whole call with `READ_CONTROL`, and `owner` and `descriptor` are writable;
        // no other part is asked for.
        let error = unsafe {
            GetSecurityInfo(
                opened.handle.raw(),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if error != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(error.cast_signed()));
        }
        // SAFETY: `owner` points into `descriptor`, which is freed only after this.
        let text = unsafe { sid_string(owner) };
        // SAFETY: the descriptor was allocated by `GetSecurityInfo` with `LocalAlloc`, and nothing uses it after this.
        unsafe { LocalFree(descriptor) };
        text.map(Some)
    }

    /// Opens the job `name` with `access`; `None` when no job has that name.
    fn open_with(name: &str, access: u32) -> io::Result<Option<OpenedJob>> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call. A success is a fresh handle nothing else owns.
        match unsafe { own_handle(OpenJobObjectW(access, 0, wide_name.as_ptr())) } {
            Ok(handle) => Ok(Some(OpenedJob {
                handle,
                name: name.to_owned(),
            })),
            // `OpenJobObjectW` reports a name no job has as `ERROR_FILE_NOT_FOUND`.
            Err(error) if is_error(&error, ERROR_FILE_NOT_FOUND) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Opens the job `name` as [`OpenedJob::open`] does, telling a name no object has from one an object that is no
    /// job (an event, a mutex) holds, for a clear that judges a recorded job's name: such an object holds no process.
    /// `OpenJobObjectW` reports both a wrong type and a closed handle as `ERROR_INVALID_HANDLE`, so the NT status it
    /// left is read ([`RtlGetLastNtStatus`]), and only `STATUS_OBJECT_TYPE_MISMATCH` is [`JobOpened::NotAJob`].
    ///
    /// # Errors
    ///
    /// The system's error when the name cannot be opened otherwise, `STATUS_INVALID_HANDLE` included.
    pub fn open_or_not_a_job(name: &str) -> io::Result<JobOpened> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call. A success is a fresh handle nothing else owns.
        let opened = unsafe { own_handle(OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, wide_name.as_ptr())) };
        match opened {
            Ok(handle) => Ok(JobOpened::Job(OpenedJob {
                handle,
                name: name.to_owned(),
            })),
            Err(error) if is_error(&error, ERROR_FILE_NOT_FOUND) => Ok(JobOpened::Absent),
            Err(error) if is_error(&error, ERROR_INVALID_HANDLE) => {
                // SAFETY: the call takes nothing and reads the status the failed open left in this thread's block.
                let status = unsafe { RtlGetLastNtStatus() };
                if status == STATUS_OBJECT_TYPE_MISMATCH {
                    Ok(JobOpened::NotAJob)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// The name it was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The ids of every process in the job now, those in the jobs nested inside it included.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be queried.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        job::process_ids(&self.handle)
    }

    /// Ends every process in the job, and in the jobs nested inside it, with `exit_code`.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        job::terminate(&self.handle, exit_code)
    }

    /// Whether the process `pid`, created at `created` (whole microseconds, as the process table gives start times), is
    /// in the job or in a job nested inside it. The process is opened with `PROCESS_QUERY_LIMITED_INFORMATION` alone,
    /// all `IsProcessInJob` needs; one that cannot be opened, for any reason, is [`Membership::NotOpened`].
    ///
    /// # Errors
    ///
    /// The system's error when `IsProcessInJob` fails on a process that opened.
    pub fn holds(&self, pid: u32, created: Timestamp) -> io::Result<Membership> {
        // A process snapshot reads each start with this same access, so a process the snapshot saw that cannot be
        // opened now changed after it: no process has the pid any more, or the one that has it may not be queried.
        let Ok(Some(process)) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
            return Ok(Membership::NotOpened);
        };
        // SAFETY: the handle is open for as long as `process` lives, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
        if unsafe { start_of(process.raw()) }.ok() != Some(created) {
            return Ok(Membership::Reused);
        }
        Ok(if job::holds(&self.handle, &process)? {
            Membership::Member
        } else {
            Membership::Outside
        })
    }
}

/// Whether a process, named by its pid and the creation time it is expected to have, is in a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    /// The process is in the job, or in a job nested inside it.
    Member,
    /// The process runs outside the job.
    Outside,
    /// No process of that pid could be opened: none runs, or this process may not query it.
    NotOpened,
    /// The pid belongs to a process created at another time than the one expected, or its creation time cannot be read.
    Reused,
}

/// A process opened to be stopped: with the access to read its creation time and its exit code, to wait for it and to
/// terminate it.
pub struct StoppableProcess {
    handle: OwnedHandle,
}

impl StoppableProcess {
    /// Opens the process `pid` with `PROCESS_TERMINATE`, `PROCESS_QUERY_LIMITED_INFORMATION` and `SYNCHRONIZE`
    /// access; `None` when no process has that pid. A process that has exited but whose handle another process still
    /// holds opens, and [`StoppableProcess::wait`] finds it ended.
    ///
    /// # Errors
    ///
    /// The system's error when the process exists but cannot be opened, such as one this process may not terminate.
    pub fn open(pid: u32) -> io::Result<Option<StoppableProcess>> {
        let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        Ok(open_process(pid, access)?.map(|handle| StoppableProcess { handle }))
    }

    /// Its creation time, as the process table gives start times (whole microseconds), so the two compare equal for
    /// one process.
    ///
    /// # Errors
    ///
    /// The system's error when its times cannot be read, or when the time is outside a [`Timestamp`]'s range.
    pub fn created(&self) -> io::Result<Timestamp> {
        // SAFETY: the handle is open for as long as `self` lives, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
        unsafe { start_of(self.handle.raw()) }
    }

    /// When it ended, as [`StoppableProcess::created`] gives its start (whole microseconds), so a process that started
    /// after it ended is told from one it started; it has an end once [`StoppableProcess::wait`] has found it ended.
    ///
    /// # Errors
    ///
    /// The system's error when its times cannot be read; an error when it has not ended, or when the time is outside a
    /// [`Timestamp`]'s range.
    pub fn exited(&self) -> io::Result<Timestamp> {
        // SAFETY: the handle is open for as long as `self` lives, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
        unsafe { exit_of(self.handle.raw()) }
    }

    /// The id of the process that started it, read through this handle, so it is this process's parent whatever has
    /// become of its pid since it was opened.
    ///
    /// # Errors
    ///
    /// An error naming the status when its basic information cannot be read.
    pub fn parent_pid(&self) -> io::Result<u32> {
        proc_table::win32::parent_pid_of(&self.handle)
    }

    /// Whether it is in `job`, or in a job nested inside it.
    ///
    /// # Errors
    ///
    /// The system's error when that cannot be read.
    pub fn in_job(&self, job: &Job) -> io::Result<bool> {
        job.holds(&self.handle)
    }

    /// Waits at most `timeout` for the process to end: true when it has ended, false when it still runs.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        wait_handle(&self.handle, timeout)
    }

    /// The code the process ended with, meaningful once [`StoppableProcess::wait`] has found it ended: a process that
    /// still runs reports `STILL_ACTIVE` (259) for as long as a handle keeps its process object.
    ///
    /// # Errors
    ///
    /// The system's error when the code cannot be read.
    pub fn exit_code(&self) -> io::Result<u32> {
        // SAFETY: the handle is open for the whole call, with the `PROCESS_QUERY_LIMITED_INFORMATION` access the call
        // asks for, and `code` is writable.
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.handle.raw(), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }

    /// Ends the process with `exit_code`: [`Terminated::Now`] when this ended it, [`Terminated::AlreadyEnding`] when it
    /// was ending already (`STATUS_PROCESS_IS_TERMINATING`), and keeps the code it was ending with.
    ///
    /// # Errors
    ///
    /// The system's error, converted from the NT status, when it cannot be terminated, as when the handle lacks the
    /// access.
    pub fn terminate(&self, exit_code: u32) -> io::Result<Terminated> {
        // SAFETY: the handle is open for the whole call, with the `PROCESS_TERMINATE` access the call asks for.
        let status = unsafe { NtTerminateProcess(self.handle.raw(), exit_code.cast_signed()) };
        match status {
            STATUS_PROCESS_IS_TERMINATING => Ok(Terminated::AlreadyEnding),
            _ if status >= 0 => Ok(Terminated::Now),
            _ => {
                // SAFETY: the call reads nothing but the status it is given.
                let code = unsafe { RtlNtStatusToDosError(status) };
                Err(io::Error::from_raw_os_error(code.cast_signed()))
            }
        }
    }
}

/// What a termination [`StoppableProcess::terminate`] asked for came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminated {
    /// This call ended the process.
    Now,
    /// The process was ending already, so this call changed nothing.
    AlreadyEnding,
}

/// A process opened to wait for its end, with the access a wait needs and no more: the process that started this one.
pub struct WaitableProcess {
    handle: OwnedHandle,
}

impl WaitableProcess {
    /// Opens the process `pid` with `PROCESS_QUERY_LIMITED_INFORMATION` and `SYNCHRONIZE`; `None` when no process has
    /// that pid.
    ///
    /// # Errors
    ///
    /// The system's error when the process exists but cannot be opened, such as one this process may not query.
    pub fn open(pid: u32) -> io::Result<Option<WaitableProcess>> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        Ok(open_process(pid, access)?.map(|handle| WaitableProcess { handle }))
    }

    /// Its creation time, as the process table gives start times (whole microseconds), so a process that started after
    /// the one that opened it is told apart from it.
    ///
    /// # Errors
    ///
    /// The system's error when its times cannot be read, or when the time is outside a [`Timestamp`]'s range.
    pub fn created(&self) -> io::Result<Timestamp> {
        // SAFETY: the handle is open for as long as `self` lives, with `PROCESS_QUERY_LIMITED_INFORMATION` access.
        unsafe { start_of(self.handle.raw()) }
    }

    /// Waits at most `timeout` for the process to end: true when it has ended, false when it still runs.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        wait_handle(&self.handle, timeout)
    }
}

/// The process `pid` opened with `access`; `None` when no process has that pid.
fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
    match unsafe { own_handle(OpenProcess(access, 0, pid)) } {
        Ok(handle) => Ok(Some(handle)),
        // `OpenProcess` reports a pid no process has as `ERROR_INVALID_PARAMETER`.
        Err(error) if is_error(&error, ERROR_INVALID_PARAMETER) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Waits at most `timeout` for `handle` to be signalled, as a process's is once it has ended: true when it was, false
/// when the timeout ran out first.
fn wait_handle(handle: &OwnedHandle, timeout: Duration) -> io::Result<bool> {
    // SAFETY: the handle is open for the whole call, with the `SYNCHRONIZE` access a wait asks for.
    match unsafe { WaitForSingleObject(handle.raw(), wait_millis(timeout)) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Whether `error` is the Win32 error `code`.
pub(crate) fn is_error(error: &io::Error, code: u32) -> bool {
    error.raw_os_error() == i32::try_from(code).ok()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot start or open its process must fail loudly")]
mod tests {
    use std::process::{Command, Stdio};

    use super::{JobOpened, OpenedJob, StoppableProcess};

    #[test]
    fn a_name_an_event_holds_is_no_job_a_missing_one_is_absent_and_a_job_is_opened() {
        let pid = std::process::id();
        let event_name = format!(r"Local\gate-win32-tree-event-{pid}");
        let job_name = format!(r"Local\gate-win32-tree-job-{pid}");
        let missing_name = format!(r"Local\gate-win32-tree-missing-{pid}");
        let _event = crate::NamedEvent::create(&event_name).expect("the event is made");
        let _job = crate::Job::create(&job_name).expect("the job is made");

        let at_event = OpenedJob::open_or_not_a_job(&event_name).expect("a name an event holds is no error");
        let at_missing = OpenedJob::open_or_not_a_job(&missing_name).expect("a missing name is no error");
        let at_job = OpenedJob::open_or_not_a_job(&job_name).expect("the job opens");

        assert!(matches!(at_event, JobOpened::NotAJob), "a name an event holds is no job");
        assert!(matches!(at_missing, JobOpened::Absent), "a name no object has is absent");
        let opened_name = match at_job {
            JobOpened::Job(opened) => Some(opened.name().to_owned()),
            JobOpened::Absent | JobOpened::NotAJob => None,
        };
        assert_eq!(opened_name.as_deref(), Some(job_name.as_str()), "the job is opened, by its name");
    }

    #[test]
    fn a_child_s_parent_read_through_its_handle_is_this_process() {
        let cmd = crate::system_directory().expect("the system folder is known").join("cmd.exe");
        // `set /p` waits for a line on its stdin, so the child runs until this test drops its end of the pipe.
        let mut child = crate::spawn(
            Command::new(cmd)
                .args(["/d", "/c", "set /p line="])
                .stdin(Stdio::piped())
                .stdout(Stdio::null()),
        )
        .expect("cmd starts");
        let opened = StoppableProcess::open(child.id()).expect("the child opens").expect("the child runs");

        let parent = opened.parent_pid();

        opened.terminate(1).expect("the child is ended");
        child.wait().expect("the child is waited for");
        assert_eq!(parent.expect("the child's parent is read"), std::process::id());
    }

    #[test]
    fn an_unrelated_process_s_parent_read_through_its_handle_is_the_one_the_snapshot_lists() {
        let me = std::process::id();
        let snapshot = proc_table::Win32ProcessTable.snapshot_without_command_lines();
        let listed_parent = |pid: u32| {
            snapshot
                .iter()
                .find(|process| process.pid == pid)
                .map(|process| process.parent_pid)
                .expect("the snapshot lists the process")
        };
        let parent_pid = StoppableProcess::open(me)
            .expect("this process opens")
            .expect("it runs")
            .parent_pid()
            .expect("this process's parent is read");
        assert_eq!(parent_pid, listed_parent(me), "this process's parent, as the snapshot lists it");
        // The test runner that started this test runs until it ends.
        let parent = StoppableProcess::open(parent_pid).expect("this process's parent opens").expect("it runs");

        let grandparent = parent.parent_pid().expect("the parent's own parent is read");

        assert_ne!(grandparent, me, "the parent was not started by this process");
        assert_eq!(grandparent, listed_parent(parent_pid), "the parent's parent, as the snapshot lists it");
    }
}

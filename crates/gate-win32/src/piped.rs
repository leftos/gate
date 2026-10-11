//! Commands created suspended with their output piped back: stdin from `NUL`, or fed through a third pipe, stdout and stderr into two anonymous
//! pipes whose read ends stay with the caller, outside the caller's own job or within it, and on a hidden console of their own or the
//! caller's, then put in a job, named or anonymous, and resumed. The command also inherits a handle to that job, so the job's name
//! lasts while the command runs even after the caller is gone; unless the job kills on close, which a command holding a
//! handle to it would keep open.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, GENERIC_READ, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, GetCurrentProcess, PROCESS_CREATION_FLAGS};

use crate::create::{Creation, Inherited, check_launch, create_inheriting};
use crate::launch::{open_shared, setup, struct_size};
use crate::{EnvironmentBlock, Job, LaunchError, Suspended, own_handle, wide};

/// How to start a command whose output the caller reads: the caller decides every part of it.
pub struct PipedLaunch {
    /// The program file, as an absolute path: `CreateProcessW` runs it as named and searches no folder for it. A
    /// `.cmd` or `.bat` file is refused; the caller runs one through cmd.exe itself.
    pub application: PathBuf,
    /// The command line, as the program reads it, without a terminating NUL.
    pub command_line: Vec<u16>,
    /// The working folder.
    pub directory: PathBuf,
    /// The command's whole environment.
    pub environment: EnvironmentBlock,
    /// The console the command is created on.
    pub console: PipedConsole,
}

/// The console a [`PipedLaunch`] creates its command on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipedConsole {
    /// A hidden console of its own (`CREATE_NO_WINDOW`), whose code pages are the system's OEM ones.
    Hidden,
    /// The console this process is attached to, which the command inherits, such as a [`crate::Utf8Console`].
    Attached,
}

/// A command [`PipedLaunch::start_suspended`] created: the suspended process, and the read ends of its two pipes.
/// Each read end reaches its end once every process holding the pipe's write end has closed it or exited.
pub struct PipedSuspended {
    /// The command, not yet in its job and not yet run.
    pub suspended: Suspended,
    /// The read end of the command's stdout.
    pub stdout: File,
    /// The read end of the command's stderr.
    pub stderr: File,
}

impl PipedLaunch {
    /// Creates the command suspended, in its working folder with its environment, outside this process's job
    /// (`CREATE_BREAKAWAY_FROM_JOB`, which fails when that job refuses breakaway) and on the console
    /// [`PipedLaunch::console`] names: a hidden one of its own (`CREATE_NO_WINDOW`), or this process's. Its stdin is
    /// `NUL` and its stdout and stderr the write ends of two new pipes; it inherits exactly those three handles and one
    /// to `job`, opened by the job's name with query access only, so the name lasts as long as the command runs. A job
    /// made by [`Job::create_kill_on_close`] is the exception: the command inherits no handle to it, as one would keep
    /// the job open, and the command alive, after this process's last handle to it is closed.
    /// This process's copies of those handles are closed before the call returns; the read ends are never
    /// inheritable. The command is not yet in `job`: the caller puts it there with [`Suspended::assign_and_resume`].
    ///
    /// This process's own handles to those four are never inheritable: the command inherits copies open only while the
    /// spawn lock is held, so no process started meanwhile through [`crate::spawn`] or another launch inherits them.
    ///
    /// # Errors
    ///
    /// [`LaunchError::CreateProcess`] when `CreateProcessW` fails; [`LaunchError::Setup`] when the launch is refused
    /// (an application that is relative or a batch file, or a NUL in the application, command line or working folder)
    /// or a handle, a pipe or the attribute list cannot be made.
    pub fn start_suspended(&self, job: &Job) -> Result<PipedSuspended, LaunchError> {
        self.created(job, CREATE_BREAKAWAY_FROM_JOB, nul()?)
    }

    /// Creates the command suspended as [`PipedLaunch::start_suspended`] does, but inside this process's job rather
    /// than outside it: once [`Suspended::assign_and_resume`] puts it in `job` too, it runs in `job` nested in this
    /// process's job, so a job that ends this process's tree ends it too, and a job that refuses breakaway takes it.
    ///
    /// # Errors
    ///
    /// As [`PipedLaunch::start_suspended`].
    pub fn start_suspended_within(&self, job: &Job) -> Result<PipedSuspended, LaunchError> {
        self.created(job, 0, nul()?)
    }

    /// Creates the command suspended as [`PipedLaunch::start_suspended_within`] does, but with its stdin the read end
    /// of a third new pipe, whose write end the returned [`PipedFed`] holds: the command reads what is written there,
    /// and reaches the end of its input once that end is dropped.
    ///
    /// # Errors
    ///
    /// As [`PipedLaunch::start_suspended`].
    pub fn start_suspended_within_fed(&self, job: &Job) -> Result<PipedFed, LaunchError> {
        let (input, stdin) = pipe_to_child("stdin")?;
        let PipedSuspended { suspended, stdout, stderr } = self.created(job, 0, input)?;
        Ok(PipedFed {
            suspended,
            stdin,
            stdout,
            stderr,
        })
    }

    /// Creates the command suspended with the creation flag `breakaway`, `CREATE_BREAKAWAY_FROM_JOB` or none, its stdin
    /// `input`, a handle the caller made, which is closed here once the command holds its own copy.
    fn created(&self, job: &Job, breakaway: PROCESS_CREATION_FLAGS, input: OwnedHandle) -> Result<PipedSuspended, LaunchError> {
        check_launch(&self.command_line, &self.application, &[("working folder", self.directory.as_path())])?;
        let job_handle = if job.kills_on_close() { None } else { Some(open_job(job)?) };
        let others: Vec<(&OwnedHandle, Option<u32>)> = job_handle.iter().map(|handle| (handle, Some(JOB_OBJECT_QUERY))).collect();
        let (stdout, output) = pipe_from_child("stdout")?;
        let (stderr, errors) = pipe_from_child("stderr")?;
        let creation = Creation {
            application: &self.application,
            command_line: &self.command_line,
            directory: &self.directory,
            environment: &self.environment,
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &others,
        };
        let flags = match self.console {
            PipedConsole::Hidden => breakaway | CREATE_NO_WINDOW,
            PipedConsole::Attached => breakaway,
        };
        let suspended = create_inheriting(&creation, flags, &inherited, None)?;
        // The command holds its own copies now; with these closed, each read end ends when the command's copies do.
        drop((input, output, errors, job_handle));
        Ok(PipedSuspended { suspended, stdout, stderr })
    }
}

/// A handle to `NUL` for a command's stdin.
fn nul() -> Result<OwnedHandle, LaunchError> {
    open_shared(Path::new("NUL"), GENERIC_READ, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL)
}

/// A command [`PipedLaunch::start_suspended_within_fed`] created: the suspended process, the write end of its stdin
/// and the read ends of its stdout and stderr.
pub struct PipedFed {
    /// The command, not yet in its job and not yet run.
    pub suspended: Suspended,
    /// The write end of the command's stdin: the command reads to its end once this is dropped.
    pub stdin: File,
    /// The read end of the command's stdout.
    pub stdout: File,
    /// The read end of the command's stderr.
    pub stderr: File,
}

/// Whether this process's own job refuses breakaway, so that [`PipedLaunch::start_suspended`] cannot create a command
/// outside it: true when the process is in a job whose limits allow neither breakaway nor silent breakaway.
///
/// # Errors
///
/// The system's error when the process's job or its limits cannot be read.
pub fn breakaway_refused() -> io::Result<bool> {
    let mut in_job = 0;
    // SAFETY: the pseudo-handle of this process is always open; a null job asks about any job; `in_job` is writable.
    if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &raw mut in_job) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_job == 0 {
        return Ok(false);
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let size = struct_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
    // SAFETY: a null job handle reads the job of this process; `limits` is writable for `size` bytes and no returned
    // length is asked for.
    let read = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let allowed = JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    Ok(limits.BasicLimitInformation.LimitFlags & allowed == 0)
}

/// A second handle to `job`, never inheritable, opened by its name with query access only: the command's copy keeps
/// the name alive and lets it do nothing else to the job.
fn open_job(job: &Job) -> Result<OwnedHandle, LaunchError> {
    let name = wide(job.name());
    // SAFETY: `name` is NUL-terminated and outlives the call. A success is a fresh handle nothing else owns.
    unsafe { own_handle(OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr())) }
        .map_err(|error| setup(error, &format!("open the job {} for the command to inherit", job.name())))
}

/// A new pipe the command writes its `stream` to, neither end inheritable: the read end, as a file this process reads,
/// and the write end, for the command.
pub(crate) fn pipe_from_child(stream: &str) -> Result<(File, OwnedHandle), LaunchError> {
    let operation = format!("create the {stream} pipe");
    let (read, write) = new_pipe(&operation)?;
    // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
    let reader = File::from(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(read) });
    // SAFETY: as above, for the write end.
    let writer = unsafe { own_handle(write) }.map_err(|error| setup(error, &operation))?;
    Ok((reader, writer))
}

/// A new pipe the command reads its `stream` from, neither end inheritable: the read end, for the command, and the
/// write end, as a file this process writes.
pub(crate) fn pipe_to_child(stream: &str) -> Result<(OwnedHandle, File), LaunchError> {
    let operation = format!("create the {stream} pipe");
    let (read, write) = new_pipe(&operation)?;
    // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
    let writer = File::from(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(write) });
    // SAFETY: as above, for the read end.
    let reader = unsafe { own_handle(read) }.map_err(|error| setup(error, &operation))?;
    Ok((reader, writer))
}

/// The read and write ends of a new anonymous pipe, neither inheritable, for the caller to own; `operation` names the
/// failure.
fn new_pipe(operation: &str) -> Result<(HANDLE, HANDLE), LaunchError> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both handle pointers are writable; no security attributes are passed, so neither end is inheritable,
    // and size 0 asks for the default buffer.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
        return Err(setup(io::Error::last_os_error(), operation));
    }
    Ok((read, write))
}

/// Cancels every read and write this process has pending on the open file `file` is a handle to, whichever thread
/// issued it and through whichever handle to that file: each ends with an error [`cancelled`] knows. `false` when
/// none was pending.
///
/// # Errors
///
/// The system's error when the requests cannot be cancelled.
pub fn cancel_pending_io(file: &File) -> io::Result<bool> {
    // SAFETY: the handle is open for the borrow's length; with no OVERLAPPED every request on it is cancelled.
    if unsafe { CancelIoEx(file.as_raw_handle(), std::ptr::null()) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NOT_FOUND.cast_signed()) {
        return Ok(false);
    }
    Err(error)
}

/// Whether `error` ended a read or write [`cancel_pending_io`] cancelled.
#[must_use]
pub fn cancelled(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make or use its pipe must fail loudly")]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use std::thread;
    use std::time::Duration;

    use super::{cancel_pending_io, cancelled, new_pipe};

    /// A new anonymous pipe's read and write ends.
    fn pipe() -> (File, File) {
        let (read, write) = new_pipe("create the test pipe").expect("a pipe can be made");
        // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
        unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) }
    }

    #[test]
    fn cancelling_a_copy_ends_a_read_another_thread_waits_in() {
        let (mut reader, _writer) = pipe();
        let copy = reader.try_clone().expect("the read end can be copied");
        assert!(!cancel_pending_io(&copy).expect("an idle pipe can be cancelled"), "nothing was pending");

        let blocked = thread::spawn(move || reader.read(&mut [0_u8; 16]));
        // The read waits on an empty pipe whose write end stays open; cancel until it has begun and been cut short.
        while !blocked.is_finished() {
            cancel_pending_io(&copy).expect("the read can be cancelled");
            thread::sleep(Duration::from_millis(1));
        }

        let error = blocked.join().expect("the reader returns").expect_err("a cancelled read fails");
        assert!(cancelled(&error), "{error:?}");
    }
}

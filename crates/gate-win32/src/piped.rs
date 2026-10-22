//! Commands created suspended with their output piped back: stdin from `NUL`, or fed through a third pipe, stdout and stderr into two anonymous
//! pipes whose read ends stay with the caller, outside the caller's own job or within it, and on a hidden console of their own or the
//! caller's, then put in a job, named or anonymous, and resumed. The command also inherits a handle to that job, so the job's name
//! lasts while the command runs even after the caller is gone; unless the job kills on close, which a command holding a
//! handle to it would keep open.

use std::fs::File;
use std::io;
use std::os::windows::io::{BorrowedHandle, FromRawHandle, IntoRawHandle};
use std::path::PathBuf;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, GetCurrentProcess, NORMAL_PRIORITY_CLASS, PROCESS_CREATION_FLAGS,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;

use crate::create::{Creation, DesktopPolicy, Inherited, check_launch, create_inheriting_on};
use crate::launch::{nul, setup, struct_size};
use crate::{EnvironmentBlock, Job, LaunchError, RestrictedToken, Suspended, own_handle, wide};

/// How to start a command whose output the caller reads: the caller decides every part of it.
pub struct PipedLaunch {
    /// The program file, as an absolute path: `CreateProcessW` (or `CreateProcessAsUserW`, given a token) runs it as
    /// named and searches no folder for it. A
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
    /// The token the command runs with, through `CreateProcessAsUserW`; `None` runs it with this process's own token,
    /// through `CreateProcessW`.
    pub token: Option<RestrictedToken>,
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
    /// [`LaunchError::CreateProcess`] when `CreateProcessW` (or `CreateProcessAsUserW`, for a command given a token)
    /// fails; [`LaunchError::Setup`] when the launch is refused
    /// (an application that is relative or a batch file, or a NUL in the application, command line or working folder)
    /// or a handle, a pipe or the attribute list cannot be made.
    pub fn start_suspended(&self, job: &Job) -> Result<PipedSuspended, LaunchError> {
        self.created(job, CREATE_BREAKAWAY_FROM_JOB, nul(GENERIC_READ)?)
    }

    /// Creates the command suspended as [`PipedLaunch::start_suspended`] does, but inside this process's job rather
    /// than outside it: once [`Suspended::assign_and_resume`] puts it in `job` too, it runs in `job` nested in this
    /// process's job, so a job that ends this process's tree ends it too, and a job that refuses breakaway takes it.
    ///
    /// # Errors
    ///
    /// As [`PipedLaunch::start_suspended`].
    pub fn start_suspended_within(&self, job: &Job) -> Result<PipedSuspended, LaunchError> {
        self.created(job, 0, nul(GENERIC_READ)?)
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
    /// `input`, a handle the caller made, which is closed here once the command holds its own copy. It is created with
    /// the desktop-app policy `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE`, as
    /// [`crate::Launch::start_suspended`] creates its command, so a command that is a packaged app, such as the Store's
    /// pwsh, starts its children inside its package, and so inside the job it is put in. A system that cannot set the
    /// policy (before Windows 10 1703) gets the command without it, and [`Suspended::note`] says so, rather than a
    /// failed launch.
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
            token: self.token.as_ref(),
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &others,
        };
        let flags = match self.console {
            PipedConsole::Hidden => breakaway | CREATE_NO_WINDOW,
            PipedConsole::Attached => breakaway,
        };
        let policy = DesktopPolicy::Wanted(PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE);
        let suspended = create_inheriting_on(&creation, flags, &inherited, policy, None)?;
        // The command holds its own copies now; with these closed, each read end ends when the command's copies do.
        drop((input, output, errors, job_handle));
        Ok(PipedSuspended { suspended, stdout, stderr })
    }
}

/// The window station and desktop a [`DesktopLaunch`] creates its command on: the interactive one.
const INTERACTIVE_DESKTOP: &str = r"WinSta0\Default";

/// How the desktop helper starts a command on the interactive desktop of its session, its output going to two pipe
/// write ends it was handed: the caller decides every part of it.
pub struct DesktopLaunch {
    /// The program file, as an absolute path: `CreateProcessW` runs it as named and searches no folder for it. A
    /// `.cmd` or `.bat` file is refused; the caller runs one through cmd.exe itself.
    pub application: PathBuf,
    /// The command line, as the program reads it, without a terminating NUL.
    pub command_line: Vec<u16>,
    /// The working folder.
    pub directory: PathBuf,
    /// The command's whole environment.
    pub environment: EnvironmentBlock,
}

impl DesktopLaunch {
    /// Creates the command suspended, with this process's token, in its working folder with its environment, on the
    /// interactive desktop (`lpDesktop` `WinSta0\Default`) and a hidden console of its own (`CREATE_NO_WINDOW`), in the
    /// normal priority class (`NORMAL_PRIORITY_CLASS`) whatever this process's class is, and inside this process's job:
    /// no breakaway is asked for. Its stdin is `NUL`, and its stdout and stderr `stdout` and `stderr`, which stay open
    /// and this process's; it inherits exactly those three handles, through copies open only while the spawn lock is
    /// held, as [`PipedLaunch::start_suspended`]'s command does. It is created with the desktop-app policy
    /// `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE` where the system can set it, and without it,
    /// [`Suspended::note`] saying so, before Windows 10 1703.
    ///
    /// # Errors
    ///
    /// [`LaunchError::CreateProcess`] when `CreateProcessW` fails; [`LaunchError::Setup`] when the launch is refused
    /// (an application that is relative or a batch file, or a NUL in the application, command line or working folder)
    /// or a handle or the attribute list cannot be made.
    pub fn start_suspended(&self, stdout: BorrowedHandle<'_>, stderr: BorrowedHandle<'_>) -> Result<Suspended, LaunchError> {
        check_launch(&self.command_line, &self.application, &[("working folder", self.directory.as_path())])?;
        let input = nul(GENERIC_READ)?;
        let output = own_copy(stdout, "stdout")?;
        let errors = own_copy(stderr, "stderr")?;
        let creation = Creation {
            application: &self.application,
            command_line: &self.command_line,
            directory: &self.directory,
            environment: &self.environment,
            token: None,
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &[],
        };
        let policy = DesktopPolicy::Wanted(PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE);
        let flags = CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS;
        let suspended = create_inheriting_on(&creation, flags, &inherited, policy, Some(INTERACTIVE_DESKTOP))?;
        // The command holds its own copies now; the caller's handles stay open.
        drop((input, output, errors));
        Ok(suspended)
    }
}

/// A copy of `handle`, never inheritable, that this process owns, for the command's `stream`.
fn own_copy(handle: BorrowedHandle<'_>, stream: &str) -> Result<OwnedHandle, LaunchError> {
    let operation = format!("copy the {stream} handle for the command");
    let copy = handle.try_clone_to_owned().map_err(|error| setup(error, &operation))?;
    // SAFETY: `into_raw_handle` gives up the std guard's ownership, so the new guard alone owns the open copy.
    unsafe { own_handle(copy.into_raw_handle()) }.map_err(|error| setup(error, &operation))
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
    pipe_from_child_sized(stream, 0)
}

/// A new pipe the command writes its `stream` to, as [`pipe_from_child`] makes it, with a buffer of about `buffer`
/// bytes, the system's default for 0.
pub(crate) fn pipe_from_child_sized(stream: &str, buffer: u32) -> Result<(File, OwnedHandle), LaunchError> {
    let operation = format!("create the {stream} pipe");
    let (read, write) = new_pipe(&operation, buffer)?;
    // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
    let reader = File::from(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(read) });
    // SAFETY: as above, for the write end.
    let writer = unsafe { own_handle(write) }.map_err(|error| setup(error, &operation))?;
    Ok((reader, writer))
}

/// The two anonymous pipes a command's stdout and stderr go into when another process creates it with them, as the
/// desktop helper does: the read ends, as files this process reads, and the write ends, for the other process to be
/// given. No end is inheritable.
#[derive(Debug)]
pub struct OutputPipes {
    /// The stdout pipe's read end.
    pub stdout_read: File,
    /// The stdout pipe's write end.
    pub stdout_write: std::os::windows::io::OwnedHandle,
    /// The stderr pipe's read end.
    pub stderr_read: File,
    /// The stderr pipe's write end.
    pub stderr_write: std::os::windows::io::OwnedHandle,
}

/// Two new anonymous pipes, one for a command's stdout and one for its stderr, as [`OutputPipes`] holds them.
///
/// # Errors
///
/// [`LaunchError::Setup`] naming the pipe that could not be made, with the system's error.
pub fn output_pipes() -> Result<OutputPipes, LaunchError> {
    let (stdout_read, stdout_write) = output_pipe("stdout")?;
    let (stderr_read, stderr_write) = output_pipe("stderr")?;
    Ok(OutputPipes {
        stdout_read,
        stdout_write,
        stderr_read,
        stderr_write,
    })
}

/// A new pipe for the `stream` of a command another process creates, neither end inheritable: the read end, as a file
/// this process reads, and the write end.
fn output_pipe(stream: &str) -> Result<(File, std::os::windows::io::OwnedHandle), LaunchError> {
    let (reader, writer) = pipe_from_child(stream)?;
    let writer = std::mem::ManuallyDrop::new(writer).raw();
    // SAFETY: the crate's guard of the write end was given up above without closing it, so the std guard made here
    // alone owns the open handle.
    Ok((reader, unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(writer) }))
}

/// A new pipe the command reads its `stream` from, neither end inheritable: the read end, for the command, and the
/// write end, as a file this process writes.
pub(crate) fn pipe_to_child(stream: &str) -> Result<(OwnedHandle, File), LaunchError> {
    let operation = format!("create the {stream} pipe");
    let (read, write) = new_pipe(&operation, 0)?;
    // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
    let writer = File::from(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(write) });
    // SAFETY: as above, for the read end.
    let reader = unsafe { own_handle(read) }.map_err(|error| setup(error, &operation))?;
    Ok((reader, writer))
}

/// The read and write ends of a new anonymous pipe, neither inheritable, for the caller to own, its buffer about
/// `buffer` bytes (`CreatePipe`'s suggested size; 0 for the system's default); `operation` names the failure.
fn new_pipe(operation: &str, buffer: u32) -> Result<(HANDLE, HANDLE), LaunchError> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both handle pointers are writable; no security attributes are passed, so neither end is inheritable.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), buffer) } == 0 {
        return Err(setup(io::Error::last_os_error(), operation));
    }
    Ok((read, write))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose pipe cannot be made must fail loudly")]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};

    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

    use super::output_pipes;

    /// Whether `handle` is inheritable.
    fn inheritable(handle: BorrowedHandle<'_>) -> bool {
        let mut flags = 0;
        // SAFETY: the handle is borrowed open for the whole call and `flags` is writable.
        let read = unsafe { GetHandleInformation(handle.as_raw_handle(), &raw mut flags) };
        assert_ne!(read, 0, "the handle's flags are read: {}", std::io::Error::last_os_error());
        flags & HANDLE_FLAG_INHERIT != 0
    }

    #[test]
    fn output_pipes_carry_what_is_written_to_each_write_end_to_its_read_end_and_none_is_inheritable() {
        let mut pipes = output_pipes().expect("the pipes are made");
        for handle in [
            pipes.stdout_read.as_handle(),
            pipes.stdout_write.as_handle(),
            pipes.stderr_read.as_handle(),
            pipes.stderr_write.as_handle(),
        ] {
            assert!(!inheritable(handle), "no end is inheritable");
        }

        File::from(pipes.stdout_write).write_all(b"out").expect("stdout is written");
        File::from(pipes.stderr_write).write_all(b"err").expect("stderr is written");

        let (mut out, mut err) = (String::new(), String::new());
        pipes.stdout_read.read_to_string(&mut out).expect("stdout is read to its end");
        pipes.stderr_read.read_to_string(&mut err).expect("stderr is read to its end");
        assert_eq!((out.as_str(), err.as_str()), ("out", "err"), "each pipe carries its own stream");
    }
}

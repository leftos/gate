//! Commands created suspended with their output piped back: stdin from `NUL`, stdout and stderr into two anonymous
//! pipes whose read ends stay with the caller, outside the caller's own job and on a hidden console of their own or the
//! caller's, then put in a named job and resumed. The command also inherits a handle to that job, so the job's name
//! lasts while the command runs even after the caller is gone.

use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING};
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, GetCurrentProcess};

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
    /// to `job`, opened by the job's name with query access only, so the name lasts as long as the command runs.
    /// This process's copies of those four handles are closed before the call returns; the read ends are never
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
        check_launch(&self.command_line, &self.application, &[("working folder", self.directory.as_path())])?;
        let job_handle = open_job(job)?;
        let input = open_shared(Path::new("NUL"), GENERIC_READ, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL)?;
        let (stdout, output) = pipe("stdout")?;
        let (stderr, errors) = pipe("stderr")?;
        let creation = Creation {
            application: &self.application,
            command_line: &self.command_line,
            directory: &self.directory,
            environment: &self.environment,
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &[(&job_handle, Some(JOB_OBJECT_QUERY))],
        };
        let flags = match self.console {
            PipedConsole::Hidden => CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW,
            PipedConsole::Attached => CREATE_BREAKAWAY_FROM_JOB,
        };
        let suspended = create_inheriting(&creation, flags, &inherited, None)?;
        // The command holds its own copies now; with these closed, each read end ends when the command's copies do.
        drop((input, output, errors, job_handle));
        Ok(PipedSuspended { suspended, stdout, stderr })
    }
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

/// A new pipe for the command's `stream`, neither end inheritable: the read end, as a file this process reads, and
/// the write end, for the command.
fn pipe(stream: &str) -> Result<(File, OwnedHandle), LaunchError> {
    let operation = format!("create the {stream} pipe");
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both handle pointers are writable; no security attributes are passed, so neither end is inheritable,
    // and size 0 asks for the default buffer.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
        return Err(setup(io::Error::last_os_error(), &operation));
    }
    // SAFETY: a success returns two fresh handles nothing else owns; each gets exactly one owner here.
    let reader = File::from(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(read) });
    // SAFETY: as above, for the write end.
    let writer = unsafe { own_handle(write) }.map_err(|error| setup(error, &operation))?;
    Ok((reader, writer))
}

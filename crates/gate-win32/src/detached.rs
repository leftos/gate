//! A command started to outlive its starter: it inherits no handle but three of its own to `NUL`, runs on a hidden
//! console of its own in a process group of its own, and is resumed in no job of this process's making. It leaves
//! every job of this process's that lets it break away; whether any job still holds it, one that refused it or one
//! above a job it left, is read while it is still suspended, and the result says so.

use std::io;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, PROCESS_CREATION_FLAGS};

use crate::create::{Creation, DesktopPolicy, Inherited, check_launch, create_inheriting_on};
use crate::launch::{nul, setup};
use crate::{EnvironmentBlock, LaunchError, Running, Suspended};

/// How to start a command that outlives this process and keeps nothing of it open: the caller decides every part of
/// it.
pub struct DetachedLaunch {
    /// The program file, as an absolute path: `CreateProcessW` runs it as named and searches no folder for it. A
    /// `.cmd` or `.bat` file is refused.
    pub application: PathBuf,
    /// The command line, as the program reads it, without a terminating NUL.
    pub command_line: Vec<u16>,
    /// The working folder.
    pub directory: PathBuf,
    /// The command's whole environment.
    pub environment: EnvironmentBlock,
}

/// A command [`DetachedLaunch::start`] started, running, and whether a job holds it.
pub enum Detached {
    /// In no job: it lives on whatever job this process's tree is ended by.
    OutsideJob(Running),
    /// In a job this process is in too, one that refused it breakaway or one above a job it left: it ends if that job
    /// ends its processes.
    InsideJob(Running),
}

impl Detached {
    /// The command, running.
    #[must_use]
    pub fn running(&self) -> &Running {
        match self {
            Detached::OutsideJob(running) | Detached::InsideJob(running) => running,
        }
    }
}

impl DetachedLaunch {
    /// Starts the command in its working folder with its environment, on a hidden console of its own
    /// (`CREATE_NO_WINDOW`) and in a process group of its own (`CREATE_NEW_PROCESS_GROUP`), created suspended and then
    /// resumed without being put in any job. Its stdin, stdout and stderr are three handles to `NUL` opened for it, and
    /// it inherits exactly those three, through copies open only while the spawn lock is held, so it holds no pipe,
    /// file or job of this process's. It asks first to be created outside this process's job
    /// (`CREATE_BREAKAWAY_FROM_JOB`), which leaves only the jobs that allow breakaway; when that creation is refused
    /// with access denied, as a job that refuses breakaway refuses it, it is created again inside the job. Before the
    /// resume, whether any job holds it is read (`IsProcessInJob`), and the result says so, whichever creation
    /// succeeded.
    ///
    /// # Errors
    ///
    /// [`LaunchError::CreateProcess`] when `CreateProcessW` fails for another reason, or fails inside the job too;
    /// [`LaunchError::Setup`] when the launch is refused (an application that is relative or a batch file, or a NUL in
    /// the application, command line or working folder), a handle or the attribute list cannot be made, whether a job
    /// holds the command cannot be read, or the command cannot be resumed; a created command is ended first.
    pub fn start(&self) -> Result<Detached, LaunchError> {
        check_launch(&self.command_line, &self.application, &[("working folder", self.directory.as_path())])?;
        let suspended = match self.created(CREATE_BREAKAWAY_FROM_JOB) {
            Ok(suspended) => suspended,
            Err(LaunchError::CreateProcess {
                code: ERROR_ACCESS_DENIED, ..
            }) => self.created(0)?,
            Err(error) => return Err(error),
        };
        // A failed read drops the command, which ends it.
        let held = in_any_job(&suspended)?;
        let running = suspended.resume()?;
        Ok(if held {
            Detached::InsideJob(running)
        } else {
            Detached::OutsideJob(running)
        })
    }

    /// Creates the command suspended as [`DetachedLaunch::start`] describes, with the creation flag `breakaway`,
    /// `CREATE_BREAKAWAY_FROM_JOB` or none.
    pub(crate) fn created(&self, breakaway: PROCESS_CREATION_FLAGS) -> Result<Suspended, LaunchError> {
        let input = nul(GENERIC_READ)?;
        let output = nul(GENERIC_WRITE)?;
        let errors = nul(GENERIC_WRITE)?;
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
        let flags = breakaway | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        let suspended = create_inheriting_on(&creation, flags, &inherited, DesktopPolicy::Absent, None)?;
        // The command holds its own copies now.
        drop((input, output, errors));
        Ok(suspended)
    }
}

/// Whether any job holds the suspended command.
fn in_any_job(suspended: &Suspended) -> Result<bool, LaunchError> {
    let operation = "read whether a job holds the command";
    let Some(process) = &suspended.process else {
        return Err(setup(io::Error::other("the process was already taken"), operation));
    };
    let mut in_job = 0;
    // SAFETY: the handle is open for the whole call, with the query access a created process's handle has; a null job
    // asks about any job; `in_job` is writable.
    if unsafe { IsProcessInJob(process.raw(), std::ptr::null_mut(), &raw mut in_job) } == 0 {
        return Err(setup(io::Error::last_os_error(), operation));
    }
    Ok(in_job != 0)
}

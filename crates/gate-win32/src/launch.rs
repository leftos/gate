//! Commands created suspended, with an explicit environment and exactly three inherited handles (stdin from `NUL`,
//! stdout to the log and stderr to the log's `.err` file; or stdin and stdout each a pipe this process holds the other
//! end of, and stderr this process's own), then put in a job and resumed, so nothing they start runs outside the job.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    GetFileInformationByHandle, GetFileType, OPEN_EXISTING, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetExitCodeProcess, HIGH_PRIORITY_CLASS, OpenProcess, PROCESS_CREATION_FLAGS, PROCESS_TERMINATE,
    ResumeThread, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;

use crate::create::{Creation, Inherited, check_launch, create_inheriting};
use crate::interrupt::{Woken, wait_again_with, wait_with};
use crate::piped::{pipe_from_child, pipe_to_child};
use crate::{Interrupt, Job, own_handle, wait_millis, win32_message};

/// A Unicode environment block, its entries sorted by name as `CreateProcessW` wants them.
#[derive(Clone, Debug)]
pub struct EnvironmentBlock(pub(crate) Vec<u16>);

impl EnvironmentBlock {
    /// The block of `vars`, `(name, value)` pairs. Names are compared as Windows compares them, ordinally and without
    /// regard to case over all of Unicode, and a later pair replaces an earlier one of the same name, so the gate's own
    /// environment followed by the overrides gives the overridden environment. The entries are sorted in that order.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] for an empty name, a name holding `=` past its first
    /// character, a name or value holding a NUL, or a name too long to compare, none of which a block can carry.
    pub fn new(vars: impl IntoIterator<Item = (OsString, OsString)>) -> io::Result<EnvironmentBlock> {
        let mut entries = Vec::new();
        for (name, value) in vars {
            let name: Vec<u16> = name.encode_wide().collect();
            let value: Vec<u16> = value.encode_wide().collect();
            check_entry(&name, &value)?;
            entries.push((name, value));
        }
        // A stable sort keeps the pairs of one name in the order given, so the last of them is the one kept.
        entries.sort_by(|left, right| compare_names(&left.0, &right.0));
        let mut kept: Vec<(Vec<u16>, Vec<u16>)> = Vec::with_capacity(entries.len());
        for entry in entries {
            match kept.last_mut() {
                Some(last) if compare_names(&last.0, &entry.0) == Ordering::Equal => *last = entry,
                _ => kept.push(entry),
            }
        }
        let mut block = Vec::new();
        for (name, value) in kept {
            block.extend(name);
            block.push(u16::from(b'='));
            block.extend(value);
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        Ok(EnvironmentBlock(block))
    }
}

/// Refuses a `(name, value)` pair a block cannot carry.
fn check_entry(name: &[u16], value: &[u16]) -> io::Result<()> {
    let equals = u16::from(b'=');
    let problem = if name.is_empty() {
        Some("an empty name")
    } else if name[1..].contains(&equals) {
        Some("a name holding `=`")
    } else if name.contains(&0) || value.contains(&0) {
        Some("a NUL")
    } else if i32::try_from(name.len()).is_err() {
        Some("a name this long")
    } else {
        None
    };
    match problem {
        Some(problem) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("an environment block cannot carry {problem}: {}", String::from_utf16_lossy(name)),
        )),
        None => Ok(()),
    }
}

/// Whether two environment variable names are one name to Windows, as [`EnvironmentBlock`] matches them: equal
/// ordinally without regard to case over all of Unicode.
#[must_use]
pub fn same_variable_name(left: &OsStr, right: &OsStr) -> bool {
    let left: Vec<u16> = left.encode_wide().collect();
    let right: Vec<u16> = right.encode_wide().collect();
    compare_names(&left, &right) == Ordering::Equal
}

/// The order of two names as Windows orders environment names: ordinal, without regard to case.
fn compare_names(left: &[u16], right: &[u16]) -> Ordering {
    // `check_entry` refused every name whose length does not fit an `i32`; a longer name is compared by its first
    // `i32::MAX` units, which stay within it.
    let left_len = i32::try_from(left.len()).unwrap_or(i32::MAX);
    let right_len = i32::try_from(right.len()).unwrap_or(i32::MAX);
    // SAFETY: each pointer is to as many readable units as its length says, both outliving the call.
    let result = unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) };
    match result {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        // The call fails only on invalid arguments, which the lengths checked above rule out; the units' own order
        // still gives a total order.
        _ => left.cmp(right),
    }
}

/// How to start a command: the caller decides every part of it.
pub struct Launch {
    /// The program file, as an absolute path: `CreateProcessW` runs it as named and searches no folder for it. A
    /// `.cmd` or `.bat` file is refused; the caller runs one through cmd.exe itself.
    pub application: PathBuf,
    /// The command line, as the program reads it, without a terminating NUL.
    pub command_line: Vec<u16>,
    /// The working folder.
    pub directory: PathBuf,
    /// The command's whole environment.
    pub environment: EnvironmentBlock,
    /// Where the command's stdin comes from and its stdout and stderr go.
    pub output: LaunchOutput,
    /// The priority class to create the command in; with `None` it gets `NORMAL`, or this process's class when that is
    /// below normal or idle, as `CreateProcessW` gives a command no class is asked for.
    pub priority: Option<PriorityClass>,
}

/// Where a command's stdin comes from and its stdout and stderr go.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchOutput {
    /// stdin from `NUL`, and stdout and stderr each to a file created afresh.
    Files {
        /// The file the command's stdout goes to.
        log: PathBuf,
        /// The file the command's stderr goes to: the log's `.err` file.
        err_log: PathBuf,
    },
    /// stdin and stdout each a new pipe whose other end this process keeps ([`Suspended::take_pipes`]), and stderr
    /// this process's own stderr as it stands when the command is created, or `NUL` when this process has none: the
    /// command's stderr lines reach whatever this process's own lines reach.
    Piped,
}

/// This process's ends of the pipes of a command launched with [`LaunchOutput::Piped`]; neither is inheritable.
#[derive(Debug)]
pub struct StdioPipes {
    /// The write end of the command's stdin: the command reads to its end once this is dropped.
    pub stdin: File,
    /// The read end of the command's stdout, which reaches its end once every process holding the write end has closed
    /// it or exited.
    pub stdout: File,
}

/// A priority class a command can be created in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PriorityClass {
    /// `BELOW_NORMAL_PRIORITY_CLASS`.
    BelowNormal,
    /// `HIGH_PRIORITY_CLASS`.
    High,
}

impl PriorityClass {
    /// The creation flag that asks for the class.
    fn flag(self) -> PROCESS_CREATION_FLAGS {
        match self {
            PriorityClass::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            PriorityClass::High => HIGH_PRIORITY_CLASS,
        }
    }
}

/// Why a command was not started, or not put in its job and resumed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// `CreateProcessW` refused the command: the program is missing or cannot run. Its text is the system's message
    /// alone, as the gate's `could not start` line prints it.
    #[error("{message}")]
    CreateProcess {
        /// The Win32 error code.
        code: u32,
        /// The system's message for `code`, as [`crate::win32_message`] gives it.
        message: String,
    },
    /// Any other step failed: checking the launch, opening a handle for the command, building its attributes,
    /// putting it in the job or resuming it.
    #[error("{operation}: {}", os_message(.error))]
    Setup {
        /// What was being done, such as `open X:\out.log`.
        operation: String,
        /// The system's error, or of kind [`io::ErrorKind::InvalidInput`] for a launch refused before any call.
        error: io::Error,
    },
}

/// The system's message for `error`, without the `(os error N)` suffix its own text carries.
fn os_message(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => win32_message(code.cast_unsigned()),
        None => error.to_string(),
    }
}

/// A [`LaunchError::Setup`] of `operation` with `error`.
pub(crate) fn setup(error: io::Error, operation: &str) -> LaunchError {
    LaunchError::Setup {
        operation: operation.to_owned(),
        error,
    }
}

/// A launch refused before any call, for `problem`.
pub(crate) fn refused(problem: String) -> LaunchError {
    setup(io::Error::new(io::ErrorKind::InvalidInput, problem), "check the launch")
}

impl Launch {
    /// Creates the command suspended, in its working folder with its environment and priority class, sharing this
    /// process's console and process group, so a console Ctrl+C reaches it as it reaches this process. With
    /// [`LaunchOutput::Piped`] its stdin and stdout are the command's ends of two new pipes, whose other ends the
    /// returned [`Suspended`] holds until [`Suspended::take_pipes`] takes them, and its stderr a copy of this process's
    /// own. With [`LaunchOutput::Files`] its stdin is `NUL`, its stdout the log and its stderr the `.err` file, each
    /// file created afresh (an earlier file at the path is deleted first, and a link at it is never followed) and open
    /// to every process for reading, writing and deleting while the command writes them, so a process the command left
    /// running cannot keep the next run from replacing them. Either way it inherits those three handles and no other.
    /// This process's own handles to them are never inheritable: the command inherits copies
    /// open only while the spawn lock is held, so no process started meanwhile through [`crate::spawn`] or another
    /// launch inherits them. It is created with the desktop-app policy
    /// `PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE`, as the PowerShell gate creates it: Microsoft's
    /// documentation says only that it matters to a Win32 app packaged through the Desktop Bridge, whose child
    /// processes it keeps inside the desktop app runtime environment, a policy its descendants inherit.
    ///
    /// # Errors
    ///
    /// [`LaunchError::CreateProcess`] when `CreateProcessW` fails; [`LaunchError::Setup`] when the launch is refused
    /// (an application that is relative or a batch file, or a NUL in the application, command line, working folder,
    /// log or `.err` path) or a handle or the attribute list cannot be made.
    pub fn start_suspended(&self) -> Result<Suspended, LaunchError> {
        let mut paths = vec![("working folder", self.directory.as_path())];
        if let LaunchOutput::Files { log, err_log } = &self.output {
            paths.extend([("log", log.as_path()), ("err log", err_log.as_path())]);
        }
        check_launch(&self.command_line, &self.application, &paths)?;
        let mut pipes = None;
        let (input, output, errors) = match &self.output {
            LaunchOutput::Files { log, err_log } => {
                let input = open_shared(Path::new("NUL"), GENERIC_READ, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL)?;
                (input, create_output(log)?, create_output(err_log)?)
            }
            LaunchOutput::Piped => {
                let (input, stdin) = pipe_to_child("stdin")?;
                let (stdout, output) = pipe_from_child("stdout")?;
                pipes = Some(StdioPipes { stdin, stdout });
                (input, output, own_stderr()?)
            }
        };
        let creation = Creation {
            application: &self.application,
            command_line: &self.command_line,
            directory: &self.directory,
            environment: &self.environment,
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &[],
        };
        let policy = PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;
        let mut suspended = create_inheriting(&creation, self.priority.map_or(0, PriorityClass::flag), &inherited, Some(policy))?;
        // The command holds its own copies of its ends now; with these closed, each pipe ends when the command's does.
        drop((input, output, errors));
        suspended.pipes = pipes;
        Ok(suspended)
    }
}

/// A copy of this process's stderr handle, never inheritable, or a handle to `NUL` when this process has no stderr.
fn own_stderr() -> Result<OwnedHandle, LaunchError> {
    let operation = "copy this process's stderr for the command";
    // SAFETY: `GetStdHandle` takes no pointers.
    let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if stderr.is_null() || stderr == INVALID_HANDLE_VALUE {
        return open_shared(Path::new("NUL"), GENERIC_WRITE, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL);
    }
    let mut copy: HANDLE = std::ptr::null_mut();
    // SAFETY: both process handles are this process's pseudo-handle, which is always open; the std handle stays open
    // for the whole call, as nothing in this crate closes it, and `copy` is writable. The copy is not inheritable.
    let copied = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            stderr,
            GetCurrentProcess(),
            &raw mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if copied == 0 {
        return Err(setup(io::Error::last_os_error(), operation));
    }
    // SAFETY: a success is a fresh handle nothing else owns.
    unsafe { own_handle(copy) }.map_err(|error| setup(error, operation))
}

/// Whether `CreateProcessW` would run `path` through cmd.exe: its name, less the trailing dots and spaces Windows
/// drops, ends in `.cmd` or `.bat`, in any case.
pub(crate) fn is_batch(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    Path::new(name.trim_end_matches(['.', ' ']))
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat"))
}

/// The size of `T`, as a Win32 struct's size field holds it.
#[expect(clippy::cast_possible_truncation, reason = "a Win32 struct is far smaller than 4 GiB")]
pub(crate) fn struct_size<T>() -> u32 {
    size_of::<T>() as u32
}

/// The string as a NUL-terminated UTF-16 buffer.
pub(crate) fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// A handle to `path`, never inheritable, open to every process for reading, writing and deleting.
pub(crate) fn open_shared(
    path: &Path,
    access: u32,
    disposition: FILE_CREATION_DISPOSITION,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> Result<OwnedHandle, LaunchError> {
    let name = wide_os(path.as_os_str());
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes are passed, so the handle is not
    // inheritable, and no template file is passed.
    let handle = unsafe { CreateFileW(name.as_ptr(), access, share, std::ptr::null(), disposition, flags, std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return Err(setup(error, &format!("open {}", path.display())));
    }
    // SAFETY: a success is a fresh handle nothing else owns.
    unsafe { own_handle(handle) }.map_err(|error| setup(error, &format!("open {}", path.display())))
}

/// A new file at `path` for the command to write: an earlier file there is deleted first (a link itself, never its
/// target), the file is created only if nothing is at the path then, without following a reparse point and without
/// letting a named pipe server impersonate this process, and it must be a plain disk file with one name.
fn create_output(path: &Path) -> Result<OwnedHandle, LaunchError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(setup(error, &format!("delete the earlier {}", path.display()))),
    }
    let flags = FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS;
    let handle = open_shared(path, GENERIC_WRITE, CREATE_NEW, flags)?;
    check_plain_file(handle.raw()).map_err(|error| setup(error, &format!("check {}", path.display())))?;
    Ok(handle)
}

/// Refuses a handle that is not to a disk file, is to a reparse point, or is to a file with more than one name. The
/// caller keeps `handle` open for the whole call.
pub(crate) fn check_plain_file(handle: HANDLE) -> io::Result<()> {
    // SAFETY: the handle is open for the whole call.
    let kind = unsafe { GetFileType(handle) };
    if kind != FILE_TYPE_DISK {
        return Err(io::Error::other(format!("not a disk file (file type {kind})")));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for the whole call and `info` is a writable `BY_HANDLE_FILE_INFORMATION`.
    if unsafe { GetFileInformationByHandle(handle, &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("a reparse point, not a plain file"));
    }
    if info.nNumberOfLinks != 1 {
        return Err(io::Error::other(format!("a file with {} names, not one", info.nNumberOfLinks)));
    }
    Ok(())
}

/// A command created suspended: it has not run a single instruction until [`Suspended::assign_and_resume`]. Dropped
/// without that, it is terminated with exit code 1, so a gate that fails before the resume leaves no suspended process.
pub struct Suspended {
    /// The process, until [`Suspended::assign_and_resume`] takes it.
    pub(crate) process: Option<OwnedHandle>,
    pub(crate) thread: OwnedHandle,
    pub(crate) pid: u32,
    /// This process's ends of the command's pipes, for a [`LaunchOutput::Piped`] launch, until taken.
    pub(crate) pipes: Option<StdioPipes>,
}

impl Suspended {
    /// The command's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// This process's ends of the command's stdin and stdout: `Some` once for a [`LaunchOutput::Piped`] launch, else
    /// `None`. Ends not taken close when the command is resumed or dropped.
    pub fn take_pipes(&mut self) -> Option<StdioPipes> {
        self.pipes.take()
    }

    /// Puts the command in `job`, then lets it run, so nothing it starts can run outside the job.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Setup`] when the command cannot be put in the job or resumed; it is terminated with exit code 1
    /// first.
    pub fn assign_and_resume(self, job: &Job) -> Result<Running, LaunchError> {
        self.assign(job)?;
        self.resume()
    }

    /// Puts the command in `job`, still suspended; [`Suspended::resume`] lets it run.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Setup`] when the command cannot be put in the job; it is terminated with exit code 1 once dropped.
    pub fn assign(&self, job: &Job) -> Result<(), LaunchError> {
        let Some(process) = &self.process else {
            return Err(setup(io::Error::other("the process was already taken"), "put the command in its job"));
        };
        job.assign(process)
            .map_err(|error| setup(error, &format!("put the command in the job {}", job.name())))
    }

    /// Lets the command run, once [`Suspended::assign`] has put it in its job.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Setup`] when the command cannot be resumed; it is terminated with exit code 1 first.
    pub fn resume(mut self) -> Result<Running, LaunchError> {
        let Some(process) = self.process.take() else {
            return Err(setup(io::Error::other("the process was already taken"), "resume the command"));
        };
        // SAFETY: the thread handle is open for the whole call, with the `THREAD_SUSPEND_RESUME` access a created
        // thread's handle has.
        if unsafe { ResumeThread(self.thread.raw()) } == u32::MAX {
            let error = io::Error::last_os_error();
            end_unstarted(&process);
            return Err(setup(error, "resume the command"));
        }
        Ok(Running { process, pid: self.pid })
    }
}

impl Drop for Suspended {
    fn drop(&mut self) {
        if let Some(process) = &self.process {
            end_unstarted(process);
        }
    }
}

/// Ends a command that never ran with exit code 1. Its own failure is not reported: it runs on a path that is already
/// failing or dropping, which reports its own error; a command it could not end stays suspended, never having run.
pub(crate) fn end_unstarted(process: &OwnedHandle) {
    // SAFETY: the process handle is open for the whole call, with the `PROCESS_TERMINATE` access a created process's
    // handle has.
    unsafe { TerminateProcess(process.raw(), 1) };
}

/// Ends the never-run command `pid` when the handle `CreateProcessW` returned for it could not be owned. The caller
/// keeps the command's thread handle open meanwhile, so the pid cannot name another process.
pub(crate) fn end_unstarted_by_id(pid: u32) {
    // SAFETY: `OpenProcess` takes no pointers. A success is a fresh handle nothing else owns.
    if let Ok(process) = unsafe { own_handle(OpenProcess(PROCESS_TERMINATE, 0, pid)) } {
        end_unstarted(&process);
    }
}

/// A command resumed in its job.
pub struct Running {
    pub(crate) process: OwnedHandle,
    pub(crate) pid: u32,
}

impl Running {
    /// The command's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Waits at most `timeout` for the command to exit; its exit code once it has, `None` while it still runs.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails or the exit code cannot be read.
    pub fn wait(&self, timeout: Duration) -> io::Result<Option<u32>> {
        let process: HANDLE = self.process.raw();
        // SAFETY: the handle is open for the whole call, with the `SYNCHRONIZE` access a created process's handle has.
        match unsafe { WaitForSingleObject(process, wait_millis(timeout)) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(io::Error::last_os_error()),
        }
        self.exit_code().map(Some)
    }

    /// Waits at most `timeout` for the command to exit or `interrupt` to be raised; a command that has exited is
    /// [`Waited::Exited`] even when the interrupt was raised too.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails or the exit code cannot be read.
    pub fn wait_or_interrupt(&self, timeout: Duration, interrupt: Interrupt) -> io::Result<Waited> {
        // SAFETY: the handle is open for as long as `self` lives, with the `SYNCHRONIZE` access a created process's
        // handle has.
        match unsafe { wait_with(interrupt, self.process.raw(), timeout) }? {
            Woken::Signaled => self.exit_code().map(Waited::Exited),
            Woken::Interrupted => Ok(Waited::Interrupted),
            Woken::TimedOut => Ok(Waited::Running),
        }
    }

    /// Waits at most `timeout` for the command to exit or `interrupt` to be raised a second time, as
    /// [`Running::wait_or_interrupt`] does for the first: a caller that let the first press through waits here for
    /// another. A second press that came before the call ends it at once.
    ///
    /// # Errors
    ///
    /// The system's error when the wait fails or the exit code cannot be read.
    pub fn wait_or_interrupt_again(&self, timeout: Duration, interrupt: Interrupt) -> io::Result<Waited> {
        // SAFETY: the handle is open for as long as `self` lives, with the `SYNCHRONIZE` access a created process's
        // handle has.
        match unsafe { wait_again_with(interrupt, self.process.raw(), timeout) }? {
            Woken::Signaled => self.exit_code().map(Waited::Exited),
            Woken::Interrupted => Ok(Waited::Interrupted),
            Woken::TimedOut => Ok(Waited::Running),
        }
    }

    /// The exit code of the command, which has exited.
    fn exit_code(&self) -> io::Result<u32> {
        let process: HANDLE = self.process.raw();
        let mut code = 0;
        // SAFETY: the handle is open for the whole call, with query access, and `code` is writable.
        if unsafe { GetExitCodeProcess(process, &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }
}

/// How a wait for a command that an interrupt may end ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Waited {
    /// The command exited, with this code.
    Exited(u32),
    /// The interrupt was raised while the command still ran.
    Interrupted,
    /// The command still runs.
    Running,
}

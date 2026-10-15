//! Commands created suspended, with an explicit environment and exactly three inherited handles (stdin from `NUL`,
//! stdout to the log and stderr to the log's `.err` file; or stdin and stdout each a pipe this process holds the other
//! end of, and stderr this process's own), then put in a job and resumed, so what they start runs in the job, except a
//! packaged desktop app a non-packaged descendant starts, which Windows puts in a job of its own.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::RawHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FILE_TYPE_UNKNOWN,
    GetFileInformationByHandle, GetFileType, OPEN_EXISTING, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetExitCodeProcess, HIGH_PRIORITY_CLASS, OpenProcess, PROCESS_CREATION_FLAGS, PROCESS_TERMINATE,
    ResumeThread, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;

use crate::create::{Creation, DesktopPolicy, Inherited, check_launch, create_inheriting_on};
use crate::interrupt::{Woken, wait_with};
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

    /// The block `block` holds as it is, entries and order untouched, once it is checked to be one `CreateProcessW`
    /// reads whole: it ends in two NULs, and every entry before them holds a `=` after its first character, so no entry
    /// is empty and none ends the block early. An entry that starts with `=`, as `=C:=C:\` does, is kept. This is the
    /// only way to build a block from bytes another process sent.
    ///
    /// # Errors
    ///
    /// An error of kind [`io::ErrorKind::InvalidInput`] naming why, for a block that is empty or holds no entry, does
    /// not end in two NULs, or holds an empty entry or an entry with no `=` after its first character before its end.
    pub fn from_wide(block: Vec<u16>) -> io::Result<EnvironmentBlock> {
        match block_problem(&block) {
            Some(problem) => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the environment block {problem}"))),
            None => Ok(EnvironmentBlock(block)),
        }
    }
}

/// What makes `block` a block `CreateProcessW` would not read whole, or `None` when nothing does. An entry is named by
/// its position alone, since its text may be a secret.
fn block_problem(block: &[u16]) -> Option<String> {
    if block.is_empty() || block == [0, 0] {
        return Some("is empty: it holds no entry".to_owned());
    }
    let Some(entries) = block.strip_suffix(&[0, 0]) else {
        return Some("does not end in two NULs".to_owned());
    };
    let equals = u16::from(b'=');
    entries
        .split(|&unit| unit == 0)
        .enumerate()
        .find_map(|(index, entry)| match entry.split_first() {
            None => Some(format!("holds an empty entry at position {index}, before its end")),
            Some((_, rest)) if !rest.contains(&equals) => Some(format!("holds an entry at position {index} with no `=` after its first character")),
            Some(_) => None,
        })
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
    /// `CreateProcessW`, or `CreateProcessAsUserW` for a command given a [`crate::RestrictedToken`], refused the
    /// command: the program is missing or cannot run, or the token is refused. Its text is the system's message
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
            token: None,
        };
        let inherited = Inherited {
            stdio: [&input, &output, &errors],
            others: &[],
        };
        let policy = PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;
        let mut suspended = create_inheriting_on(
            &creation,
            self.priority.map_or(0, PriorityClass::flag),
            &inherited,
            DesktopPolicy::Required(policy),
            None,
        )?;
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

/// Refuses a handle that is not to a disk file, is to a reparse point, or is to a file with more than one name, with an
/// error that carries no system error code; a handle whose file cannot be inspected is the system's error, its code
/// kept. The caller keeps `handle` open for the whole call.
pub(crate) fn check_plain_file(handle: HANDLE) -> io::Result<()> {
    // SAFETY: the call reads no memory through the handle; one that is not open makes it fail.
    let kind = unsafe { GetFileType(handle) };
    if kind == FILE_TYPE_UNKNOWN {
        // A type the system does not know and a call that failed both read as unknown; only the failure leaves an
        // error other than NO_ERROR behind.
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
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
    /// What the launch could not give the command, for the caller to log: set when a desktop-app policy it only
    /// wanted could not be set, and the command was created without it.
    pub(crate) note: Option<String>,
}

impl Suspended {
    /// Takes a suspended process another process created, from its process handle and the handle of its first thread,
    /// as [`crate::RemoteProcess::pull`] copies them out, once both are checked to stand for the process `pid`: the
    /// process's id (`GetProcessId`) and its thread's (`GetProcessIdOfThread`) must both be `pid`. The caller has
    /// checked the rest it expects. From here it is as a command this crate created: dropped before
    /// [`Suspended::assign_and_resume`], it is terminated with exit code 1. The process handle needs
    /// `PROCESS_TERMINATE`, `PROCESS_SET_QUOTA` and query access, and the thread handle `THREAD_SUSPEND_RESUME` and
    /// `THREAD_QUERY_LIMITED_INFORMATION`, as the handles `CreateProcessW` returns have.
    ///
    /// # Errors
    ///
    /// An [`AdoptError`] holding both handles, unchanged, when either id cannot be read or is not `pid`: nothing is
    /// ended, and the caller decides what becomes of the process.
    pub fn adopt(process: std::os::windows::io::OwnedHandle, thread: std::os::windows::io::OwnedHandle, pid: u32) -> Result<Suspended, AdoptError> {
        use std::os::windows::io::{AsHandle, FromRawHandle, IntoRawHandle};

        if let Err(why) = check_adopted(process.as_handle(), thread.as_handle(), pid) {
            return Err(AdoptError { process, thread, why });
        }
        let (process, thread) = (process.into_raw_handle(), thread.into_raw_handle());
        // SAFETY: `into_raw_handle` gave up the std guards' ownership, so each new guard alone owns its open handle; a
        // guard refused gives its handle back to a std guard below, unclosed.
        let owned = unsafe { (own_handle(process), own_handle(thread)) };
        match owned {
            (Ok(process), Ok(thread)) => Ok(Suspended {
                process: Some(process),
                thread,
                pid,
                pipes: None,
                note: None,
            }),
            (process_guard, thread_guard) => {
                // A guard that was made is never dropped, so both handles go back to the caller unclosed.
                let _unclosed = std::mem::ManuallyDrop::new((process_guard, thread_guard));
                let why = io::Error::other("a handle that passed the checks was refused as a guard's");
                // SAFETY: both handles are as the caller gave them, and no guard owns them any more.
                let (process, thread) = unsafe {
                    (
                        std::os::windows::io::OwnedHandle::from_raw_handle(process),
                        std::os::windows::io::OwnedHandle::from_raw_handle(thread),
                    )
                };
                Err(AdoptError { process, thread, why })
            }
        }
    }

    /// Whether the command is in `job`, or in a job nested inside it (`IsProcessInJob` on that job alone, not any job).
    ///
    /// # Errors
    ///
    /// The system's error when that cannot be read, or an error when the process was already taken.
    pub fn in_job(&self, job: &Job) -> io::Result<bool> {
        let Some(process) = &self.process else {
            return Err(io::Error::other("the process was already taken"));
        };
        job.holds(process)
    }

    /// The command's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// What the launch could not give the command, for the caller to log: `Some` when a [`crate::PipedLaunch`] could
    /// not set the desktop-app policy, which needs Windows 10 1703 or later, and created the command without it.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// This process's ends of the command's stdin and stdout: `Some` once for a [`LaunchOutput::Piped`] launch, else
    /// `None`. Ends not taken close when the command is resumed or dropped.
    pub fn take_pipes(&mut self) -> Option<StdioPipes> {
        self.pipes.take()
    }

    /// Puts the command in `job`, then lets it run, so what it starts runs in the job (but see the module doc for a
    /// packaged desktop app).
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
        let named = match job.name() {
            "" => "the job".to_owned(),
            name => format!("the job {name}"),
        };
        job.assign(process).map_err(|error| setup(error, &format!("put the command in {named}")))
    }

    /// Lets the command run, once [`Suspended::assign`] has put it in its job.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Setup`] when the command cannot be resumed, or when its thread was not suspended exactly once,
    /// so the resume did not let it run (or it ran already); it is terminated with exit code 1 first.
    pub fn resume(mut self) -> Result<Running, LaunchError> {
        let Some(process) = self.process.take() else {
            return Err(setup(io::Error::other("the process was already taken"), "resume the command"));
        };
        // SAFETY: the thread handle is open for the whole call, with the `THREAD_SUSPEND_RESUME` access a created
        // thread's handle has.
        let error = match unsafe { ResumeThread(self.thread.raw()) } {
            1 => return Ok(Running { process, pid: self.pid }),
            u32::MAX => io::Error::last_os_error(),
            count => io::Error::other(format!("the command's thread had a suspend count of {count}, not 1")),
        };
        end_unstarted(&process);
        Err(setup(error, "resume the command"))
    }

    /// Gives up the command without ending it or closing its process and thread handles, and returns its pid and the
    /// raw values of those two handles, for a process that hands them to another: the desktop helper, whose copies the
    /// box takes with `DUPLICATE_CLOSE_SOURCE`. The command stays suspended. This process's ends of its pipes, when not
    /// taken, are closed, as a resume closes them. The process value is null only for a command whose process was
    /// already taken, which no caller outside this crate can make.
    ///
    /// The caller owns both handles from here. Once it has handed them on, it must never use the values again, not even
    /// to close them: after the other process closes them, they may name other objects of this process.
    #[must_use]
    pub fn release(mut self) -> (u32, RawHandle, RawHandle) {
        let process = self
            .process
            .take()
            .map_or(std::ptr::null_mut(), |process| std::mem::ManuallyDrop::new(process).raw());
        let mut released = std::mem::ManuallyDrop::new(self);
        released.pipes = None;
        released.note = None;
        (released.pid, process, released.thread.raw())
    }
}

/// A refused [`Suspended::adopt`]: the two handles it was given, open and unchanged, and why they were refused.
/// Nothing was ended.
#[derive(Debug)]
pub struct AdoptError {
    /// The process handle, as it was given.
    pub process: std::os::windows::io::OwnedHandle,
    /// The thread handle, as it was given.
    pub thread: std::os::windows::io::OwnedHandle,
    /// Why the handles were refused.
    pub why: io::Error,
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the suspended process was not adopted: {}", self.why)
    }
}

impl std::error::Error for AdoptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.why)
    }
}

/// Refuses a process handle and a thread handle that do not both stand for the process `pid`: the process's id
/// (`GetProcessId`) and the id of the thread's process (`GetProcessIdOfThread`) must both be `pid`.
fn check_adopted(process: std::os::windows::io::BorrowedHandle<'_>, thread: std::os::windows::io::BorrowedHandle<'_>, pid: u32) -> io::Result<()> {
    let of_process = crate::pid_of_process(process)?;
    let of_thread = crate::pid_of_thread(thread)?;
    if of_process != pid || of_thread != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the process handle is of pid {of_process} and the thread handle of a thread of pid {of_thread}, not both of pid {pid}"),
        ));
    }
    Ok(())
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose block or command cannot be made must fail loudly")]
mod tests {
    use std::io;
    use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::PoisonError;
    use std::time::Duration;

    use windows_sys::Win32::System::Threading::TerminateProcess;

    use super::{EnvironmentBlock, Launch, LaunchOutput};
    use crate::{StoppableProcess, pid_of_process, system_directory};

    /// The longest a test waits for a process to end: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// `text` as UTF-16 units, its NULs kept and none added.
    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// Asserts that `from_wide` refuses `block` for a reason that says `why`.
    fn assert_refused(block: &str, why: &str) {
        let refused = EnvironmentBlock::from_wide(units(block)).expect_err("the block is refused");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{block:?}: {refused}");
        assert!(refused.to_string().contains(why), "{block:?} is refused for {why:?}: {refused}");
    }

    #[test]
    fn an_environment_block_from_wide_refuses_a_block_without_a_double_nul() {
        for block in ["A=1", "A=1\0", "A=1\0B=2\0", "\0"] {
            assert_refused(block, "two NULs");
        }
    }

    #[test]
    fn an_environment_block_from_wide_refuses_an_empty_block() {
        assert_refused("", "empty");
        assert_refused("\0\0", "empty");
    }

    #[test]
    fn an_environment_block_from_wide_refuses_an_empty_entry() {
        assert_refused("A=1\0\0B=2\0\0", "empty entry at position 1");
        assert_refused("\0A=1\0\0", "empty entry at position 0");
    }

    #[test]
    fn an_environment_block_from_wide_refuses_an_entry_without_an_equals_sign() {
        assert_refused("A=1\0SECRET\0\0", "position 1 with no `=`");
        assert_refused("=\0\0", "position 0 with no `=`");
        let refused = EnvironmentBlock::from_wide(units("SECRET\0\0")).expect_err("the block is refused");
        assert!(!refused.to_string().contains("SECRET"), "an entry's text is not repeated: {refused}");
    }

    #[test]
    fn an_environment_block_from_wide_accepts_a_drive_entry() {
        let block = units("=C:=C:\\\0A=1\0\0");

        let accepted = EnvironmentBlock::from_wide(block.clone()).expect("a drive entry is valid");

        assert_eq!(accepted.0, block, "the block is kept as it is");
    }

    #[test]
    fn release_gives_up_the_child_without_ending_it() {
        let _alone = crate::create::tests::ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let system = system_directory().expect("the system folder is read");
        let child = Launch {
            application: system.join("cmd.exe"),
            command_line: units("cmd /d /c exit 0"),
            directory: system,
            environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
            output: LaunchOutput::Piped,
            priority: None,
        }
        .start_suspended()
        .expect("the command is created");
        let watch = StoppableProcess::open(child.pid()).expect("the child opens").expect("the child exists");
        let expected_pid = child.pid();

        let (pid, process, thread) = child.release();

        // SAFETY: `release` gave up both handles, open, and nothing else owns them now.
        let (process, _thread) = unsafe { (OwnedHandle::from_raw_handle(process), OwnedHandle::from_raw_handle(thread)) };
        assert_eq!(pid, expected_pid);
        assert_eq!(pid_of_process(process.as_handle()).expect("the released process is read"), pid);
        assert!(
            !watch.wait(Duration::ZERO).expect("the wait succeeds"),
            "the released child was not ended"
        );
        // SAFETY: the process handle is open, with the `PROCESS_TERMINATE` access a created process's handle has.
        assert_ne!(unsafe { TerminateProcess(process.as_raw_handle(), 7) }, 0, "the test ends the child");
        assert!(watch.wait(HANG_GUARD).expect("the wait succeeds"), "the child ended");
        assert_eq!(
            watch.exit_code().expect("the exit code is read"),
            7,
            "ended by the test, not by the release"
        );
    }
}

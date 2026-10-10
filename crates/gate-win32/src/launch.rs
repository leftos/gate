//! Commands created suspended, with an explicit environment and exactly three inherited handles (stdin from `NUL`,
//! stdout to the log, stderr to the log's `.err` file), then put in a job and resumed, so nothing they start runs
//! outside the job.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use proc_table::win32::OwnedHandle;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    GetFileInformationByHandle, GetFileType, OPEN_EXISTING, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, HIGH_PRIORITY_CLASS, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;

use crate::{Job, own_handle, wait_millis, win32_message};

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
    /// `.cmd` or `.bat` file is refused; the caller runs one through `ComSpec` itself.
    pub application: PathBuf,
    /// The command line, as the program reads it, without a terminating NUL.
    pub command_line: Vec<u16>,
    /// The working folder.
    pub directory: PathBuf,
    /// The command's whole environment.
    pub environment: EnvironmentBlock,
    /// The file the command's stdout goes to, created afresh.
    pub log: PathBuf,
    /// The file the command's stderr goes to, created afresh: the log's `.err` file.
    pub err_log: PathBuf,
    /// The priority class to create the command in; with `None` it gets `NORMAL`, or this process's class when that is
    /// below normal or idle, as `CreateProcessW` gives a command no class is asked for.
    pub priority: Option<PriorityClass>,
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
    /// process's console and process group. Its stdin is `NUL`, its stdout the log and its stderr the `.err` file,
    /// each created afresh (an earlier file at the path is deleted first, and a link at it is never followed) and open
    /// to every process for reading, writing and deleting while the command writes them, so a process the command left
    /// running cannot keep the next run from replacing them; it inherits those three
    /// handles and no other. It is created with the desktop-app policy
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
        self.check()?;
        let input = create_inheritable(Path::new("NUL"), GENERIC_READ, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL)?;
        let output = create_output(&self.log)?;
        let errors = create_output(&self.err_log)?;
        let handles = [input.raw(), output.raw(), errors.raw()];
        let policy: u32 = PROCESS_CREATION_DESKTOP_APP_BREAKAWAY_DISABLE_PROCESS_TREE;
        let mut attributes = AttributeList::new(2)?;
        // SAFETY: `handles` is declared before the list, so it outlives it, and is not changed while the list lives; it
        // is an array of handles, as the attribute wants.
        unsafe { attributes.update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_ptr().cast(), size_of_val(&handles)) }?;
        // SAFETY: `policy` is declared before the list, so it outlives it, and is not changed while the list lives; it
        // is one DWORD, as the attribute wants.
        unsafe { attributes.update(PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY, (&raw const policy).cast(), size_of::<u32>()) }?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = struct_size::<STARTUPINFOEXW>();
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = input.raw();
        startup.StartupInfo.hStdOutput = output.raw();
        startup.StartupInfo.hStdError = errors.raw();
        startup.lpAttributeList = attributes.as_ptr();
        self.create(&startup)
    }

    /// Refuses a launch `CreateProcessW` would read otherwise than as written: a NUL cuts a string short, a relative
    /// application is searched for, and a batch file runs through cmd.exe without the caller's quoting.
    fn check(&self) -> Result<(), LaunchError> {
        if self.command_line.contains(&0) {
            return Err(refused("the command line holds a NUL".to_owned()));
        }
        let paths = [
            ("application", &self.application),
            ("working folder", &self.directory),
            ("log", &self.log),
            ("err log", &self.err_log),
        ];
        for (field, path) in paths {
            if path.as_os_str().encode_wide().any(|unit| unit == 0) {
                return Err(refused(format!("the {field} {} holds a NUL", path.display())));
            }
        }
        if !self.application.is_absolute() {
            return Err(refused(format!("the application {} is not an absolute path", self.application.display())));
        }
        if is_batch(&self.application) {
            return Err(refused(format!(
                "the application {} is a batch file, which runs only through ComSpec",
                self.application.display()
            )));
        }
        Ok(())
    }

    /// Calls `CreateProcessW` with `startup`, whose handles stay open for the whole call.
    fn create(&self, startup: &STARTUPINFOEXW) -> Result<Suspended, LaunchError> {
        let application = wide_os(self.application.as_os_str());
        let mut line: Vec<u16> = self.command_line.iter().copied().chain(Some(0)).collect();
        let directory = wide_os(self.directory.as_os_str());
        let priority = self.priority.map_or(0, PriorityClass::flag);
        let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | priority;
        let mut created = PROCESS_INFORMATION::default();
        // SAFETY: `application` and `directory` are NUL-terminated, `line` is a writable NUL-terminated buffer the call
        // may change, the environment is a block `EnvironmentBlock` built (ended by two NULs) and
        // `CREATE_UNICODE_ENVIRONMENT` says it is UTF-16; `startup` is a whole `STARTUPINFOEXW` whose `cb` says so and
        // `EXTENDED_STARTUPINFO_PRESENT` says so too; every buffer outlives the call and `created` is writable.
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                self.environment.0.as_ptr().cast(),
                directory.as_ptr(),
                std::ptr::from_ref(startup).cast::<STARTUPINFOW>(),
                &raw mut created,
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` takes no pointers; it is read before anything else can set it.
            let code = unsafe { GetLastError() };
            return Err(LaunchError::CreateProcess {
                code,
                message: win32_message(code),
            });
        }
        // SAFETY: a success returns a fresh process handle nothing else owns.
        let process = unsafe { own_handle(created.hProcess) }.map_err(|error| setup(error, "own the command's process"))?;
        // SAFETY: a success returns a fresh thread handle nothing else owns.
        let thread = unsafe { own_handle(created.hThread) }.map_err(|error| setup(error, "own the command's thread"))?;
        Ok(Suspended {
            process: Some(process),
            thread,
            pid: created.dwProcessId,
        })
    }
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

/// A handle to `path` the command can inherit, open to every process for reading, writing and deleting.
pub(crate) fn create_inheritable(
    path: &Path,
    access: u32,
    disposition: FILE_CREATION_DISPOSITION,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> Result<OwnedHandle, LaunchError> {
    let name = wide_os(path.as_os_str());
    let inheritable = SECURITY_ATTRIBUTES {
        nLength: struct_size::<SECURITY_ATTRIBUTES>(),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: `name` is NUL-terminated and `inheritable` a whole `SECURITY_ATTRIBUTES`, both outliving the call; no
    // template file is passed.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
            &raw const inheritable,
            disposition,
            flags,
            std::ptr::null_mut(),
        )
    };
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
    let handle = create_inheritable(path, GENERIC_WRITE, CREATE_NEW, flags)?;
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

/// A process-and-thread attribute list, deleted when dropped.
pub(crate) struct AttributeList {
    buffer: Vec<usize>,
}

impl AttributeList {
    /// A list with room for `count` attributes.
    pub(crate) fn new(count: u32) -> Result<AttributeList, LaunchError> {
        let mut size = 0;
        // SAFETY: a null list asks only for the size a list of `count` attributes needs, written to `size`.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
        if size == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, "size the attribute list"));
        }
        let mut buffer = vec![0_usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buffer` holds at least `size` bytes, aligned for the pointers the list holds.
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size) } == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, "initialize the attribute list"));
        }
        Ok(AttributeList { buffer })
    }

    /// The list, as `STARTUPINFOEXW` points to it.
    pub(crate) fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// Sets `attribute` to the `size` bytes at `value`.
    ///
    /// # Safety
    ///
    /// `value` points to `size` readable bytes, as `attribute` wants them, that stay valid and unchanged for as long
    /// as the list lives: the list keeps the pointer, not a copy.
    pub(crate) unsafe fn update(&mut self, attribute: u32, value: *const c_void, size: usize) -> Result<(), LaunchError> {
        // SAFETY: the list was initialized; the caller guarantees `value` and `size`.
        let updated = unsafe { UpdateProcThreadAttribute(self.as_ptr(), 0, attribute as usize, value, size, std::ptr::null_mut(), std::ptr::null()) };
        if updated == 0 {
            let error = io::Error::last_os_error();
            return Err(setup(error, &format!("set the process attribute 0x{attribute:x}")));
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialized, and is deleted only here, once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// A command created suspended: it has not run a single instruction until [`Suspended::assign_and_resume`]. Dropped
/// without that, it is terminated with exit code 1, so a gate that fails before the resume leaves no suspended process.
pub struct Suspended {
    /// The process, until [`Suspended::assign_and_resume`] takes it.
    pub(crate) process: Option<OwnedHandle>,
    pub(crate) thread: OwnedHandle,
    pub(crate) pid: u32,
}

impl Suspended {
    /// The command's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Puts the command in `job`, then lets it run, so nothing it starts can run outside the job.
    ///
    /// # Errors
    ///
    /// [`LaunchError::Setup`] when the command cannot be put in the job or resumed; it is terminated with exit code 1
    /// first.
    pub fn assign_and_resume(mut self, job: &Job) -> Result<Running, LaunchError> {
        let Some(process) = self.process.take() else {
            return Err(setup(io::Error::other("the process was already taken"), "resume the command"));
        };
        if let Err(error) = job.assign(&process) {
            terminate(&process);
            return Err(setup(error, &format!("put the command in the job {}", job.name())));
        }
        // SAFETY: the thread handle is open for the whole call, with the `THREAD_SUSPEND_RESUME` access a created
        // thread's handle has.
        if unsafe { ResumeThread(self.thread.raw()) } == u32::MAX {
            let error = io::Error::last_os_error();
            terminate(&process);
            return Err(setup(error, "resume the command"));
        }
        Ok(Running { process, pid: self.pid })
    }
}

impl Drop for Suspended {
    fn drop(&mut self) {
        if let Some(process) = &self.process {
            terminate(process);
        }
    }
}

/// Ends a command that never ran with exit code 1. Its own failure is not reported: it runs on a path that is already
/// failing or dropping, and a process that could not be terminated stays visible in its job.
fn terminate(process: &OwnedHandle) {
    // SAFETY: the process handle is open for the whole call, with the `PROCESS_TERMINATE` access a created process's
    // handle has.
    unsafe { TerminateProcess(process.raw(), 1) };
}

/// A command resumed in its job.
pub struct Running {
    process: OwnedHandle,
    pid: u32,
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
        let mut code = 0;
        // SAFETY: the handle is open for the whole call, with query access, and `code` is writable.
        if unsafe { GetExitCodeProcess(process, &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }
}

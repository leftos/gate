//! Starting a registered scheduled task in a given session through the Task Scheduler's COM API, once its definition
//! is checked: the desktop helper's task, which runs the helper beside the running program in the logged-on user's
//! session; and registering that task, with the definition the check wants. The COM work runs on a short-lived thread
//! of its own, in the multithreaded apartment. The Task Scheduler's class is made from `taskschd.dll`, loaded once for the process by its full path in the system folder and never
//! freed, never looked up in the registry, where a user's own class entry could stand in for it; every interface is
//! released before that thread ends.

use std::any::Any;
use std::ffi::{OsString, c_void};
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::num::NonZeroU32;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IClassFactory};
use windows::Win32::System::TaskScheduler::{
    IAction, IExecAction, IRegisteredTask, IRunningTask, ITaskDefinition, ITaskFolder, ITaskService, ITaskSettings, ITaskSettings3, TASK_ACTION_EXEC,
    TASK_ACTION_TYPE, TASK_CREATE_OR_UPDATE, TASK_DONT_ADD_PRINCIPAL_ACE, TASK_INSTANCES_PARALLEL, TASK_INSTANCES_POLICY,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_LOGON_TYPE, TASK_RUN_IGNORE_CONSTRAINTS, TASK_RUN_USE_SESSION_ID, TASK_RUNLEVEL_LUA, TASK_RUNLEVEL_TYPE,
    TASK_STATE, TASK_STATE_DISABLED, TASK_STATE_READY, TaskScheduler,
};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::core::{BSTR, GUID, HRESULT, IUnknown, Interface};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NONE_MAPPED, GENERIC_READ, HANDLE, HMODULE};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, INHERIT_ONLY_ACE, LookupAccountNameW,
    OWNER_SECURITY_INFORMATION, PSID, SID_NAME_USE,
};
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::SystemServices::ACCESS_DENIED_ACE_TYPE;

use crate::pipe::{Early, Entry, SecurityDescriptor, Signalled, check_wake, entries_of, is_sid_string, look_at_wake, wait_any};

/// The library the Task Scheduler's class is made from, loaded by its full path in the system folder.
const TASK_SCHEDULER_LIBRARY: &str = "taskschd.dll";

/// The only arguments the task's action may have: the `Arg0` a run passes it, as the Task Scheduler substitutes it.
const ARG0_ARGUMENTS: &str = "$(Arg0)";

/// The priority the task must run at: 4, normal, as the Task Scheduler counts from 0 (realtime) to 10 (idle).
const TASK_PRIORITY: i32 = 4;

/// The execution time limit the task must have: none, so the Task Scheduler never ends the helper or its job.
const NO_TIME_LIMIT: &str = "PT0S";

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: no task of the name in the folder, and also what `GetFolder` gives for a
/// folder that does not exist, nested or not.
const TASK_NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();

/// `HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)`: a path that names no folder; the Task Scheduler may give it for a folder
/// that does not exist, though `GetFolder` gives [`TASK_NOT_FOUND`]'s code for one.
const FOLDER_NOT_FOUND: i32 = 0x8007_0003_u32.cast_signed();

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`: a folder made that another made first.
const ALREADY_EXISTS: i32 = 0x8007_00B7_u32.cast_signed();

/// The most characters of a text read from the task that an error shows, once escaped.
const SHOWN_LIMIT: usize = 200;

/// How long a look for the started task's process sleeps before it asks the Task Scheduler again.
const PID_LOOK_STEP: Duration = Duration::from_millis(50);

/// The SIDs that may own or change the task and its folder: SYSTEM and the Administrators group.
const TRUSTED_SIDS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

/// The only rights an entry for anyone but SYSTEM and Administrators, the user running gate included, may allow on the task:
/// reading it, `FILE_GENERIC_READ` and `GENERIC_READ`, as the Task Scheduler maps a task's rights onto a file's.
const READ_ONLY: u32 = FILE_GENERIC_READ | GENERIC_READ;

/// The parts of a task's or its folder's security descriptor the checks read: its owner and its DACL.
const OWNER_AND_DACL: u32 = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;

/// The `Arg0` a run of the task passes it: text of `0-9`, `a-f` and `:` alone, at most [`TaskArgument::LONGEST`]
/// characters, as `gate_desktop::arg0` writes it, so nothing else can reach the task's command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskArgument(String);

impl TaskArgument {
    /// The longest argument: a pid of 10 digits, the `:`, and 32 hex digits.
    pub const LONGEST: usize = 43;

    /// `text` as a task argument; `None` when it is empty, longer than [`TaskArgument::LONGEST`], or holds anything
    /// but `0-9`, `a-f` and `:`.
    #[must_use]
    pub fn new(text: &str) -> Option<TaskArgument> {
        let allowed =
            !text.is_empty() && text.len() <= TaskArgument::LONGEST && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b':'));
        allowed.then(|| TaskArgument(text.to_owned()))
    }

    /// The argument's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What [`run_task`] starts, and what it checks the task against.
#[derive(Clone, Copy, Debug)]
pub struct TaskStart<'a> {
    /// The task's path, such as `\gate\desktop-helper`.
    pub path: &'a str,
    /// The value of the task's `$(Arg0)`.
    pub argument: &'a TaskArgument,
    /// The session to run the task in.
    pub session_id: u32,
    /// The program the task's one action must run.
    pub helper: &'a Path,
    /// How long, once the task is started, to keep asking the Task Scheduler for its process while the instance is
    /// queued or running without one; the deadline [`run_task`] is given ends that wait first when it comes sooner.
    pub pid_wait: Duration,
}

/// A task [`run_task`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartedTask {
    /// The id of the process the Task Scheduler started for the task's action.
    Process(NonZeroU32),
    /// The task was started, but the Task Scheduler gave no process for it within the pid wait, or the instance ended
    /// without one: its helper may be running, and only the caller's own deadline bounds the wait for it.
    NoPid,
}

/// Why [`run_task`] did not give the task's process. Every text is escaped, and each part taken from the task itself
/// cut too, so it holds no control character, line or paragraph separator, or format character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    /// No task, or no folder, of the path is registered; nothing was started.
    NotRegistered,
    /// The task is registered with a definition other than the one the check wants, or its folder's security fails
    /// the check; the text names the first setting that differs. Nothing was started, or registered.
    SetUpWrong(String),
    /// A call of the Task Scheduler or of COM failed before the task was started, so nothing was; the text names the
    /// call and holds the system's message. For a registration, also a task read back that fails the check.
    Failed(String),
    /// The task was started, then a call of the Task Scheduler failed; the text names the call and holds the system's
    /// message. The instance may be running, and its helper ends by its own limits.
    StartedUnknown(String),
    /// The Task Scheduler did not answer before the deadline. The thread that asked it is left to finish on its own, and
    /// told to start nothing if it has not yet; the task may have been started; the caller drops its pipe server.
    TimedOut,
    /// The wake handle at this index was signalled before the Task Scheduler answered, the thread left as
    /// [`TaskError::TimedOut`] leaves it; the task may have been started; the caller drops its pipe server.
    Woken(usize),
    /// The wait for the Task Scheduler failed, or the thread that asked it panicked, after the start may have been
    /// requested; the text says which. The task may have been started; the caller drops its pipe server.
    Unanswered(String),
}

impl TaskError {
    /// A [`TaskError::Failed`] of `why`, escaped.
    fn failed(why: &str) -> TaskError {
        TaskError::Failed(escaped(why))
    }

    /// A [`TaskError::StartedUnknown`] of `why`, escaped.
    fn started_unknown(why: &str) -> TaskError {
        TaskError::StartedUnknown(escaped(why))
    }

    /// A [`TaskError::SetUpWrong`] of `what`, escaped.
    fn set_up_wrong(what: &str) -> TaskError {
        TaskError::SetUpWrong(escaped(what))
    }

    /// A [`TaskError::Unanswered`] of `why`, escaped.
    fn unanswered(why: &str) -> TaskError {
        TaskError::Unanswered(escaped(why))
    }
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::NotRegistered => write!(formatter, "the task is not registered"),
            TaskError::SetUpWrong(what) => write!(formatter, "the task is set up wrong: {what}"),
            TaskError::Failed(why) => write!(formatter, "{why}"),
            TaskError::StartedUnknown(why) => write!(formatter, "the task was started, but {why}"),
            TaskError::TimedOut => write!(formatter, "the Task Scheduler did not answer in time"),
            TaskError::Woken(index) => write!(formatter, "the wait for the Task Scheduler was ended by wake handle {index}"),
            TaskError::Unanswered(why) => write!(formatter, "the Task Scheduler gave no answer ({why}); the task may have been started"),
        }
    }
}

impl std::error::Error for TaskError {}

/// Starts the registered task `start.path` in the session `start.session_id`, with `start.argument` as the value of
/// its `$(Arg0)`, once its definition passes the check, and gives the process the Task Scheduler started for it. The
/// definition must be: enabled; started on demand; a second instance run in parallel; priority 4; neither kept from
/// starting nor stopped on batteries; no execution time limit; no triggers; one action, which runs `start.helper`
/// with the arguments `$(Arg0)` alone and no working folder; run with the interactive token, at the least privilege,
/// of the user running gate; and a security descriptor owned by SYSTEM or Administrators, which lets no one else, that
/// user included, do more than read it; the task's folder's descriptor likewise. The run ignores the task's constraints, such as on batteries. It never registers, changes or
/// stops a task.
///
/// The Task Scheduler is asked on a thread of its own, and the answer waited for until `deadline` or until a handle of
/// `wake` is signalled. A deadline passed or a wake handle signalled already gives its error before that thread is
/// made, so nothing is started.
///
/// # Errors
///
/// A [`TaskError`]; [`TaskError::Failed`] also when the path holds no task name, the running user cannot be read, or
/// `wake` holds more handles than a wait watches.
pub fn run_task(start: &TaskStart<'_>, deadline: Instant, wake: &[BorrowedHandle<'_>]) -> Result<StartedTask, TaskError> {
    check_wake(wake, "task start").map_err(|error| TaskError::failed(&error.to_string()))?;
    let Some((folder, name)) = split_task_path(start.path) else {
        return Err(TaskError::failed(&format!(
            "{} is not a task path, \\<folder>\\<name>",
            shown(start.path)
        )));
    };
    let user_sid = crate::current_user_sid().map_err(|error| TaskError::failed(&format!("the running user could not be read: {error}")))?;
    if let Some(given_up) = given_up(deadline, wake)? {
        return Err(given_up);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let request = Request {
        folder: folder.to_owned(),
        name: name.to_owned(),
        argument: start.argument.as_str().to_owned(),
        session_id: start.session_id,
        expected: Expected {
            helper: start.helper.to_owned(),
            user_sid,
        },
        pid_wait: start.pid_wait,
        deadline,
        cancel: Arc::clone(&cancel),
    };
    let thread = std::thread::Builder::new()
        .name("gate-task".to_owned())
        .spawn(move || start_on_this_thread(&request))
        .map_err(|error| TaskError::failed(&format!("the thread that starts the task could not be made: {error}")))?;
    let mut handles = vec![thread.as_raw_handle()];
    handles.extend(wake.iter().map(AsRawHandle::as_raw_handle));
    let waited = wait_any(&handles, deadline);
    if !matches!(waited, Ok(Signalled::Event)) {
        // The thread is left to finish on its own; one that has not reached the run yet starts nothing.
        cancel.store(true, Ordering::SeqCst);
    }
    answered(waited, thread, "starts the task")
}

/// The answer of the `thread` that `doing` (such as `starts the task`) asked the Task Scheduler, as the wait `waited`
/// for that thread and the wake handles found it: the thread's own result once it ended; [`TaskError::Unanswered`] when
/// it panicked or the wait failed. A thread the wait did not see end is left to finish on its own.
fn answered<T>(waited: io::Result<Signalled>, thread: JoinHandle<Result<T, TaskError>>, doing: &str) -> Result<T, TaskError> {
    match waited {
        Ok(Signalled::Event) => thread.join().unwrap_or_else(|panic| {
            Err(TaskError::unanswered(&format!(
                "the thread that {doing} panicked: {}",
                panic_text(&*panic)
            )))
        }),
        Ok(Signalled::Early(Early::TimedOut)) => Err(TaskError::TimedOut),
        Ok(Signalled::Early(Early::Woken(index))) => Err(TaskError::Woken(index)),
        Err(error) => Err(TaskError::unanswered(&format!("the wait for the Task Scheduler failed: {error}"))),
    }
}

/// The error of a start given up before it is asked for: [`TaskError::TimedOut`] once `deadline` has passed,
/// [`TaskError::Woken`] with the lowest index of a handle of `wake` signalled already; `None` when neither holds.
fn given_up(deadline: Instant, wake: &[BorrowedHandle<'_>]) -> Result<Option<TaskError>, TaskError> {
    if Instant::now() >= deadline {
        return Ok(Some(TaskError::TimedOut));
    }
    let handles: Vec<HANDLE> = wake.iter().map(AsRawHandle::as_raw_handle).collect();
    let signalled = look_at_wake(&handles).map_err(|error| TaskError::failed(&format!("the wake handles could not be looked at: {error}")))?;
    Ok(signalled.map(TaskError::Woken))
}

/// What the thread that asks the Task Scheduler needs, owned so it can outlive [`run_task`]'s wait.
struct Request {
    folder: String,
    name: String,
    argument: String,
    session_id: u32,
    expected: Expected,
    pid_wait: Duration,
    /// The deadline [`run_task`] waits until, which bounds the pid wait too.
    deadline: Instant,
    /// Set once [`run_task`] has stopped waiting, so a thread that has not run the task yet does not.
    /// Untested: it needs a live task, and the race it narrows cannot be driven.
    cancel: Arc<AtomicBool>,
}

/// What the task's definition is checked against besides the fixed settings.
#[derive(Clone, Debug)]
struct Expected {
    /// The program the one action must run.
    helper: PathBuf,
    /// The string form of the SID of the user running gate.
    user_sid: String,
}

/// The message of a panic's payload, when it is text.
fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// The folder and the name of the task path `path`: the folder is everything before its last `\`, the root `\` when
/// that is nothing. `None` for a path that does not start with `\` or ends in it.
fn split_task_path(path: &str) -> Option<(&str, &str)> {
    let (folder, name) = path.rsplit_once('\\')?;
    if !path.starts_with('\\') || name.is_empty() {
        return None;
    }
    Some((if folder.is_empty() { "\\" } else { folder }, name))
}

/// The task started on this thread: COM joined, then the work, whose interfaces are all released when it returns,
/// before COM is left.
fn start_on_this_thread(request: &Request) -> Result<StartedTask, TaskError> {
    let library = task_library()?;
    let _apartment = Apartment::join()?;
    start_checked(library, request)
}

/// `taskschd.dll`, once loaded: kept for the rest of the process and never freed, as COM itself keeps an in-process
/// server no one asks it to unload, so nothing the library keeps past its last interface's release runs unmapped.
static TASK_LIBRARY: OnceLock<SystemLibrary> = OnceLock::new();

/// `taskschd.dll`, loaded by its full path in the system folder the first time it is asked for.
fn task_library() -> Result<&'static SystemLibrary, TaskError> {
    if let Some(library) = TASK_LIBRARY.get() {
        return Ok(library);
    }
    let loaded = SystemLibrary::load()?;
    // Two threads may load it at once; the load that loses only counts the module once more, and is never freed either.
    Ok(TASK_LIBRARY.get_or_init(|| loaded))
}

/// A library loaded from the system folder, never freed.
struct SystemLibrary(HMODULE);

// SAFETY: a module handle is the module's address in this process, good on every thread for as long as the module is
// loaded, and this one is never freed.
unsafe impl Send for SystemLibrary {}

// SAFETY: as for `Send`; the handle is only read.
unsafe impl Sync for SystemLibrary {}

/// The signature every in-process COM server's `DllGetClassObject` has.
type GetClassObject = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

impl SystemLibrary {
    /// Loads [`TASK_SCHEDULER_LIBRARY`] by its full path in the system folder, and refuses the module the load gives
    /// when it is not the file of that path, as one of the same name loaded before from elsewhere would be.
    fn load() -> Result<SystemLibrary, TaskError> {
        let system = crate::system_directory().map_err(|error| TaskError::failed(&format!("the system folder could not be read: {error}")))?;
        let path = system.join(TASK_SCHEDULER_LIBRARY);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; no file handle is passed, as the call requires.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(TaskError::failed(&format!(
                "{} could not be loaded: {}",
                path.display(),
                io::Error::last_os_error()
            )));
        }
        let loaded = module_path(module)
            .map_err(|error| TaskError::failed(&format!("the file {TASK_SCHEDULER_LIBRARY} was loaded from could not be read: {error}")))?;
        check_loaded_from(&path, &loaded)?;
        Ok(SystemLibrary(module))
    }

    /// A new Task Scheduler service object, made by this library's own class factory.
    fn task_service(&self) -> Result<ITaskService, TaskError> {
        // SAFETY: the module is never freed, and the name is a NUL-terminated ASCII string.
        let Some(found) = (unsafe { GetProcAddress(self.0, c"DllGetClassObject".as_ptr().cast()) }) else {
            return Err(TaskError::failed(&format!(
                "{TASK_SCHEDULER_LIBRARY} has no DllGetClassObject: {}",
                io::Error::last_os_error()
            )));
        };
        // SAFETY: `DllGetClassObject` is exported with the signature every in-process COM server gives it.
        let get_class_object = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetClassObject>(found) };
        let mut factory: *mut c_void = std::ptr::null_mut();
        let (class, interface) = (TaskScheduler, IClassFactory::IID);
        // SAFETY: both GUIDs outlive the call and `factory` is writable; a success leaves in it one reference to an
        // `IClassFactory`, which the interface made below owns.
        unsafe { get_class_object(&raw const class, &raw const interface, &raw mut factory) }
            .ok()
            .map_err(failed("get the Task Scheduler's class factory"))?;
        // SAFETY: as above: the reference is owned once, by this interface, released when it drops.
        let factory = unsafe { IClassFactory::from_raw(factory) };
        // SAFETY: no outer object is passed; the result is a new object this thread owns.
        unsafe { factory.CreateInstance::<Option<&IUnknown>, ITaskService>(None) }.map_err(failed("make the Task Scheduler"))
    }
}

/// Refuses a [`TASK_SCHEDULER_LIBRARY`] loaded from `loaded` when that is not `wanted`, its path in the system folder,
/// compared ordinally without regard to case.
fn check_loaded_from(wanted: &Path, loaded: &Path) -> Result<(), TaskError> {
    let (left, right): (Vec<u16>, Vec<u16>) = (wanted.as_os_str().encode_wide().collect(), loaded.as_os_str().encode_wide().collect());
    if crate::launch::ordinal_equal_ignore_case(&left, &right) {
        return Ok(());
    }
    Err(TaskError::failed(&format!(
        "{TASK_SCHEDULER_LIBRARY} was loaded from {}, not {}",
        shown(&loaded.display().to_string()),
        wanted.display()
    )))
}

/// The path of the file the loaded module `module` was loaded from (`GetModuleFileNameW`).
fn module_path(module: HMODULE) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 260];
    loop {
        let size = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: `buffer` is writable for `size` units, and the call writes at most that many; the module is loaded.
        let length = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), size) } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(OsString::from_wide(&buffer).into());
        }
        // A path that filled the buffer was cut: ask again with room for a longer one.
        buffer.resize(buffer.len() * 2, 0);
    }
}

/// This thread's place in the multithreaded apartment, left when dropped, so a panic in the work still leaves it.
struct Apartment(PhantomData<*const ()>);

impl Apartment {
    /// Joins this thread to the multithreaded apartment.
    fn join() -> Result<Apartment, TaskError> {
        // SAFETY: no reserved pointer is passed; the apartment is left once, when the guard made here drops.
        let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if joined.is_err() {
            return Err(TaskError::failed(&format!(
                "COM could not be set up: {}",
                windows::core::Error::from(joined)
            )));
        }
        Ok(Apartment(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: the apartment this thread joined in `join` is left once, here, on the same thread, since the guard
        // cannot be sent to another.
        unsafe { CoUninitialize() };
    }
}

/// Connects to the Task Scheduler, finds the task, checks its definition, then runs it and reads the process it
/// started.
fn start_checked(library: &SystemLibrary, request: &Request) -> Result<StartedTask, TaskError> {
    let service = connected(library)?;
    let task = checked_task(&service, &request.folder, &request.name, &request.expected)?;
    run(&task, request)
}

/// A new Task Scheduler service object of `library`, connected to this machine as the running user.
fn connected(library: &SystemLibrary) -> Result<ITaskService, TaskError> {
    let service = library.task_service()?;
    let local = VARIANT::default();
    // SAFETY: four empty variants connect to this machine as the running user.
    unsafe { service.Connect(&local, &local, &local, &local) }.map_err(failed("connect to the Task Scheduler"))?;
    Ok(service)
}

/// The task `name` of the folder `folder`, once the folder's security and the task's definition pass the check against
/// `expected`.
fn checked_task(service: &ITaskService, folder: &str, name: &str, expected: &Expected) -> Result<IRegisteredTask, TaskError> {
    // SAFETY: the folder's path is a valid BSTR for the call.
    let found = unsafe { service.GetFolder(&BSTR::from(folder)) }.map_err(missing("open the task's folder"))?;
    // SAFETY: the task's name is a valid BSTR for the call.
    let task = unsafe { found.GetTask(&BSTR::from(name)) }.map_err(missing("open the task"))?;
    let (owner, dacl) = read_folder_security(&found)?;
    check_folder(owner.as_deref(), dacl.as_deref(), folder).map_err(|what| TaskError::set_up_wrong(&what))?;
    let definition = read_definition(&task)?;
    check_definition(&definition, expected).map_err(|what| TaskError::set_up_wrong(&what))?;
    Ok(task)
}

/// The security descriptor string the desktop helper's task and its folder are registered with, `user_sid` naming the
/// user it runs as: owned by Administrators, its DACL protected from inheritance, SYSTEM and Administrators allowed
/// everything and the user reading alone (`FR`, which [`READ_ONLY`] holds).
fn security_sddl(user_sid: &str) -> String {
    format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;{user_sid})")
}

/// The owner and the DACL [`security_sddl`] names for `user_sid`, as the check reads them.
#[cfg(test)]
fn wanted_security(user_sid: &str) -> Result<(Option<String>, Option<Vec<Entry>>), TaskError> {
    owner_and_dacl(&BSTR::from(security_sddl(user_sid).as_str()), "the wanted")
}

/// The definition the desktop helper's task is registered with, the one [`check_definition`] wants but for its security,
/// which the task gets from the descriptor [`security_sddl`] at its registration and the XML never holds, so its owner
/// and DACL are left empty: running `helper` with `$(Arg0)` as the user whose SID is `user_sid`. A `helper` that is not
/// Unicode cannot be written into a task, and one that holds a `%` is refused, since the Task Scheduler expands
/// `%VAR%` in the action's program from the environment of the task's user, who can change it.
fn wanted_definition(helper: &Path, user_sid: &str) -> Result<Definition, TaskError> {
    let Some(path) = helper.to_str() else {
        return Err(TaskError::failed(&format!(
            "the desktop helper's path {} is not Unicode",
            shown(&helper.display().to_string())
        )));
    };
    if path.contains('%') {
        return Err(TaskError::failed(&format!("the desktop helper's path holds a %: {}", shown(path))));
    }
    Ok(Definition {
        enabled: true,
        allow_demand_start: true,
        parallel: true,
        priority: TASK_PRIORITY,
        restart_count: 0,
        maintenance: false,
        no_start_on_batteries: false,
        stop_on_batteries: false,
        time_limit: NO_TIME_LIMIT.to_owned(),
        triggers: 0,
        actions: vec![Action::Exec {
            path: path.to_owned(),
            arguments: ARG0_ARGUMENTS.to_owned(),
            directory: String::new(),
        }],
        interactive_token: true,
        least_privilege: true,
        user: user_sid.to_owned(),
        user_sid: Some(user_sid.to_owned()),
        owner: None,
        dacl: None,
    })
}

/// `definition` as Task Scheduler XML, every text XML-escaped. It writes no triggers, no restart on failure and no
/// maintenance settings, the only values of those [`check_definition`] accepts; its owner and DACL are not part of the
/// XML, but of the descriptor the task is registered with.
fn task_xml(definition: &Definition) -> String {
    let flag = |set: bool| if set { "true" } else { "false" };
    let logon = if definition.interactive_token { "InteractiveToken" } else { "Password" };
    let level = if definition.least_privilege {
        "LeastPrivilege"
    } else {
        "HighestAvailable"
    };
    let policy = if definition.parallel { "Parallel" } else { "IgnoreNew" };
    let actions: String = definition.actions.iter().map(exec_xml).collect();
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n",
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
            "  <Principals>\n",
            "    <Principal id=\"Author\">\n",
            "      <UserId>{user}</UserId>\n",
            "      <LogonType>{logon}</LogonType>\n",
            "      <RunLevel>{level}</RunLevel>\n",
            "    </Principal>\n",
            "  </Principals>\n",
            "  <Settings>\n",
            "    <Enabled>{enabled}</Enabled>\n",
            "    <AllowStartOnDemand>{on_demand}</AllowStartOnDemand>\n",
            "    <MultipleInstancesPolicy>{policy}</MultipleInstancesPolicy>\n",
            "    <Priority>{priority}</Priority>\n",
            "    <DisallowStartIfOnBatteries>{no_start}</DisallowStartIfOnBatteries>\n",
            "    <StopIfGoingOnBatteries>{stop}</StopIfGoingOnBatteries>\n",
            "    <ExecutionTimeLimit>{limit}</ExecutionTimeLimit>\n",
            "  </Settings>\n",
            "  <Actions Context=\"Author\">\n",
            "{actions}",
            "  </Actions>\n",
            "</Task>\n",
        ),
        user = xml_escaped(&definition.user),
        logon = logon,
        level = level,
        enabled = flag(definition.enabled),
        on_demand = flag(definition.allow_demand_start),
        policy = policy,
        priority = definition.priority,
        no_start = flag(definition.no_start_on_batteries),
        stop = flag(definition.stop_on_batteries),
        limit = xml_escaped(&definition.time_limit),
        actions = actions,
    )
}

/// `action` as the `Exec` element of a task's XML; nothing for an action that starts no program, which the
/// registration never writes.
fn exec_xml(action: &Action) -> String {
    let Action::Exec { path, arguments, directory } = action else {
        return String::new();
    };
    let directory = if directory.is_empty() {
        String::new()
    } else {
        format!("      <WorkingDirectory>{}</WorkingDirectory>\n", xml_escaped(directory))
    };
    format!(
        "    <Exec>\n      <Command>{}</Command>\n      <Arguments>{}</Arguments>\n{directory}    </Exec>\n",
        xml_escaped(path),
        xml_escaped(arguments)
    )
}

/// `text` with each of XML's markup characters, `&`, `<`, `>`, `"` and `'`, written as its entity.
fn xml_escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// What the thread that registers the task needs, owned.
struct Registration {
    folder: String,
    name: String,
    expected: Expected,
}

/// Registers the task `path`, such as `\gate\desktop-helper`, with the definition [`run_task`] checks for: running
/// `helper` with `$(Arg0)`, as the user whose SID is `user_sid`, the task and its folder owned by Administrators with a
/// protected DACL that lets that user only read them. A folder that does not exist is made with that descriptor; one
/// that exists must pass the check [`run_task`] makes of it, and a task that exists must have an owner and a DACL that
/// pass it, or the registration is refused and neither is changed. A task that passes is updated in place. Once
/// registered, the task is read back and checked as [`run_task`] checks it, without being started. It never deletes,
/// stops or starts a task. The Task Scheduler is asked on a thread of its own, which this waits for.
///
/// # Errors
///
/// [`TaskError::SetUpWrong`] naming why a folder or a task that exists is refused; [`TaskError::Failed`] naming the
/// call that failed and its HRESULT, a task read back that fails the check, a helper path that is not Unicode or holds
/// a `%`, or a path that holds no task name; [`TaskError::TimedOut`] when the Task Scheduler did not answer by
/// `deadline`, the thread that asked it left to finish on its own; [`TaskError::Unanswered`] when the wait failed or
/// that thread panicked.
pub fn register_task(path: &str, helper: &Path, user_sid: &str, deadline: Instant) -> Result<(), TaskError> {
    let Some((folder, name)) = split_task_path(path) else {
        return Err(TaskError::failed(&format!("{} is not a task path, \\<folder>\\<name>", shown(path))));
    };
    let registration = Registration {
        folder: folder.to_owned(),
        name: name.to_owned(),
        expected: Expected {
            helper: helper.to_owned(),
            user_sid: user_sid.to_owned(),
        },
    };
    let thread = std::thread::Builder::new()
        .name("gate-task-register".to_owned())
        .spawn(move || register_on_this_thread(&registration))
        .map_err(|error| TaskError::failed(&format!("the thread that registers the task could not be made: {error}")))?;
    let waited = wait_any(&[thread.as_raw_handle()], deadline);
    answered(waited, thread, "registers the task")
}

/// The task registered on this thread: COM joined, then the work, whose interfaces are all released when it returns,
/// before COM is left.
fn register_on_this_thread(registration: &Registration) -> Result<(), TaskError> {
    let library = task_library()?;
    let _apartment = Apartment::join()?;
    register_checked(library, registration)
}

/// Finds or makes the folder, refuses a folder or a task whose security fails the check, registers the task, then reads
/// it back and checks it.
fn register_checked(library: &SystemLibrary, registration: &Registration) -> Result<(), TaskError> {
    let Expected { helper, user_sid } = &registration.expected;
    let definition = wanted_definition(helper, user_sid)?;
    let sddl = security_sddl(user_sid);
    let service = connected(library)?;
    let folder = wanted_folder(&service, &registration.folder, &sddl)?;
    refuse_unsafe_task(&folder, &registration.name)?;
    let flags = TASK_CREATE_OR_UPDATE.0 | TASK_DONT_ADD_PRINCIPAL_ACE.0;
    register_definition(&folder, &registration.name, &task_xml(&definition), user_sid, &sddl, flags)?;
    match checked_task(&service, &registration.folder, &registration.name, &registration.expected) {
        Ok(_) => Ok(()),
        Err(TaskError::SetUpWrong(what)) => Err(TaskError::failed(&format!("the task read back is set up wrong: {what}"))),
        Err(TaskError::NotRegistered) => Err(TaskError::failed("the task read back is not registered")),
        Err(other) => Err(other),
    }
}

/// Registers the task `name` of `folder` from the XML `xml` with `flags`, as the user whose SID is `user_sid` with the
/// interactive token and no password, secured by the descriptor string `sddl`; the user and the descriptor go in
/// variants of [`handed_variant`].
fn register_definition(folder: &ITaskFolder, name: &str, xml: &str, user_sid: &str, sddl: &str, flags: i32) -> Result<(), TaskError> {
    // SAFETY: both BSTRs outlive the call; the two string variants each own a string this side never frees, and the
    // empty variant passes no password, as an interactive token needs none.
    unsafe {
        folder.RegisterTask(
            &BSTR::from(name),
            &BSTR::from(xml),
            flags,
            &handed_variant(user_sid),
            &VARIANT::default(),
            TASK_LOGON_INTERACTIVE_TOKEN,
            &handed_variant(sddl),
        )
    }
    .map(drop)
    .map_err(com_failed("RegisterTask"))
}

/// A variant holding a new BSTR of `text`, for a string variant passed to the Task Scheduler. With `OANOCACHE=1`,
/// `a_task_registered_run_and_read_on_one_thread_leaves_the_heap_whole` failed while `CreateFolder` and `RegisterTask`
/// took a variant borrowing a string this side also freed, and passes with this fresh string that this side never
/// frees: the variant type has no destructor and the string sits in a `ManuallyDrop` field. Which call frees the
/// string is not known; `RunEx`'s parameters take the same form by analogy, unmeasured. Where no call frees it, it is
/// a leak of that one string.
fn handed_variant(text: &str) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: the variant is set to hold a BSTR it alone points to, in a `ManuallyDrop` field nothing on this side drops.
    unsafe {
        let value = &mut variant.Anonymous.Anonymous;
        value.vt = VT_BSTR;
        value.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(text));
    }
    variant
}

/// The folder `folder`: made with the descriptor `sddl` when it does not exist; when it does, refused unless its
/// security passes the check.
fn wanted_folder(service: &ITaskService, folder: &str, sddl: &str) -> Result<ITaskFolder, TaskError> {
    // SAFETY: the folder's path is a valid BSTR for the call.
    match unsafe { service.GetFolder(&BSTR::from(folder)) } {
        Ok(found) => checked_folder(found, folder),
        Err(error) if is_not_found(&error) => {
            // SAFETY: the root's path is a valid BSTR for the call.
            let root = unsafe { service.GetFolder(&BSTR::from("\\")) }.map_err(com_failed("GetFolder"))?;
            // SAFETY: the name is a valid BSTR, and the variant owns a string this side never frees.
            match unsafe { root.CreateFolder(&BSTR::from(folder.trim_start_matches('\\')), &handed_variant(sddl)) } {
                Ok(made) => Ok(made),
                // Another registration made it between the look and the make: it is checked as one found would be.
                Err(error) if error.code().0 == ALREADY_EXISTS => {
                    // SAFETY: the folder's path is a valid BSTR for the call.
                    let found = unsafe { service.GetFolder(&BSTR::from(folder)) }.map_err(com_failed("GetFolder"))?;
                    checked_folder(found, folder)
                }
                Err(error) => Err(com_failed("CreateFolder")(error)),
            }
        }
        Err(error) => Err(com_failed("GetFolder")(error)),
    }
}

/// `found`, the folder `folder`, once its security passes the check; else the refusal naming why.
fn checked_folder(found: ITaskFolder, folder: &str) -> Result<ITaskFolder, TaskError> {
    let (owner, dacl) = read_folder_security(&found)?;
    check_folder(owner.as_deref(), dacl.as_deref(), folder).map_err(|what| TaskError::set_up_wrong(&what))?;
    Ok(found)
}

/// Refuses the task `name` of `folder` when it exists and its owner or DACL fails the check; a task that does not exist,
/// or one whose security passes, is left to the registration to make or update.
fn refuse_unsafe_task(folder: &ITaskFolder, name: &str) -> Result<(), TaskError> {
    // SAFETY: the task's name is a valid BSTR for the call.
    let task = match unsafe { folder.GetTask(&BSTR::from(name)) } {
        Ok(task) => task,
        Err(error) if is_not_found(&error) => return Ok(()),
        Err(error) => return Err(com_failed("GetTask")(error)),
    };
    let mut security = Definition::default();
    read_security(&task, &mut security)?;
    check_owner(security.owner.as_deref())
        .and_then(|()| check_dacl(security.dacl.as_deref()))
        .map_err(|what| TaskError::set_up_wrong(&what))
}

/// The error of a failed registration call `call`: the call's name, the system's message and the HRESULT.
fn com_failed(call: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| TaskError::failed(&format!("{call}: {} (0x{:08X})", shown(&error.message()), error.code().0))
}

/// Runs `task` in the request's session, as no named user and ignoring its constraints, with the request's argument
/// as its one parameter, and reads the process it started; runs nothing once the request is cancelled or its deadline
/// has passed.
fn run(task: &IRegisteredTask, request: &Request) -> Result<StartedTask, TaskError> {
    if request.cancel.load(Ordering::SeqCst) || Instant::now() >= request.deadline {
        return Err(TaskError::failed("the start was given up before the task was run"));
    }
    let parameters = handed_variant(&request.argument);
    let flags = TASK_RUN_USE_SESSION_ID.0 | TASK_RUN_IGNORE_CONSTRAINTS.0;
    // SAFETY: the variant owns a string this side never frees, and the empty user name asks for the task's own principal.
    let running = unsafe { task.RunEx(&parameters, flags, request.session_id.cast_signed(), &BSTR::new()) }.map_err(failed("run the task"))?;
    let by = Instant::now()
        .checked_add(request.pid_wait)
        .map_or(request.deadline, |end| end.min(request.deadline));
    engine_pid(&running, by)
}

/// What the pid poll reads of a started instance.
trait Instance {
    /// Takes a new snapshot of the instance, which the reads after it give.
    fn refresh(&self) -> windows::core::Result<()>;
    /// The instance's state in the last snapshot.
    fn state(&self) -> windows::core::Result<TASK_STATE>;
    /// The id of the instance's process in the last snapshot; 0 for none.
    fn pid(&self) -> windows::core::Result<u32>;
}

impl Instance for IRunningTask {
    fn refresh(&self) -> windows::core::Result<()> {
        // SAFETY: the running task's interface is valid for the call.
        unsafe { self.Refresh() }
    }

    fn state(&self) -> windows::core::Result<TASK_STATE> {
        // SAFETY: as above.
        unsafe { self.State() }
    }

    fn pid(&self) -> windows::core::Result<u32> {
        // SAFETY: as above.
        unsafe { self.EnginePID() }
    }
}

/// The process of the started instance `running`, read after a refresh, and again while it reads 0 and the instance
/// has not ended, until `by`; [`StartedTask::NoPid`] when none is given by then, or the instance ended without one. A
/// start given up goes on polling until `by`, which bounds it, since the cancel flag is not passed here.
fn engine_pid(running: &impl Instance, by: Instant) -> Result<StartedTask, TaskError> {
    loop {
        running.refresh().map_err(unseen("refresh the started task"))?;
        // Both reads come from the snapshot the refresh just took.
        let state = running.state().map_err(unseen("read the started task's state"))?;
        let pid = running.pid().map_err(unseen("read the started task's process"))?;
        if let Some(pid) = NonZeroU32::new(pid) {
            return Ok(StartedTask::Process(pid));
        }
        let now = Instant::now();
        if has_ended(state) || now >= by {
            return Ok(StartedTask::NoPid);
        }
        std::thread::sleep(PID_LOOK_STEP.min(by - now));
    }
}

/// Whether an instance in `state` has ended: the task reads ready, or disabled, once no instance of it runs. Queued,
/// running and unknown are waited on.
fn has_ended(state: TASK_STATE) -> bool {
    state == TASK_STATE_READY || state == TASK_STATE_DISABLED
}

/// What the check reads of a registered task's definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent setting of the definition, read and checked on its own"
)]
struct Definition {
    /// Whether the task and its settings are both enabled.
    enabled: bool,
    /// Whether the task may be started on demand.
    allow_demand_start: bool,
    /// Whether a second instance runs in parallel with the first.
    parallel: bool,
    /// The task's priority, 0 (realtime) to 10 (idle).
    priority: i32,
    /// How many times the Task Scheduler restarts the task after a failure (`RestartCount`).
    restart_count: i32,
    /// Whether the task has maintenance settings (`MaintenanceSettings`), so it may run during automatic maintenance.
    maintenance: bool,
    /// Whether the task is kept from starting on batteries (`DisallowStartIfOnBatteries`).
    no_start_on_batteries: bool,
    /// Whether the task is stopped when the machine goes on batteries (`StopIfGoingOnBatteries`).
    stop_on_batteries: bool,
    /// The task's execution time limit, as the Task Scheduler writes it (`PT0S` for none).
    time_limit: String,
    /// How many triggers the task has.
    triggers: i32,
    /// The task's actions, in order.
    actions: Vec<Action>,
    /// Whether the task runs with the interactive token of the user logged on.
    interactive_token: bool,
    /// Whether the task runs at the least privilege (`TASK_RUNLEVEL_LUA`).
    least_privilege: bool,
    /// The principal's user, as the task names it.
    user: String,
    /// The SID that user names; `None` when it names no account.
    user_sid: Option<String>,
    /// The SID of the task's owner, which may change its DACL; `None` for a security descriptor that names none.
    owner: Option<String>,
    /// The entries of the task's DACL; `None` for a task with no DACL, which anyone may change and run.
    dacl: Option<Vec<Entry>>,
}

/// One action of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    /// An action that starts `path` with `arguments`, in the folder `directory` (empty for none).
    Exec { path: String, arguments: String, directory: String },
    /// Any other kind of action.
    Other,
}

/// The first thing of `definition` that differs from the one the desktop helper's task must have; `Ok` when nothing
/// does.
fn check_definition(definition: &Definition, expected: &Expected) -> Result<(), String> {
    check_settings(definition)?;
    check_limits(definition)?;
    check_action(&definition.actions, &expected.helper)?;
    check_principal(definition, &expected.user_sid)?;
    check_owner(definition.owner.as_deref())?;
    check_dacl(definition.dacl.as_deref())
}

/// The first of the task's run settings that differs.
fn check_settings(definition: &Definition) -> Result<(), String> {
    if !definition.enabled {
        return Err("it is disabled".to_owned());
    }
    if !definition.allow_demand_start {
        return Err("it may not be started on demand".to_owned());
    }
    if !definition.parallel {
        return Err("a second instance does not run in parallel".to_owned());
    }
    if definition.priority != TASK_PRIORITY {
        return Err(format!("its priority is {}, not {TASK_PRIORITY}", definition.priority));
    }
    if definition.restart_count != 0 {
        return Err(format!(
            "it restarts after a failure (RestartCount {}), not never",
            definition.restart_count
        ));
    }
    if definition.maintenance {
        return Err("it has maintenance settings (MaintenanceSettings), not none".to_owned());
    }
    Ok(())
}

/// The first of the task's limits that differs: its battery settings, its time limit and its triggers.
fn check_limits(definition: &Definition) -> Result<(), String> {
    if definition.no_start_on_batteries {
        return Err("it may not start on batteries (DisallowStartIfOnBatteries)".to_owned());
    }
    if definition.stop_on_batteries {
        return Err("it stops when the machine goes on batteries (StopIfGoingOnBatteries)".to_owned());
    }
    if definition.time_limit != NO_TIME_LIMIT {
        return Err(format!(
            "its execution time limit (ExecutionTimeLimit) is {}, not {NO_TIME_LIMIT}",
            shown(&definition.time_limit)
        ));
    }
    if definition.triggers != 0 {
        return Err(format!("it has {} triggers, not none", definition.triggers));
    }
    Ok(())
}

/// The first thing of `actions` that differs from one action running `helper` with the arguments `$(Arg0)` and no
/// working folder. One pair of quotes around the action's path is taken off; anything else, `%VAR%` among it, is
/// compared as written, and a path that holds a `%` is refused even when it is the helper's, since the Task Scheduler
/// expands it from the environment of the task's user, who can change it.
fn check_action(actions: &[Action], helper: &Path) -> Result<(), String> {
    let [action] = actions else {
        return Err(format!("it has {} actions, not one", actions.len()));
    };
    let Action::Exec { path, arguments, directory } = action else {
        return Err("its action does not start a program".to_owned());
    };
    let unquoted = path.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(path);
    if !same_path_text(unquoted, helper) {
        return Err(format!("its action runs {}, not {}", shown(path), shown(&helper.display().to_string())));
    }
    if unquoted.contains('%') {
        return Err(format!(
            "its action's program {} holds a %, which the Task Scheduler expands from its user's environment",
            shown(path)
        ));
    }
    if arguments != ARG0_ARGUMENTS {
        return Err(format!("its action's arguments are {}, not {ARG0_ARGUMENTS}", shown(arguments)));
    }
    if !directory.is_empty() {
        return Err(format!("its action's working directory is {}, not none", shown(directory)));
    }
    Ok(())
}

/// The first thing of the task's principal that differs from the interactive token, at the least privilege, of the
/// user whose SID is `user_sid`.
fn check_principal(definition: &Definition, user_sid: &str) -> Result<(), String> {
    if !definition.interactive_token {
        return Err("it does not run with the interactive token of the user logged on".to_owned());
    }
    if !definition.least_privilege {
        return Err("it runs with the highest privileges, not the least (RunLevel)".to_owned());
    }
    match &definition.user_sid {
        Some(sid) if sid.eq_ignore_ascii_case(user_sid) => Ok(()),
        _ => Err(format!("its user (UserId) is {}, not the user running gate", shown(&definition.user))),
    }
}

/// Refuses an owner of the task, `owner`, that is not SYSTEM or Administrators, since an owner may rewrite the DACL
/// whatever it says; and a security descriptor that names no owner.
fn check_owner(owner: Option<&str>) -> Result<(), String> {
    match owner {
        Some(sid) if is_trusted(sid) => Ok(()),
        Some(sid) => Err(format!("its owner ({}) may change who can run it", shown(sid))),
        None => Err("its security descriptor names no owner".to_owned()),
    }
}

/// The first entry of the task's DACL, `dacl`, that allows anyone but SYSTEM and Administrators a right outside
/// [`READ_ONLY`]; an entry of a type the check does not read is refused too, and so is no DACL. An inherit-only entry
/// grants nothing on the object itself, so it is passed over whatever it holds.
fn check_dacl(dacl: Option<&[Entry]>) -> Result<(), String> {
    let Some(entries) = dacl else {
        return Err("its security descriptor has no DACL, so anyone may change or run it".to_owned());
    };
    for entry in entries.iter().filter(|entry| !is_inherit_only(entry)) {
        match entry {
            Entry::Allowed { mask, sid, .. } if mask & !READ_ONLY != 0 && !is_trusted(sid) => {
                return Err(format!(
                    "its security descriptor lets {} do more than read it (access 0x{:x})",
                    shown(sid),
                    mask & !READ_ONLY
                ));
            }
            Entry::Other { kind, .. } if u32::from(*kind) != ACCESS_DENIED_ACE_TYPE => {
                return Err(format!(
                    "its security descriptor holds an entry of type {kind}, which the check does not read"
                ));
            }
            Entry::Allowed { .. } | Entry::Label { .. } | Entry::Other { .. } => {}
        }
    }
    Ok(())
}

/// Refuses the task's folder `folder`, whose security descriptor names the owner `owner` and holds the DACL `dacl`,
/// as [`check_owner`] and [`check_dacl`] refuse the task's own: whoever may change the folder may replace the task.
fn check_folder(owner: Option<&str>, dacl: Option<&[Entry]>, folder: &str) -> Result<(), String> {
    check_owner(owner)
        .and_then(|()| check_dacl(dacl))
        .map_err(|what| format!("its folder ({}): {what}", shown(folder)))
}

/// Whether `sid` may change and run the task: SYSTEM or Administrators. The user running gate is not, since a process
/// of theirs at medium could change the task between its check and its run.
fn is_trusted(sid: &str) -> bool {
    TRUSTED_SIDS.iter().any(|trusted| sid.eq_ignore_ascii_case(trusted))
}

/// Whether `entry` is inherit-only (`INHERIT_ONLY_ACE`): it applies to the objects made inside, not to this one.
fn is_inherit_only(entry: &Entry) -> bool {
    let (Entry::Allowed { flags, .. } | Entry::Label { flags, .. } | Entry::Other { flags, .. }) = entry;
    u32::from(*flags) & INHERIT_ONLY_ACE != 0
}

/// Whether the path text `text` names `path` to Windows: equal ordinally without regard to case.
fn same_path_text(text: &str, path: &Path) -> bool {
    let left: Vec<u16> = text.encode_utf16().collect();
    let right: Vec<u16> = path.as_os_str().encode_wide().collect();
    crate::launch::ordinal_equal_ignore_case(&left, &right)
}

/// `text`, read from a task, as an error may show it: [`escaped`], and cut to [`SHOWN_LIMIT`] characters, `...`
/// marking a cut, so it is one line that says what it holds.
fn shown(text: &str) -> String {
    let escaped = escaped(text);
    if escaped.chars().count() <= SHOWN_LIMIT {
        return escaped;
    }
    let mut cut: String = escaped.chars().take(SHOWN_LIMIT).collect();
    cut.push_str("...");
    cut
}

/// `text` with each character that could break or forge a line, or hide or reorder part of it, escaped as `\u{…}`:
/// every control character (Cc), the line and paragraph separators, and every format character (Cf), the zero-width
/// characters and the bidirectional marks, embeddings, overrides and isolates among them. Escaping twice changes
/// nothing more.
fn escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') || is_format(character) {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Whether `character` is in Unicode's format category (Cf).
fn is_format(character: char) -> bool {
    matches!(
        character,
        '\u{ad}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// What the check needs of `task`'s registered definition.
fn read_definition(task: &IRegisteredTask) -> Result<Definition, TaskError> {
    let read = failed("read the task's definition");
    // SAFETY: the interface is valid for each call.
    let (definition, enabled) = unsafe { (task.Definition().map_err(&read)?, task.Enabled().map_err(&read)?) };
    let mut checked = Definition {
        enabled: enabled.0 != 0,
        ..Definition::default()
    };
    read_settings(&definition, &mut checked)?;
    read_principal(&definition, &mut checked)?;
    read_actions(&definition, &mut checked)?;
    read_security(task, &mut checked)?;
    Ok(checked)
}

/// Reads the settings of `definition` into `into`; its enabled flag is kept only when the settings' is set too.
fn read_settings(definition: &ITaskDefinition, into: &mut Definition) -> Result<(), TaskError> {
    let read = failed("read the task's settings");
    let set = |flag: VARIANT_BOOL| flag.0 != 0;
    // SAFETY: each interface is valid for its call, and each out pointer is to a local of the call's type.
    unsafe {
        let settings = definition.Settings().map_err(&read)?;
        let mut flag = VARIANT_BOOL::default();
        settings.Enabled(&raw mut flag).map_err(&read)?;
        into.enabled &= set(flag);
        settings.AllowDemandStart(&raw mut flag).map_err(&read)?;
        into.allow_demand_start = set(flag);
        settings.DisallowStartIfOnBatteries(&raw mut flag).map_err(&read)?;
        into.no_start_on_batteries = set(flag);
        settings.StopIfGoingOnBatteries(&raw mut flag).map_err(&read)?;
        into.stop_on_batteries = set(flag);
        let mut policy = TASK_INSTANCES_POLICY::default();
        settings.MultipleInstances(&raw mut policy).map_err(&read)?;
        into.parallel = policy == TASK_INSTANCES_PARALLEL;
        settings.Priority(&raw mut into.priority).map_err(&read)?;
        settings.RestartCount(&raw mut into.restart_count).map_err(&read)?;
        let mut limit = BSTR::new();
        settings.ExecutionTimeLimit(&raw mut limit).map_err(&read)?;
        into.time_limit = limit.to_string();
        into.maintenance = has_maintenance_settings(&settings)?;
    }
    Ok(())
}

/// Whether `settings` hold maintenance settings. The getter succeeds with no object for a task that has none, which
/// the `windows` crate turns into an error whose code is `S_OK`.
fn has_maintenance_settings(settings: &ITaskSettings) -> Result<bool, TaskError> {
    let read = failed("read the task's maintenance settings");
    let settings: ITaskSettings3 = settings.cast().map_err(&read)?;
    // SAFETY: the interface is valid for the call.
    match unsafe { settings.MaintenanceSettings() } {
        Ok(_) => Ok(true),
        Err(error) if error.code().is_ok() => Ok(false),
        Err(error) => Err(read(error)),
    }
}

/// Reads the principal of `definition` into `into`, with the SID its user names.
fn read_principal(definition: &ITaskDefinition, into: &mut Definition) -> Result<(), TaskError> {
    let read = failed("read the task's principal");
    // SAFETY: each interface is valid for its call, and each out pointer is to a local of the call's type.
    unsafe {
        let principal = definition.Principal().map_err(&read)?;
        let mut logon = TASK_LOGON_TYPE::default();
        principal.LogonType(&raw mut logon).map_err(&read)?;
        let mut level = TASK_RUNLEVEL_TYPE::default();
        principal.RunLevel(&raw mut level).map_err(&read)?;
        let mut user = BSTR::new();
        principal.UserId(&raw mut user).map_err(&read)?;
        into.interactive_token = logon == TASK_LOGON_INTERACTIVE_TOKEN;
        into.least_privilege = level == TASK_RUNLEVEL_LUA;
        into.user = user.to_string();
    }
    into.user_sid = sid_of_account(&into.user)?;
    Ok(())
}

/// Reads the actions and the count of triggers of `definition` into `into`.
fn read_actions(definition: &ITaskDefinition, into: &mut Definition) -> Result<(), TaskError> {
    let read = failed("read the task's actions and triggers");
    // SAFETY: each interface is valid for its call, and each out pointer is to a local of the call's type.
    unsafe {
        let collection = definition.Actions().map_err(&read)?;
        let mut count = 0;
        collection.Count(&raw mut count).map_err(&read)?;
        // The collection counts from 1.
        into.actions = (1..=count)
            .map(|index| read_action(&collection.get_Item(index).map_err(&read)?))
            .collect::<Result<_, _>>()?;
        definition.Triggers().map_err(&read)?.Count(&raw mut into.triggers).map_err(&read)?;
    }
    Ok(())
}

/// What the check needs of one action: its program, arguments and working folder when it starts one.
fn read_action(action: &IAction) -> Result<Action, TaskError> {
    let read = failed("read the task's action");
    // SAFETY: each interface is valid for its call, and each out pointer is to a local of the call's type.
    unsafe {
        let mut kind = TASK_ACTION_TYPE::default();
        action.Type(&raw mut kind).map_err(&read)?;
        if kind != TASK_ACTION_EXEC {
            return Ok(Action::Other);
        }
        let exec: IExecAction = action.cast().map_err(&read)?;
        let (mut path, mut arguments, mut directory) = (BSTR::new(), BSTR::new(), BSTR::new());
        exec.Path(&raw mut path).map_err(&read)?;
        exec.Arguments(&raw mut arguments).map_err(&read)?;
        exec.WorkingDirectory(&raw mut directory).map_err(&read)?;
        Ok(Action::Exec {
            path: path.to_string(),
            arguments: arguments.to_string(),
            directory: directory.to_string(),
        })
    }
}

/// Reads the owner and the DACL of `task`'s security descriptor into `into`.
fn read_security(task: &IRegisteredTask, into: &mut Definition) -> Result<(), TaskError> {
    // SAFETY: the interface is valid for the call.
    let sddl = unsafe { task.GetSecurityDescriptor(OWNER_AND_DACL.cast_signed()) }.map_err(failed("read the task's security descriptor"))?;
    (into.owner, into.dacl) = owner_and_dacl(&sddl, "the task's")?;
    Ok(())
}

/// The owner and the DACL of `folder`'s security descriptor.
fn read_folder_security(folder: &ITaskFolder) -> Result<(Option<String>, Option<Vec<Entry>>), TaskError> {
    // SAFETY: the interface is valid for the call.
    let sddl = unsafe { folder.GetSecurityDescriptor(OWNER_AND_DACL.cast_signed()) }.map_err(failed("read the task folder's security descriptor"))?;
    owner_and_dacl(&sddl, "the task folder's")
}

/// The owner and the DACL the descriptor string `sddl` of `whose` security descriptor names.
fn owner_and_dacl(sddl: &BSTR, whose: &str) -> Result<(Option<String>, Option<Vec<Entry>>), TaskError> {
    let unreadable = |error: io::Error| TaskError::failed(&format!("{whose} security descriptor could not be read: {error}"));
    let descriptor = SecurityDescriptor::from_sddl(&sddl.to_string()).map_err(unreadable)?;
    Ok((owner_of(&descriptor).map_err(unreadable)?, dacl_of(&descriptor).map_err(unreadable)?))
}

/// The SID of the owner `descriptor` names; `None` when it names none.
fn owner_of(descriptor: &SecurityDescriptor) -> io::Result<Option<String>> {
    let (mut owner, mut defaulted): (PSID, _) = (std::ptr::null_mut(), 0);
    // SAFETY: the descriptor is valid for as long as `descriptor` lives, and every out pointer is to a local.
    if unsafe { GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() {
        return Ok(None);
    }
    // SAFETY: `owner` points into the descriptor, which outlives the read.
    unsafe { crate::token::sid_string(owner) }.map(Some)
}

/// The entries of the DACL of `descriptor`; `None` when it has none.
fn dacl_of(descriptor: &SecurityDescriptor) -> io::Result<Option<Vec<Entry>>> {
    let (mut present, mut defaulted, mut acl) = (0, 0, std::ptr::null_mut::<ACL>());
    // SAFETY: the descriptor is valid for as long as `descriptor` lives, and every out pointer is to a local.
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &raw mut present, &raw mut acl, &raw mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || acl.is_null() {
        return Ok(None);
    }
    // SAFETY: `acl` points into the descriptor, which outlives the read.
    unsafe { entries_of(acl) }.map(Some)
}

/// The string form of the SID the account `name` names: `name` itself when it is a SID's string form; `None` when it
/// is empty or names no account.
fn sid_of_account(name: &str) -> Result<Option<String>, TaskError> {
    if is_sid_string(name) {
        return Ok(Some(name.to_owned()));
    }
    if name.is_empty() {
        return Ok(None);
    }
    let lookup_failed = |error: io::Error| TaskError::failed(&format!("the task's user {} could not be looked up: {error}", shown(name)));
    let wide = crate::wide(name);
    let (mut sid_size, mut domain_size, mut kind) = (0_u32, 0_u32, SID_NAME_USE::default());
    // SAFETY: `wide` is NUL-terminated and outlives the call; with null buffers of size 0 the call only writes the
    // sizes it needs, into the locals given.
    let sized = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null_mut(),
            &raw mut sid_size,
            std::ptr::null_mut(),
            &raw mut domain_size,
            &raw mut kind,
        )
    };
    let asked = io::Error::last_os_error();
    match asked.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
        _ if sized != 0 => return Err(lookup_failed(io::Error::other("the lookup gave no size"))),
        Some(ERROR_NONE_MAPPED) => return Ok(None),
        Some(ERROR_INSUFFICIENT_BUFFER) => {}
        _ => return Err(lookup_failed(asked)),
    }
    // A SID is read in 4-byte units, so its buffer is of them.
    let mut sid = vec![0_u32; (sid_size as usize).div_ceil(4)];
    let mut domain = vec![0_u16; domain_size as usize];
    // SAFETY: as above, with buffers of at least the sizes the first call asked for, which outlive the call.
    let looked_up = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide.as_ptr(),
            sid.as_mut_ptr().cast(),
            &raw mut sid_size,
            domain.as_mut_ptr(),
            &raw mut domain_size,
            &raw mut kind,
        )
    };
    if looked_up == 0 {
        return Err(lookup_failed(io::Error::last_os_error()));
    }
    // SAFETY: the call wrote a whole SID into `sid`, which outlives the read.
    unsafe { crate::token::sid_string(sid.as_mut_ptr().cast()) }
        .map(Some)
        .map_err(lookup_failed)
}

/// The error of a failed call that did `what`, before anything was started, with the system's message and the HRESULT.
fn failed(what: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| TaskError::failed(&call_failed(what, &error))
}

/// As [`failed`], for a call made once the task was started: [`TaskError::StartedUnknown`].
fn unseen(what: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| TaskError::started_unknown(&call_failed(what, &error))
}

/// As [`failed`], but a task or folder that does not exist is [`TaskError::NotRegistered`].
fn missing(what: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| {
        if is_not_found(&error) {
            TaskError::NotRegistered
        } else {
            failed(what)(error)
        }
    }
}

/// Whether `error` says the task or the folder asked for does not exist: [`TASK_NOT_FOUND`] or [`FOLDER_NOT_FOUND`].
fn is_not_found(error: &windows::core::Error) -> bool {
    matches!(error.code().0, TASK_NOT_FOUND | FOLDER_NOT_FOUND)
}

/// The text of a failed call that did `what`.
fn call_failed(what: &str, error: &windows::core::Error) -> String {
    format!(
        "the Task Scheduler could not {what}: {} (0x{:08X})",
        shown(&error.message()),
        error.code().0
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose setup fails must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::num::NonZeroU32;
    use std::os::windows::io::{AsHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::System::TaskScheduler::{TASK_STATE, TASK_STATE_DISABLED, TASK_STATE_QUEUED, TASK_STATE_READY, TASK_STATE_RUNNING};
    use windows::core::HRESULT;
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::{
        Action, Definition, Entry, Expected, FOLDER_NOT_FOUND, Instance, StartedTask, TASK_NOT_FOUND, TaskArgument, TaskError, TaskStart, answered,
        check_action, check_definition, check_folder, check_loaded_from, engine_pid, failed, given_up, is_not_found, missing, register_task,
        run_task, split_task_path, task_xml, unseen, wanted_definition, wanted_security,
    };
    use crate::pipe::Signalled;

    /// The longest a test waits for anything: it never runs out unless the code is broken.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    const HELPER: &str = r"C:\Users\someone\AppData\Local\gate\bin\gate-desktop-helper.exe";

    const USER: &str = "S-1-5-21-1-2-3-1001";

    /// What the definitions of these tests are checked against.
    fn expected() -> Expected {
        Expected {
            helper: PathBuf::from(HELPER),
            user_sid: USER.to_owned(),
        }
    }

    /// An entry allowing `sid` the access `mask`.
    fn allowed(sid: &str, mask: u32) -> Entry {
        Entry::Allowed {
            flags: 0,
            mask,
            sid: sid.to_owned(),
        }
    }

    /// An action running `path` with `arguments` and no working folder.
    fn exec(path: &str, arguments: &str) -> Action {
        Action::Exec {
            path: path.to_owned(),
            arguments: arguments.to_owned(),
            directory: String::new(),
        }
    }

    /// The definition the desktop helper's task must have.
    fn good() -> Definition {
        Definition {
            enabled: true,
            allow_demand_start: true,
            parallel: true,
            priority: 4,
            restart_count: 0,
            maintenance: false,
            no_start_on_batteries: false,
            stop_on_batteries: false,
            time_limit: "PT0S".to_owned(),
            triggers: 0,
            actions: vec![exec(HELPER, "$(Arg0)")],
            interactive_token: true,
            least_privilege: true,
            user: r"BUILDER\someone".to_owned(),
            user_sid: Some(USER.to_owned()),
            owner: Some("S-1-5-32-544".to_owned()),
            dacl: Some(vec![
                allowed("S-1-5-18", 0x001f_01ff),
                allowed("S-1-5-32-544", 0x001f_01ff),
                allowed(USER, 0x0012_0089),
                allowed("S-1-5-11", 0x0012_0089),
                allowed("S-1-1-0", 0x8000_0000),
                Entry::Other { kind: 1, flags: 0 },
            ]),
        }
    }

    /// The mismatch the check names for `definition`.
    fn mismatch(definition: &Definition) -> String {
        check_definition(definition, &expected()).err().unwrap_or_default()
    }

    /// A start of the task `path` in session 1, for the helper of these tests.
    fn start_of<'a>(path: &'a str, argument: &'a TaskArgument) -> TaskStart<'a> {
        TaskStart {
            path,
            argument,
            session_id: 1,
            helper: Path::new(HELPER),
            pid_wait: Duration::from_secs(1),
        }
    }

    #[test]
    fn the_definition_the_helper_task_must_have_passes() {
        assert_eq!(check_definition(&good(), &expected()), Ok(()));
        for path in [HELPER.to_uppercase(), format!("\"{HELPER}\"")] {
            let other = Definition {
                actions: vec![exec(&path, "$(Arg0)")],
                ..good()
            };

            assert_eq!(
                check_definition(&other, &expected()),
                Ok(()),
                "{path} is compared without regard to case, one pair of quotes taken off"
            );
        }
        let sid_named = Definition {
            user: USER.to_lowercase(),
            ..good()
        };
        assert_eq!(
            check_definition(&sid_named, &expected()),
            Ok(()),
            "the user's SID is compared without regard to case"
        );
        for owner in ["S-1-5-18", "S-1-5-32-544"] {
            let owned = Definition {
                owner: Some(owner.to_owned()),
                ..good()
            };
            assert_eq!(check_definition(&owned, &expected()), Ok(()), "{owner} may own the task");
        }
    }

    #[test]
    fn a_task_that_restarts_after_a_failure_is_set_up_wrong() {
        let restarting = Definition { restart_count: 3, ..good() };

        assert_eq!(mismatch(&restarting), "it restarts after a failure (RestartCount 3), not never");
    }

    #[test]
    fn a_task_with_maintenance_settings_is_set_up_wrong() {
        let maintained = Definition { maintenance: true, ..good() };

        assert_eq!(mismatch(&maintained), "it has maintenance settings (MaintenanceSettings), not none");
    }

    /// The DACL a `\gate` folder of SYSTEM and Administrators has, with the user and everyone else allowed to read it.
    fn folder_dacl() -> Vec<Entry> {
        vec![
            allowed("S-1-5-18", 0x001f_01ff),
            allowed("S-1-5-32-544", 0x001f_01ff),
            allowed(USER, 0x0012_0089),
            allowed("S-1-5-11", 0x0012_0089),
        ]
    }

    /// The entry every Task Scheduler folder carries: full control for CREATOR OWNER, object and container inherit,
    /// inherit-only, so it grants nothing on the folder itself.
    fn creator_owner_inherited() -> Entry {
        Entry::Allowed {
            flags: 0x0b,
            mask: 0x001f_01ff,
            sid: "S-1-3-0".to_owned(),
        }
    }

    #[test]
    fn an_inherit_only_entry_for_creator_owner_is_not_refused() {
        let mut dacl = folder_dacl();
        dacl.push(creator_owner_inherited());
        dacl.push(Entry::Other { kind: 5, flags: 0x08 });

        assert_eq!(check_folder(Some("S-1-5-32-544"), Some(&dacl), "\\gate"), Ok(()), "the folder");
        let task = Definition { dacl: Some(dacl), ..good() };
        assert_eq!(check_definition(&task, &expected()), Ok(()), "the task");
    }

    #[test]
    fn an_inherited_or_inheritable_entry_that_is_not_inherit_only_is_refused() {
        // INHERITED_ACE, then OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE: both apply to the object itself.
        for flags in [0x10, 0x03] {
            let mut dacl = folder_dacl();
            dacl.push(Entry::Allowed {
                flags,
                mask: 0x001f_01ff,
                sid: "S-1-5-21-9-9-9-1002".to_owned(),
            });
            let what = "its security descriptor lets S-1-5-21-9-9-9-1002 do more than read it (access 0xd0176)";

            let folder = check_folder(Some("S-1-5-32-544"), Some(&dacl), "\\gate");

            assert_eq!(folder, Err(format!("its folder (\\gate): {what}")), "the folder, flags 0x{flags:x}");
            let task = Definition { dacl: Some(dacl), ..good() };
            assert_eq!(mismatch(&task), what, "the task, flags 0x{flags:x}");
        }
    }

    #[test]
    fn a_task_the_user_running_gate_may_change_is_set_up_wrong() {
        let mut dacl = folder_dacl();
        dacl.push(allowed(USER, 0x001f_01ff));
        let writable = Definition { dacl: Some(dacl), ..good() };

        assert_eq!(
            mismatch(&writable),
            "its security descriptor lets S-1-5-21-1-2-3-1001 do more than read it (access 0xd0176)"
        );
        let owned = Definition {
            owner: Some(USER.to_owned()),
            ..good()
        };
        assert_eq!(mismatch(&owned), "its owner (S-1-5-21-1-2-3-1001) may change who can run it");
    }

    #[test]
    fn a_task_folder_owned_by_the_user_running_gate_is_set_up_wrong() {
        let refused = check_folder(Some(USER), Some(&folder_dacl()), "\\gate");

        assert_eq!(
            refused,
            Err("its folder (\\gate): its owner (S-1-5-21-1-2-3-1001) may change who can run it".to_owned())
        );
    }

    #[test]
    fn a_task_folder_owned_by_an_untrusted_account_is_set_up_wrong() {
        let refused = check_folder(Some("S-1-5-21-9-9-9-1002"), Some(&folder_dacl()), "\\gate");

        assert_eq!(
            refused,
            Err("its folder (\\gate): its owner (S-1-5-21-9-9-9-1002) may change who can run it".to_owned())
        );
        let unowned = check_folder(None, Some(&folder_dacl()), "\\gate");
        assert_eq!(unowned, Err("its folder (\\gate): its security descriptor names no owner".to_owned()));
    }

    #[test]
    fn a_task_folder_that_lets_an_untrusted_account_write_is_set_up_wrong() {
        let mut dacl = folder_dacl();
        dacl.push(allowed("S-1-5-11", 0x0012_0116));

        let refused = check_folder(Some("S-1-5-32-544"), Some(&dacl), "\\gate");

        assert_eq!(
            refused,
            Err("its folder (\\gate): its security descriptor lets S-1-5-11 do more than read it (access 0x116)".to_owned())
        );
    }

    #[test]
    fn a_task_folder_with_no_dacl_is_set_up_wrong() {
        let refused = check_folder(Some("S-1-5-32-544"), None, "\\ga\u{202e}te");

        assert_eq!(
            refused,
            Err("its folder (\\ga\\u{202e}te): its security descriptor has no DACL, so anyone may change or run it".to_owned())
        );
    }

    #[test]
    fn a_task_folder_of_system_administrators_and_the_user_passes() {
        for owner in ["S-1-5-18", "S-1-5-32-544"] {
            assert_eq!(
                check_folder(Some(owner), Some(&folder_dacl()), "\\gate"),
                Ok(()),
                "{owner} may own the folder"
            );
        }
    }

    #[test]
    fn each_setting_that_differs_is_named() {
        let cases = [
            (Definition { enabled: false, ..good() }, "disabled"),
            (
                Definition {
                    allow_demand_start: false,
                    ..good()
                },
                "on demand",
            ),
            (Definition { parallel: false, ..good() }, "parallel"),
            (Definition { priority: 7, ..good() }, "priority is 7, not 4"),
            (
                Definition {
                    no_start_on_batteries: true,
                    ..good()
                },
                "DisallowStartIfOnBatteries",
            ),
            (
                Definition {
                    stop_on_batteries: true,
                    ..good()
                },
                "StopIfGoingOnBatteries",
            ),
            (
                Definition {
                    time_limit: "PT72H".to_owned(),
                    ..good()
                },
                "(ExecutionTimeLimit) is PT72H, not PT0S",
            ),
            (Definition { triggers: 1, ..good() }, "1 triggers, not none"),
            (
                Definition {
                    interactive_token: false,
                    ..good()
                },
                "interactive token",
            ),
            (
                Definition {
                    least_privilege: false,
                    ..good()
                },
                "RunLevel",
            ),
            (
                Definition {
                    user_sid: Some("S-1-5-21-1-2-3-1002".to_owned()),
                    ..good()
                },
                r"(UserId) is BUILDER\someone, not the user running gate",
            ),
            (Definition { user_sid: None, ..good() }, "(UserId)"),
        ];
        for (definition, named) in cases {
            let why = mismatch(&definition);

            assert!(why.contains(named), "{definition:?} is refused for {named:?}: {why:?}");
        }
    }

    #[test]
    fn the_first_mismatch_is_the_one_named() {
        let definition = Definition {
            enabled: false,
            priority: 7,
            actions: Vec::new(),
            ..good()
        };

        assert_eq!(mismatch(&definition), "it is disabled");
    }

    #[test]
    fn an_action_other_than_one_run_of_the_helper_with_arg0_is_named() {
        let cases = [
            (Vec::new(), "0 actions, not one"),
            (vec![exec(HELPER, "$(Arg0)"), exec(HELPER, "$(Arg0)")], "2 actions, not one"),
            (vec![Action::Other], "does not start a program"),
            (vec![exec(r"C:\Windows\System32\cmd.exe", "$(Arg0)")], r"runs C:\Windows\System32\cmd.exe"),
            (vec![exec(&format!("\"{HELPER}"), "$(Arg0)")], "its action runs \""),
            (vec![exec(&format!("\"\"{HELPER}\"\""), "$(Arg0)")], "its action runs \"\""),
            (
                vec![exec(r"%LOCALAPPDATA%\gate\bin\gate-desktop-helper.exe", "$(Arg0)")],
                "its action runs %LOCALAPPDATA%",
            ),
            (vec![exec(HELPER, "$(Arg0) --more")], "arguments are $(Arg0) --more"),
            (vec![exec(HELPER, "")], "arguments are , not"),
            (
                vec![Action::Exec {
                    path: HELPER.to_owned(),
                    arguments: "$(Arg0)".to_owned(),
                    directory: r"C:\Users\someone".to_owned(),
                }],
                r"working directory is C:\Users\someone, not none",
            ),
        ];
        for (actions, named) in cases {
            let definition = Definition { actions, ..good() };

            let why = mismatch(&definition);

            assert!(why.contains(named), "{:?} is refused for {named:?}: {why:?}", definition.actions);
        }
    }

    #[test]
    fn a_security_descriptor_that_lets_another_do_more_than_read_the_task_is_named() {
        let with_dacl = |entries: Vec<Entry>| Definition {
            dacl: Some(entries),
            ..good()
        };
        let cases = [
            (Definition { dacl: None, ..good() }, "no DACL"),
            (
                with_dacl(vec![allowed("S-1-5-11", 0x0012_00a0)]),
                "lets S-1-5-11 do more than read it (access 0x20)",
            ),
            (
                with_dacl(vec![allowed("S-1-1-0", 0x1000_0000)]),
                "lets S-1-1-0 do more than read it (access 0x10000000)",
            ),
            (
                with_dacl(vec![allowed("S-1-5-21-1-2-3-1002", 0x0004_0000)]),
                "lets S-1-5-21-1-2-3-1002 do more than read it (access 0x40000)",
            ),
            (
                with_dacl(vec![allowed("S-1-1-0", 0x0100_0000)]),
                "lets S-1-1-0 do more than read it (access 0x1000000)",
            ),
            (
                with_dacl(vec![allowed("S-1-1-0", 0x0000_0400)]),
                "lets S-1-1-0 do more than read it (access 0x400)",
            ),
            (with_dacl(vec![Entry::Other { kind: 5, flags: 0 }]), "an entry of type 5"),
            (
                Definition {
                    owner: Some("S-1-5-21-1-2-3-1002".to_owned()),
                    ..good()
                },
                "its owner (S-1-5-21-1-2-3-1002) may change who can run it",
            ),
            (Definition { owner: None, ..good() }, "names no owner"),
        ];
        for (definition, named) in cases {
            let why = mismatch(&definition);

            assert!(
                why.contains(named),
                "{:?} owned by {:?} is refused for {named:?}: {why:?}",
                definition.dacl,
                definition.owner
            );
        }
    }

    #[test]
    fn task_text_with_a_line_break_or_bidi_control_is_shown_on_one_line_and_cut() {
        let forged = "C:\\x\ngate-remote-refused: busy\r\n\u{202e}exe.\u{2028}gate-remote-refused: busy\u{200b}\u{feff}";
        let forging = |character: char| character.is_control() || matches!(character, '\u{202e}' | '\u{2028}' | '\u{200b}' | '\u{feff}');
        let long = "x".repeat(500);
        for path in [forged, long.as_str()] {
            let definition = Definition {
                actions: vec![exec(path, "$(Arg0)")],
                ..good()
            };

            let why = mismatch(&definition);

            assert_eq!(why.lines().count(), 1, "{why:?}");
            assert!(!why.chars().any(forging), "{why:?}");
            assert!(why.chars().count() < 200 + HELPER.len() + 40, "the task's text is cut: {why:?}");
        }
        let why = mismatch(&Definition {
            actions: vec![exec(forged, "$(Arg0)")],
            ..good()
        });
        assert!(
            why.contains(r"C:\x\u{a}gate-remote-refused: busy\u{d}\u{a}\u{202e}exe.\u{2028}gate-remote-refused: busy\u{200b}\u{feff}"),
            "{why:?}"
        );
    }

    #[test]
    fn a_task_or_folder_not_found_is_not_registered_and_another_code_is_a_failure() {
        for code in [TASK_NOT_FOUND, FOLDER_NOT_FOUND] {
            assert_eq!(
                missing("open the task")(windows::core::Error::from_hresult(HRESULT(code))),
                TaskError::NotRegistered
            );
        }
        let denied = missing("open the task")(windows::core::Error::from_hresult(HRESULT(0x8007_0005_u32.cast_signed())));

        let named = match &denied {
            TaskError::Failed(why) => why.starts_with("the Task Scheduler could not open the task: ") && why.ends_with("(0x80070005)"),
            _ => false,
        };
        assert!(named, "{denied:?}");
    }

    #[test]
    fn a_task_argument_holds_only_what_arg0_writes() {
        let longest = format!("4294967295:{}", "0123456789abcdef".repeat(2));
        assert_eq!(longest.len(), TaskArgument::LONGEST);
        for good in ["1:00", longest.as_str()] {
            assert_eq!(
                TaskArgument::new(good).map(|argument| argument.as_str().to_owned()),
                Some(good.to_owned())
            );
        }
        let too_long = format!("{longest}0");
        for bad in ["", "1:AB", "1 :00", "1:00\"", "$(Arg0)", "1:00\n", too_long.as_str()] {
            assert_eq!(TaskArgument::new(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_task_path_splits_into_its_folder_and_name() {
        assert_eq!(split_task_path(r"\gate\desktop-helper"), Some((r"\gate", "desktop-helper")));
        assert_eq!(split_task_path(r"\a\b\c"), Some((r"\a\b", "c")));
        assert_eq!(split_task_path(r"\task"), Some((r"\", "task")));
        for bad in ["", r"\", r"\gate\", "gate", r"gate\task"] {
            assert_eq!(split_task_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_task_whose_folder_is_not_registered_is_not_registered() {
        let argument = TaskArgument::new("1:00").expect("the argument is allowed");
        let path = format!(r"\gate-no-such-folder-{}\desktop-helper", std::process::id());

        let started = run_task(&start_of(&path, &argument), Instant::now() + Duration::from_secs(600), &[]);

        assert_eq!(started, Err(TaskError::NotRegistered));
    }

    #[test]
    fn a_task_not_registered_in_a_folder_that_exists_is_not_registered() {
        let argument = TaskArgument::new("1:00").expect("the argument is allowed");
        let path = format!(r"\gate-no-such-task-{}", std::process::id());

        let started = run_task(&start_of(&path, &argument), Instant::now() + Duration::from_secs(600), &[]);

        assert_eq!(started, Err(TaskError::NotRegistered));
    }

    /// A started instance whose snapshots are scripted: each refresh takes the next, `None` or one past the last
    /// failing it.
    struct Scripted {
        snapshots: Vec<Option<(TASK_STATE, u32)>>,
        refreshes: Cell<usize>,
    }

    impl Scripted {
        fn new(snapshots: Vec<Option<(TASK_STATE, u32)>>) -> Scripted {
            Scripted {
                snapshots,
                refreshes: Cell::new(0),
            }
        }

        /// The snapshot the last refresh took.
        fn current(&self) -> windows::core::Result<(TASK_STATE, u32)> {
            let taken = self
                .refreshes
                .get()
                .checked_sub(1)
                .and_then(|index| self.snapshots.get(index).copied().flatten());
            taken.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))
        }
    }

    impl Instance for Scripted {
        fn refresh(&self) -> windows::core::Result<()> {
            self.refreshes.set(self.refreshes.get() + 1);
            self.current().map(|_| ())
        }

        fn state(&self) -> windows::core::Result<TASK_STATE> {
            self.current().map(|(state, _)| state)
        }

        fn pid(&self) -> windows::core::Result<u32> {
            self.current().map(|(_, pid)| pid)
        }
    }

    #[test]
    fn the_pid_poll_waits_through_queued_and_running_and_stops_at_its_bound_or_an_end() {
        let later = Instant::now() + HANG_GUARD;
        let process = StartedTask::Process(NonZeroU32::new(42).expect("42 is not 0"));
        let cases = [
            (
                "queued, running, then a pid",
                vec![
                    Some((TASK_STATE_QUEUED, 0)),
                    Some((TASK_STATE_RUNNING, 0)),
                    Some((TASK_STATE_RUNNING, 42)),
                ],
                later,
                process,
                3,
            ),
            (
                "running with no pid past the bound",
                vec![Some((TASK_STATE_RUNNING, 0))],
                Instant::now(),
                StartedTask::NoPid,
                1,
            ),
            ("ready with no pid", vec![Some((TASK_STATE_READY, 0))], later, StartedTask::NoPid, 1),
            ("disabled with no pid", vec![Some((TASK_STATE_DISABLED, 0))], later, StartedTask::NoPid, 1),
        ];
        for (named, snapshots, by, expected, refreshes) in cases {
            let running = Scripted::new(snapshots);

            let found = engine_pid(&running, by);

            assert_eq!(found, Ok(expected), "{named}");
            assert_eq!(running.refreshes.get(), refreshes, "{named}");
        }
        let unseen_instance = Scripted::new(vec![None]);
        let refused = engine_pid(&unseen_instance, later);
        assert!(
            matches!(&refused, Err(TaskError::StartedUnknown(why)) if why.contains("could not refresh the started task")),
            "{refused:?}"
        );
    }

    /// A fresh manual-reset event, signalled when `set`.
    fn event(set: bool) -> OwnedHandle {
        // SAFETY: null security attributes and a null name are allowed; the 1 makes the event manual-reset. A success
        // is a fresh handle nothing else owns.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, i32::from(set), std::ptr::null()) };
        assert!(!event.is_null(), "the event is made: {}", io::Error::last_os_error());
        // SAFETY: as above.
        unsafe { OwnedHandle::from_raw_handle(event) }
    }

    #[test]
    fn a_start_given_up_already_names_why() {
        let (unset, set) = (event(false), event(true));
        let later = Instant::now() + HANG_GUARD;

        assert_eq!(given_up(Instant::now(), &[]), Ok(Some(TaskError::TimedOut)));
        assert_eq!(
            given_up(Instant::now(), &[set.as_handle()]),
            Ok(Some(TaskError::TimedOut)),
            "a deadline passed comes first"
        );
        assert_eq!(given_up(later, &[]), Ok(None));
        assert_eq!(given_up(later, &[unset.as_handle()]), Ok(None));
        assert_eq!(given_up(later, &[unset.as_handle(), set.as_handle()]), Ok(Some(TaskError::Woken(1))));
    }

    #[test]
    #[expect(clippy::panic, reason = "the thread that asks the Task Scheduler is made to panic")]
    fn a_start_the_task_scheduler_gave_no_answer_to_may_have_happened() {
        let panicked = std::thread::spawn(|| -> Result<StartedTask, TaskError> { panic!("the call broke\nhere") });

        let answer = answered(Ok(Signalled::Event), panicked, "starts the task");

        assert_eq!(
            answer,
            Err(TaskError::Unanswered(
                r"the thread that starts the task panicked: the call broke\u{a}here".to_owned()
            ))
        );
        let finished = std::thread::spawn(|| Ok(StartedTask::NoPid));
        let answer = answered(Err(io::Error::other("refused")), finished, "starts the task");
        assert_eq!(
            answer,
            Err(TaskError::Unanswered("the wait for the Task Scheduler failed: refused".to_owned()))
        );
        assert_eq!(
            answer.err().map(|error| error.to_string()).as_deref(),
            Some("the Task Scheduler gave no answer (the wait for the Task Scheduler failed: refused); the task may have been started")
        );
    }

    #[test]
    fn every_task_error_text_is_escaped() {
        let forged = "x\ngate-remote-refused: busy\u{2028}";
        let call = || windows::core::Error::new(E_FAIL, forged);
        let errors = [
            failed("open the task")(call()),
            unseen("refresh the started task")(call()),
            TaskError::unanswered(forged),
            TaskError::failed(&format!("the thread could not be made: {}", io::Error::other(forged))),
        ];
        for error in errors {
            let text = error.to_string();

            assert!(
                !text.chars().any(|character| character.is_control() || character == '\u{2028}'),
                "{text:?}"
            );
            assert!(text.contains(r"x\u{a}gate-remote-refused: busy\u{2028}"), "{text:?}");
        }
    }

    #[test]
    fn the_registered_definition_passes_the_check() {
        let (owner, dacl) = wanted_security(USER).expect("the wanted descriptor is read");
        let wanted = Definition {
            owner: owner.clone(),
            dacl: dacl.clone(),
            ..wanted_definition(Path::new(HELPER), USER).expect("the wanted definition is built")
        };

        assert_eq!(check_definition(&wanted, &expected()), Ok(()));
        assert_eq!(owner.as_deref(), Some("S-1-5-32-544"), "owned by Administrators");
        assert_eq!(
            check_folder(owner.as_deref(), dacl.as_deref(), "\\gate"),
            Ok(()),
            "the folder's descriptor"
        );
        assert_eq!(
            dacl,
            Some(vec![
                allowed("S-1-5-18", 0x001f_01ff),
                allowed("S-1-5-32-544", 0x001f_01ff),
                allowed(USER, 0x0012_0089),
            ]),
            "SYSTEM and Administrators in full, the user reading"
        );
    }

    #[test]
    fn a_task_or_folder_not_found_is_either_not_found_code() {
        for code in [TASK_NOT_FOUND, FOLDER_NOT_FOUND] {
            assert!(is_not_found(&windows::core::Error::from_hresult(HRESULT(code))), "0x{code:08X}");
        }
        for code in [0x8007_0005_u32.cast_signed(), 0x8007_00B7_u32.cast_signed(), E_FAIL.0] {
            assert!(!is_not_found(&windows::core::Error::from_hresult(HRESULT(code))), "0x{code:08X}");
        }
    }

    #[test]
    fn a_helper_path_that_holds_a_percent_is_refused() {
        let helper = r"C:\Users\some%USERNAME%one\gate-desktop-helper.exe";

        let refused = wanted_definition(Path::new(helper), USER);

        assert_eq!(
            refused.err().map(|error| error.to_string()).as_deref(),
            Some(r"the desktop helper's path holds a %: C:\Users\some%USERNAME%one\gate-desktop-helper.exe")
        );
        let checked = Expected {
            helper: PathBuf::from(helper),
            user_sid: USER.to_owned(),
        };
        let task = Definition {
            actions: vec![exec(helper, "$(Arg0)")],
            ..good()
        };
        assert_eq!(
            check_definition(&task, &checked),
            Err(
                r"its action's program C:\Users\some%USERNAME%one\gate-desktop-helper.exe holds a %, which the Task Scheduler expands from its user's environment"
                    .to_owned()
            ),
            "the check refuses it even when it is the helper's own path"
        );
    }

    #[test]
    fn the_registered_task_xml_says_the_wanted_definition() {
        let wanted = wanted_definition(Path::new(HELPER), USER).expect("the wanted definition is built");

        let xml = task_xml(&wanted);

        let want = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n",
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
            "  <Principals>\n",
            "    <Principal id=\"Author\">\n",
            "      <UserId>S-1-5-21-1-2-3-1001</UserId>\n",
            "      <LogonType>InteractiveToken</LogonType>\n",
            "      <RunLevel>LeastPrivilege</RunLevel>\n",
            "    </Principal>\n",
            "  </Principals>\n",
            "  <Settings>\n",
            "    <Enabled>true</Enabled>\n",
            "    <AllowStartOnDemand>true</AllowStartOnDemand>\n",
            "    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>\n",
            "    <Priority>4</Priority>\n",
            "    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n",
            "    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n",
            "    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n",
            "  </Settings>\n",
            "  <Actions Context=\"Author\">\n",
            "    <Exec>\n",
            "      <Command>C:\\Users\\someone\\AppData\\Local\\gate\\bin\\gate-desktop-helper.exe</Command>\n",
            "      <Arguments>$(Arg0)</Arguments>\n",
            "    </Exec>\n",
            "  </Actions>\n",
            "</Task>\n",
        );
        assert_eq!(xml, want);
    }

    #[test]
    fn a_helper_path_with_xml_markup_is_escaped() {
        let helper = r#"C:\a&b\<c>\"d'\gate-desktop-helper.exe"#;
        let wanted = wanted_definition(Path::new(helper), USER).expect("the wanted definition is built");

        let xml = task_xml(&wanted);

        assert!(
            xml.contains(r"<Command>C:\a&amp;b\&lt;c&gt;\&quot;d&apos;\gate-desktop-helper.exe</Command>"),
            "{xml}"
        );
        assert_eq!(
            check_action(&wanted.actions, Path::new(helper)),
            Ok(()),
            "the definition keeps the path as it is"
        );
    }

    /// The security descriptor strings of the test folder `folder` and of its task `name`, read after a registration,
    /// then the task and the folder deleted, on a thread in the multithreaded apartment of its own.
    fn read_and_delete(folder: String, name: &'static str) -> [Result<String, String>; 4] {
        std::thread::spawn(move || {
            let library = super::task_library().expect("the Task Scheduler's library loads");
            let _apartment = super::Apartment::join().expect("COM is set up");
            let service = super::connected(library).expect("the Task Scheduler connects");
            let text = |error: windows::core::Error| error.to_string();
            let information = super::OWNER_AND_DACL.cast_signed();
            // SAFETY: each interface is valid for its call, and each BSTR outlives it.
            unsafe {
                let found = service.GetFolder(&windows::core::BSTR::from(folder.as_str())).map_err(text);
                let folder_sddl = found
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|found| found.GetSecurityDescriptor(information).map(|sddl| sddl.to_string()).map_err(text));
                let task_sddl = found.as_ref().map_err(Clone::clone).and_then(|found| {
                    let task = found.GetTask(&windows::core::BSTR::from(name)).map_err(text)?;
                    task.GetSecurityDescriptor(information).map(|sddl| sddl.to_string()).map_err(text)
                });
                let task_deleted = found
                    .and_then(|found| found.DeleteTask(&windows::core::BSTR::from(name), 0).map_err(text))
                    .map(|()| "deleted".to_owned());
                // The folder is deleted whether or not its task was, and each result is reported.
                let folder_deleted = service
                    .GetFolder(&windows::core::BSTR::from("\\"))
                    .and_then(|root| root.DeleteFolder(&windows::core::BSTR::from(folder.trim_start_matches('\\')), 0))
                    .map(|()| "deleted".to_owned())
                    .map_err(text);
                [folder_sddl, task_sddl, task_deleted, folder_deleted]
            }
        })
        .join()
        .expect("the thread that reads and deletes the test task ends")
    }

    /// What [`make_register_run_read_and_delete`] found at each step.
    struct HeapSteps {
        made: Result<(), TaskError>,
        registered: Result<(), TaskError>,
        started: Result<StartedTask, TaskError>,
        read: Result<Definition, TaskError>,
        deleted: Result<(), String>,
    }

    /// Through the production calls, on one thread: makes the folder `\gate-test-<pid>` ([`super::wanted_folder`]),
    /// registers a task in it ([`register_definition`]), runs it ([`super::run`]; its program does not exist, so nothing
    /// starts), reads its definition back, then deletes the task and the folder. Each string variant goes to the Task
    /// Scheduler there; the descriptor is one an unelevated process may set.
    fn make_register_run_read_and_delete() -> HeapSteps {
        let folder = format!(r"\gate-test-{}", std::process::id());
        let user_sid = crate::current_user_sid().expect("the running user is read");
        let mut session = 0;
        // SAFETY: the call writes only to `session`, which is writable.
        let found = unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &raw mut session) };
        assert_ne!(found, 0, "this process's session is read: {}", io::Error::last_os_error());
        std::thread::spawn(move || {
            use windows::core::BSTR;
            let library = super::task_library().expect("the Task Scheduler's library loads");
            let _apartment = super::Apartment::join().expect("COM is set up");
            let service = super::connected(library).expect("the Task Scheduler connects");
            let wanted = wanted_definition(Path::new(HELPER), &user_sid).expect("the wanted definition is built");
            // An unelevated registrant may neither name Administrators as owner nor keep less than full access itself.
            let sddl = format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user_sid})");
            let made = super::wanted_folder(&service, &folder, &sddl);
            let Ok(made) = made else {
                return HeapSteps {
                    made: made.map(drop),
                    registered: Err(TaskError::failed("not tried")),
                    started: Err(TaskError::failed("not tried")),
                    read: Err(TaskError::failed("not tried")),
                    deleted: Err("not tried".to_owned()),
                };
            };
            let registered = super::register_definition(&made, "t", &task_xml(&wanted), &user_sid, &sddl, super::TASK_CREATE_OR_UPDATE.0);
            // SAFETY: each interface is valid for its call, and each BSTR outlives it.
            let task = unsafe { made.GetTask(&BSTR::from("t")) }.map_err(super::failed("open the task"));
            let request = super::Request {
                folder: folder.clone(),
                name: "t".to_owned(),
                argument: "1:00".to_owned(),
                session_id: session,
                expected: expected(),
                pid_wait: Duration::from_millis(200),
                deadline: Instant::now() + HANG_GUARD,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            let started = task.as_ref().map_err(Clone::clone).and_then(|task| super::run(task, &request));
            let read = task.and_then(|task| super::read_definition(&task));
            // SAFETY: each interface is valid for its call, and each BSTR outlives it.
            let deleted = unsafe {
                made.DeleteTask(&BSTR::from("t"), 0).and_then(|()| {
                    let root = service.GetFolder(&BSTR::from("\\"))?;
                    root.DeleteFolder(&BSTR::from(folder.trim_start_matches('\\')), 0)
                })
            }
            .map_err(|error| error.to_string());
            HeapSteps {
                made: Ok(()),
                registered,
                started,
                read,
                deleted,
            }
        })
        .join()
        .expect("the thread that makes, registers, runs, reads and deletes the test task ends")
    }

    /// Set in the child run of [`a_task_registered_run_and_read_on_one_thread_leaves_the_heap_whole`], which does the
    /// work; unset, the test starts that child.
    const HEAP_CHILD: &str = "GATE_TEST_TASK_HEAP_CHILD";

    /// Makes a folder, registers, runs and reads back a task on one thread through the production calls, then deletes
    /// both. A string freed twice sits unseen in OLE Automation's per-thread cache of freed strings, so the work runs in
    /// a child of this test binary with that cache off (`OANOCACHE=1`), where the heap catches a second free at once and
    /// ends the child with `STATUS_HEAP_CORRUPTION`.
    #[test]
    #[ignore = "makes and deletes the folder \\gate-test-<pid> and a task in it on this machine; runs unelevated"]
    fn a_task_registered_run_and_read_on_one_thread_leaves_the_heap_whole() {
        if std::env::var_os(HEAP_CHILD).is_none() {
            let name = "task::tests::a_task_registered_run_and_read_on_one_thread_leaves_the_heap_whole";
            let mut command = std::process::Command::new(std::env::current_exe().expect("the test binary's path is read"));
            command
                .args(["--ignored", "--exact", name, "--test-threads", "1"])
                .env(HEAP_CHILD, "1")
                .env("OANOCACHE", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let child = crate::spawn(&mut command)
                .and_then(std::process::Child::wait_with_output)
                .expect("the child run starts and ends");
            let said = format!("{}{}", String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
            assert!(child.status.success(), "the child run ends cleanly: {:?}\n{said}", child.status);
            assert!(said.contains("1 passed"), "the child ran the test: {said}");
            return;
        }
        let steps = make_register_run_read_and_delete();

        assert_eq!(steps.made, Ok(()), "the test folder is made");
        assert_eq!(steps.registered, Ok(()), "the test task is registered");
        // Its program does not exist, so the instance may end before its process is asked for; `RunEx` itself must pass.
        assert!(
            !matches!(steps.started, Err(TaskError::Failed(_))),
            "the test task is run: {:?}",
            steps.started
        );
        let read = steps.read.expect("the test task's definition is read");
        assert_eq!(read.actions, vec![exec(HELPER, "$(Arg0)")], "its action is the one registered");
        assert_eq!(read.user_sid, crate::current_user_sid().ok(), "it runs as this user");
        assert_eq!(steps.deleted, Ok(()), "the test task and folder are deleted");
    }

    #[test]
    #[ignore = "needs an elevated session: registers and deletes a task under a temporary folder"]
    #[expect(clippy::print_stdout, reason = "the hand proof reads the real descriptors the registration left")]
    fn a_registered_task_reads_back_and_passes_the_check() {
        let folder = format!(r"\gate-test-{}", std::process::id());
        let path = format!(r"{folder}\desktop-helper");
        let user_sid = crate::current_user_sid().expect("the running user is read");

        let registered = register_task(&path, Path::new(HELPER), &user_sid, Instant::now() + HANG_GUARD);

        let [folder_sddl, task_sddl, task_deleted, folder_deleted] = read_and_delete(folder.clone(), "desktop-helper");
        println!("the folder {folder}: {folder_sddl:?}");
        println!("the task {path}: {task_sddl:?}");
        println!("the task's delete: {task_deleted:?}; the folder's delete: {folder_deleted:?}");
        assert_eq!(registered, Ok(()), "the registration, read back and checked");
        assert_eq!(task_deleted, Ok("deleted".to_owned()), "the test task is deleted");
        assert_eq!(folder_deleted, Ok("deleted".to_owned()), "the test folder is deleted");
    }

    #[test]
    fn a_library_loaded_from_elsewhere_is_refused_and_named() {
        let wanted = Path::new(r"C:\Windows\System32\taskschd.dll");

        assert_eq!(check_loaded_from(wanted, Path::new(r"C:\WINDOWS\SYSTEM32\TASKSCHD.DLL")), Ok(()));
        assert_eq!(
            check_loaded_from(wanted, Path::new(r"C:\x\taskschd.dll")),
            Err(TaskError::Failed(
                r"taskschd.dll was loaded from C:\x\taskschd.dll, not C:\Windows\System32\taskschd.dll".to_owned()
            ))
        );
        assert_eq!(
            check_loaded_from(wanted, Path::new("C:\\x\n\u{202e}\\taskschd.dll")),
            Err(TaskError::Failed(
                r"taskschd.dll was loaded from C:\x\u{a}\u{202e}\taskschd.dll, not C:\Windows\System32\taskschd.dll".to_owned()
            ))
        );
    }
}

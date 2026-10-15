//! Starting a registered scheduled task in a given session through the Task Scheduler's COM API, once its definition
//! is checked: the desktop helper's task, which runs the helper beside the running program in the logged-on user's
//! session. The COM work runs on a short-lived thread of its own, in the multithreaded apartment, and every interface
//! is released before that thread ends.

use std::ffi::OsStr;
use std::fmt;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::TaskScheduler::{
    IAction, IExecAction, IRegisteredTask, ITaskService, TASK_ACTION_EXEC, TASK_ACTION_TYPE, TASK_INSTANCES_PARALLEL, TASK_INSTANCES_POLICY,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_LOGON_TYPE, TASK_RUN_USE_SESSION_ID, TaskScheduler,
};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::core::{BSTR, Interface};

/// The file name of the desktop helper, which the task must run from the running program's own folder.
const HELPER_FILE: &str = "gate-desktop-helper.exe";

/// The only arguments the task's action may have: the `Arg0` a run passes it, as the Task Scheduler substitutes it.
const ARG0_ARGUMENTS: &str = "$(Arg0)";

/// The priority the task must run at: 4, normal, as the Task Scheduler counts from 0 (realtime) to 10 (idle).
const TASK_PRIORITY: i32 = 4;

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)`: no task of the name in the folder.
const TASK_NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();

/// `HRESULT_FROM_WIN32(ERROR_PATH_NOT_FOUND)`: no folder of the name.
const FOLDER_NOT_FOUND: i32 = 0x8007_0003_u32.cast_signed();

/// A task [`run_task`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartedTask {
    /// The id of the process the Task Scheduler started for the task's action.
    pub engine_pid: u32,
}

/// Why [`run_task`] started nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    /// No task, or no folder, of the path is registered.
    NotRegistered,
    /// The task is registered with a definition other than the one the check wants; the text names the first thing
    /// that differs.
    SetUpWrong(String),
    /// A call of the Task Scheduler or of COM failed; the text names the call and holds the system's message.
    Failed(String),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::NotRegistered => write!(formatter, "the task is not registered"),
            TaskError::SetUpWrong(what) => write!(formatter, "the task is set up wrong: {what}"),
            TaskError::Failed(why) => write!(formatter, "{why}"),
        }
    }
}

impl std::error::Error for TaskError {}

/// Starts the registered task `path` (such as `\gate\desktop-helper`) in the session `session_id`, with `arg0` as the
/// value of its `$(Arg0)`, once its definition passes the check: enabled; started on demand; a second instance run in
/// parallel; priority 4; one action, which runs the helper beside the running program with the arguments `$(Arg0)`
/// alone; and run with the interactive token of the user logged on. It never registers, changes or stops a task.
///
/// # Errors
///
/// [`TaskError::NotRegistered`] when the task or its folder does not exist; [`TaskError::SetUpWrong`] naming the first
/// thing of the definition that differs; [`TaskError::Failed`] when the path holds no task name, the running program's
/// path cannot be read, or a COM call fails.
pub fn run_task(path: &str, arg0: &str, session_id: u32) -> Result<StartedTask, TaskError> {
    let Some((folder, name)) = split_task_path(path) else {
        return Err(TaskError::Failed(format!("{path} is not a task path, \\<folder>\\<name>")));
    };
    let helper = helper_path()?;
    let (folder, name, arg0) = (folder.to_owned(), name.to_owned(), arg0.to_owned());
    let thread = std::thread::Builder::new()
        .name("gate-task".to_owned())
        .spawn(move || in_apartment(|| start_checked(&folder, &name, &arg0, session_id, &helper)))
        .map_err(|error| TaskError::Failed(format!("the thread that starts the task could not be made: {error}")))?;
    thread
        .join()
        .unwrap_or_else(|_| Err(TaskError::Failed("the thread that starts the task panicked".to_owned())))
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

/// The helper the task must run: [`HELPER_FILE`] in the running program's folder.
fn helper_path() -> Result<PathBuf, TaskError> {
    let program = std::env::current_exe().map_err(|error| TaskError::Failed(format!("the running program's path could not be read: {error}")))?;
    Ok(program.with_file_name(HELPER_FILE))
}

/// Runs `work` with COM set up for this thread in the multithreaded apartment, and takes it down again after; every
/// interface `work` made is released when it returns, before that.
fn in_apartment<T>(work: impl FnOnce() -> Result<T, TaskError>) -> Result<T, TaskError> {
    // SAFETY: no reserved pointer is passed; the apartment is this thread's alone, and is left below exactly when this
    // call succeeded.
    let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if joined.is_err() {
        return Err(TaskError::Failed(format!(
            "COM could not be set up: {}",
            windows::core::Error::from(joined)
        )));
    }
    let result = work();
    // SAFETY: the apartment this thread joined above is left once, after every interface made in it was released.
    unsafe { CoUninitialize() };
    result
}

/// Connects to the Task Scheduler, finds the task `name` in `folder`, checks its definition against `helper`, then
/// runs it in `session_id` with `arg0`, and reads the process it started.
fn start_checked(folder: &str, name: &str, arg0: &str, session_id: u32, helper: &Path) -> Result<StartedTask, TaskError> {
    // SAFETY: the class and the interface are the Task Scheduler's, made in-process with no outer object.
    let service: ITaskService = unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }.map_err(failed("make the Task Scheduler"))?;
    let local = VARIANT::default();
    // SAFETY: four empty variants connect to this machine as the running user.
    unsafe { service.Connect(&local, &local, &local, &local) }.map_err(failed("connect to the Task Scheduler"))?;
    // SAFETY: the folder's path is a valid BSTR for the call.
    let found = unsafe { service.GetFolder(&BSTR::from(folder)) }.map_err(missing("open the task's folder"))?;
    // SAFETY: the task's name is a valid BSTR for the call.
    let task = unsafe { found.GetTask(&BSTR::from(name)) }.map_err(missing("open the task"))?;
    let definition = read_definition(&task)?;
    check_definition(&definition, helper).map_err(TaskError::SetUpWrong)?;
    run(&task, arg0, session_id)
}

/// Runs `task` in `session_id`, as no named user, with `arg0` as its one parameter, and reads the process it started.
fn run(task: &IRegisteredTask, arg0: &str, session_id: u32) -> Result<StartedTask, TaskError> {
    let argument = BSTR::from(arg0);
    let mut parameters = VARIANT::default();
    // SAFETY: the variant is set to hold a BSTR whose pointer is a bitwise copy of `argument`'s, in a field that never
    // frees it; `argument` alone owns the string and outlives the call that reads the variant.
    unsafe {
        let value = &mut parameters.Anonymous.Anonymous;
        value.vt = VT_BSTR;
        value.Anonymous.bstrVal = ManuallyDrop::new(std::ptr::read(&raw const argument));
    }
    let flags = TASK_RUN_USE_SESSION_ID.0;
    // SAFETY: the variant holds a valid BSTR, and the empty user name asks for the task's own principal.
    let running = unsafe { task.RunEx(&parameters, flags, session_id.cast_signed(), &BSTR::new()) }.map_err(failed("run the task"))?;
    drop(argument);
    // SAFETY: the running task's interface is valid for the call.
    let engine_pid = unsafe { running.EnginePID() }.map_err(failed("read the started task's process"))?;
    Ok(StartedTask { engine_pid })
}

/// What the check reads of a registered task's definition.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The task's actions, in order.
    actions: Vec<Action>,
    /// Whether the task runs with the interactive token of the user logged on.
    interactive_token: bool,
}

/// One action of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    /// An action that starts `path` with `arguments`.
    Exec { path: String, arguments: String },
    /// Any other kind of action.
    Other,
}

/// The first thing of `definition` that differs from the one the desktop helper's task must have, whose one action
/// runs `helper`; `Ok` when nothing does.
fn check_definition(definition: &Definition, helper: &Path) -> Result<(), String> {
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
    check_action(&definition.actions, helper)?;
    if !definition.interactive_token {
        return Err("it does not run with the interactive token of the user logged on".to_owned());
    }
    Ok(())
}

/// The first thing of `actions` that differs from one action running `helper` with the arguments `$(Arg0)`.
fn check_action(actions: &[Action], helper: &Path) -> Result<(), String> {
    let [action] = actions else {
        return Err(format!("it has {} actions, not one", actions.len()));
    };
    let Action::Exec { path, arguments } = action else {
        return Err("its action does not start a program".to_owned());
    };
    if !crate::same_variable_name(OsStr::new(path), helper.as_os_str()) {
        return Err(format!("its action runs {path}, not {}", helper.display()));
    }
    if arguments != ARG0_ARGUMENTS {
        return Err(format!("its action's arguments are {arguments}, not {ARG0_ARGUMENTS}"));
    }
    Ok(())
}

/// What the check needs of `task`'s registered definition.
fn read_definition(task: &IRegisteredTask) -> Result<Definition, TaskError> {
    let read = failed("read the task's definition");
    // SAFETY: each interface is valid for its call, and each out pointer is to a local of the call's type.
    unsafe {
        let definition = task.Definition().map_err(&read)?;
        let settings = definition.Settings().map_err(&read)?;
        let (mut settings_enabled, mut demand) = (VARIANT_BOOL::default(), VARIANT_BOOL::default());
        settings.Enabled(&raw mut settings_enabled).map_err(&read)?;
        settings.AllowDemandStart(&raw mut demand).map_err(&read)?;
        let mut policy = TASK_INSTANCES_POLICY::default();
        settings.MultipleInstances(&raw mut policy).map_err(&read)?;
        let mut priority = 0;
        settings.Priority(&raw mut priority).map_err(&read)?;
        let mut logon = TASK_LOGON_TYPE::default();
        definition.Principal().map_err(&read)?.LogonType(&raw mut logon).map_err(&read)?;
        let collection = definition.Actions().map_err(&read)?;
        let mut count = 0;
        collection.Count(&raw mut count).map_err(&read)?;
        // The collection counts from 1.
        let actions = (1..=count)
            .map(|index| read_action(&collection.get_Item(index).map_err(&read)?))
            .collect::<Result<_, _>>()?;
        Ok(Definition {
            enabled: task.Enabled().map_err(&read)?.0 != 0 && settings_enabled.0 != 0,
            allow_demand_start: demand.0 != 0,
            parallel: policy == TASK_INSTANCES_PARALLEL,
            priority,
            actions,
            interactive_token: logon == TASK_LOGON_INTERACTIVE_TOKEN,
        })
    }
}

/// What the check needs of one action: its program and arguments when it starts one.
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
        let (mut path, mut arguments) = (BSTR::new(), BSTR::new());
        exec.Path(&raw mut path).map_err(&read)?;
        exec.Arguments(&raw mut arguments).map_err(&read)?;
        Ok(Action::Exec {
            path: path.to_string(),
            arguments: arguments.to_string(),
        })
    }
}

/// The error of a failed call that did `what`, with the system's message and the HRESULT.
fn failed(what: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| {
        TaskError::Failed(format!(
            "the Task Scheduler could not {what}: {} (0x{:08X})",
            error.message(),
            error.code().0
        ))
    }
}

/// As [`failed`], but a task or folder that does not exist is [`TaskError::NotRegistered`].
fn missing(what: &'static str) -> impl Fn(windows::core::Error) -> TaskError {
    move |error| match error.code().0 {
        TASK_NOT_FOUND | FOLDER_NOT_FOUND => TaskError::NotRegistered,
        _ => failed(what)(error),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Action, Definition, TaskError, check_definition, run_task, split_task_path};

    const HELPER: &str = r"C:\Users\someone\AppData\Local\gate\bin\gate-desktop-helper.exe";

    /// The definition the desktop helper's task must have.
    fn good() -> Definition {
        Definition {
            enabled: true,
            allow_demand_start: true,
            parallel: true,
            priority: 4,
            actions: vec![Action::Exec {
                path: HELPER.to_owned(),
                arguments: "$(Arg0)".to_owned(),
            }],
            interactive_token: true,
        }
    }

    /// The mismatch the check names for `definition`.
    fn mismatch(definition: &Definition) -> String {
        check_definition(definition, Path::new(HELPER)).err().unwrap_or_default()
    }

    #[test]
    fn the_definition_the_helper_task_must_have_passes() {
        assert_eq!(check_definition(&good(), Path::new(HELPER)), Ok(()));
        let other_case = Definition {
            actions: vec![Action::Exec {
                path: HELPER.to_uppercase(),
                arguments: "$(Arg0)".to_owned(),
            }],
            ..good()
        };
        assert_eq!(
            check_definition(&other_case, Path::new(HELPER)),
            Ok(()),
            "a path is compared without regard to case"
        );
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
                    interactive_token: false,
                    ..good()
                },
                "interactive token",
            ),
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
        let exec = |path: &str, arguments: &str| Action::Exec {
            path: path.to_owned(),
            arguments: arguments.to_owned(),
        };
        let cases = [
            (Vec::new(), "0 actions, not one"),
            (vec![exec(HELPER, "$(Arg0)"), exec(HELPER, "$(Arg0)")], "2 actions, not one"),
            (vec![Action::Other], "does not start a program"),
            (vec![exec(r"C:\Windows\System32\cmd.exe", "$(Arg0)")], r"runs C:\Windows\System32\cmd.exe"),
            (vec![exec(&format!("\"{HELPER}\""), "$(Arg0)")], "its action runs \""),
            (vec![exec(HELPER, "$(Arg0) --more")], "arguments are $(Arg0) --more"),
            (vec![exec(HELPER, "")], "arguments are , not"),
        ];
        for (actions, named) in cases {
            let definition = Definition { actions, ..good() };

            let why = mismatch(&definition);

            assert!(why.contains(named), "{:?} is refused for {named:?}: {why:?}", definition.actions);
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
    #[ignore = "starts a scheduled task on this machine: run by hand"]
    fn a_task_that_is_not_registered_is_not_registered() {
        let started = run_task(r"\gate-no-such-folder\desktop-helper", "1:00", 1);

        assert_eq!(started, Err(TaskError::NotRegistered));
    }
}

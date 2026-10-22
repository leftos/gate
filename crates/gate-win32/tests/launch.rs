//! Commands started suspended with three inherited handles and an explicit environment, put in a named job, then
//! resumed: nothing runs before the resume, the exit code and the output bytes come back, and a start failure carries
//! the Windows message. The job holds what the command starts, a process whose parent has exited included, and ends
//! them all at once; a job name already held is refused. A launch that names its program loosely, a batch file, or a
//! NUL is refused; the log is created afresh, never through a link; a suspended command that is dropped is ended. A
//! piped command reads what is written to its stdin pipe, writes its stdout to its pipe and its stderr to this
//! process's own, its job holds it and its environment is the block given.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, Launch, LaunchError, LaunchOutput, PriorityClass, Running, win32_message};
use proc_table::ProcessTable;
use proc_table::win32::{OwnedHandle, Win32ProcessTable, open_process};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject};

/// The longest any command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);

/// A job name no other test or process uses.
fn unique_name(test: &str) -> String {
    format!(r"Local\gate-win32-test-{}-{test}", std::process::id())
}

/// An empty folder of this test's own.
fn case_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-launch-{}-{test}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
    }
    fs::create_dir_all(&dir).expect("the case folder is created");
    dir
}

/// A program under `%SystemRoot%`, as an absolute path.
fn system_program(relative: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join(relative)
}

/// `cmd.exe`, as an absolute path.
fn cmd() -> PathBuf {
    system_program(r"System32\cmd.exe")
}

/// The command `line` of `application` to run in `dir` with this process's environment and `extra` set over it, its
/// output and its errors each to a pipe whose read end the launch keeps ([`LaunchOutput::Pipes`]).
fn launch(dir: &Path, application: &Path, line: &str, extra: &[(&str, &str)]) -> Launch {
    let overrides = extra.iter().map(|(name, value)| (OsString::from(name), OsString::from(value)));
    Launch {
        application: application.to_path_buf(),
        command_line: line.encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment: EnvironmentBlock::new(std::env::vars_os().chain(overrides)).expect("the block is built"),
        output: LaunchOutput::Pipes,
        priority: None,
    }
}

/// The command `line` of `application` to run in `dir` with `environment`, its stdin and stdout piped and its stderr
/// this process's own.
fn piped(dir: &Path, application: PathBuf, line: &str, environment: EnvironmentBlock) -> Launch {
    Launch {
        application,
        command_line: line.encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment,
        output: LaunchOutput::Piped,
        priority: None,
    }
}

/// Starts `launch`, puts it in a fresh job named for `test`, writes `input` to its stdin and closes it, and lets it
/// run to its exit code: that, and what it wrote to its stdout.
fn run_piped(test: &str, launch: &Launch, input: &[u8]) -> (u32, String) {
    let job = Job::create(&unique_name(test)).expect("the job is created");
    let mut suspended = launch.start_suspended().expect("the command starts");
    let mut pipes = suspended.take_pipes().expect("a piped launch has pipes");
    assert!(suspended.take_pipes().is_none(), "the pipes are taken once");
    let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
    pipes.stdin.write_all(input).expect("the command's stdin is written");
    drop(pipes.stdin);
    let mut output = String::new();
    pipes.stdout.read_to_string(&mut output).expect("the command's stdout is read to its end");
    (exit_code(&running), output)
}

/// The variable naming the folder the helper case runs its piped command in; unset, the helper does nothing.
const HELPER: &str = "GATE_WIN32_TEST_PIPED_HELPER";

/// The exit code of the process behind `process`, once it has exited.
fn exit_code_of(process: &OwnedHandle) -> u32 {
    let millis = u32::try_from(HANG_GUARD.as_millis()).expect("the guard fits a wait");
    // SAFETY: the handle is open for the whole call, with the `SYNCHRONIZE` access it was opened with.
    let waited = unsafe { WaitForSingleObject(process.raw(), millis) };
    assert_eq!(waited, WAIT_OBJECT_0, "the process exits");
    let mut code = 0;
    // SAFETY: the handle is open for the whole call, with query access, and `code` is writable.
    let read = unsafe { GetExitCodeProcess(process.raw(), &raw mut code) };
    assert_ne!(read, 0, "the exit code is read: {}", io::Error::last_os_error());
    code
}

/// Starts the `cmd` command `line` in `dir`, puts it in a fresh job named for `test` and lets it run to its exit code:
/// that, and what it wrote to its stdout and its stderr.
fn run(test: &str, dir: &Path, line: &str, extra: &[(&str, &str)]) -> (u32, String, String) {
    let job = Job::create(&unique_name(test)).expect("the job is created");
    let mut suspended = launch(dir, &cmd(), line, extra).start_suspended().expect("the command starts");
    let (mut stdout, mut stderr) = suspended.take_output().expect("a launch with output pipes has them");
    let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
    let (mut output, mut errors) = (String::new(), String::new());
    stdout.read_to_string(&mut output).expect("the command's stdout is read to its end");
    stderr.read_to_string(&mut errors).expect("the command's stderr is read to its end");
    (exit_code(&running), output, errors)
}

/// The exit code of `running`, once it has exited.
fn exit_code(running: &Running) -> u32 {
    running.wait(HANG_GUARD).expect("the wait answers").expect("the command exits")
}

/// Asserts that `launch` is refused before anything starts, with an error naming `needle`.
fn assert_refused(launch: &Launch, needle: &str) {
    let refused = launch.start_suspended().err().expect("the launch is refused");

    assert!(
        matches!(&refused, LaunchError::Setup { error, .. } if error.kind() == io::ErrorKind::InvalidInput),
        "{refused:?}"
    );
    assert!(refused.to_string().contains(needle), "{refused}");
}

#[test]
fn a_suspended_command_runs_only_once_resumed() {
    let dir = case_dir("suspended");
    let job = Job::create(&unique_name("suspended")).expect("the job is created");
    let suspended = launch(&dir, &cmd(), "cmd /d /c echo ran>marker.txt", &[])
        .start_suspended()
        .expect("the command starts");

    assert!(!dir.join("marker.txt").exists(), "a suspended command has not run");

    let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
    assert_eq!(exit_code(&running), 0);
    assert!(dir.join("marker.txt").exists(), "a resumed command runs");
}

#[test]
fn a_dropped_suspended_command_is_ended_with_exit_code_1() {
    let dir = case_dir("dropped");
    let suspended = launch(&dir, &cmd(), "cmd /d /c exit 0", &[])
        .start_suspended()
        .expect("the command starts");
    let process = open_process(suspended.pid(), PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).expect("cmd opens");

    drop(suspended);

    assert_eq!(exit_code_of(&process), 1);
}

#[test]
fn the_exit_code_comes_back() {
    let dir = case_dir("exit-3");

    assert_eq!(run("exit-3", &dir, "cmd /d /c exit 3", &[]).0, 3);
}

#[test]
fn output_and_errors_come_from_the_given_environment() {
    let dir = case_dir("output");
    let extra = [("gate_win32_test_value", "overridden"), ("GATE_WIN32_TEST_VALUE", "from-block")];

    let (code, output, errors) = run("output", &dir, "cmd /d /c echo out-%GATE_WIN32_TEST_VALUE%&1>&2 echo err", &extra);

    assert_eq!(code, 0);
    assert_eq!((output.as_str(), errors.as_str()), ("out-from-block\r\n", "err\r\n"));
    assert!(
        std::env::var_os("GATE_WIN32_TEST_VALUE").is_none(),
        "the gate's own environment is unchanged"
    );
}

#[test]
fn output_and_errors_come_through_their_own_pipes_and_stdin_ends_at_once() {
    let dir = case_dir("pipes");
    let job = Job::create(&unique_name("pipes")).expect("the job is created");
    let mut pipes = launch(&dir, &cmd(), "cmd /d /c sort&echo out&1>&2 echo err", &[]);
    pipes.output = LaunchOutput::Pipes;
    let mut suspended = pipes.start_suspended().expect("the command starts");
    assert!(suspended.take_pipes().is_none(), "a launch with output pipes has no stdin pipe");
    let (mut stdout, mut stderr) = suspended.take_output().expect("a launch with output pipes has them");
    assert!(suspended.take_output().is_none(), "the output pipes are taken once");

    // sort reads its stdin to its end before it writes anything: a stdin that did not end would hold it to the guard.
    let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
    let mut output = String::new();
    stdout.read_to_string(&mut output).expect("the command's stdout is read to its end");
    let mut errors = String::new();
    stderr.read_to_string(&mut errors).expect("the command's stderr is read to its end");

    assert_eq!(exit_code(&running), 0);
    assert_eq!((output.as_str(), errors.as_str()), ("out\r\n", "err\r\n"));
}

#[test]
fn environment_names_match_without_regard_to_case_beyond_ascii() {
    let dir = case_dir("unicode-name");
    let extra = [("ÄPFEL", "1"), ("äpfel", "2")];

    let (code, output, _) = run("unicode-name", &dir, "cmd /d /c echo %ÄPFEL%", &extra);

    assert_eq!(code, 0);
    assert_eq!(output, "2\r\n", "one entry, the later value");
}

#[test]
fn a_missing_program_is_a_create_process_failure_with_the_windows_message() {
    let dir = case_dir("missing");
    let missing = dir.join("gate-win32-test-no-such-program.exe");

    let error = launch(&dir, &missing, "gate-win32-test-no-such-program", &[])
        .start_suspended()
        .err()
        .expect("the start fails");

    assert!(
        matches!(&error, LaunchError::CreateProcess { code: 2, message } if *message == win32_message(2)),
        "{error:?}"
    );
    assert_eq!(error.to_string(), win32_message(2), "no `(os error 2)` suffix");
}

#[test]
fn a_relative_application_is_refused() {
    let dir = case_dir("relative");

    assert_refused(&launch(&dir, Path::new("cmd.exe"), "cmd /d /c exit 0", &[]), "cmd.exe");
}

#[test]
fn a_batch_application_is_refused() {
    let dir = case_dir("batch");
    let batch = dir.join("tool.CMD");
    fs::write(&batch, "@exit 0\r\n").expect("the batch file is written");

    assert_refused(&launch(&dir, &batch, "tool.CMD", &[]), "tool.CMD");
}

#[test]
fn a_command_line_holding_a_nul_is_refused() {
    let dir = case_dir("nul");

    assert_refused(&launch(&dir, &cmd(), "cmd /d /c echo a\0b", &[]), "command line");
}

#[test]
fn the_priority_class_reaches_the_command() {
    let dir = case_dir("priority");
    let job = Job::create(&unique_name("priority")).expect("the job is created");
    let mut high = launch(
        &dir,
        &system_program(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        "powershell -NoProfile -NonInteractive -Command [Diagnostics.Process]::GetCurrentProcess().PriorityClass",
        &[],
    );
    high.priority = Some(PriorityClass::High);

    let mut suspended = high.start_suspended().expect("the command starts");
    let (mut stdout, _stderr) = suspended.take_output().expect("a launch with output pipes has them");
    let running = suspended.assign_and_resume(&job).expect("it is resumed");
    let mut printed = String::new();
    stdout.read_to_string(&mut printed).expect("the command's stdout is read to its end");

    assert_eq!(exit_code(&running), 0);
    assert_eq!(printed.trim_end(), "High");
}

#[test]
fn a_job_holds_a_grandchild_whose_parent_has_exited_and_terminate_ends_it() {
    let dir = case_dir("grandchild");
    let job = Job::create(&unique_name("grandchild")).expect("the job is created");
    let running = launch(&dir, &cmd(), "cmd /d /c start /b ping -n 120 127.0.0.1", &[])
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");
    assert_eq!(exit_code(&running), 0, "cmd exits, leaving its child running");

    let grandchild = Win32ProcessTable
        .snapshot()
        .into_iter()
        .find(|process| process.parent_pid == running.pid() && process.name.eq_ignore_ascii_case("ping.exe"))
        .expect("cmd started ping")
        .pid;
    let process = open_process(grandchild, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).expect("ping opens");
    assert!(
        job.process_ids().expect("the job lists its processes").contains(&grandchild),
        "ping is in the job"
    );

    job.terminate(124).expect("the job is terminated");

    assert_eq!(exit_code_of(&process), 124, "terminating the job ended ping");
}

/// Run by [`a_piped_command_writes_stdout_to_its_pipe_and_stderr_to_this_process_s`] in a process of its own, whose
/// stderr and stdout that case reads: a piped command writes a line to each of its own, and the helper asserts the
/// stdout line came through the pipe.
#[test]
fn piped_helper() {
    let Some(dir) = std::env::var_os(HELPER) else {
        return;
    };
    let environment = EnvironmentBlock::new(std::env::vars_os()).expect("the block is built");
    let launch = piped(Path::new(&dir), cmd(), "cmd /d /c echo child-stdout&1>&2 echo child-stderr", environment);

    let (code, output) = run_piped("helper", &launch, b"");

    assert_eq!((code, output.as_str()), (0, "child-stdout\r\n"));
}

#[test]
fn a_piped_command_writes_stdout_to_its_pipe_and_stderr_to_this_process_s() {
    let dir = case_dir("piped");
    let mut command = Command::new(std::env::current_exe().expect("the test binary is known"));
    command
        .args(["piped_helper", "--exact", "--nocapture", "--test-threads", "1"])
        .env(HELPER, &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = gate_win32::spawn(&mut command)
        .expect("the helper starts")
        .wait_with_output()
        .expect("the helper ends");

    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("child-stderr"), "the command's stderr is the helper's: {stderr}");
    assert!(
        !stdout.contains("child-stdout") && !stderr.contains("child-stdout"),
        "the command's stdout goes to its pipe alone: {stdout}\n{stderr}"
    );
}

#[test]
fn a_piped_command_reads_what_is_written_to_its_stdin() {
    let dir = case_dir("piped-stdin");
    let environment = EnvironmentBlock::new(std::env::vars_os()).expect("the block is built");
    let launch = piped(&dir, system_program(r"System32\sort.exe"), "sort", environment);

    let (code, output) = run_piped("piped-stdin", &launch, b"second\r\nfirst\r\n");

    assert_eq!((code, output.as_str()), (0, "first\r\nsecond\r\n"));
}

#[test]
fn a_piped_command_is_in_its_job_and_ends_with_it() {
    let dir = case_dir("piped-job");
    let job = Job::create(&unique_name("piped-job")).expect("the job is created");
    let environment = EnvironmentBlock::new(std::env::vars_os()).expect("the block is built");
    let running = piped(&dir, cmd(), "cmd /d /c ping -n 120 127.0.0.1 >nul", environment)
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");

    assert!(
        job.process_ids().expect("the job lists its processes").contains(&running.pid()),
        "the command is in the job"
    );
    job.terminate(124).expect("the job is terminated");
    assert_eq!(exit_code(&running), 124, "terminating the job ended the command");
}

#[test]
fn a_piped_command_gets_exactly_the_environment_given() {
    let dir = case_dir("piped-env");
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    let given = [
        (OsString::from("SystemRoot"), system_root),
        (OsString::from("GATE_WIN32_ONLY"), OsString::from("given")),
    ];
    let environment = EnvironmentBlock::new(given).expect("the block is built");

    let (code, text) = run_piped("piped-env", &piped(&dir, cmd(), "cmd /d /c set", environment), b"");

    assert_eq!(code, 0, "{text}");
    // cmd.exe defines COMSPEC, PATHEXT and PROMPT itself when the block it is given lacks them.
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .filter(|name| !["COMSPEC", "PATHEXT", "PROMPT"].contains(name))
        .collect();
    assert_eq!(names, ["GATE_WIN32_ONLY", "SystemRoot"], "{text}");
}

/// cmd.exe gives `COMSPEC`, `PATHEXT` and `PROMPT` a value of its own when the block it is started with lacks them,
/// which is why a batch target's `%COMSPEC%`, `%PATHEXT%` and `%PROMPT%` are refused though no such variable is set.
#[test]
fn cmd_sets_comspec_pathext_and_prompt_when_the_block_lacks_them() {
    let dir = case_dir("piped-cmd-own-names");
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    let environment = EnvironmentBlock::new([(OsString::from("SystemRoot"), system_root)]).expect("the block is built");

    let (code, text) = run_piped("piped-cmd-own-names", &piped(&dir, cmd(), "cmd /d /c set", environment), b"");

    assert_eq!(code, 0, "{text}");
    for name in ["COMSPEC", "PATHEXT", "PROMPT"] {
        let set = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .any(|(said, value)| said.eq_ignore_ascii_case(name) && !value.is_empty());
        assert!(set, "cmd.exe set {name} itself: {text}");
    }
}

#[test]
fn a_second_job_of_a_held_name_is_refused() {
    let name = unique_name("held");
    let _first = Job::create(&name).expect("the job is created");

    let error = Job::create(&name).err().expect("a second job of the name is refused");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
}

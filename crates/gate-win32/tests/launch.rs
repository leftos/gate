//! Commands started suspended with three inherited handles and an explicit environment, put in a named job, then
//! resumed: nothing runs before the resume, the exit code and the output bytes come back, and a start failure carries
//! the Windows message. The job holds what the command starts, a process whose parent has exited included, and ends
//! them all at once; a job name already held is refused. A launch that names its program loosely, a batch file, or a
//! NUL is refused; the log is created afresh, never through a link; a suspended command that is dropped is ended.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, Launch, LaunchError, PriorityClass, Running, win32_message};
use proc_table::ProcessTable;
use proc_table::win32::{OwnedHandle, Win32ProcessTable, open_process};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject};

/// The longest any command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(60);

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
/// output to `out.log` and its errors to `out.log.err` there.
fn launch(dir: &Path, application: &Path, line: &str, extra: &[(&str, &str)]) -> Launch {
    let overrides = extra.iter().map(|(name, value)| (OsString::from(name), OsString::from(value)));
    Launch {
        application: application.to_path_buf(),
        command_line: line.encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment: EnvironmentBlock::new(std::env::vars_os().chain(overrides)).expect("the block is built"),
        log: dir.join("out.log"),
        err_log: dir.join("out.log.err"),
        priority: None,
    }
}

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

/// Starts the `cmd` command `line` in `dir`, puts it in a fresh job named for `test` and lets it run to its exit code.
fn run(test: &str, dir: &Path, line: &str, extra: &[(&str, &str)]) -> u32 {
    let job = Job::create(&unique_name(test)).expect("the job is created");
    let running = launch(dir, &cmd(), line, extra)
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");
    exit_code(&running)
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

    assert_eq!(run("exit-3", &dir, "cmd /d /c exit 3", &[]), 3);
}

#[test]
fn output_and_errors_land_in_the_log_and_err_files_from_the_given_environment() {
    let dir = case_dir("output");
    let extra = [("gate_win32_test_value", "overridden"), ("GATE_WIN32_TEST_VALUE", "from-block")];

    let code = run("output", &dir, "cmd /d /c echo out-%GATE_WIN32_TEST_VALUE%&1>&2 echo err", &extra);

    assert_eq!(code, 0);
    assert_eq!(fs::read(dir.join("out.log")).expect("the log is read"), b"out-from-block\r\n");
    assert_eq!(fs::read(dir.join("out.log.err")).expect("the errors are read"), b"err\r\n");
    assert!(
        std::env::var_os("GATE_WIN32_TEST_VALUE").is_none(),
        "the gate's own environment is unchanged"
    );
}

#[test]
fn environment_names_match_without_regard_to_case_beyond_ascii() {
    let dir = case_dir("unicode-name");
    let extra = [("ÄPFEL", "1"), ("äpfel", "2")];

    let code = run("unicode-name", &dir, "cmd /d /c echo %ÄPFEL%", &extra);

    assert_eq!(code, 0);
    assert_eq!(
        fs::read(dir.join("out.log")).expect("the log is read"),
        b"2\r\n",
        "one entry, the later value"
    );
}

#[test]
fn an_existing_log_is_replaced() {
    let dir = case_dir("replaced");
    fs::write(dir.join("out.log"), "old content that is longer than the new\r\n").expect("the old log is written");

    assert_eq!(run("replaced", &dir, "cmd /d /c echo new", &[]), 0);

    assert_eq!(fs::read(dir.join("out.log")).expect("the log is read"), b"new\r\n");
}

#[test]
#[expect(clippy::print_stderr, reason = "a skipped case says why it skipped")]
fn a_log_path_that_is_a_symlink_never_writes_the_links_target() {
    let dir = case_dir("symlink");
    let target = dir.join("target.txt");
    fs::write(&target, "keep").expect("the target is written");
    if let Err(error) = std::os::windows::fs::symlink_file(&target, dir.join("out.log")) {
        eprintln!("skipped: this process may not create a symlink: {error}");
        return;
    }
    let job = Job::create(&unique_name("symlink")).expect("the job is created");

    if let Ok(suspended) = launch(&dir, &cmd(), "cmd /d /c echo new", &[]).start_suspended() {
        let running = suspended.assign_and_resume(&job).expect("the command is assigned and resumed");
        assert_eq!(exit_code(&running), 0);
        let log = fs::symlink_metadata(dir.join("out.log")).expect("the log exists");
        assert!(!log.file_type().is_symlink(), "the log is a fresh file at the link's own path");
        assert_eq!(fs::read(dir.join("out.log")).expect("the log is read"), b"new\r\n");
    }

    assert_eq!(fs::read(&target).expect("the target is read"), b"keep", "the link's target is untouched");
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

    let running = high
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("it is resumed");

    assert_eq!(exit_code(&running), 0);
    let printed = fs::read_to_string(dir.join("out.log")).expect("the log is read");
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

#[test]
fn a_second_job_of_a_held_name_is_refused() {
    let name = unique_name("held");
    let _first = Job::create(&name).expect("the job is created");

    let error = Job::create(&name).err().expect("a second job of the name is refused");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
}

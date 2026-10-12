//! What a watch samples: a job's accounting keeps the CPU of a member that has exited, a running process has its
//! creation and CPU times and a gone one has none, and the system's times only ever rise.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a test starts its child directly: what it checks is the child itself"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gate_win32::{Accounting, EnvironmentBlock, Job, Launch, LaunchOutput, file_time_now, process_times, system_times};

/// The longest any command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);

/// How often a poll looks again.
const LOOK_AGAIN: Duration = Duration::from_millis(10);

/// An empty folder of this test's own.
fn case_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-times-{}-{test}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
    }
    fs::create_dir_all(&dir).expect("the case folder is created");
    dir
}

/// The `cmd.exe` command `line`, run in `dir` with this process's environment and its output to `out.log` there.
fn launch(dir: &Path, line: &str) -> Launch {
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
    Launch {
        application: system.join(r"System32\cmd.exe"),
        command_line: line.encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Files {
            log: dir.join("out.log"),
            err_log: dir.join("out.log.err"),
        },
        priority: None,
    }
}

/// The job's accounting once no process is in it. A member's wait returns as soon as it is signalled, before the
/// job's accounting has dropped it from the active count, so the count is read until it has, and the last accounting
/// read is the failure's message when the hang guard passes without it.
fn accounting_without_members(job: &Job) -> Accounting {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let accounting = job.accounting().expect("the accounting is read after the exit");
        if accounting.active_processes == 0 {
            return accounting;
        }
        assert!(
            Instant::now() < deadline,
            "the member left the job's active count within the hang guard, the last accounting read being {accounting:?}"
        );
        thread::sleep(LOOK_AGAIN);
    }
}

#[test]
fn accounting_keeps_the_cpu_of_a_member_that_exited() {
    let dir = case_dir("accounting");
    let job = Job::create(&format!(r"Local\gate-win32-test-{}-accounting", std::process::id())).expect("the job is created");
    let before = job.accounting().expect("an empty job's accounting is read");
    assert_eq!((before.cpu, before.active_processes), (0, 0), "an empty job has used nothing");

    // A loop long enough that the clock-tick sampling of CPU time cannot miss all of it.
    let running = launch(&dir, "cmd /d /c for /l %i in (1,1,300000) do @rem")
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");
    assert_eq!(
        job.accounting().expect("the accounting is read").active_processes,
        1,
        "the command is in the job"
    );
    let code = running.wait(HANG_GUARD).expect("the wait answers").expect("the command exits");
    assert_eq!(code, 0);

    let after = accounting_without_members(&job);
    assert!(after.cpu > 0, "the CPU of the member that exited still counts: {after:?}");
}

#[test]
fn this_process_has_times_and_was_created_before_now() {
    let times = process_times(std::process::id())
        .expect("this process can be queried")
        .expect("this process runs");

    assert!(times.created <= file_time_now(), "{times:?}");
}

#[test]
fn a_pid_that_does_not_exist_has_no_times() {
    // The kernel ignores a pid's low two bits, so this is looked up as 0xFFFFFFFC, which no process has.
    assert_eq!(process_times(u32::MAX - 1).expect("a pid no process has is no error"), None);
}

#[test]
fn an_exited_child_has_no_times() {
    let mut child = Command::new("cmd").args(["/c", "exit 0"]).spawn().expect("cmd starts");
    assert!(child.wait().expect("the child can be waited for").success());

    assert_eq!(process_times(child.id()).expect("an exited child is no error"), None);
}

#[test]
fn the_times_of_an_exited_259_process_are_none() {
    let mut child = Command::new("cmd").args(["/c", "exit 259"]).spawn().expect("cmd starts");
    let status = child.wait().expect("the child can be waited for");
    assert_eq!(status.code(), Some(259), "{status}");

    // The child is gone, and `Child` still holds its handle, so its exit code, `STILL_ACTIVE`, stays readable.
    assert_eq!(process_times(child.id()).expect("an exited child is no error"), None);
}

#[test]
fn a_child_started_after_now_was_created_at_or_after_it() {
    let since = file_time_now();
    let mut child = Command::new("cmd").args(["/c", "ping -n 30 127.0.0.1 >nul"]).spawn().expect("cmd starts");
    let times = process_times(child.id()).expect("the child can be queried");
    child.kill().expect("the child can be killed");
    child.wait().expect("the killed child can be waited for");

    let times = times.expect("the child ran when it was queried");
    assert!(times.created >= since, "created {} before {since}", times.created);
}

#[test]
fn system_times_only_rise() {
    let first = system_times().expect("the system's times are read");
    let second = system_times().expect("the system's times are read again");

    assert!(first.kernel >= first.idle, "kernel time includes idle time: {first:?}");
    assert!(
        second.idle >= first.idle && second.kernel >= first.kernel && second.user >= first.user,
        "{first:?} then {second:?}"
    );
}

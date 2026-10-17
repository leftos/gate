//! Detached commands: started from inside a job that allows breakaway, the command leaves that job and lives on when the
//! job's last handle closes and kills what is in it, and the start says it runs inside a job exactly when a job above
//! still holds it; started from a job that allows breakaway nested in one that refuses it, the start says the command
//! runs inside a job, and it sits in the outer one; started from inside a job that refuses breakaway, the start says so
//! and the command runs in that job, ending with it. In each the starter's output pipes reach their end once
//! the starter exits, though the command still runs, as it holds no handle of the starter's.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use gate_win32::{Detached, DetachedLaunch, EnvironmentBlock, Job, PipedConsole, PipedLaunch, PipedSuspended, StoppableProcess};
use proc_table::win32::open_process;
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

/// The longest any command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);
/// The variable that makes [`detached_helper`] start its command: the test it runs for sets it.
const HELPER: &str = "GATE_WIN32_DETACHED_HELPER";
/// The start of the line the helper prints: where its command runs, then the command's pid.
const REPORT: &str = "detached: ";

/// The system folder, where cmd.exe and ping are.
fn system_folder() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32")
}

/// Run by the cases below in a process of their own, inside the jobs each made: starts a detached ping that never ends
/// on its own, and prints where it runs and its pid.
#[test]
#[expect(clippy::print_stdout, reason = "the helper reports to the case that started it on its stdout")]
fn detached_helper() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let system = system_folder();
    let launch = DetachedLaunch {
        application: system.join("PING.EXE"),
        command_line: "ping -t 127.0.0.1".encode_utf16().collect(),
        directory: system,
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
    };

    let detached = launch.start().expect("the detached command starts");

    let place = match detached {
        Detached::OutsideJob(_) => "outside",
        Detached::InsideJob(_) => "inside",
    };
    println!("{REPORT}{place} {}", detached.running().pid());
}

/// A detached command the case ends when it is dropped, failed or not: the command never ends on its own.
struct Command(StoppableProcess);

impl Drop for Command {
    fn drop(&mut self) {
        // A command its job already ended cannot be ended again; either way nothing of it is left running.
        let _ended = self.0.terminate(1);
    }
}

/// What [`detached_helper`] reported: its command's pid, where the command runs, and the command, opened.
struct Reported {
    pid: u32,
    place: String,
    command: Command,
}

/// Runs [`detached_helper`] in `jobs`, each nested in the one before it, and returns what it reported, once the helper
/// has exited and both its output pipes have reached their end.
#[expect(clippy::panic, reason = "a helper that reports nothing fails the case")]
fn run_helper(jobs: &[&Job]) -> Reported {
    let (innermost, outer) = jobs.split_last().expect("the helper runs in a job");
    let exe = std::env::current_exe().expect("the test binary is known");
    let line = format!("\"{}\" detached_helper --exact --nocapture --test-threads 1", exe.display());
    let variables = std::env::vars_os().chain([(OsString::from(HELPER), OsString::from("1"))]);
    let launch = PipedLaunch {
        application: exe,
        command_line: line.encode_utf16().collect(),
        directory: system_folder(),
        environment: EnvironmentBlock::new(variables).expect("the block is built"),
        console: PipedConsole::Hidden,
        token: None,
    };
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended_within(innermost).expect("the helper is created");
    for job in outer {
        suspended.assign(job).expect("the helper enters an outer job");
    }
    let helper = suspended.assign_and_resume(innermost).expect("the helper runs in its job");
    let output = read_all(stdout);
    let errors = read_all(stderr);
    let code = helper.wait(HANG_GUARD).expect("the helper is waited for");
    // The test harness's own `test detached_helper ... ` may stand before the report on its line.
    let report = output
        .lines()
        .find_map(|line| line.split_once(REPORT).map(|(_, report)| report))
        .unwrap_or_else(|| panic!("the helper reports its command: {output}{errors}"));
    let Some((place, pid)) = report.split_once(' ') else {
        panic!("the report names a place and a pid: {report}");
    };
    let pid: u32 = pid.parse().expect("the pid is a number");
    let command = Command(StoppableProcess::open(pid).expect("the command opens").expect("the command runs"));
    assert_eq!(code, Some(0), "the helper passed: {output}{errors}");
    Reported {
        pid,
        place: place.to_owned(),
        command,
    }
}

/// All of `file`, read to its end.
fn read_all(mut file: File) -> String {
    let mut text = String::new();
    file.read_to_string(&mut text).expect("the pipe is read to its end");
    text
}

/// Whether the process `pid` is in any job.
fn in_any_job(pid: u32) -> bool {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).expect("the command opens");
    let mut in_job = 0;
    // SAFETY: the handle is open for the whole call, with the query access the call needs; a null job asks about any
    // job; `in_job` is writable.
    let read = unsafe { IsProcessInJob(process.raw(), std::ptr::null_mut(), &raw mut in_job) };
    assert_ne!(read, 0, "the membership is read: {}", std::io::Error::last_os_error());
    in_job != 0
}

#[test]
fn a_command_detached_from_a_job_that_allows_breakaway_leaves_it_and_outlives_its_kill_on_close() {
    let job = Job::anonymous_kill_on_close_breakaway().expect("the job is created");

    let Reported { pid, place, command } = run_helper(&[&job]);

    assert!(!command.0.in_job(&job).expect("the membership is read"), "the command is not in the job");
    let held = in_any_job(pid);
    assert_eq!(
        place,
        if held { "inside" } else { "outside" },
        "the start says whether a job still holds the command"
    );
    drop(job);
    assert!(
        !command.0.wait(Duration::ZERO).expect("the command is waited for"),
        "the command still runs once the job has killed what it held"
    );
}

#[test]
fn a_command_detached_from_a_breakaway_job_inside_one_that_refuses_it_runs_inside_a_job() {
    let outer = Job::anonymous_kill_on_close().expect("the outer job is created");
    let inner = Job::anonymous_kill_on_close_breakaway().expect("the inner job is created");

    let Reported { place, command, .. } = run_helper(&[&outer, &inner]);

    assert_eq!(place, "inside", "the start says a job still holds the command");
    assert!(
        !command.0.in_job(&inner).expect("the membership is read"),
        "the command left the inner job"
    );
    assert!(
        command.0.in_job(&outer).expect("the membership is read"),
        "the outer job holds the command"
    );
}

#[test]
fn a_command_detached_from_a_job_that_refuses_breakaway_runs_inside_it_and_ends_with_it() {
    let job = Job::anonymous_kill_on_close().expect("the job is created");

    let Reported { place, command, .. } = run_helper(&[&job]);

    assert_eq!(place, "inside", "the start says the command runs inside the job");
    assert!(command.0.in_job(&job).expect("the membership is read"), "the command is in the job");
    drop(job);
    assert!(
        command.0.wait(HANG_GUARD).expect("the command is waited for"),
        "the job's kill-on-close ends the command"
    );
}

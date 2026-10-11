//! Commands started with their output piped back: stdout and stderr arrive on the two read ends, which end once the
//! command exits; the command runs in the named job; and the job's name still opens while the command runs after the
//! starter has closed its own handle to the job, through the handle the command inherited, unless the job kills on
//! close, when the starter's last handle ends the command.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Once, mpsc};
use std::thread;
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, OpenedJob, PipedConsole, PipedLaunch, PipedSuspended, Running};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The longest any command of these tests may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);

static BREAKAWAY: Once = Once::new();

/// Puts this test process in a fresh job that allows breakaway, nested in the job it already runs in. The gate's own
/// job refuses breakaway, and a command asking to break away from a job that refuses it is not created; from this inner
/// job it breaks away and stays in the gate's, as the wrapper's self-test arranges it. The job lasts as long as this
/// process, which it holds.
fn allow_breakaway() {
    BREAKAWAY.call_once(|| {
        // SAFETY: no security attributes and no name are allowed; a null handle is a failure.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "the breakaway job is created: {}", std::io::Error::last_os_error());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let size = u32::try_from(size_of_val(&limits)).expect("the limits fit a u32");
        // SAFETY: the job is open and `limits` is a whole extended limit structure of `size` bytes.
        let set = unsafe { SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) };
        assert_ne!(set, 0, "breakaway is allowed: {}", std::io::Error::last_os_error());
        // SAFETY: the job is open and the pseudo-handle of this process is always open.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert_ne!(assigned, 0, "this process enters the job: {}", std::io::Error::last_os_error());
    });
}

/// A job name no other test or process uses.
fn unique_name(test: &str) -> String {
    format!(r"Local\gate-win32-piped-{}-{test}", std::process::id())
}

/// `cmd.exe /d /c <line>` in the system folder, with this process's environment.
fn cmd(line: &str) -> PipedLaunch {
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set")).join("System32");
    PipedLaunch {
        application: system.join("cmd.exe"),
        command_line: format!("cmd /d /c {line}").encode_utf16().collect(),
        directory: system,
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        console: PipedConsole::Hidden,
    }
}

/// Starts `launch` in `job` and resumes it; the running command and its two read ends.
fn start(launch: &PipedLaunch, job: &Job) -> (Running, File, File) {
    allow_breakaway();
    let PipedSuspended { suspended, stdout, stderr } = launch.start_suspended(job).expect("the command starts");
    let running = suspended.assign_and_resume(job).expect("the command is assigned and resumed");
    (running, stdout, stderr)
}

/// The pipe read to its end on a thread of its own, within the hang guard.
fn read_all(mut file: File) -> String {
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let read = file.read_to_string(&mut text).map(|_| text);
        // The test stops listening only when it has already failed on the hang guard.
        let _ = sender.send(read);
    });
    received
        .recv_timeout(HANG_GUARD)
        .expect("the pipe reaches its end within the hang guard")
        .expect("the pipe is read")
}

/// Terminates the job of its name when dropped, so a command never outlives a test that failed.
struct EndJob(String);

impl Drop for EndJob {
    fn drop(&mut self) {
        // A job already gone, or one that cannot be ended, leaves nothing more this guard can do.
        if let Ok(Some(job)) = OpenedJob::open(&self.0) {
            let _ = job.terminate(1);
        }
    }
}

#[test]
fn stdout_and_stderr_arrive_on_the_two_read_ends() {
    let name = unique_name("output");
    let _end = EndJob(name.clone());
    let job = Job::create(&name).expect("the job is created");
    let (running, stdout, stderr) = start(&cmd("echo out&1>&2 echo err"), &job);

    assert_eq!(running.wait(HANG_GUARD).expect("the wait answers"), Some(0));
    assert_eq!(read_all(stdout), "out\r\n");
    assert_eq!(read_all(stderr), "err\r\n");
}

#[test]
fn the_command_runs_in_the_job_whose_name_outlives_the_starters_handle() {
    let name = unique_name("named");
    let _end = EndJob(name.clone());
    let job = Job::create(&name).expect("the job is created");
    let (running, _stdout, _stderr) = start(&cmd("ping -n 120 127.0.0.1 >NUL"), &job);
    assert!(
        job.process_ids().expect("the job lists its processes").contains(&running.pid()),
        "the command is in the job"
    );

    drop(job);
    let opened = OpenedJob::open(&name)
        .expect("the job opens")
        .expect("the name lasts through the handle the command inherited");

    assert!(opened.process_ids().expect("the job lists its processes").contains(&running.pid()));
    opened.terminate(124).expect("the job is terminated");
    assert_eq!(running.wait(HANG_GUARD).expect("the wait answers"), Some(124));
}

#[test]
fn a_kill_on_close_jobs_command_ends_when_the_last_job_handle_is_dropped() {
    let name = unique_name("kill-on-close");
    let _end = EndJob(name.clone());
    let job = Job::create_kill_on_close(&name).expect("the job is created");
    // About 1000 s of pings: longer than the hang guard, so only the job's end can end it within the wait below.
    let (running, _stdout, _stderr) = start(&cmd("ping -n 1000 127.0.0.1 >NUL"), &job);
    assert!(
        job.process_ids().expect("the job lists its processes").contains(&running.pid()),
        "the command is in the job"
    );

    drop(job);

    assert!(
        running.wait(HANG_GUARD).expect("the wait answers").is_some(),
        "closing the job's last handle ended the command"
    );
    assert!(
        OpenedJob::open(&name).expect("the job can be looked for").is_none(),
        "the command held no handle to its job, so the job is gone with the starter's"
    );
}

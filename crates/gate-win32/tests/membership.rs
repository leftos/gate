//! A job opened by its name answers whether a process, named by its pid and its creation time, is in it: a child put in
//! the job is a member, a child started outside it is not, and neither is a member's pid asked with another creation
//! time, nor a pid no process has.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start its children must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a test starts its outside child directly: what it checks is that child itself"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Job, Launch, LaunchOutput, Membership, OpenedJob, Running, process_start_of};
use jiff::{SignedDuration, Timestamp};

/// The arguments of a `ping` that runs long enough for every question of a test, and is ended before that. It starts
/// no process of its own, so ending it ends everything the test started.
const LINGER: [&str; 3] = ["-n", "300", "127.0.0.1"];
/// The longest a case waits, as it is dropped, for its ended member to be gone before it removes its folder.
const MEMBER_END: Duration = Duration::from_secs(60);

/// A case of a test's own: an empty folder, a job, and the job's one member, `ping` [`LINGER`] started in the job with
/// its output in the folder. Dropping it ends the member, waits for its end and removes the folder.
struct Case {
    dir: PathBuf,
    name: String,
    job: Job,
    member: Running,
}

impl Case {
    fn start(test: &str) -> Case {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-membership-{}-{test}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
        }
        fs::create_dir_all(&dir).expect("the case folder is created");
        let name = format!(r"Local\gate-win32-test-{}-{test}", std::process::id());
        let job = Job::create_kill_on_close(&name).expect("the job is created");
        let member = member(&dir, &job);
        Case { dir, name, job, member }
    }
}

impl Drop for Case {
    #[expect(clippy::print_stderr, reason = "a case that cannot clean up says so without failing the test it ends")]
    fn drop(&mut self) {
        let cleaned = self
            .job
            .terminate(1)
            .and_then(|()| self.member.wait(MEMBER_END))
            .and_then(|_| fs::remove_dir_all(&self.dir));
        if let Err(error) = cleaned {
            eprintln!("the case folder {} is left behind: {error}", self.dir.display());
        }
    }
}

/// `ping` [`LINGER`] started in `job`, its output in `dir`.
fn member(dir: &Path, job: &Job) -> Running {
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
    Launch {
        application: system.join(r"System32\PING.EXE"),
        command_line: format!("ping {}", LINGER.join(" ")).encode_utf16().collect(),
        directory: dir.to_path_buf(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Files {
            log: dir.join("out.log"),
            err_log: dir.join("out.log.err"),
        },
        priority: None,
    }
    .start_suspended()
    .expect("the member starts")
    .assign_and_resume(job)
    .expect("the member is assigned and resumed")
}

/// `ping` [`LINGER`] started outside the job, which starts no process of its own, ended when dropped.
struct Outsider(Child);

impl Outsider {
    fn start() -> Outsider {
        let child = Command::new("ping")
            .args(LINGER)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the outside child starts");
        Outsider(child)
    }
}

impl Drop for Outsider {
    fn drop(&mut self) {
        // The child may have ended already; either way it is waited for, so no zombie handle is left.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The creation time of the running process `pid`.
fn created(pid: u32) -> Timestamp {
    process_start_of(pid).expect("the start is read").expect("the process runs")
}

#[test]
fn a_job_holds_its_member_and_not_a_child_started_outside_it() {
    let case = Case::start("holds");
    let outsider = Outsider::start();
    let opened = OpenedJob::open_to_list(&case.name).expect("the job opens").expect("the job exists");
    let inside = case.member.pid();

    assert_eq!(opened.holds(inside, created(inside)).expect("the job answers"), Membership::Member);
    assert_eq!(
        opened.holds(outsider.0.id(), created(outsider.0.id())).expect("the job answers"),
        Membership::Outside
    );
    assert_eq!(
        opened.holds(std::process::id(), created(std::process::id())).expect("the job answers"),
        Membership::Outside,
        "the test process, which made the job, is not in it"
    );
}

#[test]
fn a_member_pid_asked_with_another_creation_time_is_not_a_member() {
    let case = Case::start("reused");
    let opened = OpenedJob::open_to_list(&case.name).expect("the job opens").expect("the job exists");
    let inside = case.member.pid();

    let later = created(inside).checked_add(SignedDuration::from_micros(1)).expect("a later time");
    assert_eq!(opened.holds(inside, later).expect("the job answers"), Membership::Reused);
    // A pid no process runs as: Windows gives out multiples of 4 far below this one.
    assert_eq!(opened.holds(u32::MAX - 3, later).expect("the job answers"), Membership::NotOpened);
}

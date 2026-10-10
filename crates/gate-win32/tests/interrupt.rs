//! The console interrupt: once raised it stays raised, and it ends a wait on an event or a command at once, while a
//! handle already signalled wins over it. A test binary of its own, since the interrupt is the process's and is never
//! lowered.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, EventWait, Interrupt, Job, Launch, NamedEvent, Waited};

/// The longest any wait of this test may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// An empty folder of this test's own.
fn case_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-interrupt-{}", std::process::id()));
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
        log: dir.join("out.log"),
        err_log: dir.join("out.log.err"),
        priority: None,
    }
}

#[test]
fn a_raised_interrupt_ends_every_wait_but_loses_to_a_signalled_handle() {
    let interrupt = Interrupt::install().expect("the console handler is installed");
    Interrupt::install().expect("a second install is the same interrupt");
    let event = NamedEvent::create(&format!(r"Local\gate-win32-test-{}-interrupt", std::process::id())).expect("the event is created");
    let dir = case_dir();
    let job = Job::create(&format!(r"Local\gate-win32-test-{}-interrupt-job", std::process::id())).expect("the job is created");
    let running = launch(&dir, "cmd /d /c ping -n 120 127.0.0.1 >nul")
        .start_suspended()
        .expect("the command starts")
        .assign_and_resume(&job)
        .expect("the command is assigned and resumed");

    assert!(!interrupt.is_raised(), "nothing has raised it yet");
    assert_eq!(
        event.wait_or_interrupt(Duration::ZERO, interrupt).expect("the wait answers"),
        EventWait::TimedOut
    );
    assert_eq!(
        running.wait_or_interrupt(Duration::ZERO, interrupt).expect("the wait answers"),
        Waited::Running
    );

    interrupt.raise();

    assert!(interrupt.is_raised());
    assert_eq!(
        event.wait_or_interrupt(HANG_GUARD, interrupt).expect("the wait answers"),
        EventWait::Interrupted,
        "the interrupt ends a wait on an unset event"
    );
    assert_eq!(
        running.wait_or_interrupt(HANG_GUARD, interrupt).expect("the wait answers"),
        Waited::Interrupted,
        "and a wait on a command that still runs"
    );
    assert!(Interrupt::install().expect("installed again").is_raised(), "and it stays raised");

    event.set().expect("the event is set");
    assert_eq!(
        event.wait_or_interrupt(HANG_GUARD, interrupt).expect("the wait answers"),
        EventWait::Signaled,
        "a set event wins over the interrupt"
    );
    job.terminate(7).expect("the job is terminated");
    assert_eq!(running.wait(HANG_GUARD).expect("the wait answers"), Some(7), "the command ends");
    assert_eq!(
        running.wait_or_interrupt(Duration::ZERO, interrupt).expect("the wait answers"),
        Waited::Exited(7),
        "an exited command wins over the interrupt"
    );
}

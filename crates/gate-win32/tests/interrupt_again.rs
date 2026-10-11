//! A second press of the console interrupt: each press is counted, the first ends no wait for a second, and a second
//! ends that wait at once, while a command that has exited wins over it. A test binary of its own, since the interrupt
//! is the process's and is never lowered.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its command must fail loudly")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_win32::{EnvironmentBlock, Interrupt, Job, Launch, LaunchOutput, Waited};

/// The longest any wait of this test may take: a guard against a hang, never a timing bound.
const HANG_GUARD: Duration = Duration::from_secs(600);

/// An empty folder of this test's own.
fn case_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-interrupt-again-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
    }
    fs::create_dir_all(&dir).expect("the case folder is created");
    dir
}

#[test]
fn a_second_press_is_counted_and_ends_the_wait_for_it() {
    let interrupt = Interrupt::install().expect("the console handler is installed");
    let dir = case_dir();
    let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
    let job = Job::create(&format!(r"Local\gate-win32-test-{}-interrupt-again", std::process::id())).expect("the job is created");
    let running = Launch {
        application: system.join(r"System32\cmd.exe"),
        command_line: "cmd /d /c ping -n 120 127.0.0.1 >nul".encode_utf16().collect(),
        directory: dir.clone(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        output: LaunchOutput::Files {
            log: dir.join("out.log"),
            err_log: dir.join("out.log.err"),
        },
        priority: None,
    }
    .start_suspended()
    .expect("the command starts")
    .assign_and_resume(&job)
    .expect("the command is assigned and resumed");
    assert_eq!(interrupt.presses(), 0, "nothing has raised it yet");

    interrupt.raise();

    assert_eq!(interrupt.presses(), 1);
    assert_eq!(
        running.wait_or_interrupt(HANG_GUARD, interrupt).expect("the wait answers"),
        Waited::Interrupted,
        "the first press ends a wait for any press"
    );
    assert_eq!(
        running.wait_or_interrupt_again(Duration::ZERO, interrupt).expect("the wait answers"),
        Waited::Running,
        "but not a wait for a second press"
    );

    interrupt.raise();

    assert_eq!(interrupt.presses(), 2);
    assert_eq!(
        running.wait_or_interrupt_again(HANG_GUARD, interrupt).expect("the wait answers"),
        Waited::Interrupted,
        "the second press ends it"
    );
    job.terminate(7).expect("the job is terminated");
    assert_eq!(running.wait(HANG_GUARD).expect("the wait answers"), Some(7), "the command ends");
    assert_eq!(
        running.wait_or_interrupt_again(Duration::ZERO, interrupt).expect("the wait answers"),
        Waited::Exited(7),
        "an exited command wins over a second press"
    );
}

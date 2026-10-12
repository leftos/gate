//! `gate burn`: its start line and status, its refusals, and its end when the process that started it ends.

use std::fs;
use std::process::Command;
use std::time::Duration;

use crate::support::{Case, FAKE_TOOL, GATE, USAGE, shown, start, wait_for_file};

/// How long a burn whose parent has exited may take to end: generous on a loaded machine, and far under the 170 s
/// deadline the case gives the burn, so a pass proves the parent's end ended it, not the deadline.
const BURN_BOUND: Duration = Duration::from_secs(60);

#[test]
fn a_short_burn_ends_zero_after_its_line() {
    let case = Case::new("a_short_burn_ends_zero_after_its_line");
    let outcome = case.run(&["burn", "--cores", "1", "--seconds", "1"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: burn: 1 thread(s) for 1 s\n");
    assert_eq!(outcome.stderr, "");
}

#[test]
fn a_burn_over_the_cap_is_a_usage_error() {
    let case = Case::new("a_burn_over_the_cap_is_a_usage_error");
    let outcome = case.run(&["burn", "--seconds", "181"]);

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "", "nothing is burned");
    assert_eq!(
        outcome.stderr,
        format!("gate: --seconds must be a whole number from 1 to 180, got '181'\n{USAGE}")
    );
}

#[test]
fn more_cores_than_the_machine_has_is_a_usage_error() {
    let case = Case::new("more_cores_than_the_machine_has_is_a_usage_error");
    let logical = std::thread::available_parallelism().expect("the machine has logical processors").get();
    let over = (logical + 1).to_string();
    let outcome = case.run(&["burn", "--cores", &over, "--seconds", "1"]);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "nothing is burned");
    assert_eq!(
        outcome.stderr,
        format!("gate: --cores must be at most this machine's {logical} logical processors, got {over}\n{USAGE}")
    );
}

/// The burn a case started: this one open handle pins its pid for the case's life, and dropping this stops the burn
/// while it still runs, so no failure path leaves a burner spinning.
struct Burn {
    process: gate_win32::StoppableProcess,
}

impl Burn {
    /// The burn `pid` names, opened once while the process that started it is still up, so this handle is the one that
    /// pins the pid: nothing else can take it while the handle is open.
    fn open(pid: u32) -> Burn {
        let process = gate_win32::StoppableProcess::open(pid)
            .expect("the burn can be opened")
            .expect("the burn the starter holds is running");
        Burn { process }
    }

    /// Waits up to `bound` for the burn to end; whether it did.
    fn wait(&self, bound: Duration) -> bool {
        self.process.wait(bound).expect("the burn can be waited for")
    }

    /// The code the burn ended with.
    fn exit_code(&self) -> u32 {
        self.process.exit_code().expect("the burn's exit code can be read")
    }
}

impl Drop for Burn {
    fn drop(&mut self) {
        let ended = self.process.wait(Duration::ZERO).unwrap_or(false);
        if !ended {
            // A burn that is stopping already refuses this, which is no failure of its case.
            let _ = self.process.terminate(1);
        }
    }
}

#[test]
fn a_burn_ends_when_its_parent_exits() {
    let case = Case::new("a_burn_ends_when_its_parent_exits");
    let pid_file = case.folder().join("burn.pid");
    let release = case.folder().join("burn.release");
    let mut starter = Command::new(FAKE_TOOL);
    case.prepare_without_state_dir(
        &mut starter,
        &[
            "spawn-pid",
            &shown(&pid_file),
            &shown(&release),
            GATE,
            "burn",
            "--cores",
            "1",
            "--seconds",
            "170",
        ],
    );
    let starter = start(starter);
    let starter_pid = starter.id();

    wait_for_file(&pid_file);
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the pid file can be read")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    let burn = Burn::open(pid);
    fs::write(&release, "").expect("the release file can be written");
    let ended = burn.wait(BURN_BOUND);
    let started = starter.finish();

    assert!(
        ended,
        "burn {pid} was still running {BURN_BOUND:?} after the process that started it exited"
    );
    assert_eq!(burn.exit_code(), 1, "the burn ends with 1 when the process that started it exits");
    assert_eq!(started.code, Some(0), "the starter exits with 0: {started:?}");
    assert!(
        started.stderr.contains(&format!("gate: burn: the parent process {starter_pid} ended")),
        "the burn says why it ended: {started:?}"
    );
    assert!(
        started.stdout.contains("gate: burn: 1 thread(s) for 170 s"),
        "the burn said its start line: {started:?}"
    );
}

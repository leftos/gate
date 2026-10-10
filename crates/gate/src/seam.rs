//! The `--test-journal-run` seam: a run's whole slot lifecycle, journalled, without launching its command. The gate
//! queues, waits its turn for a slot of its pool, takes it, holds it while `--test-hold-until` asks, and releases it.

use std::path::Path;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::{JournalEvent, RunFacts, Verdict};

use crate::acquire;
use crate::console::Console;
use crate::journal::RunJournal;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, Pool, Survey, ThisGate};
use crate::wait::Wait;

/// How often a held seam run looks for its file.
const HOLD_STEP: Duration = Duration::from_millis(20);

/// `--test-journal-run <status>`: journals the run's queue, its wait for a slot and the take, then its release with
/// `status`, and returns `status` as the exit status. The command is not launched. With `--test-hold-until`, it holds
/// between the take and the release, and the hold counts as held time. Problem lines are said on stderr as they are
/// found.
pub(crate) fn seam_run(request: &RunRequest, log: &Path, root: &ResolvedRoot, status: u8, console: &mut Console) -> u8 {
    let gate = ThisGate::read(request);
    let mut journal = RunJournal::open(root, &gate);
    journal.write(
        &JournalEvent::Queue {
            log: log.to_string_lossy().into_owned(),
        },
        console,
    );
    let queued = Instant::now();
    let Some(pool) = Pool::of(request.slot) else {
        // A critical run takes no slot here and writes no ticket: it journals a take and a release of none.
        let took = Instant::now();
        journal.write(&take(Vec::new(), 0.0, Vec::new()), console);
        hold(request, console);
        journal.write(&release(Vec::new(), took, status), console);
        return status;
    };
    let mut survey = Survey::new(&gate);
    let mut wait = Wait::queue(root, pool, &gate, request, queued, console);
    let taken = wait.until_taken(&mut survey, console);
    let waited = queued.elapsed().as_secs_f64();
    let name = taken.held.name().to_owned();
    let mut abandoned = Vec::new();
    if taken.abandoned {
        acquire::say_taken_abandoned(console, &name);
        abandoned.push(name.clone());
    }
    wait.end(console);
    let took = Instant::now();
    journal.write(&take(vec![name.clone()], waited, abandoned), console);
    slots::write_claim(&taken.held, pool, &gate, request, log, console);
    slots::wake_waiters(root, &mut survey, console);
    hold(request, console);
    slots::delete_claim(&taken.held, console);
    journal.write(&release(vec![name], took, status), console);
    drop(taken);
    slots::wake_waiters(root, &mut survey, console);
    status
}

fn take(slots: Vec<String>, waited_seconds: f64, abandoned: Vec<String>) -> JournalEvent {
    JournalEvent::Take {
        slots,
        waited_seconds,
        abandoned,
    }
}

/// The release of `slots`, held since `took`, by a run that ended with `status`.
fn release(slots: Vec<String>, took: Instant, status: u8) -> JournalEvent {
    let held = took.elapsed().as_secs_f64();
    let code = i64::from(status);
    JournalEvent::Release {
        slots,
        held_seconds: held,
        status: Some(code),
        facts: Some(RunFacts {
            wall_seconds: held,
            adjusted_seconds: None,
            verdict: Verdict::of(None, code, code),
        }),
    }
}

/// Holds until the `--test-hold-until` file exists, when the run asks for a hold.
fn hold(request: &RunRequest, console: &mut Console) {
    if let Some(path) = &request.test_seams.hold_until {
        hold_until(path, console);
    }
}

/// `--test-hold-until <path>`: sleeps until `path` exists. It has no timeout of its own; the suite's hang guard ends a
/// gate that is never let go. A path that cannot be checked is said on stderr and ends the hold, so the run goes on.
fn hold_until(path: &Path, console: &mut Console) {
    loop {
        match path.try_exists() {
            Ok(true) => return,
            Ok(false) => std::thread::sleep(HOLD_STEP),
            Err(error) => {
                console.err(&format!("gate: cannot check {}: {error}", path.display()));
                return;
            }
        }
    }
}

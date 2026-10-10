//! This gate's journal: the identity every one of its lines carries, its state root's journal mutex, and the problem
//! lines, a missing line and a failed rollover each said once a run. The `--test-journal-run` seam writes a run's
//! queue, take and release through it without launching the command or taking a slot.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::{
    Held, JOURNAL_FILE, JournalEvent, JournalIdentity, JournalLock, JournalNote, KernelObject, LockError, RunFacts, Verdict, append_line,
    journal_line,
};
use gate_win32::{AcquireError, MutexGuard, NamedMutex};
use jiff::Timestamp;

use crate::console::Console;
use crate::resolve::ResolvedRoot;

/// How often a held seam run looks for its file.
const HOLD_STEP: Duration = Duration::from_millis(20);

/// A state root's journal mutex.
struct JournalMutex(NamedMutex);

impl JournalLock for JournalMutex {
    type Guard<'a> = MutexGuard<'a>;

    fn name(&self) -> &str {
        self.0.name()
    }

    fn acquire(&self, timeout: Duration) -> Result<Held<MutexGuard<'_>>, LockError> {
        match self.0.acquire(timeout) {
            Ok(guard) => {
                let abandoned = guard.abandoned();
                Ok(Held { guard, abandoned })
            }
            Err(AcquireError::Timeout) => Err(LockError(format!("{} was not free within {} s", self.0.name(), timeout.as_secs()))),
            Err(error @ AcquireError::Failed(_)) => Err(LockError(format!("could not take {}: {error}", self.0.name()))),
        }
    }
}

/// Where this gate journals, as whom, and which of its once-a-run problem lines it has said.
pub(crate) struct RunJournal {
    root: PathBuf,
    /// The gate's identity and the journal mutex, or why the journal cannot be written at all.
    writer: Result<(JournalIdentity, JournalMutex), String>,
    said_not_written: bool,
    said_roll_failed: bool,
}

impl RunJournal {
    pub(crate) fn open(root: &ResolvedRoot, request: &RunRequest) -> RunJournal {
        let name = root.root.object_name(KernelObject::Journal);
        let writer = identity(request).and_then(|identity| match NamedMutex::open(&name) {
            Ok(mutex) => Ok((identity, JournalMutex(mutex))),
            Err(error) => Err(format!("could not open {name}: {error}")),
        });
        RunJournal {
            root: root.path.clone(),
            writer,
            said_not_written: false,
            said_roll_failed: false,
        }
    }

    /// Appends `event`'s line, saying on stderr a mutex taken abandoned and what the append had to say.
    pub(crate) fn write(&mut self, event: &JournalEvent, console: &mut Console) {
        let appended = match &self.writer {
            Ok((identity, mutex)) => {
                let now = Timestamp::now();
                append_line(&self.root, mutex, |abandoned| journal_line(identity, now, event, abandoned), now)
            }
            Err(reason) => {
                let note = JournalNote::NotWritten {
                    journal: self.root.join(JOURNAL_FILE),
                    reason: reason.clone(),
                };
                self.say(&note, console);
                return;
            }
        };
        if appended.abandoned
            && let Ok((_, mutex)) = &self.writer
        {
            console.err(&format!("gate: took {}, which a killed gate had left held", mutex.name()));
        }
        for note in &appended.notes {
            self.say(note, console);
        }
    }

    /// Says `note` on stderr, unless it is a missing line or a failed rollover and one of its kind was said already.
    fn say(&mut self, note: &JournalNote, console: &mut Console) {
        let said = match note {
            JournalNote::NotWritten { .. } => &mut self.said_not_written,
            JournalNote::RollFailed { .. } => &mut self.said_roll_failed,
            JournalNote::Archive(_) => {
                console.err(&format!("gate: {note}"));
                return;
            }
        };
        if !*said {
            *said = true;
            console.err(&format!("gate: {note}"));
        }
    }
}

/// What every journal line of this gate carries.
fn identity(request: &RunRequest) -> Result<JournalIdentity, String> {
    let process_start = gate_win32::process_start().map_err(|error| format!("cannot read this gate's start time: {error}"))?;
    let cwd = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
    let command: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
    Ok(JournalIdentity {
        pid: std::process::id(),
        process_start,
        kind: request.slot.as_str().to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        command: command.join(" "),
    })
}

/// `--test-journal-run <status>`: journals the run's queue, a take of no slot, and a release with `status`, then returns
/// it as the exit status. The command is not launched. With `--test-hold-until`, it holds between the take and the
/// release, and the hold counts as held time. Problem lines are said on stderr as they are found.
pub(crate) fn seam_run(request: &RunRequest, log: &Path, root: &ResolvedRoot, status: u8, console: &mut Console) -> u8 {
    let mut journal = RunJournal::open(root, request);
    journal.write(
        &JournalEvent::Queue {
            log: log.to_string_lossy().into_owned(),
        },
        console,
    );
    let took = Instant::now();
    journal.write(
        &JournalEvent::Take {
            slots: Vec::new(),
            waited_seconds: 0.0,
            abandoned: Vec::new(),
        },
        console,
    );
    if let Some(path) = &request.test_seams.hold_until {
        hold_until(path, console);
    }
    let held = took.elapsed().as_secs_f64();
    let code = i64::from(status);
    journal.write(
        &JournalEvent::Release {
            slots: Vec::new(),
            held_seconds: held,
            status: Some(code),
            facts: Some(RunFacts {
                wall_seconds: held,
                adjusted_seconds: None,
                verdict: Verdict::of(None, code, code),
            }),
        },
        console,
    );
    status
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

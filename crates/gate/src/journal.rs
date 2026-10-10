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

use crate::report::Report;
use crate::resolve::ResolvedRoot;

/// A state root's journal mutex.
struct JournalMutex(NamedMutex);

impl JournalLock for JournalMutex {
    type Guard<'a> = MutexGuard<'a>;

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

/// Where this gate journals, as whom, and the problem lines said so far.
pub(crate) struct RunJournal {
    root: PathBuf,
    /// The gate's identity and the journal mutex, or why the journal cannot be written at all.
    writer: Result<(JournalIdentity, JournalMutex), String>,
    said_not_written: bool,
    said_roll_failed: bool,
    /// The `gate: ` lines for stderr.
    lines: Vec<String>,
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
            lines: Vec::new(),
        }
    }

    /// Appends `event`'s line, saying a mutex taken abandoned and what the append had to say.
    pub(crate) fn write(&mut self, event: &JournalEvent) {
        let (appended, name) = match &self.writer {
            Ok((identity, mutex)) => {
                let now = Timestamp::now();
                let appended = append_line(&self.root, mutex, &journal_line(identity, now, event), now);
                (appended, mutex.0.name().to_owned())
            }
            Err(reason) => {
                let note = JournalNote::NotWritten {
                    journal: self.root.join(JOURNAL_FILE),
                    reason: reason.clone(),
                };
                self.say(&note);
                return;
            }
        };
        if appended.abandoned {
            self.lines.push(format!("gate: took {name}, which a killed gate had left held"));
        }
        for note in &appended.notes {
            self.say(note);
        }
    }

    /// Says `note`, unless it is a missing line or a failed rollover and one of its kind was said already.
    fn say(&mut self, note: &JournalNote) {
        let said = match note {
            JournalNote::NotWritten { .. } => &mut self.said_not_written,
            JournalNote::RollFailed { .. } => &mut self.said_roll_failed,
            JournalNote::Archive(_) => {
                self.lines.push(format!("gate: {note}"));
                return;
            }
        };
        if !*said {
            *said = true;
            self.lines.push(format!("gate: {note}"));
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

/// `--test-journal-run <status>`: journals the run's queue, a take of no slot, and a release with `status`, then exits
/// with it. The command is not launched.
pub(crate) fn seam_run(request: &RunRequest, log: &Path, root: &ResolvedRoot, status: u8) -> Report {
    let mut journal = RunJournal::open(root, request);
    journal.write(&JournalEvent::Queue {
        log: log.to_string_lossy().into_owned(),
    });
    let took = Instant::now();
    journal.write(&JournalEvent::Take {
        slots: Vec::new(),
        waited_seconds: 0.0,
    });
    let held = took.elapsed().as_secs_f64();
    let code = i64::from(status);
    journal.write(&JournalEvent::Release {
        slots: Vec::new(),
        held_seconds: held,
        status: Some(code),
        facts: Some(RunFacts {
            wall_seconds: held,
            adjusted_seconds: None,
            verdict: Verdict::of(None, code, code),
        }),
    });
    Report {
        status,
        stdout: Vec::new(),
        stderr: journal.lines,
    }
}

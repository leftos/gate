//! This gate's journal: the identity every one of its lines carries, its state root's journal mutex, and the problem
//! lines, a missing line and a failed rollover each said once a run.

use std::path::PathBuf;
use std::time::Duration;

use gate_state::{Held, JOURNAL_FILE, JournalEvent, JournalIdentity, JournalLock, JournalNote, KernelObject, LockError, append_line, journal_line};
use gate_win32::{AcquireError, MutexGuard, NamedMutex};
use jiff::Timestamp;

use crate::acquire;
use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::slots::ThisGate;

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
    pub(crate) fn open(root: &ResolvedRoot, gate: &ThisGate) -> RunJournal {
        let name = root.root.object_name(KernelObject::Journal);
        let writer = gate.journal_identity().and_then(|identity| match NamedMutex::open(&name) {
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
            acquire::say_taken_abandoned(console, mutex.name());
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

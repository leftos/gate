//! This gate's journal: the identity every one of its lines carries, its state root's journal mutex, and the problem
//! lines, a missing line and a failed rollover each said once a run.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_state::{
    Held, JOURNAL_FILE, JournalEvent, JournalIdentity, JournalLock, JournalNote, KernelObject, LockError, ROLLED_JOURNAL_FILE, RemoteTag,
    append_line, journal_line,
};
use gate_win32::{AcquireError, MutexGuard, NamedMutex};
use jiff::Timestamp;
use serde_json::Value;

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

    /// Appends `event`'s line, with the `remote` tag of a routed run before its fallback, saying on stderr a mutex taken
    /// abandoned and what the append had to say.
    pub(crate) fn write(&mut self, event: &JournalEvent, remote: Option<&RemoteTag>, console: &mut Console) {
        let appended = match &self.writer {
            Ok((identity, mutex)) => {
                let now = Timestamp::now();
                append_line(&self.root, mutex, |abandoned| journal_line(identity, now, event, remote, abandoned), now)
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

/// Whether a gate under the gate `pid` took a slot of its own at or after `since`: a `take` line of a slot or of the
/// machine naming `pid` as its `parentPid` in the journal of the state root at `root`, or in the journal it rolled over to when the current one
/// begins after `since`. A gate's marker names its parent by pid and start, and only a live parent is honoured, so
/// such a line is normally from a gate under this one. A nested gate orphaned by an earlier gate of the same pid could
/// still name it; that can only drop this run's cores, never add a hint.
///
/// # Errors
///
/// The system's error when a journal that exists cannot be read.
pub(crate) fn nested_took_slot(root: &Path, pid: u32, since: Timestamp) -> io::Result<bool> {
    let current = read_if_present(&root.join(JOURNAL_FILE))?;
    if names_nested_take(&current, pid, since) {
        return Ok(true);
    }
    let rolled_since = current.lines().next().and_then(line_time).is_none_or(|first| first >= since);
    if !rolled_since {
        return Ok(false);
    }
    let rolled = read_if_present(&root.join(ROLLED_JOURNAL_FILE))?;
    Ok(names_nested_take(&rolled, pid, since))
}

/// The text of the file at `path`, empty when it does not exist.
fn read_if_present(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        read => read.map_err(|error| io::Error::new(error.kind(), format!("cannot read {}: {error}", path.display()))),
    }
}

/// Whether one of the journal lines in `text` is a `take` of a slot or of the machine naming `pid` as its `parentPid` at
/// or after `since`: a take that lists a slot, or a critical gate's, which carries what it found of the machine. A
/// no-slot gate's take lists none and counts for nothing. A line that does not parse, as a torn last line may not,
/// names no one.
fn names_nested_take(text: &str, pid: u32, since: Timestamp) -> bool {
    text.lines()
        .filter(|line| line.contains(r#""event":"take""#) && line.contains(r#""parentPid":"#))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|line| {
            let took = line["slots"].as_array().is_some_and(|slots| !slots.is_empty()) || line.get("counts").is_some();
            took && line["parentPid"].as_u64() == Some(u64::from(pid))
                && line["time"]
                    .as_str()
                    .and_then(|time| time.parse::<Timestamp>().ok())
                    .is_some_and(|at| at >= since)
        })
}

/// The time of the journal line `line`, or `None` when it does not parse or carries none.
fn line_time(line: &str) -> Option<Timestamp> {
    let line: Value = serde_json::from_str(line).ok()?;
    line["time"].as_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::names_nested_take;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap_or(Timestamp::UNIX_EPOCH)
    }

    fn take(time: &str, parent: &str) -> String {
        format!(r#"{{"v":1,"time":"{time}","event":"take","pid":9,"slots":["Local\\gate-slot-0"],"waitedSeconds":0.0{parent}}}"#)
    }

    #[test]
    fn only_a_nested_take_of_a_slot_or_the_machine_counts() {
        let since = at("2026-10-11T10:00:00Z");
        let of_no_slot =
            |extra: &str| format!(r#"{{"v":1,"time":"2026-10-11T10:00:01.0000000Z","event":"take","pid":9,"slots":[]{extra},"parentPid":42}}"#);
        let machine = r#","counts":{"heavy":0,"light":0},"otherHeld":{"heavy":0,"light":0},"otherClaims":[]"#;
        let rows = [
            ("a take of no slot", of_no_slot(r#","waitedSeconds":0.0"#), false),
            ("a take of the machine that listed no slot", of_no_slot(machine), true),
        ];
        for (name, line, want) in rows {
            assert_eq!(names_nested_take(&format!("{line}\n"), 42, since), want, "{name}");
        }
    }

    #[test]
    fn only_a_take_naming_the_parent_at_or_after_its_start_counts() {
        let since = at("2026-10-11T10:00:00Z");
        let named = take("2026-10-11T10:00:00.0000000Z", r#","parentPid":42"#);
        let rows: [(&str, String, bool); 6] = [
            ("a take naming the parent at its start", named.clone(), true),
            (
                "a take naming another gate",
                take("2026-10-11T10:00:01.0000000Z", r#","parentPid":420"#),
                false,
            ),
            (
                "a take before the parent started",
                take("2026-10-11T09:59:59.0000000Z", r#","parentPid":42"#),
                false,
            ),
            ("a take naming no parent", take("2026-10-11T10:00:01.0000000Z", ""), false),
            (
                "a covered line naming the parent",
                r#"{"v":1,"time":"2026-10-11T10:00:01.0000000Z","event":"covered","pid":9,"parentPid":42}"#.to_owned(),
                false,
            ),
            ("a torn line", named[..named.len() - 2].to_owned(), false),
        ];
        for (name, line, want) in rows {
            assert_eq!(names_nested_take(&format!("{line}\n"), 42, since), want, "{name}");
        }
    }
}

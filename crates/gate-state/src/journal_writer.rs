//! The journal's writer: the v1 line of each event, and the append of one line to `<root>\events.jsonl` under the
//! journal lock. Before an append, a journal at or above [`JOURNAL_CAP_BYTES`] is moved over `events.1.jsonl` and the
//! moved file's `release` lines are archived ([`archive_rolled_journal`]) within the same lock hold; a torn last line
//! gets a line feed before the next. The lock comes in through [`JournalLock`], so the binary supplies the named mutex
//! and the tests their own.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::archive::archive_rolled_journal;
use crate::state::iso_utc;
use crate::verdict::Verdict;

/// The size at which an append first moves the journal over `events.1.jsonl`: 5 MiB.
pub const JOURNAL_CAP_BYTES: u64 = 5_242_880;
/// How long an append waits for the journal lock.
pub const JOURNAL_WAIT: Duration = Duration::from_secs(2);
/// The journal's file name under the state root.
pub const JOURNAL_FILE: &str = "events.jsonl";
/// The file name a rollover moves the journal to.
pub const ROLLED_JOURNAL_FILE: &str = "events.1.jsonl";
/// The format version every line the writer builds carries.
const VERSION: u64 = 1;

/// The lock every append of every gate under one state root holds: the journal mutex in the binary.
pub trait JournalLock {
    /// What holds the lock until it is dropped.
    type Guard<'a>
    where
        Self: 'a;

    /// Takes the lock, waiting at most `timeout`.
    ///
    /// # Errors
    ///
    /// Why the lock was not taken, such as that it was not free in time.
    fn acquire(&self, timeout: Duration) -> Result<Held<Self::Guard<'_>>, LockError>;
}

/// A taken journal lock.
#[derive(Debug)]
pub struct Held<G> {
    /// Holds the lock until dropped.
    pub guard: G,
    /// True when the lock was taken abandoned: its last holder was killed while it held it.
    pub abandoned: bool,
}

/// Why the journal lock was not taken, as the problem line says it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LockError(pub String);

/// What every line of one gate starts with after `v`, `time` and `event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalIdentity {
    /// The gate's process id.
    pub pid: u32,
    /// The gate process's start time.
    pub process_start: Timestamp,
    /// The slot kind the gate asked for: `heavy`, `light` or `critical`.
    pub kind: String,
    /// The gate's working directory.
    pub cwd: String,
    /// The gated command as one line.
    pub command: String,
}

/// One journal event with its own fields.
#[derive(Clone, Debug, PartialEq)]
pub enum JournalEvent {
    /// The gate queued for its slot.
    Queue {
        /// The log's full path.
        log: String,
    },
    /// The gate took its slot.
    Take {
        /// The names of the slots' mutexes.
        slots: Vec<String>,
        /// How long the gate waited, in seconds.
        waited_seconds: f64,
    },
    /// The gate is letting its slot go.
    Release {
        /// The names of the slots' mutexes.
        slots: Vec<String>,
        /// How long the gate held its slot, in seconds.
        held_seconds: f64,
        /// The gate's exit status, or `None` when it failed before its command had one.
        status: Option<i64>,
        /// The run's facts, or `None` when the gate ended before its command had a verdict.
        facts: Option<RunFacts>,
    },
}

/// What a `release` line says of the run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunFacts {
    /// The command's wall time in seconds.
    pub wall_seconds: f64,
    /// The load-adjusted seconds, or `None` when there are none.
    pub adjusted_seconds: Option<f64>,
    /// How the command ended.
    pub verdict: Verdict,
}

/// What an append did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Appended {
    /// True once the line is in the journal.
    pub written: bool,
    /// True when the journal lock was taken abandoned.
    pub abandoned: bool,
    /// What went wrong, or what the archive has to say, in the order it happened.
    pub notes: Vec<JournalNote>,
}

/// Something an append has to say, each a line the gate prints after `gate: `.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalNote {
    /// The line was not written.
    NotWritten {
        /// The journal's path.
        journal: PathBuf,
        /// Why.
        reason: String,
    },
    /// The journal was at the cap but could not be moved, so the line was appended to it as it is.
    RollFailed {
        /// The journal's path.
        journal: PathBuf,
        /// The rolled journal's path.
        rolled: PathBuf,
        /// Why.
        reason: String,
    },
    /// What the archive of the rolled journal said: lines it skipped, or why it could not archive or prune.
    Archive(String),
}

impl fmt::Display for JournalNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalNote::NotWritten { journal, reason } => write!(f, "could not write the journal {} ({reason})", journal.display()),
            JournalNote::RollFailed { journal, rolled, reason } => write!(
                f,
                "could not move {} over {}: {reason}; appended to it as it is",
                journal.display(),
                rolled.display()
            ),
            JournalNote::Archive(text) => f.write_str(text),
        }
    }
}

/// The v1 journal line of `event`, written by the gate `identity` at `time`: `v`, `time`, `event`, `pid`,
/// `processStart`, `kind`, `cwd` and `command`, then the event's own fields, times as ISO 8601 UTC with seven
/// fractional digits and seconds rounded to three decimals. A `release` without facts writes null times and the verdict
/// `unknown`.
#[must_use]
pub fn journal_line(identity: &JournalIdentity, time: Timestamp, event: &JournalEvent) -> String {
    let mut line = Map::new();
    line.insert("v".to_owned(), Value::from(VERSION));
    line.insert("time".to_owned(), Value::from(iso_utc(time)));
    line.insert("event".to_owned(), Value::from(event_name(event)));
    line.insert("pid".to_owned(), Value::from(identity.pid));
    line.insert("processStart".to_owned(), Value::from(iso_utc(identity.process_start)));
    line.insert("kind".to_owned(), Value::from(identity.kind.as_str()));
    line.insert("cwd".to_owned(), Value::from(identity.cwd.as_str()));
    line.insert("command".to_owned(), Value::from(identity.command.as_str()));
    match event {
        JournalEvent::Queue { log } => {
            line.insert("log".to_owned(), Value::from(log.as_str()));
        }
        JournalEvent::Take { slots, waited_seconds } => {
            line.insert("slots".to_owned(), Value::from(slots.clone()));
            line.insert("waitedSeconds".to_owned(), seconds(*waited_seconds));
        }
        JournalEvent::Release {
            slots,
            held_seconds,
            status,
            facts,
        } => {
            line.insert("slots".to_owned(), Value::from(slots.clone()));
            line.insert("heldSeconds".to_owned(), seconds(*held_seconds));
            line.insert("status".to_owned(), Value::from(*status));
            line.insert("wallSeconds".to_owned(), facts.map_or(Value::Null, |facts| seconds(facts.wall_seconds)));
            let adjusted = facts.and_then(|facts| facts.adjusted_seconds).map_or(Value::Null, seconds);
            line.insert("adjustedSeconds".to_owned(), adjusted);
            let verdict = facts.map_or(Verdict::Unknown, |facts| facts.verdict);
            line.insert("verdict".to_owned(), Value::from(verdict.as_str()));
        }
    }
    Value::Object(line).to_string()
}

fn event_name(event: &JournalEvent) -> &'static str {
    match event {
        JournalEvent::Queue { .. } => "queue",
        JournalEvent::Take { .. } => "take",
        JournalEvent::Release { .. } => "release",
    }
}

/// Seconds rounded to three decimals, a midpoint to the even digit, as .NET's `[math]::Round(x, 3)` gives them.
fn seconds(value: f64) -> Value {
    Value::from((value * 1000.0).round_ties_even() / 1000.0)
}

/// Appends `line` and a line feed to `root`'s journal while holding `lock`, waiting [`JOURNAL_WAIT`] for it. Under the
/// one hold it creates the root when it is missing, moves a journal at or above the cap over `events.1.jsonl` and
/// archives the moved file's release lines at `now` ([`archive_rolled_journal`]), then appends, a line feed first when
/// the journal's last byte is not one. A move that fails leaves the journal where it is and appends to it. A lock that
/// is not taken, or a write that fails, leaves the line unwritten and says why.
pub fn append_line<L: JournalLock>(root: &Path, lock: &L, line: &str, now: Timestamp) -> Appended {
    let journal = root.join(JOURNAL_FILE);
    let held = match lock.acquire(JOURNAL_WAIT) {
        Ok(held) => held,
        Err(error) => {
            return Appended {
                notes: vec![JournalNote::NotWritten {
                    journal,
                    reason: error.to_string(),
                }],
                ..Appended::default()
            };
        }
    };
    let mut appended = Appended {
        abandoned: held.abandoned,
        ..Appended::default()
    };
    if let Err(error) = fs::create_dir_all(root) {
        appended.notes.push(JournalNote::NotWritten {
            journal,
            reason: error.to_string(),
        });
        return appended;
    }
    roll(root, &journal, now, &mut appended.notes);
    match write_line(&journal, line) {
        Ok(()) => appended.written = true,
        Err(error) => appended.notes.push(JournalNote::NotWritten {
            journal,
            reason: error.to_string(),
        }),
    }
    drop(held);
    appended
}

/// Moves a journal at or above the cap over `events.1.jsonl` and archives it, or notes why it could not be moved.
fn roll(root: &Path, journal: &Path, now: Timestamp, notes: &mut Vec<JournalNote>) {
    let rolled = root.join(ROLLED_JOURNAL_FILE);
    let moved = match fs::metadata(journal) {
        Ok(metadata) if metadata.len() >= JOURNAL_CAP_BYTES => fs::rename(journal, &rolled).map(|()| true),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    };
    match moved {
        Ok(true) => notes.extend(archive_rolled_journal(root, now).into_iter().map(JournalNote::Archive)),
        Ok(false) => {}
        Err(error) => notes.push(JournalNote::RollFailed {
            journal: journal.to_path_buf(),
            rolled,
            reason: error.to_string(),
        }),
    }
}

/// Appends `line` and a line feed in one write, a line feed first when the file's last byte is not one.
fn write_line(journal: &Path, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).append(true).create(true).open(journal)?;
    let mut text = String::with_capacity(line.len() + 2);
    if ends_torn(&mut file)? {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
    file.write_all(text.as_bytes())
}

/// Whether the file is not empty and its last byte is not a line feed.
fn ends_torn(file: &mut File) -> io::Result<bool> {
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

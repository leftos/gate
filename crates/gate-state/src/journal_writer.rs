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
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
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

    /// The lock's kernel object name, which a line appended under an abandoned acquisition lists in `abandoned`.
    fn name(&self) -> &str;

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
    /// The slot kind the gate asked for: `heavy`, `light`, `critical` or `none`.
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
        /// Why a gate that could have been routed was kept on this machine, such as `--local`; `None` writes no
        /// `notRouted` field.
        not_routed: Option<String>,
    },
    /// The gate took its slot.
    Take {
        /// The names of the slots' mutexes.
        slots: Vec<String>,
        /// How long the gate waited, in seconds.
        waited_seconds: f64,
        /// The slot mutexes this gate took from a gate that died holding them; empty when it took none.
        abandoned: Vec<String>,
        /// What a critical gate found as it took the machine; `None` for any other gate.
        machine: Option<MachineSnapshot>,
        /// The pid of the parent gate whose slot did not cover this one, so it took its own; `None` for a gate under no
        /// parent.
        parent_pid: Option<u32>,
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
        /// Whether the run was routed to a work tree the builder's preflight found new, so its ceiling was raised:
        /// written as `cold: true`, the last of the event's own fields, and left out when false.
        cold: bool,
    },
    /// The gate left the queue without taking a slot.
    Abandon {
        /// How long the gate waited, in seconds.
        waited_seconds: f64,
    },
    /// The gate gave up its remote attempt and runs the command on this machine.
    Fallback {
        /// Why.
        reason: FallbackReason,
    },
    /// The gate runs under the slot of a parent gate that covers it, taking no slot of its own.
    Covered {
        /// The parent gate's process id.
        parent_pid: u32,
    },
    /// The gate runs on the slot of a parent gate that lends it while a critical request drains, taking no slot of its
    /// own.
    Parent {
        /// The parent gate's process id.
        parent_pid: u32,
    },
}

/// What a critical gate found as it took the machine, which its `take` line carries after `waitedSeconds`: the counts
/// of the pass that took it, the slots other gates hold within them, and the other gates' live claims or why they could
/// not be read.
///
/// It serializes as the line has it, `{"counts", "otherHeld", "otherClaims"}` or `{"counts", "otherHeld",
/// "snapshotError"}`, and reads back only from an object holding exactly one of the last two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineSnapshot {
    /// The heavy and light counts, as `counts`.
    pub counts: HeavyLight,
    /// The heavy and light slots within the counts whose mutex another gate has open, as `otherHeld`.
    pub other_held: HeavyLight,
    /// The live claims of other gates, the remote pool's left out, as `otherClaims`; else the reason they could not be
    /// read, as `snapshotError`.
    pub other_claims: Result<Vec<OtherClaim>, String>,
}

/// A [`MachineSnapshot`]'s JSON fields: `otherClaims` and `snapshotError` both optional, so reading one checks that
/// exactly one is there.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotFields {
    counts: HeavyLight,
    other_held: HeavyLight,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    other_claims: Option<Vec<OtherClaim>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot_error: Option<String>,
}

impl Serialize for MachineSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (other_claims, snapshot_error) = match &self.other_claims {
            Ok(claims) => (Some(claims.clone()), None),
            Err(reason) => (None, Some(reason.clone())),
        };
        SnapshotFields {
            counts: self.counts,
            other_held: self.other_held,
            other_claims,
            snapshot_error,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MachineSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = SnapshotFields::deserialize(deserializer)?;
        let other_claims = match (fields.other_claims, fields.snapshot_error) {
            (Some(claims), None) => Ok(claims),
            (None, Some(reason)) => Err(reason),
            _ => return Err(de::Error::custom("a machine snapshot has exactly one of otherClaims and snapshotError")),
        };
        Ok(Self {
            counts: fields.counts,
            other_held: fields.other_held,
            other_claims,
        })
    }
}

/// A number for each local pool, written as `{"heavy": <n>, "light": <n>}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeavyLight {
    pub heavy: u32,
    pub light: u32,
}

/// Another gate's live claim as a critical gate's `take` lists it, written as `{pid, kind, index, command, file}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherClaim {
    /// The claiming gate's pid.
    pub pid: u32,
    /// The claim's kind: `heavy`, `light` or `critical`.
    pub kind: String,
    /// The slot's index.
    pub index: u32,
    /// The claiming gate's command.
    pub command: String,
    /// The claim's file name in the claims folder.
    pub file: String,
}

/// Why a routed run gave up its remote attempt: the remote pool stayed full, the builder refused the run, or the
/// remote client could not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackReason {
    /// No remote slot came free within the wait's limit.
    Full,
    /// The builder was busy.
    Busy,
    /// The builder could not be reached.
    Unreachable,
    /// The builder's toolchain lacks what the caller's tree needs.
    Parity,
    /// The builder rebooted during the run.
    Rebooted,
    /// The builder lacks a tool the command needs.
    Tools,
    /// The builder could not clear or lay out the work folder for the run's repos.
    Layout,
    /// The builder's gate is of another version.
    Version,
    /// The remote client could not start.
    Client,
    /// The run's ignored inputs could not be sent to the builder, or its work folder no longer held them at the run.
    Inputs,
    /// The builder's work folder was not warm for the run's warm-up file. No run falls back for it; it is kept so
    /// journal lines that name it still read.
    Cold,
    /// The builder could not give the run its desktop.
    Desktop,
    /// The builder's gate is not installed where only administrators can change it.
    Install,
    /// The run's Git LFS content could not be sent to the builder, or the builder's store did not hold it at the run.
    Lfs,
}

impl FallbackReason {
    /// The word the journal's `fallback` line and the fallback line give.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackReason::Full => "full",
            FallbackReason::Busy => "busy",
            FallbackReason::Unreachable => "unreachable",
            FallbackReason::Parity => "parity",
            FallbackReason::Rebooted => "rebooted",
            FallbackReason::Tools => "tools",
            FallbackReason::Layout => "layout",
            FallbackReason::Version => "version",
            FallbackReason::Client => "client",
            FallbackReason::Inputs => "inputs",
            FallbackReason::Cold => "cold",
            FallbackReason::Desktop => "desktop",
            FallbackReason::Install => "install",
            FallbackReason::Lfs => "lfs",
        }
    }
}

/// The remote fields of a routed run's lines up to its fallback: `remote: true`, `host` and `key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTag {
    /// The build box the run was routed to, as `user@host`.
    pub host: String,
    /// The run's key on the build box.
    pub key: String,
}

/// What a `release` line says of the run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunFacts {
    /// The command's wall time in seconds.
    pub wall_seconds: f64,
    /// The load-adjusted seconds, or `None` when there are none.
    pub adjusted_seconds: Option<f64>,
    /// The command's average busy cores, or `None` when no sample measured them.
    pub avg_cores: Option<f64>,
    /// The command's peak busy cores over one sampler interval, or `None` when no sample measured them.
    pub peak_cores: Option<f64>,
    /// The cores other processes left free on average, or `None` when no sample measured them.
    pub free_cores: Option<f64>,
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
///
/// `remote` is the tag of a routed run up to its fallback: when it is `Some`, the event's own fields are followed by
/// `remote: true`, `host` and `key`; when it is `None`, the line has none of the three.
///
/// `lock_abandoned` is the journal lock's name when it was taken abandoned. When it is `Some`, the line carries
/// `abandoned` as its last field: a `take`'s own names followed by the lock's, any other event's just the lock's.
/// Without it, only a `take` that took a slot abandoned writes the field.
#[must_use]
pub fn journal_line(
    identity: &JournalIdentity,
    time: Timestamp,
    event: &JournalEvent,
    remote: Option<&RemoteTag>,
    lock_abandoned: Option<&str>,
) -> String {
    let mut line = Map::new();
    line.insert("v".to_owned(), Value::from(VERSION));
    line.insert("time".to_owned(), Value::from(iso_utc(time)));
    line.insert("event".to_owned(), Value::from(event_name(event)));
    line.insert("pid".to_owned(), Value::from(identity.pid));
    line.insert("processStart".to_owned(), Value::from(iso_utc(identity.process_start)));
    line.insert("kind".to_owned(), Value::from(identity.kind.as_str()));
    line.insert("cwd".to_owned(), Value::from(identity.cwd.as_str()));
    line.insert("command".to_owned(), Value::from(identity.command.as_str()));
    insert_own_fields(&mut line, event);
    if let Some(tag) = remote {
        line.insert("remote".to_owned(), Value::Bool(true));
        line.insert("host".to_owned(), Value::from(tag.host.as_str()));
        line.insert("key".to_owned(), Value::from(tag.key.as_str()));
    }
    let mut abandoned: Vec<Value> = match event {
        JournalEvent::Take { abandoned, .. } => abandoned.iter().map(|name| Value::from(name.as_str())).collect(),
        _ => Vec::new(),
    };
    abandoned.extend(lock_abandoned.map(Value::from));
    if !abandoned.is_empty() {
        line.insert("abandoned".to_owned(), Value::Array(abandoned));
    }
    Value::Object(line).to_string()
}

/// Inserts `event`'s own fields after the identity's.
fn insert_own_fields(line: &mut Map<String, Value>, event: &JournalEvent) {
    match event {
        JournalEvent::Queue { log, not_routed } => {
            line.insert("log".to_owned(), Value::from(log.as_str()));
            if let Some(not_routed) = not_routed {
                line.insert("notRouted".to_owned(), Value::from(not_routed.as_str()));
            }
        }
        JournalEvent::Take {
            slots,
            waited_seconds,
            machine,
            parent_pid,
            ..
        } => {
            line.insert("slots".to_owned(), Value::from(slots.clone()));
            line.insert("waitedSeconds".to_owned(), seconds(*waited_seconds));
            if let Some(machine) = machine {
                insert_machine(line, machine);
            }
            if let Some(parent_pid) = parent_pid {
                line.insert("parentPid".to_owned(), Value::from(*parent_pid));
            }
        }
        JournalEvent::Release {
            slots,
            held_seconds,
            status,
            facts,
            cold,
        } => {
            line.insert("slots".to_owned(), Value::from(slots.clone()));
            line.insert("heldSeconds".to_owned(), seconds(*held_seconds));
            line.insert("status".to_owned(), Value::from(*status));
            line.insert("wallSeconds".to_owned(), facts.map_or(Value::Null, |facts| seconds(facts.wall_seconds)));
            let adjusted = facts.and_then(|facts| facts.adjusted_seconds).map_or(Value::Null, seconds);
            line.insert("adjustedSeconds".to_owned(), adjusted);
            if let Some(facts) = facts {
                let cores = [
                    ("avgCores", facts.avg_cores),
                    ("peakCores", facts.peak_cores),
                    ("freeCores", facts.free_cores),
                ];
                for (key, value) in cores {
                    if let Some(value) = value {
                        line.insert(key.to_owned(), tenths(value));
                    }
                }
            }
            let verdict = facts.map_or(Verdict::Unknown, |facts| facts.verdict);
            line.insert("verdict".to_owned(), Value::from(verdict.as_str()));
            if *cold {
                line.insert("cold".to_owned(), Value::Bool(true));
            }
        }
        JournalEvent::Abandon { waited_seconds } => {
            line.insert("waitedSeconds".to_owned(), seconds(*waited_seconds));
        }
        JournalEvent::Fallback { reason } => {
            line.insert("reason".to_owned(), Value::from(reason.as_str()));
        }
        JournalEvent::Covered { parent_pid } | JournalEvent::Parent { parent_pid } => {
            line.insert("parentPid".to_owned(), Value::from(*parent_pid));
        }
    }
}

/// Inserts a critical take's snapshot: `counts`, `otherHeld`, then `otherClaims` or `snapshotError`.
fn insert_machine(line: &mut Map<String, Value>, machine: &MachineSnapshot) {
    // The snapshot is numbers, strings and arrays of objects with string keys, which always serialize to an object.
    if let Ok(Value::Object(fields)) = serde_json::to_value(machine) {
        line.extend(fields);
    }
}

fn event_name(event: &JournalEvent) -> &'static str {
    match event {
        JournalEvent::Queue { .. } => "queue",
        JournalEvent::Take { .. } => "take",
        JournalEvent::Release { .. } => "release",
        JournalEvent::Abandon { .. } => "abandon",
        JournalEvent::Fallback { .. } => "fallback",
        JournalEvent::Covered { .. } => "covered",
        JournalEvent::Parent { .. } => "parent",
    }
}

/// Seconds rounded to three decimals, a midpoint to the even digit, as .NET's `[math]::Round(x, 3)` gives them.
fn seconds(value: f64) -> Value {
    Value::from((value * 1000.0).round_ties_even() / 1000.0)
}

/// Cores rounded to one decimal, a midpoint to the even digit.
fn tenths(value: f64) -> Value {
    Value::from((value * 10.0).round_ties_even() / 10.0)
}

/// Appends the line `build` returns and a line feed to `root`'s journal while holding `lock`, waiting [`JOURNAL_WAIT`]
/// for it. Under the one hold it creates the root when it is missing, moves a journal at or above the cap over
/// `events.1.jsonl` and archives the moved file's release lines at `now` ([`archive_rolled_journal`]), then appends, a
/// line feed first when the journal's last byte is not one. A move that fails leaves the journal where it is and appends
/// to it. A lock that is not taken, or a write that fails, leaves the line unwritten and says why.
///
/// `build` is called once, with the lock's name when the lock was taken abandoned and `None` otherwise, so the line it
/// returns carries `abandoned`; [`Appended::abandoned`] reports the abandoned acquisition either way.
pub fn append_line<L: JournalLock>(root: &Path, lock: &L, build: impl FnOnce(Option<&str>) -> String, now: Timestamp) -> Appended {
    append_line_then(root, lock, build, now, || ()).0
}

/// [`append_line`], then `after` once the line is written and before the lock is let go, so what `after` writes is
/// written in the same hold as the line; returns what `after` returned, or `None` when the line was not written and
/// `after` did not run. `after` holds every other gate's append for as long as it runs, so it runs no git and parses no
/// journal.
pub fn append_line_then<L: JournalLock, T>(
    root: &Path,
    lock: &L,
    build: impl FnOnce(Option<&str>) -> String,
    now: Timestamp,
    after: impl FnOnce() -> T,
) -> (Appended, Option<T>) {
    let journal = root.join(JOURNAL_FILE);
    let held = match lock.acquire(JOURNAL_WAIT) {
        Ok(held) => held,
        Err(error) => {
            let appended = Appended {
                notes: vec![JournalNote::NotWritten {
                    journal,
                    reason: error.to_string(),
                }],
                ..Appended::default()
            };
            return (appended, None);
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
        return (appended, None);
    }
    roll(root, &journal, now, &mut appended.notes);
    let line = build(held.abandoned.then(|| lock.name()));
    let ran = match write_line(&journal, &line) {
        Ok(()) => {
            appended.written = true;
            Some(after())
        }
        Err(error) => {
            appended.notes.push(JournalNote::NotWritten {
                journal,
                reason: error.to_string(),
            });
            None
        }
    };
    drop(held);
    (appended, ran)
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

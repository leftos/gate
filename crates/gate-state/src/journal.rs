//! The gate journal: the `events.jsonl` that `gate.ps1` appends a line to as each gate queues, takes its slot and ends,
//! read into runs, the newest runs and, per command, how long the runs that passed held their slot over the last week.
//!
//! A run is keyed by the gate's `pid` and `processStart`, since the journal carries no run id: its `queue` line, its
//! `take` once it holds a slot, then `release` as it lets the slot go or `abandon` when it ended still waiting. A run
//! routed to the remote pool carries `remote` and `host` on those lines, and a `fallback` line when it was sent back to
//! the local queue, which ends its remote take (no `release` follows for the remote slot): its local `take` follows. A
//! `parent` line that names a parent gate marks a run on that gate's slot, whether it fell back onto it or was lent it
//! while a critical request drained; a gate lent the slot before it queued writes only that line, which starts its run.
//! A `covered` line, which `gate.exe` writes alone for a gate running under its parent's slot, is a run of its own, on
//! its parent's slot. A critical gate's `take` carries every slot it took and what it found of the machine. A `parent`
//! or `covered` line naming no parent and every line of the gate's own self-test (a `cwd` holding `gate-selftest`) are
//! ignored, except that any line's `abandoned` names join a run the cache already knows, and a line that does not parse
//! is skipped. The folder (the PowerShell gate's or `gate.exe`'s state root), the prefix, the clock and the process
//! liveness check all come from the caller.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::iter::{Peekable, repeat_n};
use std::path::{Path, PathBuf};
use std::str::Chars;
use std::time::SystemTime;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::journal_writer::MachineSnapshot;

/// How many runs that are not covered the history lists: the newest ones.
const RECENT_RUNS: usize = 50;
/// How many covered runs the history lists at most: the newest of those newer than the oldest run it lists that is not
/// covered.
const RECENT_COVERED_RUNS: usize = 50;
/// How far back a run's take may be for the summary to count it.
const SUMMARY_WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);
/// What every working directory of the gate's self-test holds.
const SELFTEST_MARKER: &str = "gate-selftest";
/// The flags whose next word names the tree a command runs in.
const TREE_FLAGS: [&str; 4] = ["--manifest-path", "--directory", "--dir", "-C"];
/// What a scratch file under `.tmp` becomes.
pub(crate) const ELIDED: &str = "\u{2026}";

/// How a run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    /// Released with status 0.
    Ok,
    /// Released with a non-zero status, or with none when the gate failed before its command ran.
    Failed,
    /// Queued, then ended without a slot.
    Abandoned,
    /// Not ended, and its gate is still running.
    Running,
    /// Not ended, and its gate is gone: it was killed, or its end was never written.
    Unrecorded,
    /// Ran on its parent gate's slot, fallen back onto it or lent it while a critical request drained: its command ran
    /// under the parent's run, which owns its time.
    Parent,
    /// Ran under a parent gate whose held slot covers its kind, from a `covered` line: it took no slot and wrote no
    /// ticket, and its command ran under the parent's run, which owns its time.
    Covered,
}

/// One run of a gate, in the JSON shape `GET /api/gate/history` serves.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRun {
    /// The gated command, as [`normalize_command`] gives it.
    pub command: String,
    /// The folder the gate ran in, as its first journal line read for the run wrote it; `None` when no line carried one.
    pub cwd: Option<String>,
    /// The repository the run's folder belongs to, as the caller's repo function names it; `None` without a folder or
    /// when the function names none.
    pub repo: Option<String>,
    /// The gate's `-Slot`: `heavy`, `light`, `critical` or `none`; a string because `critical` and `none` are not a
    /// `SlotKind`.
    pub kind: String,
    /// The index of the first slot the run took, or `None` when it took none.
    pub slot: Option<u32>,
    /// Every slot the run's take took, by mutex name as the line writes it (`Local\gate-slot-0`), in line order; a
    /// critical gate's lists every heavy slot and the light slots it does not keep free. Empty when it took none.
    pub slots: Vec<String>,
    /// What a critical gate's take found of the machine: `counts`, `otherHeld`, then `otherClaims` or `snapshotError`;
    /// `None`, serialized as null, for any other take, a run that took no slot, or a snapshot that does not read as one.
    pub machine: Option<MachineSnapshot>,
    /// True for a run routed to the remote pool: its latest queue, take or release line says `remote` and no `fallback`
    /// line came after it. It stays true once the run has released its remote slot, and the summary counts the run's
    /// held time on the command's remote row.
    pub remote: bool,
    /// The build box a remote run waits for or runs on, its ssh destination; `None` for a local run.
    pub host: Option<String>,
    /// Why the run, routed to the remote pool, was sent back to the local queue: `full` (the pool stayed full for
    /// 60 s), `busy` (the box's work folder is locked by another run), `unreachable`, `rebooted`, `parity`, `tools`
    /// (the box could not restore the repo's dotnet local tools), `layout` (the box could not clear or lay out the
    /// work folder for the run's repos), `version` (the box's gate is of another version) or
    /// `client` (the remote client could not start); `unknown` when the line names none. `None` when it was not.
    pub fallback: Option<String>,
    /// The free text the run gave `--reason`, from its `queue` line; serialised only when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The run's own gate's pid, from its run key.
    pub pid: u32,
    /// The run's own gate's process start, as the journal wrote it (ISO 8601 UTC, seven fractional digits); with
    /// [`HistoryRun::pid`] the run's key.
    pub process_start: String,
    /// The pid of the gate whose slot a [`RunState::Parent`] or [`RunState::Covered`] run ran on, from its `parent` or
    /// `covered` line; `None` for any other run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_pid: Option<u32>,
    /// How the run stands.
    pub state: RunState,
    /// True while the run's own gate is live by the read's liveness check, whatever its state: a
    /// [`RunState::Covered`] or [`RunState::Parent`] run stays in that state while its gate runs.
    pub running: bool,
    /// When the gate queued, as the journal wrote it (ISO 8601 UTC, seven fractional digits); `None` when its queue
    /// line is in neither journal file.
    pub queued: Option<String>,
    /// When the gate took its slot, as the journal wrote it; for a run that took no slot (`kind` `none`), when it
    /// journalled its take, which is when it started. `None` when it journalled no take.
    pub started: Option<String>,
    /// How long the gate waited for its slot, from its take or its abandon.
    pub waited_seconds: Option<f64>,
    /// How long the gate held its slot, from its release; for a [`RunState::Parent`] or [`RunState::Covered`] run, how
    /// long its parent's run held its slot, `None` until the journal holds that run's release. The status, verdict, wall
    /// and adjusted seconds below follow the same rule.
    pub held_seconds: Option<f64>,
    /// The gate's exit status, from its release; `None` without a release or when the gate failed before its command
    /// ran. For a [`RunState::Parent`] or [`RunState::Covered`] run, its parent's run's status, `None` until the journal
    /// holds that run's release.
    pub status: Option<i64>,
    /// How the command ended, from its release's `verdict`, which both gates write; `None` without a release or when the
    /// release line lacks the field, or holds null. For a [`RunState::Parent`] or [`RunState::Covered`] run, its parent's run's, as for `status`.
    pub verdict: Option<String>,
    /// The command's wall time, from its release's `wallSeconds` (the PowerShell gate rounds it to one decimal, the Rust
    /// gate to three); `None` without a release or when the release line lacks the field, or holds null. For a
    /// [`RunState::Parent`] or [`RunState::Covered`] run, its parent's run's.
    pub wall_seconds: Option<f64>,
    /// The command's load-adjusted seconds, from its release's `adjustedSeconds`; `None` without one. For a
    /// [`RunState::Parent`] or [`RunState::Covered`] run, its parent's run's.
    pub adjusted_seconds: Option<f64>,
    /// Every name a line of the run lists in `abandoned`: slots it took from a gate that died holding them and a journal
    /// mutex it took abandoned, in line order without repeats; empty when none.
    pub abandoned: Vec<String>,
    /// The remote slot the run held before a fallback sent it back to run here: the remote take's start and wait, and
    /// the time of the `fallback` line that ended it. `None` for a run that never fell back, one whose fallback came
    /// before any remote take (`full`, key-lock `busy`, `client`), and one a later remote line made remote again.
    /// Serialized as `remoteTake`, absent for `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_take: Option<RemoteTake>,
    /// Whether the run's release says it was routed to a builder with a cold work folder: the release line's `cold`,
    /// `true` only for a cold routed run; `None` when the run has not released or its release has no `cold`. Serialized
    /// as `cold`, absent for `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold: Option<bool>,
}

/// The remote slot a routed run held before it fell back to run here: the `take` of its remote attempt, and when the
/// attempt ended. A later remote line for the same run drops it with the fallback.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTake {
    /// When the run took the remote slot, as the journal wrote it (ISO 8601 UTC, seven fractional digits).
    pub started: String,
    /// How long the run waited for the remote slot, from its take; `None` when that line has no `waitedSeconds`.
    pub waited_seconds: Option<f64>,
    /// When the run fell back, as the journal wrote it: the `fallback` line's own time, after which the remote take no
    /// longer stands.
    pub fell_back_at: String,
}

/// How long the runs of one command in one repository that passed held their slot, the remote runs apart from the local
/// ones.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSummary {
    /// The command, as [`normalize_command`] gives it.
    pub command: String,
    /// The repository the runs' folders belong to, as [`HistoryRun::repo`] names it.
    pub repo: Option<String>,
    /// True for the runs that held a remote slot, as [`HistoryRun::remote`] has it.
    pub remote: bool,
    /// The runs counted.
    pub count: usize,
    /// The median held time, by nearest rank.
    pub p50_seconds: f64,
    /// The 90th percentile held time, by nearest rank.
    pub p90_seconds: f64,
    /// The longest held time.
    pub max_seconds: f64,
}

/// The journal as the Gates tab shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct History {
    /// The 50 newest runs that are not covered and the 50 newest covered runs newer than the oldest of them, in every
    /// state: at most 100 runs. When fewer than 50 runs are not covered, every one of them is listed with the 50 newest
    /// covered runs. Newest first by the time of the first journal line read for each run (its queue line unless that
    /// rolled out of the journal; the `parent` or `covered` line of a gate that never queued, as a covered gate never
    /// does), then by pid and process start.
    pub runs: Vec<HistoryRun>,
    /// Every open run, uncapped, newest take first, then by pid and process start: a run that took its slot, has not
    /// released or abandoned it, did not run on its parent's slot, and whose gate is live by the read's liveness check.
    /// Each row is built as a [`History::runs`] row, a run may be in both lists, and a reader dedupes the two by pid and
    /// process start. An open run whose take has rolled out of both journal files is not found.
    pub open: Vec<HistoryRun>,
    /// One row per repository, command and remote flag over the runs taken in the last 7 days that passed, the most run
    /// first, then by command, then by repository (none first), then local before remote.
    pub summary: Vec<CommandSummary>,
}

/// The journal's runs kept across reads, so each read parses only the complete lines appended since the last.
#[derive(Debug, Default)]
pub struct JournalCache {
    /// The journal last read; another one starts the cache again.
    journal: Option<PathBuf>,
    /// The journal's creation time as of the last read.
    created: Option<SystemTime>,
    /// The rolled journal's identity as of the last read: `None` when it was absent.
    rolled: Option<FileIdentity>,
    /// The byte just past the last complete line read from the journal.
    offset: u64,
    /// The runs read so far.
    runs: HashMap<RunKey, Run>,
}

/// A run's key: its gate's pid and that process's start time as the journal wrote it.
type RunKey = (u32, String);

/// A file's length and last write time, compared between reads to see that it changed.
pub(crate) type FileIdentity = (u64, Option<SystemTime>);

/// The identity of the file at `path`, or `None` when there is none.
pub(crate) fn file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.len(), metadata.modified().ok()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A journal time: the moment, and the text the journal wrote for it.
#[derive(Clone, Debug)]
struct Stamp {
    at: Timestamp,
    text: String,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Released,
    Abandoned,
}

/// One run as read so far.
#[derive(Clone, Debug)]
struct Run {
    command: String,
    /// The `cwd` of the first line read for the run; `None` when it was empty.
    cwd: Option<String>,
    kind: String,
    /// The time of the first line read for the run, its queue line when that was read: what the runs are ordered by.
    first: Timestamp,
    queued: Option<Stamp>,
    started: Option<Stamp>,
    slot: Option<u32>,
    /// The slot names of the run's take.
    slots: Vec<String>,
    /// The snapshot of the run's critical take.
    machine: Option<MachineSnapshot>,
    remote: bool,
    host: Option<String>,
    fallback: Option<String>,
    /// Whether the run's release says it was routed to a builder with a cold work folder: the release line's `cold`.
    cold: Option<bool>,
    /// The free text the run's `queue` line carries from `--reason`; the first such line's when more than one names one.
    reason: Option<String>,
    /// The remote take a fallback ended, kept for the reader: its start and wait, and when the run fell back.
    remote_take: Option<RemoteTake>,
    waited_seconds: Option<f64>,
    held_seconds: Option<f64>,
    status: Option<i64>,
    verdict: Option<String>,
    wall_seconds: Option<f64>,
    adjusted_seconds: Option<f64>,
    end: Option<End>,
    /// The parent gate's pid and the time of the run's `parent` or `covered` line, once it read one naming a parent:
    /// after its `fallback`, after its `queue` when it was lent the slot as it waited, or alone when it was lent it before
    /// it queued or ran covered.
    parent: Option<(u32, Timestamp)>,
    /// True when the line that named the parent was a `covered` line.
    covered: bool,
    /// Every name the run's lines listed in `abandoned`, in line order without repeats.
    abandoned: Vec<String>,
}

/// The fields of a journal line the history reads. The release's cores fields (`avgCores`, `peakCores`, `freeCores`)
/// are not read, and a line parses with or without them. A critical take's snapshot fields are read as they
/// are, so a snapshot that does not read as one leaves the run without it rather than dropping the line.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    /// The format version: 1 for the Rust gate's lines, absent from the PowerShell gate's.
    #[serde(default)]
    v: Option<u64>,
    time: String,
    event: String,
    pid: u32,
    process_start: String,
    kind: String,
    cwd: String,
    command: String,
    #[serde(default)]
    slots: Value,
    /// The names a line lists as taken abandoned: read from any event's line, whatever its type.
    #[serde(default)]
    abandoned: Value,
    #[serde(default)]
    waited_seconds: Option<f64>,
    #[serde(default)]
    held_seconds: Option<f64>,
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    verdict: Option<String>,
    #[serde(default)]
    wall_seconds: Option<f64>,
    #[serde(default)]
    adjusted_seconds: Option<f64>,
    #[serde(default)]
    remote: bool,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    /// Whether the run was routed to a builder with a cold work folder: read from a `release` line only.
    #[serde(default)]
    cold: Option<bool>,
    #[serde(default)]
    parent_pid: Option<u32>,
    #[serde(default)]
    counts: Option<Value>,
    #[serde(default)]
    other_held: Option<Value>,
    #[serde(default)]
    other_claims: Option<Value>,
    #[serde(default)]
    snapshot_error: Option<Value>,
}

impl Line {
    /// The line's machine snapshot, `counts`, `otherHeld`, then `otherClaims` or `snapshotError`; `None` when it has no
    /// `counts`, or the fields do not read as a snapshot.
    fn machine(&self) -> Option<MachineSnapshot> {
        let mut fields = Map::new();
        fields.insert("counts".to_owned(), self.counts.clone()?);
        let rest = [
            ("otherHeld", &self.other_held),
            ("otherClaims", &self.other_claims),
            ("snapshotError", &self.snapshot_error),
        ];
        for (key, value) in rest {
            if let Some(value) = value {
                fields.insert(key.to_owned(), value.clone());
            }
        }
        serde_json::from_value(Value::Object(fields)).ok()
    }
}

impl JournalCache {
    /// The history in `dir`'s `<prefix>events.jsonl` and `<prefix>events.1.jsonl` as of `now`, a run with no release being
    /// [`RunState::Running`] when `is_live(pid, processStart)` says its gate still runs, and a run's repository being `repo_of(cwd)`,
    /// the caller's rule for naming the repository a folder belongs to.
    ///
    /// The first read, and a read after the journal was rolled (it is shorter than the bytes read so far, or its creation time changed, or
    /// `events.1.jsonl` appeared, went or changed its length or write time) or after `dir` or `prefix` changed, reads `events.1.jsonl` and then the
    /// journal from its start; any other read parses only the journal's complete lines appended since the last, a trailing fragment without its line
    /// feed being left for the next. Runs older than 7 days are then dropped unless they are among the runs the history lists ([`History::runs`]) or
    /// are open ([`History::open`]), which it lists whatever their age. A missing file reads as an empty one.
    ///
    /// # Errors
    ///
    /// Fails when a journal file exists but cannot be read; the cache then starts again at the next read.
    pub fn read(
        &mut self,
        dir: &Path,
        prefix: &str,
        now: Timestamp,
        is_live: impl Fn(u32, &str) -> bool,
        repo_of: impl Fn(&str) -> Option<String>,
    ) -> io::Result<History> {
        if let Err(error) = self.refresh(dir, prefix) {
            *self = Self::default();
            return Err(error);
        }
        let listed = listed(&self.runs);
        self.prune(now, &listed, &is_live);
        Ok(self.history(now, &listed, &is_live, &repo_of))
    }

    fn refresh(&mut self, dir: &Path, prefix: &str) -> io::Result<()> {
        let journal = dir.join(format!("{prefix}events.jsonl"));
        let (len, created) = match fs::metadata(&journal) {
            Ok(metadata) => (metadata.len(), metadata.created().ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (0, None),
            Err(error) => return Err(error),
        };
        // A roll moves the journal over the rolled file and begins a new one at once, so NTFS's tunnelling can give the new
        // journal the old one's creation time: a change to the rolled file is the signal that does not depend on it.
        let rolled = dir.join(format!("{prefix}events.1.jsonl"));
        let rolled_identity = file_identity(&rolled)?;
        let same_journal = self.journal.as_deref() == Some(journal.as_path());
        if !same_journal || len < self.offset || created != self.created || rolled_identity != self.rolled {
            *self = Self {
                journal: Some(journal.clone()),
                ..Self::default()
            };
            self.read_rolled(&rolled)?;
        }
        self.created = created;
        self.rolled = rolled_identity;
        if len > self.offset {
            self.read_appended(&journal)?;
        }
        Ok(())
    }

    /// Reads every line of the rolled journal, its last line included: nothing appends to it any more.
    fn read_rolled(&mut self, path: &Path) -> io::Result<()> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            self.apply(line);
        }
        Ok(())
    }

    /// Reads the journal's complete lines past [`Self::offset`], moving it past each.
    fn read_appended(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
                return Ok(());
            }
            self.apply(&line);
            self.offset += u64::try_from(line.len()).unwrap_or(u64::MAX);
        }
    }

    /// Adds one journal line to its run; a line that does not parse, a line whose `v` is not 1 (a line without `v` is the
    /// PowerShell gate's), and the self-test's are skipped.
    fn apply(&mut self, bytes: &[u8]) {
        let Ok(line) = serde_json::from_slice::<Line>(bytes.trim_ascii()) else {
            return;
        };
        if line.v.is_some_and(|v| v != 1) || line.cwd.contains(SELFTEST_MARKER) {
            return;
        }
        let Ok(at) = line.time.parse::<Timestamp>() else {
            return;
        };
        self.fold(&line, at);
    }

    /// Folds one parsed line into its run: the five events a run is made of create one, as does a `parent` or `covered`
    /// line naming a parent gate, which marks a run on that gate's slot ([`Self::apply_parent`]). Every line, `parent`
    /// and `covered` included, adds the names it lists in `abandoned` to a run the cache already knows. A run's first
    /// `queue` line sets its queued time; a later one does not. A `fallback` line sends a remote run back to the local
    /// queue: the run is no longer remote, and keeps the reason.
    fn fold(&mut self, line: &Line, at: Timestamp) {
        let key = (line.pid, line.process_start.clone());
        if matches!(line.event.as_str(), "parent" | "covered") {
            self.apply_parent(key, line, at);
            return;
        }
        if !matches!(line.event.as_str(), "queue" | "take" | "release" | "abandon" | "fallback") {
            self.note_abandoned(&key, &line.abandoned);
            return;
        }
        let run = self.runs.entry(key).or_insert_with(|| Run::new(line, at));
        run.note_abandoned(&line.abandoned);
        run.note(line, Stamp { at, text: line.time.clone() });
    }

    /// Adds `line`'s `abandoned` names to run `key` when the cache knows it; a line of an event that made no run adds
    /// nothing.
    fn note_abandoned(&mut self, key: &RunKey, line: &Value) {
        if let Some(run) = self.runs.get_mut(key) {
            run.note_abandoned(line);
        }
    }

    /// Marks run `key` as run on the slot of the parent gate its `parent` or `covered` line names: fallen back onto it or
    /// lent it while a critical request drained, as it waited or before it queued, or covered by it. A gate lent the slot
    /// before it queued, or covered, has no run yet, and the line starts one, with no queued or started time and the
    /// line's own time and kind. A line naming no parent only adds its `abandoned` names to a run the cache already knows.
    fn apply_parent(&mut self, key: RunKey, line: &Line, at: Timestamp) {
        let Some(parent_pid) = line.parent_pid else {
            self.note_abandoned(&key, &line.abandoned);
            return;
        };
        let run = self.runs.entry(key).or_insert_with(|| Run::new(line, at));
        run.note_abandoned(&line.abandoned);
        run.parent = Some((parent_pid, at));
        run.covered = line.event == "covered";
    }

    /// The run a gate that said `parent` or `covered` at `at` ran under, once that run has released its slot: the run of
    /// gate `pid` alive at `at`, the one whose first line is the latest at or before it, whether it had taken its slot
    /// by then or, standing in for an ended parent, took it after. An older run of a gate that had the same pid is never
    /// it.
    fn released_parent(&self, (pid, at): (u32, Timestamp)) -> Option<(&RunKey, &Run)> {
        self.runs
            .iter()
            .filter(|((run_pid, _), run)| *run_pid == pid && run.first <= at)
            .max_by_key(|(_, run)| run.first)
            .filter(|(_, run)| run.end == Some(End::Released))
    }

    /// Drops the runs that started more than 7 days before `now`, unless `listed`, the keys [`listed`] gives of the runs
    /// the history lists, names them, or it is the run [`Self::released_parent`] picks for a listed covered or fallen-back
    /// run, whose row fills its status and held time from it, or it is open ([`Run::is_open`]), which the history lists
    /// whatever its age.
    fn prune(&mut self, now: Timestamp, listed: &[RunKey], is_live: &impl Fn(u32, &str) -> bool) {
        let cutoff = now - SUMMARY_WINDOW;
        let parents: Vec<RunKey> = listed
            .iter()
            .filter_map(|key| self.runs.get(key))
            .filter_map(|run| run.parent)
            .filter_map(|parent| self.released_parent(parent).map(|(key, _)| key.clone()))
            .collect();
        self.runs.retain(|key, run| {
            run.started.as_ref().map_or(run.first, |started| started.at) >= cutoff
                || listed.contains(key)
                || parents.contains(key)
                || run.is_open(key.0, &key.1, is_live)
        });
    }

    /// The history of the `listed` runs, in their order, and the summary of every run; `listed` runs [`Self::prune`]
    /// kept, so each is still in the cache.
    fn history(&self, now: Timestamp, listed: &[RunKey], is_live: &impl Fn(u32, &str) -> bool, repo_of: &impl Fn(&str) -> Option<String>) -> History {
        let runs = listed
            .iter()
            .filter_map(|key| self.runs.get(key).map(|run| (key, run)))
            .map(|(key, run)| {
                let parent = run.parent.and_then(|parent| self.released_parent(parent)).map(|(_, run)| run);
                run.row(key, is_live, run.repo(repo_of), parent)
            })
            .collect();
        History {
            runs,
            open: self.open_runs(is_live, repo_of),
            summary: summarize(self.runs.values(), now - SUMMARY_WINDOW, repo_of),
        }
    }

    /// The row of every open run ([`Run::is_open`]), newest take first, then by pid and process start.
    fn open_runs(&self, is_live: &impl Fn(u32, &str) -> bool, repo_of: &impl Fn(&str) -> Option<String>) -> Vec<HistoryRun> {
        let mut open: Vec<(&RunKey, &Run)> = self
            .runs
            .iter()
            .filter(|((pid, process_start), run)| run.is_open(*pid, process_start, is_live))
            .collect();
        let taken = |run: &Run| run.started.as_ref().map(|stamp| stamp.at);
        open.sort_by(|(a_key, a_run), (b_key, b_run)| taken(b_run).cmp(&taken(a_run)).then_with(|| b_key.cmp(a_key)));
        open.into_iter()
            .map(|(key, run)| {
                let parent = run.parent.and_then(|parent| self.released_parent(parent)).map(|(_, run)| run);
                run.row(key, is_live, run.repo(repo_of), parent)
            })
            .collect()
    }
}

impl Run {
    /// A run from the first line read for it.
    fn new(line: &Line, at: Timestamp) -> Self {
        Run {
            command: normalize_command(&line.command),
            cwd: Some(line.cwd.clone()).filter(|cwd| !cwd.is_empty()),
            kind: line.kind.clone(),
            first: at,
            queued: None,
            started: None,
            slot: None,
            slots: Vec::new(),
            machine: None,
            remote: false,
            host: None,
            fallback: None,
            cold: None,
            reason: None,
            remote_take: None,
            waited_seconds: None,
            held_seconds: None,
            status: None,
            verdict: None,
            wall_seconds: None,
            adjusted_seconds: None,
            end: None,
            parent: None,
            covered: false,
            abandoned: Vec::new(),
        }
    }

    /// Adds the names `line` lists in `abandoned` to the run's list, in line order without repeats.
    fn note_abandoned(&mut self, line: &Value) {
        for name in names(line) {
            if !self.abandoned.contains(&name) {
                self.abandoned.push(name);
            }
        }
    }

    /// Folds one event's line into the run.
    fn note(&mut self, line: &Line, stamp: Stamp) {
        match line.event.as_str() {
            "queue" => {
                self.queued.get_or_insert(stamp);
                if self.reason.is_none() {
                    self.reason.clone_from(&line.reason);
                }
                self.route(line.remote, line.host.clone());
            }
            "take" => {
                self.started = Some(stamp);
                self.slot = first_slot(&line.slots);
                self.slots = names(&line.slots).collect();
                self.machine = line.machine();
                self.waited_seconds = line.waited_seconds;
                self.route(line.remote, line.host.clone());
            }
            "release" => {
                self.held_seconds = line.held_seconds;
                self.status = line.status;
                self.verdict.clone_from(&line.verdict);
                self.wall_seconds = line.wall_seconds;
                self.adjusted_seconds = line.adjusted_seconds;
                self.cold = line.cold;
                self.end = Some(End::Released);
                // A release line that says `remote` marks a run whose queue and take rolled out of the journal; one that
                // does not leaves the run as its take left it.
                if line.remote {
                    self.route(true, line.host.clone());
                }
            }
            "fallback" => {
                // The remote take the fallback ended stands for the reader, with the fallback line's own time as the
                // moment it fell back; a run the full pool sent back took no remote slot, so it keeps none.
                let kept = match (&self.started, self.remote) {
                    (Some(started), true) => Some(RemoteTake {
                        started: started.text.clone(),
                        waited_seconds: self.waited_seconds,
                        fell_back_at: stamp.text,
                    }),
                    _ => None,
                };
                // The run holds nothing until a local take: a remote take's slots, snapshot, start and wait no longer stand.
                self.route(false, None);
                self.slot = None;
                self.slots.clear();
                self.machine = None;
                self.started = None;
                self.waited_seconds = None;
                self.remote_take = kept;
                self.fallback = Some(line.reason.clone().unwrap_or_else(|| "unknown".to_owned()));
            }
            _ => {
                self.waited_seconds = line.waited_seconds;
                self.end = Some(End::Abandoned);
            }
        }
    }

    /// Marks the run remote on `host`, or local. A remote line after a `fallback` makes the run a plain remote one again,
    /// dropping the remote take that fallback kept.
    fn route(&mut self, remote: bool, host: Option<String>) {
        self.remote = remote;
        self.host = host.filter(|_| remote);
        if remote {
            self.fallback = None;
            self.remote_take = None;
        }
    }

    /// Whether the run is a covered run that took a slot of its own once its parent ended as its command ran: a covered
    /// run journals a `take` for no other slot.
    fn took_late(&self) -> bool {
        self.covered && self.started.is_some()
    }

    /// Whether the run ran on its parent's slot, which its row reads its end from: a run with a parent, unless it is a
    /// covered run that took a slot of its own ([`Self::took_late`]), which reads as any run with its own take does,
    /// running, unrecorded or released.
    fn on_parent(&self) -> bool {
        self.parent.is_some() && !self.took_late()
    }

    /// Whether the run is open: it took its slot, has not released or abandoned it, did not run on its parent's slot
    /// ([`Self::on_parent`]), and its gate is live by `is_live`.
    fn is_open(&self, pid: u32, process_start: &str, is_live: &impl Fn(u32, &str) -> bool) -> bool {
        self.started.is_some() && self.end.is_none() && !self.on_parent() && is_live(pid, process_start)
    }

    fn state(&self, (pid, process_start): &RunKey, is_live: &impl Fn(u32, &str) -> bool) -> RunState {
        if self.on_parent() {
            return if self.covered { RunState::Covered } else { RunState::Parent };
        }
        match self.end {
            Some(End::Released) if self.status == Some(0) => RunState::Ok,
            Some(End::Released) => RunState::Failed,
            Some(End::Abandoned) => RunState::Abandoned,
            None if is_live(*pid, process_start) => RunState::Running,
            None => RunState::Unrecorded,
        }
    }

    /// The repository the run's folder belongs to, by `repo_of`; `None` without a folder.
    fn repo(&self, repo_of: &impl Fn(&str) -> Option<String>) -> Option<String> {
        self.cwd.as_deref().and_then(repo_of)
    }

    /// The row of the run keyed `key`, its state and whether its gate runs judged by `is_live`; a run on its parent's
    /// slot ([`Self::on_parent`]) takes its status and held time from `parent`, its parent's ended run.
    fn row(&self, key: &RunKey, is_live: &impl Fn(u32, &str) -> bool, repo: Option<String>, parent: Option<&Self>) -> HistoryRun {
        let end = if self.on_parent() { parent } else { Some(self) };
        let (pid, process_start) = key;
        HistoryRun {
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            repo,
            kind: self.kind.clone(),
            slot: self.slot,
            slots: self.slots.clone(),
            machine: self.machine.clone(),
            remote: self.remote,
            host: self.host.clone(),
            fallback: self.fallback.clone(),
            reason: self.reason.clone(),
            pid: *pid,
            process_start: process_start.clone(),
            parent_pid: self.parent.map(|(pid, _)| pid),
            state: self.state(key, is_live),
            running: is_live(*pid, process_start),
            queued: self.queued.as_ref().map(|stamp| stamp.text.clone()),
            started: self.started.as_ref().map(|stamp| stamp.text.clone()),
            waited_seconds: self.waited_seconds,
            held_seconds: end.and_then(|end| end.held_seconds),
            status: end.and_then(|end| end.status),
            verdict: end.and_then(|end| end.verdict.clone()),
            wall_seconds: end.and_then(|end| end.wall_seconds),
            adjusted_seconds: end.and_then(|end| end.adjusted_seconds),
            abandoned: self.abandoned.clone(),
            remote_take: self.remote_take.clone(),
            cold: self.cold,
        }
    }

    /// The held time of a run that passed and took its slot at or after `cutoff`; never a run on its parent's slot
    /// ([`Self::on_parent`]), whose parent's run counts the time, nor a covered run that took its slot late
    /// ([`Self::took_late`]), whose held time covers only the stretch after its parent ended.
    fn counted_held_seconds(&self, cutoff: Timestamp) -> Option<f64> {
        let passed = self.end == Some(End::Released) && self.status == Some(0) && !self.on_parent() && !self.took_late();
        let recent = self.started.as_ref().is_some_and(|started| started.at >= cutoff);
        if passed && recent { self.held_seconds } else { None }
    }
}

/// The runs, newest first by their first line's time, then by pid and process start for runs that share it.
fn newest_first(runs: &HashMap<RunKey, Run>) -> Vec<(&RunKey, &Run)> {
    let mut ordered: Vec<(&RunKey, &Run)> = runs.iter().collect();
    ordered.sort_by(|(a_key, a_run), (b_key, b_run)| b_run.first.cmp(&a_run.first).then_with(|| b_key.cmp(a_key)));
    ordered
}

/// The keys of the runs the history lists, newest first as [`newest_first`] orders them: the [`RECENT_RUNS`] newest
/// runs that are not covered, and the [`RECENT_COVERED_RUNS`] newest covered runs newer than the oldest of them; every
/// run that is not covered when fewer are. A covered run that took a slot of its own ([`Run::took_late`]) counts as a
/// run that is not covered.
fn listed(runs: &HashMap<RunKey, Run>) -> Vec<RunKey> {
    let mut listed = Vec::new();
    let (mut counted, mut covered) = (0, 0);
    for (key, run) in newest_first(runs) {
        if counted == RECENT_RUNS {
            break;
        }
        if run.covered && !run.took_late() {
            if covered == RECENT_COVERED_RUNS {
                continue;
            }
            covered += 1;
        } else {
            counted += 1;
        }
        listed.push(key.clone());
    }
    listed
}

/// One row per repository, command and remote flag over the runs that passed and took their slot at or after `cutoff`, a
/// run's repository named by `repo_of` its folder: the most run first, then by command, then by repository (none
/// first), then local before remote.
fn summarize<'a>(runs: impl Iterator<Item = &'a Run>, cutoff: Timestamp, repo_of: &impl Fn(&str) -> Option<String>) -> Vec<CommandSummary> {
    let mut held: BTreeMap<(Option<String>, &str, bool), Vec<f64>> = BTreeMap::new();
    for run in runs {
        if let Some(seconds) = run.counted_held_seconds(cutoff) {
            held.entry((run.repo(repo_of), run.command.as_str(), run.remote))
                .or_default()
                .push(seconds);
        }
    }
    let mut summary: Vec<CommandSummary> = held
        .into_iter()
        .filter_map(|((repo, command, remote), mut seconds)| {
            seconds.sort_by(f64::total_cmp);
            Some(CommandSummary {
                command: command.to_owned(),
                repo,
                remote,
                count: seconds.len(),
                p50_seconds: nearest_rank(&seconds, 50)?,
                p90_seconds: nearest_rank(&seconds, 90)?,
                max_seconds: *seconds.last()?,
            })
        })
        .collect();
    summary.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.command.cmp(&b.command))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.remote.cmp(&b.remote))
    });
    summary
}

/// The `percent`th percentile of the sorted `values` by nearest rank: the value at `ceil(percent / 100 * n) - 1`.
pub(crate) fn nearest_rank(sorted: &[f64], percent: usize) -> Option<f64> {
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

/// The names a line lists in `abandoned` or `slots`: the strings of its array. A field that is not an array, or an
/// element that is not a string, adds nothing.
fn names(value: &Value) -> impl Iterator<Item = String> + '_ {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned)
}

/// The number at the end of the first slot name, such as 0 for `Local\gate-light-slot-0`.
fn first_slot(slots: &Value) -> Option<u32> {
    let name = slots.as_array()?.first()?.as_str()?;
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    name[name.len() - digits..].parse().ok()
}

/// The gated command with what names one tree or one scratch file taken out, so the runs of one command in different
/// worktrees count together.
///
/// The command is split as the Microsoft C runtime reads a command line's arguments: space and tab outside quotes
/// separate words, `"` opens or closes a quoted span, `""` inside a span is one literal quote and the span continues,
/// and a backslash run before a `"` is halved with the quote literal when the run is odd (any other backslash is
/// literal). Each word's rules run on its unquoted value:
///
/// 1. `--manifest-path`, `--directory`, `--dir` or `-C` followed by an absolute path is dropped with that path;
/// 2. a word starting `.tmp/` or `.tmp\` becomes `…`;
/// 3. any other absolute path with a component ending `.wt` (in any case) and one more after it (the worktree's) becomes what follows
///    that component, with `/` separators, or `.` when nothing does;
/// 4. each word is written back as `gate_args::cmdline::quote_arg_str` writes it (plain when non-empty with no
///    whitespace and no `"`; otherwise quoted, `"` as `\"`, and a backslash run before a quote or at the end doubled),
///    and the words are joined by single spaces.
///
/// An absolute path starts with a drive letter and `:\` or `:/`, or with `/`, `\\` or `~`.
#[must_use]
pub fn normalize_command(command: &str) -> String {
    let words = split_words(command);
    let mut kept: Vec<String> = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        if TREE_FLAGS.contains(&word) && words.get(index + 1).is_some_and(|next| is_absolute(next)) {
            index += 2;
            continue;
        }
        kept.push(rewrite_word(word));
        index += 1;
    }
    kept.iter().map(String::as_str).map(quote_word).collect::<Vec<_>>().join(" ")
}

/// The command's words as the Microsoft C runtime reads a command line's arguments: space and tab outside quotes
/// separate words, `"` opens or closes a quoted span, `""` inside a span is one literal quote and the span continues,
/// and a backslash run before a `"` is halved with the quote literal when the run is odd (any other backslash is
/// literal). A word that was `""` is empty and kept. The first word takes the same rule as the rest.
pub(crate) fn split_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if !quoted && matches!(character, ' ' | '\t') {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            started = true;
            read_word_char(character, &mut chars, &mut word, &mut quoted);
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Reads one character of a word, `chars` following it, into `word`, opening or closing a quoted span as the C runtime
/// does.
fn read_word_char(character: char, chars: &mut Peekable<Chars<'_>>, word: &mut String, quoted: &mut bool) {
    match character {
        '\\' => read_backslashes(chars, word),
        '"' if *quoted => {
            if chars.peek().copied() == Some('"') {
                word.push('"');
                chars.next();
            } else {
                *quoted = false;
            }
        }
        '"' => *quoted = true,
        _ => word.push(character),
    }
}

/// Reads the backslash run starting with the backslash just read, `chars` following it, into `word`: before a `"` the
/// run is halved and the quote is literal when the run is odd, and any other backslash is literal.
fn read_backslashes(chars: &mut Peekable<Chars<'_>>, word: &mut String) {
    let mut count = 1;
    while chars.peek().copied() == Some('\\') {
        chars.next();
        count += 1;
    }
    if chars.peek().copied() == Some('"') {
        word.extend(repeat_n('\\', count / 2));
        if count % 2 == 1 {
            word.push('"');
            chars.next();
        }
    } else {
        word.extend(repeat_n('\\', count));
    }
}

fn is_absolute(word: &str) -> bool {
    let bytes = word.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/');
    drive || word.starts_with('/') || word.starts_with(r"\\") || word.starts_with('~')
}

/// A scratch file elided, a worktree path shortened, or any other word as it is.
fn rewrite_word(word: &str) -> String {
    if word.starts_with(".tmp/") || word.starts_with(r".tmp\") {
        return ELIDED.to_owned();
    }
    worktree_path(word).map_or_else(|| word.to_owned(), |path| path.rest)
}

/// An absolute path through a repo's worktrees folder: a component ending `.wt` (in any case) and one more after it.
pub(crate) struct WorktreePath<'a> {
    /// The worktrees folder's name without its `.wt`.
    pub(crate) repo: &'a str,
    /// The worktree's folder name, the component after the worktrees folder.
    pub(crate) slug: &'a str,
    /// What follows the worktree's folder, with `/` separators, or `.` when nothing does.
    pub(crate) rest: String,
}

/// The worktree `word` is a path through, or `None` when it is no absolute path through a worktrees folder.
pub(crate) fn worktree_path(word: &str) -> Option<WorktreePath<'_>> {
    if !is_absolute(word) {
        return None;
    }
    let components: Vec<&str> = word.split(['/', '\\']).collect();
    let worktrees = components.iter().position(|component| is_worktrees_folder(component))?;
    let slug = *components.get(worktrees + 1)?;
    let folder = components[worktrees];
    let rest = components[worktrees + 2..].join("/");
    Some(WorktreePath {
        repo: &folder[..folder.len() - ".wt".len()],
        slug,
        rest: if rest.is_empty() { ".".to_owned() } else { rest },
    })
}

/// Whether a path component names a repo's worktrees folder: it ends `.wt`, in any case, after a name.
pub(crate) fn is_worktrees_folder(component: &str) -> bool {
    Path::new(component)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wt"))
}

/// One word written back as the C runtime's quoting writes it: plain when it is non-empty and holds no whitespace
/// (Unicode's, as .NET's `\s`) and no `"`; otherwise wrapped in quotes, `"` as `\"`, and a backslash run before a quote
/// or at the end doubled.
pub(crate) fn quote_word(word: &str) -> String {
    if is_plain_word(word) {
        return word.to_owned();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    write_escaped(word, &mut out);
    out.push('"');
    out
}

/// Whether a word is written plain: non-empty, holding no whitespace (Unicode's, as .NET's `\s`) and no `"`.
fn is_plain_word(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|character| !character.is_whitespace() && character != '"')
}

/// Writes `word` into `out` with each `"` as `\"` and a backslash run before a quote or at the end doubled.
fn write_escaped(word: &str, out: &mut String) {
    let mut backslashes = 0;
    for character in word.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(repeat_n('\\', backslashes * 2));
                out.push('\\');
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(repeat_n('\\', backslashes));
                out.push(character);
                backslashes = 0;
            }
        }
    }
    out.extend(repeat_n('\\', backslashes * 2));
}

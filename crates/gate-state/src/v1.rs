//! The formats `gate.exe` writes under a state root (design.md "Formats"): the claim a gate holding a slot writes, the
//! arrival ticket a waiting gate writes, the covered marker a gate running under a parent gate's slot writes, the slot
//! counts file people and ops-tower write, plus the paths they sit at.
//!
//! Each file is read as strict JSON with case-sensitive keys: a syntax error, a byte order mark, a duplicate key
//! anywhere, a missing or unknown `v`, a missing required key or a wrong type makes it unreadable, while an unknown key
//! is ignored. The crate's other readers read the PowerShell gate's files, which carry no `v` and are read leniently.

use std::cell::Cell;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::counts::{PoolCount, ignored_line, write_replacing, write_replacing_once};
use crate::model::{SlotCount, SlotCounts, SlotKind};
use crate::root::KernelObject;

/// The only format version this module reads and the one it writes, as `v`.
const FORMAT_VERSION: u64 = 1;

/// Why a file in one of these formats is unreadable. Its text is the `<reason>` of the gate's ignore line.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// The file could not be read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The text is not strict JSON, or starts with a byte order mark.
    #[error("not JSON: {0}")]
    Syntax(String),
    /// An object, at any depth, names this key twice.
    #[error("duplicate key \"{0}\"")]
    DuplicateKey(String),
    /// The top-level object has no `v`.
    #[error("missing key \"v\"")]
    MissingVersion,
    /// The top-level `v` is not the number 1; carries the value as JSON text.
    #[error("unknown format v {0}")]
    UnknownVersion(String),
    /// The JSON does not fit the format: not an object, a required key missing or a value of the wrong type.
    #[error("{0}")]
    Invalid(String),
}

/// The folder of the claims, `<root>\slots`.
#[must_use]
pub fn slots_dir(root: &Path) -> PathBuf {
    root.join("slots")
}

/// The claim file of `object` under `root`, `<root>\slots\<claim file name>`, or `None` for an object no gate claims.
#[must_use]
pub fn claim_path(root: &Path, object: KernelObject) -> Option<PathBuf> {
    Some(slots_dir(root).join(object.claim_file_name()?))
}

/// The folder of the arrival tickets, `<root>\waiting`.
#[must_use]
pub fn tickets_dir(root: &Path) -> PathBuf {
    root.join("waiting")
}

/// The ticket of the gate with `pid` started at `start_ms` (Unix epoch milliseconds), `<root>\waiting\<pid>-<start_ms>.json`.
#[must_use]
pub fn ticket_path(root: &Path, pid: u32, start_ms: u64) -> PathBuf {
    tickets_dir(root).join(format!("{pid}-{start_ms}.json"))
}

/// The folder of the covered markers, `<root>\covered`.
#[must_use]
pub fn covered_dir(root: &Path) -> PathBuf {
    root.join("covered")
}

/// The covered marker of the gate with `pid` started at `start_ms` (Unix epoch milliseconds),
/// `<root>\covered\<pid>-<start_ms>.json`.
#[must_use]
pub fn covered_path(root: &Path, pid: u32, start_ms: u64) -> PathBuf {
    covered_dir(root).join(format!("{pid}-{start_ms}.json"))
}

/// The slot counts file, `<root>\slot-counts.json`.
#[must_use]
pub fn counts_path(root: &Path) -> PathBuf {
    root.join("slot-counts.json")
}

/// The folder of the pause markers, `<root>\paused`.
#[must_use]
pub fn paused_dir(root: &Path) -> PathBuf {
    root.join("paused")
}

/// The name of the pause marker of the gate `pid` started at `start_ms` (Unix epoch milliseconds) waiting under the
/// parent gate `parent_pid` started at `parent_start_ms`: `<parent pid>-<parent start>-<pid>-<start>.pause`.
#[must_use]
pub fn pause_name(parent_pid: u32, parent_start_ms: u64, pid: u32, start_ms: u64) -> String {
    format!("{parent_pid}-{parent_start_ms}-{pid}-{start_ms}.pause")
}

/// The pause marker of the gate `pid` started at `start_ms` waiting under the parent gate `parent_pid` started at
/// `parent_start_ms`, `<root>\paused\<name>` ([`pause_name`]).
#[must_use]
pub fn pause_path(root: &Path, parent_pid: u32, parent_start_ms: u64, pid: u32, start_ms: u64) -> PathBuf {
    paused_dir(root).join(pause_name(parent_pid, parent_start_ms, pid, start_ms))
}

/// The folder of the remote client's state, `<root>\remote`.
#[must_use]
pub fn remote_dir(root: &Path) -> PathBuf {
    root.join("remote")
}

/// The folder of the per-worktree caches of the builder whose id is `id`: [`remote_dir`] for `host`, which has none,
/// else `<root>\remote\<id>`, so two builders' caches of one worktree never mix.
#[must_use]
pub fn builder_cache_dir(root: &Path, id: Option<&str>) -> PathBuf {
    debug_assert!(
        id.is_none_or(|id| id.len() == 8 && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))),
        "a builder's id is 8 lower-case hex digits: {id:?}"
    );
    let remote = remote_dir(root);
    match id {
        Some(id) => remote.join(id),
        None => remote,
    }
}

/// The folder of the drain markers, `<root>\remote\drain`: one folder a builder ([`drain_marker_dir`]).
#[must_use]
pub fn drain_dir(root: &Path) -> PathBuf {
    remote_dir(root).join("drain")
}

/// The folder of the drain markers of the builder whose id is `id`, `<root>\remote\drain\<id>`, or
/// `<root>\remote\drain\host` for `host`, which has none. A routed run waiting behind that builder's drain holds one
/// file of it, `<pid>-<session>`, open sharing nothing for the whole wait, so a file another open finds held is a live
/// wait and one that opens is dead.
#[must_use]
pub fn drain_marker_dir(root: &Path, id: Option<&str>) -> PathBuf {
    debug_assert!(
        id.is_none_or(|id| id.len() == 8 && id.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))),
        "a builder's id is 8 lower-case hex digits: {id:?}"
    );
    drain_dir(root).join(id.unwrap_or("host"))
}

/// The pending marker of the work folder `key`, `<root>\remote\<key>.outputs-pending`, whichever builder the run went
/// to, since it says the PC's outputs may be wrong: written before a routed run with output globs makes its run call
/// and removed once its outputs step completes, so the next routed run of that worktree, on any builder, asks for every
/// output that differs from the PC's (`OutputsFull`).
#[must_use]
pub fn outputs_pending_path(root: &Path, key: &str) -> PathBuf {
    remote_dir(root).join(format!("{key}.outputs-pending"))
}

/// The record of the work folder `key`'s last complete outputs pull, `<root>\remote\<key>.outputs-from`, whichever
/// builder that pull was made on: its whole content is the alias of the builder whose run last brought the worktree's
/// outputs back whole, `host`'s or an entry's as the routing file names it, overwritten by each such
/// run. A routed run on another builder asks for every output that differs from the PC's (`OutputsFull`), since that
/// builder's work folder holds files its own run never wrote.
#[must_use]
pub fn outputs_from_path(root: &Path, key: &str) -> PathBuf {
    remote_dir(root).join(format!("{key}.outputs-from"))
}

/// Writes the record of the work folder's last complete outputs pull at `path` ([`outputs_from_path`]) whole or not at
/// all, as [`write_nothing_back`] writes, but with one move over the record and no retry, since a routed run writes it
/// on its way: the temp file beside it is made new, a link or junction standing at its name opened as itself rather
/// than followed, then moved over any record there. `alias` is the builder's alias.
///
/// # Errors
///
/// A folder, temp file or move that fails.
pub fn write_outputs_from(path: &Path, alias: &str) -> io::Result<()> {
    write_replacing_once(path, alias)
}

/// The hash cache of the work folder `key` on the builder whose id is `id`, `<folder>\<key>.outputs-hashes` in its
/// folder ([`builder_cache_dir`]): the outputs manifest line of the worktree's last built manifest, its files whose
/// last write came well before that build, so the next build reuses the hash of each file whose size and last write
/// still match instead of reading it again.
#[must_use]
pub fn outputs_hashes_path(root: &Path, id: Option<&str>, key: &str) -> PathBuf {
    builder_cache_dir(root, id).join(format!("{key}.outputs-hashes"))
}

/// The inputs hash cache of the work folder `key` on the builder whose id is `id`, `<folder>\<key>.inputs-hashes` in
/// its folder ([`builder_cache_dir`]): the inputs manifest line of the worktree's last built inputs manifest, its
/// files whose last write came well before that build, kept apart from the outputs cache, whose saves drop every file
/// the outputs walk no longer finds.
#[must_use]
pub fn inputs_hashes_path(root: &Path, id: Option<&str>, key: &str) -> PathBuf {
    builder_cache_dir(root, id).join(format!("{key}.inputs-hashes"))
}

/// The carried marker of the work folder `key` on the builder whose id is `id`, `<folder>\<key>.inputs-carried` in its
/// folder ([`builder_cache_dir`]), beside the inputs hash cache: an empty file, there once an inputs step that carried a
/// path a command named landed, until a run with no inputs has removed the copies it put.
#[must_use]
pub fn inputs_carried_path(root: &Path, id: Option<&str>, key: &str) -> PathBuf {
    builder_cache_dir(root, id).join(format!("{key}.inputs-carried"))
}

/// The sent-build hash cache of the work folder `key` on the builder whose id is `id`, `<folder>\<key>.sent-build-hashes`
/// in its folder ([`builder_cache_dir`]): the manifest line of the PC's build of one project last sent to it for a
/// routed `dotnet test --no-build`, its files whose last write came well before that build, kept apart from the inputs
/// cache, whose saves drop every file the inputs walk no longer finds.
#[must_use]
pub fn sent_build_hashes_path(root: &Path, id: Option<&str>, key: &str) -> PathBuf {
    builder_cache_dir(root, id).join(format!("{key}.sent-build-hashes"))
}

/// The nothing-back record of the work folder `key`, `<root>\remote\<key>.nothing-back`: the JSON object
/// `{"v": 1, "host": "<builder>", "configuration": …, "framework": …, "runtime": …, "built": […]}`
/// ([`NothingBackRecord`]). It is written when a routed dotnet build of that worktree ends without its outputs known to
/// be brought back, and when a routed dotnet `--no-build` run whose word the builder dropped ends while no record stands.
/// It is removed by a passing dotnet build of the whole worktree here, and under the same rule by a routed one that
/// brought all its outputs back, so a dotnet `--no-build` run here meanwhile builds, unless a passing build of its
/// project here added that project to `built`.
#[must_use]
pub fn nothing_back_path(root: &Path, key: &str) -> PathBuf {
    remote_dir(root).join(format!("{key}.nothing-back"))
}

/// The body of a nothing-back record ([`nothing_back_path`]) after its `v`: the builder the routed build ran on, the
/// configuration, framework and runtime its words set, each left out when they set none, and the project folders built
/// here since it was written, left out when there are none and read as none when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NothingBackRecord {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub built: Vec<String>,
}

/// The text [`write_nothing_back`] writes for `record`.
///
/// # Errors
///
/// A record that cannot be serialized.
pub fn nothing_back_text(record: &NothingBackRecord) -> io::Result<String> {
    versioned_text(record)
}

/// Writes the nothing-back `record` at `path` whole or not at all, as [`write_replacing`] writes: the temp file beside
/// it is made new, a link or junction standing at its name opened as itself rather than followed, then moved over any
/// record there.
///
/// # Errors
///
/// A folder, temp file or move that fails.
pub fn write_nothing_back(path: &Path, record: &NothingBackRecord) -> io::Result<()> {
    write_versioned(path, record)
}

/// The nothing-back record a file's bytes hold, read strictly as every file of this module is; a record without the
/// settings, written before they were, reads with none.
///
/// # Errors
///
/// Bytes that are not UTF-8 or not the record's format.
pub fn nothing_back_record(bytes: &[u8]) -> Result<NothingBackRecord, FormatError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))?;
    parse_strict(text)
}

/// The outputs cap record of the work folder `key` on the builder whose id is `id`, `<folder>\<key>.outputs-capped` in
/// its folder ([`builder_cache_dir`]): the JSON object `{"v": 1, "hits": <n>}` ([`OutputsCappedRecord`]), the number
/// of that worktree's routed runs in a row whose outputs fetch passed the fetch cap; written at each such fetch and
/// removed when a fetch ends any other way.
#[must_use]
pub fn outputs_capped_path(root: &Path, id: Option<&str>, key: &str) -> PathBuf {
    builder_cache_dir(root, id).join(format!("{key}.outputs-capped"))
}

/// The body of an outputs cap record ([`outputs_capped_path`]) after its `v`: the fetches in a row that passed the cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputsCappedRecord {
    pub hits: u64,
}

/// Writes the outputs cap `record` at `path` whole or not at all, as [`write_nothing_back`] writes.
///
/// # Errors
///
/// A folder, temp file or move that fails.
pub fn write_outputs_capped(path: &Path, record: &OutputsCappedRecord) -> io::Result<()> {
    write_versioned(path, record)
}

/// The outputs cap record a file's bytes hold, read strictly as every file of this module is.
///
/// # Errors
///
/// Bytes that are not UTF-8 or not the record's format.
pub fn outputs_capped_record(bytes: &[u8]) -> Result<OutputsCappedRecord, FormatError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))?;
    parse_strict(text)
}

/// The worktree record of the work folder `key`, `<root>\remote\<key>.worktree` ([`WorktreeRecord`]): which worktree
/// the key, a one-way hash of its root, was made from, written by a routed run or `gate warm` as [`needs_write`] says.
#[must_use]
pub fn worktree_record_path(root: &Path, key: &str) -> PathBuf {
    remote_dir(root).join(format!("{key}.worktree"))
}

/// How long a worktree record's `lastRouted` stands before a routed run or `gate warm` of the same worktree writes it
/// again, moving `lastRouted`.
const WORKTREE_RECORD_FRESH: SignedDuration = SignedDuration::from_hours(1);

/// The worktree record of a work folder ([`worktree_record_path`]): the JSON object `{"v": 1, "root": …, "commonDir": …,
/// "repo": …, "lastRouted": …}`, with `"goneSince": …` once the worktree was found gone ([`mark_gone`]). A field it does
/// not know is ignored when read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    /// The worktree root as routing spells it: the text the key hashes.
    pub root: String,
    /// The absolute path of the worktree's git common folder.
    pub common_dir: String,
    /// The repo name the routing file listed.
    pub repo: String,
    /// When a routed run or `gate warm` last wrote the record, written as a claim's times are.
    #[serde(with = "iso_time")]
    pub last_routed: Timestamp,
    /// When the worktree was first found gone, written as `lastRouted` is; `None`, and no key, while it is live.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_iso_time")]
    pub gone_since: Option<Timestamp>,
}

/// Whether a routed run or `gate warm`, whose record of a live worktree would be `fresh`, writes it over `existing`,
/// the record on disk, `None` when there is none or it cannot be read: when there is none, when it says the worktree
/// is gone (`goneSince`), when its root, common folder or repo differ from `fresh`'s, or when its `lastRouted` is an
/// hour or more before `fresh`'s, or after it (a clock that went back).
#[must_use]
pub fn needs_write(existing: Option<&WorktreeRecord>, fresh: &WorktreeRecord) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    if existing.gone_since.is_some() {
        return true;
    }
    if existing.root != fresh.root || existing.common_dir != fresh.common_dir || existing.repo != fresh.repo {
        return true;
    }
    let age = fresh.last_routed.duration_since(existing.last_routed);
    age.is_negative() || age >= WORKTREE_RECORD_FRESH
}

/// `record` marked as the record of a worktree found gone at `now`: its `goneSince` set to `now` when it had none, and
/// kept as it was when it had one.
#[must_use]
pub fn mark_gone(record: WorktreeRecord, now: Timestamp) -> WorktreeRecord {
    WorktreeRecord {
        gone_since: record.gone_since.or(Some(now)),
        ..record
    }
}

/// Writes the worktree `record` at `path` whole or not at all, as [`write_nothing_back`] writes, but with one move over
/// the record and no retry, since a routed run writes it on its way to the builder.
///
/// # Errors
///
/// A folder, temp file or move that fails.
pub fn write_worktree_record(path: &Path, record: &WorktreeRecord) -> io::Result<()> {
    write_replacing_once(path, &versioned_text(record)?)
}

/// The worktree record a file's bytes hold, read strictly as every file of this module is; fields it does not know
/// are ignored.
///
/// # Errors
///
/// Bytes that are not UTF-8 or not the record's format, its `v` missing or not 1 among them.
pub fn worktree_record(bytes: &[u8]) -> Result<WorktreeRecord, FormatError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))?;
    parse_strict(text)
}

/// The folder of the warm-up files, `<root>\remote\warm`.
#[must_use]
pub fn warm_dir(root: &Path) -> PathBuf {
    remote_dir(root).join("warm")
}

/// The warm-up record of the work folder `key`, `<root>\remote\warm\<key>.json` ([`WarmRecord`]).
#[must_use]
pub fn warm_record_path(root: &Path, key: &str) -> PathBuf {
    warm_dir(root).join(format!("{key}.json"))
}

/// The log of the warm-up of the work folder `key`, `<root>\remote\warm\<key>.log`.
#[must_use]
pub fn warm_log_path(root: &Path, key: &str) -> PathBuf {
    warm_dir(root).join(format!("{key}.log"))
}

/// The lock the warm-up of the work folder `key` holds for its whole life, `<root>\remote\warm\<key>.lock`.
#[must_use]
pub fn warm_lock_path(root: &Path, key: &str) -> PathBuf {
    warm_dir(root).join(format!("{key}.lock"))
}

/// The lock that lets one warm-up at a time run on this PC, `<root>\remote\warm.lock`.
#[must_use]
pub fn warm_cap_lock_path(root: &Path) -> PathBuf {
    remote_dir(root).join("warm.lock")
}

/// The stamp of the last background prune sweep a routed run started, `<root>\remote\prune-sweep.stamp`
/// ([`PruneSweepStamp`]).
#[must_use]
pub fn prune_sweep_stamp_path(root: &Path) -> PathBuf {
    remote_dir(root).join("prune-sweep.stamp")
}

/// The output of the last background prune sweep, `<root>\remote\prune-sweep.log`, replaced at each start.
#[must_use]
pub fn prune_sweep_log_path(root: &Path) -> PathBuf {
    remote_dir(root).join("prune-sweep.log")
}

/// The stamp of the last background prune sweep ([`prune_sweep_stamp_path`]): the JSON object `{"v": 1, "at": …}`,
/// `at` when the routed run that started it did so, written as a claim's times are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneSweepStamp {
    /// When the sweep was started.
    #[serde(with = "iso_time")]
    pub at: Timestamp,
}

/// Writes the sweep `stamp` at `path` whole or not at all, with one move over it and no retry, as
/// [`write_worktree_record`] writes.
///
/// # Errors
///
/// A folder, temp file or move that fails.
pub fn write_prune_sweep_stamp(path: &Path, stamp: &PruneSweepStamp) -> io::Result<()> {
    write_replacing_once(path, &versioned_text(stamp)?)
}

/// The sweep stamp a file's bytes hold, read strictly as every file of this module is; fields it does not know are
/// ignored.
///
/// # Errors
///
/// Bytes that are not UTF-8 or not the stamp's format, its `v` missing or not 1 among them.
pub fn prune_sweep_stamp(bytes: &[u8]) -> Result<PruneSweepStamp, FormatError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))?;
    parse_strict(text)
}

/// Where a work folder's warm-up stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarmState {
    /// A warm-up is running, or was when the record was written.
    Warming,
    /// The warm-up's commands passed on the builder.
    Warm,
    /// The warm-up failed.
    Failed,
}

/// How a warm-up failed, which sets how long until the next one may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarmFailure {
    /// The commands failed or ran out of time.
    Commands,
    /// The builder or the client refused the warm-up before any command ran.
    Refused,
}

/// The warm-up record of a work folder ([`warm_record_path`]): the JSON object `{"v": 1, "host": …, "state": …, "at": …,
/// "hash": …, "gate": …, "pid": …, "started": …}`, with `failure` and `reason` after them when, and only when, `state` is
/// `failed`. Its times are written as a claim's are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmRecord {
    /// The builder the warm-up ran on.
    pub host: String,
    pub state: WarmState,
    /// When the record was written.
    #[serde(with = "iso_time")]
    pub at: Timestamp,
    /// The lower-case hex SHA-256 of the `.gate/warmup.toml` bytes the warm-up ran.
    pub hash: String,
    /// The build id of the PC gate that wrote the record.
    pub gate: String,
    /// The warming process's pid, for the lines only.
    pub pid: u32,
    /// The warming process's start, for the lines only.
    #[serde(with = "iso_time")]
    pub started: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<WarmFailure>,
    /// Why the warm-up failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WarmRecord {
    /// Why the record breaks a rule its types cannot carry: an empty `host` or `gate`, a `hash` that is not 64 lower-case
    /// hex digits, a `failure` or `reason` present without `state` `failed` or absent with it, or a failed record whose
    /// `reason` is empty.
    fn problem(&self) -> Option<String> {
        if self.host.is_empty() {
            return Some("host is empty".to_owned());
        }
        if self.gate.is_empty() {
            return Some("gate is empty".to_owned());
        }
        let hex = self.hash.len() == 64 && self.hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex {
            return Some("hash is not 64 lower-case hex digits".to_owned());
        }
        let failed = self.state == WarmState::Failed;
        if failed && self.reason.as_deref() == Some("") {
            return Some("state failed with an empty reason".to_owned());
        }
        match (failed, self.failure.is_some(), self.reason.is_some()) {
            (true, false, _) => Some("state failed without failure".to_owned()),
            (true, _, false) => Some("state failed without reason".to_owned()),
            (false, true, _) => Some("failure without state failed".to_owned()),
            (false, _, true) => Some("reason without state failed".to_owned()),
            _ => None,
        }
    }
}

/// Writes the warm-up `record` at `path` whole or not at all, as [`write_nothing_back`] writes.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] for a record [`warm_record`] would refuse; else a folder, temp file or move that
/// fails.
pub fn write_warm_record(path: &Path, record: &WarmRecord) -> io::Result<()> {
    if let Some(problem) = record.problem() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, problem));
    }
    write_versioned(path, record)
}

/// The warm-up record a file's bytes hold, read strictly as every file of this module is, and refused when it breaks a
/// rule of its shape (see [`WarmRecord`]).
///
/// # Errors
///
/// Bytes that are not UTF-8 or not the record's format.
pub fn warm_record(bytes: &[u8]) -> Result<WarmRecord, FormatError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))?;
    let record: WarmRecord = parse_strict(text)?;
    match record.problem() {
        Some(problem) => Err(FormatError::Invalid(problem)),
        None => Ok(record),
    }
}

/// A pause marker's name read: the gate waiting under a parent's slot, and the parent gate it runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PauseMarker {
    /// The parent gate, the one whose slot the marker's gate runs under.
    pub parent_pid: u32,
    /// The parent gate's start, Unix epoch milliseconds.
    pub parent_start_ms: u64,
    /// The gate waiting, which keeps the marker.
    pub pid: u32,
    /// The waiting gate's start, Unix epoch milliseconds.
    pub start_ms: u64,
}

/// `name` read as a pause marker's name ([`pause_name`]): four whole numbers without a sign, the pids within `u32`;
/// `None` for a name of any other shape.
#[must_use]
pub fn parse_pause_name(name: &str) -> Option<PauseMarker> {
    let mut parts = name.strip_suffix(".pause")?.split('-');
    let parent_pid = parts.next().and_then(whole)?;
    let parent_start_ms = parts.next().and_then(whole)?;
    let pid = parts.next().and_then(whole)?;
    let start_ms = parts.next().and_then(whole)?;
    let marker = PauseMarker {
        parent_pid: u32::try_from(parent_pid).ok()?,
        parent_start_ms,
        pid: u32::try_from(pid).ok()?,
        start_ms,
    };
    parts.next().is_none().then_some(marker)
}

/// `text` as a whole number of at least one ASCII digit and no sign.
fn whole(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// The kind of slot a claim holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimKind {
    Heavy,
    Light,
    Critical,
    Remote,
}

impl ClaimKind {
    /// The kind as a claim writes it: `heavy`, `light`, `critical` or `remote`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Heavy => "heavy",
            ClaimKind::Light => "light",
            ClaimKind::Critical => "critical",
            ClaimKind::Remote => "remote",
        }
    }
}

/// The claim a gate holding a slot writes at `slots\<name>.json`. Its times are written to the 100 ns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub kind: ClaimKind,
    /// The slot index; 0 for the critical claim.
    pub index: u32,
    /// The gate's pid.
    pub pid: u32,
    /// The gate's process start.
    #[serde(with = "iso_time")]
    pub process_start: Timestamp,
    /// When the gate took the slot.
    #[serde(with = "iso_time")]
    pub started: Timestamp,
    /// The gate's working directory.
    pub cwd: String,
    /// The command's words, each quoted as for `CreateProcess`, joined by spaces.
    pub command: String,
    /// The log's full path.
    pub log: String,
    /// The ceiling, in seconds.
    pub timeout_seconds: f64,
    /// A remote slot's claim only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<bool>,
    /// The ssh destination of a remote slot's claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The remote work folder's key of a remote slot's claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The free text the run gave `--reason`, for a local, critical or alone run or any run that said one; absent when
    /// it said none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Reserved, read and written through untouched. Any value but an object or `null` makes the claim unreadable;
    /// `null` reads as absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Map<String, Value>>,
}

/// The kind of slot a ticket waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketKind {
    Heavy,
    Light,
    Critical,
}

impl TicketKind {
    /// The kind as a ticket writes it: `heavy`, `light` or `critical`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TicketKind::Heavy => "heavy",
            TicketKind::Light => "light",
            TicketKind::Critical => "critical",
        }
    }
}

/// The arrival ticket a waiting gate writes at `waiting\<pid>-<start>.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub kind: TicketKind,
    /// The gate's first arrival, Unix epoch milliseconds.
    pub arrived: u64,
    /// The gate's pid.
    pub pid: u32,
    /// The gate's process start, Unix epoch milliseconds.
    #[serde(rename = "startTicks")]
    pub start_ms: u64,
    /// True only for a critical gate that holds the machine.
    pub holds: bool,
    /// True only for a critical gate that holds every slot it takes and waits for the gates lent a light parent's slot
    /// to end: no light parent lends its slot while it says so. Written only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lends_closed: bool,
    /// True only for a critical gate asked for `--alone`: while it waits or holds, no light gate runs beside it, and it
    /// lends no slot. Written only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alone: bool,
    /// True while queued for the remote pool. Written only when `Some(true)`, so a local queue's ticket is `None`.
    #[serde(skip_serializing_if = "is_not_true")]
    pub remote: Option<bool>,
    /// The id of the `builders` entry's own remote pool a remote ticket queues for ([`crate::PoolId`]); absent for
    /// `host`'s pool and for a local ticket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// True only for a run pinned to this builder with `--builder`, which no pick chose. Written only when true, so a
    /// ticket without it is not pinned.
    #[serde(default, skip_serializing_if = "is_not_true")]
    pub pinned: Option<bool>,
    /// The free text the run gave `--reason`, for a local, critical or alone run or any run that said one; absent when
    /// it said none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Reserved, read and written through untouched. Any value but an object or `null` makes the ticket unreadable;
    /// `null` reads as absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Map<String, Value>>,
}

impl Ticket {
    /// The ticket's place among waiters of its kind: by arrival, ties broken by pid.
    #[must_use]
    pub fn queue_order(&self) -> (u64, u32) {
        (self.arrived, self.pid)
    }

    /// Whether the ticket is a critical request that holds the machine alone, light slots included.
    #[must_use]
    pub fn holds_alone(&self) -> bool {
        self.kind == TicketKind::Critical && self.alone
    }
}

/// The covered marker a gate running under a parent gate's slot keeps at `covered\<pid>-<start>.json` while its command
/// runs. Its machine times, `processStart` and `since`, are Unix epoch milliseconds, as a ticket's are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoveredMarker {
    /// The covered gate's pid.
    pub pid: u32,
    /// The covered gate's process start, Unix epoch milliseconds.
    pub process_start: u64,
    /// The pid of the parent gate whose slot covers it.
    pub parent_pid: u32,
    /// The command's words, each quoted as for `CreateProcess`, joined by spaces, as a claim's `command`.
    pub command: String,
    /// The covered gate's working directory.
    pub cwd: String,
    /// When the covered gate wrote the marker, Unix epoch milliseconds.
    pub since: u64,
    /// The covered gate's own slot kind, `heavy`, `light`, `critical` or `none`: the kind of slot it takes for the rest
    /// of its run should its parent gate end first, which a gate its command starts may run under then. `None` in a
    /// marker written without it, whose kind is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Writes `marker` at `path` through `<path>.<pid>.<n>.tmp` moved over it, making a missing folder.
///
/// # Errors
///
/// The io error when the folder or the file beside the target cannot be written, or the move fails every try.
pub fn write_covered(path: &Path, marker: &CoveredMarker) -> io::Result<()> {
    write_versioned(path, marker)
}

/// Reads every covered marker in `dir`, sorted by file name, each with its path; a missing folder holds none. Only a
/// `*.json` file named `<pid>-<start>.json` after the pid and `processStart` it holds counts: a file that cannot be read,
/// is unreadable as a v1 marker or is named after another gate is skipped, as is a temporary `*.tmp` file. Whether a
/// marker's gate is still alive, and deleting the marker of one that is not, is the caller's job.
///
/// # Errors
///
/// The io error when the folder exists but cannot be listed.
pub fn read_covered(dir: &Path) -> io::Result<Vec<(PathBuf, CoveredMarker)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let named = path.file_name().and_then(|name| name.to_str()).and_then(parse_covered_name)?;
            let marker: CoveredMarker = read_strict(&path).ok()?;
            (named == (marker.pid, marker.process_start)).then_some((path, marker))
        })
        .collect())
}

/// `name` read as a covered marker's name, `<pid>-<start>.json`: the pid within `u32` and the start, both whole numbers
/// without a sign; `None` for a name of any other shape.
fn parse_covered_name(name: &str) -> Option<(u32, u64)> {
    let (pid, start_ms) = name.strip_suffix(".json")?.split_once('-')?;
    Some((u32::try_from(whole(pid)?).ok()?, whole(start_ms)?))
}

/// Reads the claim at `path`.
///
/// # Errors
///
/// [`FormatError`] when the file cannot be read or is unreadable as a v1 claim.
pub fn read_claim(path: &Path) -> Result<Claim, FormatError> {
    read_strict(path)
}

/// Writes `claim` at `path` through `<path>.<pid>.<n>.tmp` moved over it, making a missing folder.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] when `timeout_seconds` is not finite, which JSON cannot carry; else the io error
/// when the folder or the file beside the target cannot be written, or the move fails every try.
pub fn write_claim(path: &Path, claim: &Claim) -> io::Result<()> {
    if !claim.timeout_seconds.is_finite() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "timeoutSeconds must be finite"));
    }
    write_versioned(path, claim)
}

/// Reads the ticket at `path`.
///
/// # Errors
///
/// [`FormatError`] when the file cannot be read or is unreadable as a v1 ticket.
pub fn read_ticket(path: &Path) -> Result<Ticket, FormatError> {
    read_strict(path)
}

/// Writes `ticket` at `path` through `<path>.<pid>.<n>.tmp` moved over it, making a missing folder.
///
/// # Errors
///
/// The io error when the folder or the file beside the target cannot be written, or the move fails every try.
pub fn write_ticket(path: &Path, ticket: &Ticket) -> io::Result<()> {
    write_versioned(path, ticket)
}

/// Reads every `*.json` ticket in `dir`, sorted by file name, each with its path and what reading it gave; a missing
/// folder holds none. A temporary `*.tmp` file is skipped, as is an entry the listing could not read or tell the type
/// of. Whether a ticket's gate is still alive, and deleting the ticket of one that is not, is the gate's job, not this
/// reader's.
///
/// # Errors
///
/// The io error when the folder exists but cannot be listed.
pub fn read_tickets(dir: &Path) -> io::Result<Vec<(PathBuf, Result<Ticket, FormatError>)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_file = entry.file_type().is_ok_and(|file_type| file_type.is_file());
        if is_file && path.extension().is_some_and(|extension| extension == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| read_listed_ticket(&path).map(|ticket| (path, ticket)))
        .collect())
}

/// A ticket [`read_tickets`] listed, read; `None` when it is gone by then, since a gate that took its slot deletes it.
fn read_listed_ticket(path: &Path) -> Option<Result<Ticket, FormatError>> {
    match read_ticket(path) {
        Err(FormatError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
        read => Some(read),
    }
}

/// One pool's count as the counts file sets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSetting {
    /// The count the gate uses: the file's value, else the default.
    pub count: u32,
    /// The whole line the gate says when it ignored the file or the pool's value:
    /// `gate: ignored <path>: <reason>; using <n> <kind> slots`.
    pub ignored: Option<String>,
}

/// The heavy and light counts the counts file sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counts {
    pub heavy: PoolSetting,
    pub light: PoolSetting,
}

/// Reads the counts file at `path` on a machine with `processors` logical processors. A missing file or key gives the
/// default silently. An unreadable file gives both pools their default, each with its own ignore line; a value that is
/// not a whole number from 1 to 64 gives that pool alone its default and a line.
#[must_use]
pub fn read_counts(path: &Path, processors: usize) -> Counts {
    let [heavy, light] = read_pool_counts(path, processors);
    Counts {
        heavy: pool_setting(heavy),
        light: pool_setting(light),
    }
}

/// The counts file at `path` read as [`read_counts`] reads it, heavy then light, keeping whether the file set each value
/// (the Gates tab shows a set value even when it equals the default).
pub(crate) fn read_pool_counts(path: &Path, processors: usize) -> [PoolCount; 2] {
    pool_counts_of(&read_counts_file(path), path, processors)
}

/// A counts file read strictly: `Ok(None)` when it does not exist, `Err` when it cannot be read or is not a v1 counts
/// file.
pub(crate) type CountsFile = Result<Option<Map<String, Value>>, FormatError>;

/// The counts file at `path`, read strictly.
pub(crate) fn read_counts_file(path: &Path) -> CountsFile {
    match read_text(path) {
        Err(FormatError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(text) => parse_strict::<Map<String, Value>>(&text).map(Some),
    }
}

/// Each pool's counts, heavy then light, in the counts `file` read from `path`.
pub(crate) fn pool_counts_of(file: &CountsFile, path: &Path, processors: usize) -> [PoolCount; 2] {
    SlotKind::ALL.map(|kind| pool_count(file, path, kind, processors))
}

/// The count a pool's read gives the gate: the file's value, else the default.
fn pool_setting(pool: PoolCount) -> PoolSetting {
    PoolSetting {
        count: pool.set.map_or(pool.default, SlotCount::get),
        ignored: pool.problem,
    }
}

fn pool_count(file: &CountsFile, path: &Path, kind: SlotKind, processors: usize) -> PoolCount {
    let default = kind.default_count(processors);
    let ignored = |reason: &dyn fmt::Display| PoolCount {
        default,
        set: None,
        problem: Some(ignored_line(path, reason, default, kind)),
    };
    let value = match file {
        Ok(Some(counts)) => counts.get(kind.as_str()),
        Ok(None) => None,
        Err(error) => return ignored(error),
    };
    let Some(value) = value else {
        return PoolCount {
            default,
            set: None,
            problem: None,
        };
    };
    let count = value.as_u64().and_then(|count| u32::try_from(count).ok()).and_then(SlotCount::new);
    match count {
        Some(count) => PoolCount {
            default,
            set: Some(count),
            problem: None,
        },
        None => ignored(&format!(
            "\"{}\" must be a whole number from {} to {}",
            kind.as_str(),
            SlotCount::MIN,
            SlotCount::MAX
        )),
    }
}

fn read_strict<T: DeserializeOwned>(path: &Path) -> Result<T, FormatError> {
    parse_strict(&read_text(path)?)
}

/// The file at `path` as text; bytes that are not UTF-8 are a syntax error, not an io error.
fn read_text(path: &Path) -> Result<String, FormatError> {
    String::from_utf8(fs::read(path)?).map_err(|_| FormatError::Syntax("not UTF-8".to_owned()))
}

/// A format's body written after its `v`, which therefore leads the file.
#[derive(Serialize)]
struct Versioned<'a, T> {
    v: u64,
    #[serde(flatten)]
    body: &'a T,
}

fn write_versioned<T: Serialize>(path: &Path, body: &T) -> io::Result<()> {
    write_replacing(path, &versioned_text(body)?)
}

/// The text of a format's file: compact JSON, `v` first, then `body`.
fn versioned_text<T: Serialize>(body: &T) -> io::Result<String> {
    serde_json::to_string(&Versioned { v: FORMAT_VERSION, body }).map_err(io::Error::other)
}

/// The counts file's text as `gate.exe` reads it: compact JSON, `v` first, then the set counts heavy then light, such as
/// `{"v":1,"heavy":4}`.
pub(crate) fn counts_text(counts: SlotCounts) -> serde_json::Result<String> {
    serde_json::to_string(&Versioned {
        v: FORMAT_VERSION,
        body: &counts,
    })
}

/// Parses `text` strictly: JSON with no duplicate key at any depth, an object whose `v` is the number 1, then the
/// format's type.
fn parse_strict<T: DeserializeOwned>(text: &str) -> Result<T, FormatError> {
    let value = parse_value(text)?;
    check_version(&value)?;
    serde_json::from_value(value).map_err(|error| FormatError::Invalid(error.to_string()))
}

fn check_version(value: &Value) -> Result<(), FormatError> {
    let Value::Object(object) = value else {
        return Err(FormatError::Invalid("not a JSON object".to_owned()));
    };
    match object.get("v") {
        None => Err(FormatError::MissingVersion),
        Some(version) if version.as_u64() == Some(FORMAT_VERSION) => Ok(()),
        Some(version) => Err(FormatError::UnknownVersion(version.to_string())),
    }
}

/// The whole of `text` as one JSON value, failing at the first object that names a key twice.
fn parse_value(text: &str) -> Result<Value, FormatError> {
    let duplicate = Cell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed = StrictValue { duplicate: &duplicate }
        .deserialize(&mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value));
    parsed.map_err(|error| match duplicate.take() {
        Some(key) => FormatError::DuplicateKey(key),
        None => FormatError::Syntax(error.to_string()),
    })
}

/// A JSON value built as it is read, refusing an object that names a key twice; the first such key is left in
/// `duplicate`.
#[derive(Clone, Copy)]
struct StrictValue<'a> {
    duplicate: &'a Cell<Option<String>>,
}

impl<'de> DeserializeSeed<'de> for StrictValue<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictValue<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Value, A::Error> {
        let mut array = Vec::new();
        while let Some(item) = items.next_element_seed(self)? {
            array.push(item);
        }
        Ok(Value::Array(array))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            if object.contains_key(&key) {
                self.duplicate.set(Some(key));
                return Err(de::Error::custom("duplicate key"));
            }
            let value = entries.next_value_seed(self)?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[expect(
    clippy::ref_option,
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_not_true(value: &Option<bool>) -> bool {
    *value != Some(true)
}

/// A time as an ISO 8601 UTC string: written with seven fractional digits and `Z`, read from any RFC 3339 time.
mod iso_time {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer, de};

    use crate::state::iso_utc;

    pub(super) fn serialize<S: Serializer>(time: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&iso_utc(*time))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// A time that may be absent, written as [`iso_time`] writes one; read as absent when it is `null`, not text, or text
/// that is no time, so the record that holds it still reads.
mod optional_iso_time {
    use jiff::Timestamp;
    use serde::de::IgnoredAny;
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::state::iso_utc;

    #[expect(clippy::ref_option, reason = "serde's serialize_with passes the field by reference")]
    pub(super) fn serialize<S: Serializer>(time: &Option<Timestamp>, serializer: S) -> Result<S::Ok, S::Error> {
        match time {
            Some(time) => serializer.serialize_str(&iso_utc(*time)),
            None => serializer.serialize_none(),
        }
    }

    /// The value at the key: text, or anything else, which is not looked into.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Text(String),
        Other(IgnoredAny),
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Timestamp>, D::Error> {
        Ok(match Loose::deserialize(deserializer)? {
            Loose::Text(text) => text.parse().ok(),
            Loose::Other(_) => None,
        })
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose record cannot be made must fail loudly")]
mod tests {
    use std::path::Path;

    use jiff::{SignedDuration, Timestamp};

    use super::{WorktreeRecord, mark_gone, needs_write, read_listed_ticket, versioned_text, worktree_record};

    #[test]
    fn a_ticket_deleted_between_list_and_read_is_dropped() {
        let gone = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-ticket").join("1-1.json");

        assert!(read_listed_ticket(&gone).is_none(), "a ticket gone after the listing is not reported");
    }

    /// The time `text` names.
    fn at(text: &str) -> Timestamp {
        text.parse().expect("a time")
    }

    /// A worktree record of the worktree `X:/dev/repo.wt/a`, last routed at noon, gone since `gone_since`.
    fn record(gone_since: Option<Timestamp>) -> WorktreeRecord {
        WorktreeRecord {
            root: "X:/dev/repo.wt/a".to_owned(),
            common_dir: r"X:\dev\repo\.git".to_owned(),
            repo: "repo".to_owned(),
            last_routed: at("2026-10-21T12:00:00Z"),
            gone_since,
        }
    }

    #[test]
    fn a_record_with_gone_since_round_trips_and_one_without_writes_no_key() {
        let gone = record(Some(at("2026-10-21T13:00:00Z")));
        let text = versioned_text(&gone).expect("the record has a text");
        assert!(text.contains(r#""goneSince":"2026-10-21T13:00:00.0000000Z""#), "{text}");
        assert_eq!(worktree_record(text.as_bytes()).expect("the record reads"), gone);

        let live = versioned_text(&record(None)).expect("the record has a text");
        assert!(!live.contains("goneSince"), "{live}");
        assert_eq!(worktree_record(live.as_bytes()).expect("the record reads"), record(None));
    }

    /// The record [`record`] makes, written with `gone_since` as the raw JSON value of its `goneSince` key.
    fn with_gone_since(gone_since: &str) -> String {
        let live = versioned_text(&record(None)).expect("the record has a text");
        let body = live.strip_suffix('}').expect("the record is an object");
        format!(r#"{body},"goneSince":{gone_since}}}"#)
    }

    #[test]
    fn a_gone_since_of_null_reads_as_none() {
        let text = with_gone_since("null");

        assert_eq!(worktree_record(text.as_bytes()).expect("the record reads"), record(None), "{text}");
    }

    #[test]
    fn a_gone_since_it_cannot_parse_reads_as_none_and_the_record_reads() {
        for gone_since in ["1761051600", r#""noon""#, r#"{"at":"2026-10-21T13:00:00Z"}"#] {
            let text = with_gone_since(gone_since);

            assert_eq!(worktree_record(text.as_bytes()).expect("the record reads"), record(None), "{text}");
        }
    }

    #[test]
    fn a_live_write_clears_gone_since_even_inside_the_fresh_hour() {
        let existing = record(Some(at("2026-10-21T12:05:00Z")));
        let fresh = WorktreeRecord {
            last_routed: at("2026-10-21T12:10:00Z"),
            ..record(None)
        };

        assert!(needs_write(Some(&existing), &fresh), "a gone record is rewritten without goneSince");
        assert!(!needs_write(Some(&record(None)), &fresh), "a live record inside the hour stands");
    }

    #[test]
    fn mark_gone_keeps_the_first_gone_since() {
        let first = at("2026-10-21T13:00:00Z");
        let later = first.checked_add(SignedDuration::from_hours(2)).expect("a time");

        assert_eq!(mark_gone(record(None), first), record(Some(first)), "a live record is marked now");
        assert_eq!(
            mark_gone(record(Some(first)), later),
            record(Some(first)),
            "a gone record keeps its first time"
        );
    }
}

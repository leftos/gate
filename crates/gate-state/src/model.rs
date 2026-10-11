//! The dashboard's state as the page reads it, the slot kinds and counts, and the seams a caller supplies.

use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;

use crate::counts::{format_count_value, powershell_text};

/// The most light slots a critical gate keeps free beside it.
pub const CRITICAL_LIGHT_RESERVE: u32 = 2;

/// The light slots a critical gate keeps free beside it, out of a light pool of `light_count` slots.
#[must_use]
pub fn reserved_light(light_count: u32) -> u32 {
    light_count.min(CRITICAL_LIGHT_RESERVE)
}

/// The two slot pools, in the order the dashboard lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    Heavy,
    Light,
}

impl SlotKind {
    /// Every kind, heavy first, the order of the pools and of the counts file's keys.
    pub const ALL: [SlotKind; 2] = [SlotKind::Heavy, SlotKind::Light];

    /// The kind's name in the counts file, a request body and the page: `heavy` or `light`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SlotKind::Heavy => "heavy",
            SlotKind::Light => "light",
        }
    }

    /// The prefix of the kind's slot names, which are its mutex names and its claim files' base names.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            SlotKind::Heavy => "gate-slot-",
            SlotKind::Light => "gate-light-slot-",
        }
    }

    /// The threads one gate of the kind is taken to keep busy, which sets the kind's default count.
    #[must_use]
    pub fn threads(self) -> usize {
        match self {
            SlotKind::Heavy => 4,
            SlotKind::Light => 2,
        }
    }

    /// The default count of the kind on a machine with `processors` logical processors: one slot per
    /// [`threads`](SlotKind::threads) of all but one processor, and at least one.
    #[must_use]
    pub fn default_count(self, processors: usize) -> u32 {
        let slots = processors.saturating_sub(1) / self.threads();
        u32::try_from(slots).unwrap_or(u32::MAX).max(1)
    }

    /// The `kind` of a slot-count request: `heavy` or `light` in any letter case. A missing kind is passed as null.
    ///
    /// # Errors
    ///
    /// [`ParseError::Kind`] for any other value, carrying the value as the refusal quotes it.
    pub fn from_request(value: &Value) -> Result<Self, ParseError> {
        let text = powershell_text(value);
        for kind in SlotKind::ALL {
            if text.eq_ignore_ascii_case(kind.as_str()) {
                return Ok(kind);
            }
        }
        Err(ParseError::Kind(format!("'{text}'")))
    }
}

/// A slot count: a whole number from 1 to 64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SlotCount(u8);

impl SlotCount {
    /// The smallest count a pool may be set to.
    pub const MIN: u8 = 1;
    /// The largest count a pool may be set to.
    pub const MAX: u8 = 64;

    /// The count, or `None` outside 1 to 64.
    #[must_use]
    pub fn new(count: u32) -> Option<Self> {
        let count = u8::try_from(count).ok()?;
        (Self::MIN..=Self::MAX).contains(&count).then_some(Self(count))
    }

    /// The count as a number.
    #[must_use]
    pub fn get(self) -> u32 {
        u32::from(self.0)
    }

    /// A JSON value that is a whole number 1 to 64 (`3.0` included), or `None` for anything else.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let Value::Number(number) = value else {
            return None;
        };
        let number = number.as_f64()?;
        if number.fract() != 0.0 || !(f64::from(Self::MIN)..=f64::from(Self::MAX)).contains(&number) {
            return None;
        }
        (Self::MIN..=Self::MAX).find(|count| f64::from(*count) >= number).map(Self)
    }

    /// The `count` of a slot-count request: a whole number 1 to 64, or null (or missing) for the kind's default.
    ///
    /// # Errors
    ///
    /// [`ParseError::Count`] for any other value, carrying the value as the refusal quotes it.
    pub fn from_request(value: &Value) -> Result<Option<Self>, ParseError> {
        if value.is_null() {
            return Ok(None);
        }
        match Self::from_json(value) {
            Some(count) => Ok(Some(count)),
            None => Err(ParseError::Count(format_count_value(value))),
        }
    }
}

/// Why a slot-count request's `kind` or `count` was refused, in the words the slot-count POST's 400 carries.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The kind was neither `heavy` nor `light`; the value is quoted as the message shows it.
    #[error("kind must be heavy or light, got {0}")]
    Kind(String),
    /// The count was neither null nor a whole number 1 to 64; the value is quoted as the message shows it.
    #[error("count must be a whole number 1 to 64 or null, got {0}")]
    Count(String),
}

/// The counts the counts file sets, one optional count per kind, serialised heavy then light without the unset ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SlotCounts {
    /// The heavy pool's count, or `None` for its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heavy: Option<SlotCount>,
    /// The light pool's count, or `None` for its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light: Option<SlotCount>,
}

impl SlotCounts {
    /// The kind's count, or `None` when it is unset.
    #[must_use]
    pub fn get(&self, kind: SlotKind) -> Option<SlotCount> {
        match kind {
            SlotKind::Heavy => self.heavy,
            SlotKind::Light => self.light,
        }
    }

    /// Sets the kind's count, or unsets it with `None`.
    pub fn set(&mut self, kind: SlotKind, count: Option<SlotCount>) {
        match kind {
            SlotKind::Heavy => self.heavy = count,
            SlotKind::Light => self.light = count,
        }
    }

    /// True when neither kind is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heavy.is_none() && self.light.is_none()
    }
}

/// The name of the cutover flag, a file in `%LOCALAPPDATA%\gate` whose presence, whatever it holds, hands the machine
/// from the PowerShell gate to `gate.exe`.
const CUTOVER_FLAG: &str = "cutover.flag";

/// Which gate's files the state and the journal are read from: the PowerShell gate's until the cutover flag exists,
/// then only `gate.exe`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Generation {
    /// The PowerShell gate's files under `<local app data>\gate`, read leniently, its tickets in .NET ticks.
    PowerShell,
    /// `gate.exe`'s files under its default state root `<local app data>\gate\v1`: the counts file and tickets read
    /// strictly as v1 formats, the tickets in Unix epoch milliseconds, and the waiting gates taken from the tickets.
    V1,
}

impl Generation {
    /// The cutover flag, `<local app data>\gate\cutover.flag`.
    #[must_use]
    pub fn cutover_flag(local_app_data: &Path) -> PathBuf {
        Self::flag_in(&local_app_data.join("gate"))
    }

    /// [`Generation::V1`] when the cutover flag exists, else [`Generation::PowerShell`].
    ///
    /// # Errors
    ///
    /// The io error when whether the flag exists cannot be told, such as access to its folder being denied.
    pub fn detect(local_app_data: &Path) -> io::Result<Self> {
        Self::detect_in(&local_app_data.join("gate"))
    }

    /// The cutover flag in `folder`, the one holding the PowerShell gate's counts file: `<folder>\cutover.flag`.
    pub(crate) fn flag_in(folder: &Path) -> PathBuf {
        folder.join(CUTOVER_FLAG)
    }

    /// [`Generation::V1`] when the cutover flag in `folder` exists, else [`Generation::PowerShell`].
    pub(crate) fn detect_in(folder: &Path) -> io::Result<Self> {
        if Self::flag_in(folder).try_exists()? {
            Ok(Self::V1)
        } else {
            Ok(Self::PowerShell)
        }
    }

    /// The folder holding the generation's `slots`, `waiting` folder, counts file and journal: `<local app data>\gate`
    /// for the PowerShell gate, `<local app data>\gate\v1` for `gate.exe`. A missing folder reads as an empty one.
    #[must_use]
    pub fn state_dir(self, local_app_data: &Path) -> PathBuf {
        let gate = local_app_data.join("gate");
        match self {
            Generation::PowerShell => gate,
            Generation::V1 => gate.join("v1"),
        }
    }
}

/// What the state assembly takes from its caller rather than reading for itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateInputs {
    /// The moment the state describes.
    pub now: Timestamp,
    /// The machine's name.
    pub machine: String,
    /// The machine's logical processor count, from which each pool's default count follows.
    pub processors: usize,
    /// The per-user local application data folder, under which each generation's state folder sits
    /// ([`Generation::state_dir`]), the cutover flag is, and `gate\remote.json` lives for both generations.
    pub local_app_data: PathBuf,
    /// The PowerShell gate's counts file to use in place of the per-user one (the gate's `GATE_TEST_SLOT_COUNTS`), for
    /// tests only; for [`Generation::V1`] the counts file read is `v1\slot-counts.json` beside it, which a counts edit
    /// writes too.
    pub slot_counts_override: Option<PathBuf>,
    /// The prefix the gate's self-test puts in front of the slot and critical mutex names, their claim files' names, the
    /// `waiting` folder's name and the journal's (the gate's `GATE_TEST_SLOT_PREFIX`), for tests only; empty otherwise.
    /// With a prefix set, only claims carrying it are read. It applies in the state folder of either generation.
    pub slot_prefix: String,
    /// Whose files are read: [`Generation::detect`] gives it from the cutover flag.
    pub generation: Generation,
}

/// Whether a gate slot's mutex exists.
pub trait MutexProbe {
    /// True when some process has the mutex `Local\<name>` open; asking must not take it.
    fn is_open(&self, name: &str) -> bool;
}

/// A process's working directory.
pub trait CwdReader {
    /// The process's current directory, or `None` when it cannot be read.
    fn current_dir(&self, pid: u32) -> Option<String>;
}

/// Everything the Gates tab shows, in the JSON shape and key order `GET /api/gate/slots` serves.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DashboardState {
    /// The moment described, ISO 8601 UTC with seven fractional digits.
    pub now: String,
    /// The machine's name.
    pub machine: String,
    /// The machine's logical processor count.
    pub processors: usize,
    /// The critical request's row: the gate holding the critical mutex, and the critical gates waiting for it.
    pub critical: CriticalState,
    /// The arrival tickets of the live gates waiting for a slot or for the machine, oldest first.
    pub queue: Vec<QueueTicket>,
    /// The heavy pool, then the light one.
    pub pools: Vec<PoolState>,
    /// The remote pool, whose slots the gate runs on a build box; `None` while the gate's `remote.json` is absent and no
    /// remote claim file exists.
    pub remote: Option<RemotePool>,
    /// The live gates running under a parent gate's slot, from the covered markers they keep, oldest marker first;
    /// always empty for [`Generation::PowerShell`], whose gate writes none.
    pub covered: Vec<CoveredRun>,
}

/// A live gate a parent gate's slot covers, from the covered marker it keeps while its command runs: it takes no slot
/// and writes no ticket or claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoveredRun {
    /// The covered gate's pid.
    pub pid: u32,
    /// The covered gate's process start, ISO 8601 UTC with seven fractional digits.
    pub start: String,
    /// The pid of the parent gate whose slot covers it.
    pub parent_pid: u32,
    /// The gated command, as the gate's claim would write it.
    pub command: String,
    /// The covered gate's working directory.
    pub cwd: String,
    /// When the covered gate wrote its marker, ISO 8601 UTC with seven fractional digits.
    pub since: String,
}

/// The remote pool: heavy gates the gate routes to a build box over ssh. Its size is the gate's `remote.json`, which
/// only the gate writes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemotePool {
    /// The slots gates may take: `remote.json`'s `slots` (1 when the key is missing), or 0 when the file is absent or
    /// ignored.
    pub count: u32,
    /// Why `remote.json` was ignored, or that it is absent while a remote claim file exists; `None` otherwise.
    pub problem: Option<String>,
    /// The slots `0..count`, plus any slot past the count that has a claim file. A gate waiting for one shows only as its
    /// ticket in the queue.
    pub slots: Vec<SlotRow>,
}

/// The critical request: who holds the machine and who waits to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CriticalState {
    /// The critical mutex's row, index 0: busy once its gate holds every slot and has written its claim, held while that
    /// gate waits for the earlier gates and the slots' holders, free when there is none. It has no `extra` key.
    pub holder: SlotRow,
    /// The gates run with `-Slot critical` that hold no live claim and have no child yet.
    pub waiting: Vec<Waiting>,
}

/// A live gate's arrival ticket, which it writes as it starts and deletes once it holds its slot; a critical gate keeps
/// its own to its end.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueueTicket {
    /// The slot the gate asked for, as the ticket has it: `heavy`, `light` or `critical`.
    pub kind: String,
    /// True for a critical gate that holds the machine.
    pub holds: bool,
    /// True for a heavy gate routed to the remote pool and waiting for a remote slot; serialised only when true.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// The gate's process id.
    pub pid: u32,
    /// When the gate arrived, ISO 8601 UTC with seven fractional digits.
    pub arrived: String,
    /// Whole seconds from the arrival to the state's moment, halves rounded to even, serialised as a float like `5.0`.
    #[serde(rename = "ageSeconds")]
    pub age_seconds: f64,
    /// The gate's working directory, when it could be read.
    pub cwd: Option<String>,
    /// The gate's command line, when it could be read.
    #[serde(rename = "commandLine")]
    pub command_line: Option<String>,
}

/// One pool of slots.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PoolState {
    /// The pool's kind.
    pub kind: SlotKind,
    /// The slots gates may take: the counts file's value, or the default.
    pub count: u32,
    /// The count the processor count gives.
    #[serde(rename = "default")]
    pub default_count: u32,
    /// The counts file's valid value for the kind, or `None`.
    pub set: Option<SlotCount>,
    /// The line the gate writes when it ignores the counts file or its value for the kind, or `None`.
    pub problem: Option<String>,
    /// The slots `0..count`, plus any slot past the count that has a claim file.
    pub slots: Vec<SlotRow>,
    /// The gates waiting for a slot of the kind, listed only when no slot within the count is free or a live critical
    /// ticket bars the pool.
    pub waiting: Vec<Waiting>,
}

/// One slot's row.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SlotRow {
    /// The slot's index in its pool.
    pub index: u32,
    /// The slot's mutex name.
    pub name: String,
    /// Whether a gate holds the slot, and whether its claim is live.
    pub state: SlotState,
    /// The claim file of the live gate holding the slot, as the file has it, when the slot is busy.
    pub claim: Option<Value>,
    /// A claim file that is not live, or `{"unreadable": <why>}` for one that could not be used.
    pub stale: Option<Value>,
    /// The number of processes under the holding gate, the gate itself excluded.
    pub processes: usize,
    /// The last non-blank line of the holding gate's log.
    #[serde(rename = "lastLine")]
    pub last_line: Option<String>,
    /// True for a held slot of a pool while a gate holds or is taking the critical mutex: the critical gate holds it.
    pub critical: bool,
    /// For a pool's slot, true when it is past the pool's count and shown only because it has a claim file; `None`, and
    /// no key, for the critical row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<bool>,
}

/// What holds a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotState {
    /// No process has the slot's mutex open.
    Free,
    /// A live gate holds the slot and its claim file names it.
    Busy,
    /// The slot's mutex is open but no live claim names its holder.
    Held,
}

/// A gate waiting for a slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Waiting {
    /// The waiting gate's process id.
    pub pid: u32,
    /// When it started, ISO 8601 UTC with seven fractional digits, or `None` when unknown.
    pub since: Option<String>,
    /// Its working directory, when it could be read.
    pub cwd: Option<String>,
    /// Its command line.
    #[serde(rename = "commandLine")]
    pub command_line: String,
}

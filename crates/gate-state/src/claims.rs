//! The claim files gates write for the slots they hold, the arrival tickets they write as they start, the last line of
//! a holding gate's log, and the rule that tells whether a process is the PowerShell gate that wrote a claim or ticket.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gate_wire::stamp::{Tag, is_process_line, unstamped};
use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::counts::powershell_text;
use crate::root::PoolId;

/// How much of a log's end is read for its last line: a busy build's last few KiB can be all process lines.
const LOG_TAIL_BYTES: u64 = 64 * 1024;
/// How far a live process's start may be from the one its PowerShell claim records, in nanoseconds (2 s).
const CLAIM_START_TOLERANCE_NANOS: i128 = 2_000_000_000;
/// How far a live process's start may be from its PowerShell ticket's `startTicks`, in .NET ticks (2 s).
const TICKET_START_TOLERANCE_TICKS: i128 = 20_000_000;
/// .NET ticks (100 ns) in a second.
pub(crate) const TICKS_PER_SECOND: i64 = 10_000_000;
/// .NET ticks from 0001-01-01 to the Unix epoch.
pub(crate) const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;
/// The last .NET tick a `DateTime` can hold, 9999-12-31T23:59:59.9999999.
const MAX_TICKS: i64 = 3_155_378_975_999_999_999;

/// A slot's claim file.
#[derive(Clone, Debug)]
pub(crate) enum Claim {
    /// A claim with every field the state needs.
    Readable(ReadableClaim),
    /// A claim that could not be read, parsed or used: `{"unreadable": <why>}`.
    Unreadable(Value),
}

impl Claim {
    /// The claim as a stale slot shows it.
    pub(crate) fn as_value(&self) -> &Value {
        match self {
            Claim::Readable(claim) => &claim.value,
            Claim::Unreadable(value) => value,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadableClaim {
    /// The claim as the file has it, in the file's key order.
    pub value: Value,
    /// The holding gate's process id.
    pub pid: u32,
    /// When the holding gate's process started.
    pub process_start: Timestamp,
    /// The holding gate's log, a relative path resolved against the claim's `cwd`.
    pub log: Option<PathBuf>,
}

/// An arrival ticket's fields, as a gate writes them.
#[derive(Clone, Debug)]
pub(crate) struct Ticket {
    /// The slot the gate asked for, as text.
    pub kind: String,
    /// True for a critical gate that holds the machine.
    pub holds: bool,
    /// The gate's process id.
    pub pid: u32,
    /// The gate's process start, in .NET ticks.
    pub start_ticks: i64,
    /// When the gate arrived, in .NET ticks.
    pub arrived_ticks: i64,
    /// True for a heavy gate routed to the remote pool; a ticket without the key is a local one.
    pub remote: bool,
    /// The id of the `builders` entry's pool the gate queues for; `None` for `host`'s pool and a local ticket.
    pub pool: Option<String>,
    /// True for a run pinned to one builder with `--builder`; a ticket without the key is not pinned.
    pub pinned: bool,
    /// For a `gate.exe` ticket, the gate's start in Unix epoch milliseconds, which its process's start must match to the
    /// millisecond as `gate.exe` checks it; `None` for a PowerShell ticket, whose start may be 2 s off.
    pub exact_start_ms: Option<u64>,
    /// For a `gate.exe` ticket, what the durations store expected of the gate's run as it queued; `None` for a ticket
    /// without one and for a PowerShell ticket, which never carries it.
    pub expected: Option<Map<String, Value>>,
    /// The free text the gate gave `--reason`; `None` for a PowerShell ticket, which never carries it.
    pub reason: Option<String>,
}

/// The claim files of the slots and of the critical gate in `dir`, keyed by mutex name in lower case: the files named
/// `<prefix>gate-slot-<n>`, `<prefix>gate-light-slot-<n>`, `<prefix>gate-remote-slot-<n>`,
/// `<prefix>gate-remote-<pool id>-slot-<n>` or `<prefix>gate-critical` with a `.json` extension, in any letter case but
/// the pool id's, which is lower case. A claim without the prefix is left out, so is one that is gone, and a
/// missing folder holds none, and so does one that cannot be listed.
pub(crate) fn read_claims(dir: &Path, prefix: &str) -> HashMap<String, Claim> {
    try_read_claims(dir, prefix).unwrap_or_default()
}

/// The claim files [`read_claims`] reads, from a folder that must list: a missing folder holds none.
///
/// # Errors
///
/// The io error when the folder exists but cannot be listed.
pub(crate) fn try_read_claims(dir: &Path, prefix: &str) -> io::Result<HashMap<String, Claim>> {
    let mut claims = HashMap::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(claims),
        Err(error) => return Err(error),
    };
    let prefix = prefix.to_ascii_lowercase();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = mutex_name(&path, &prefix) else {
            continue;
        };
        if path.is_file()
            && let Some(claim) = read_claim(&path)
        {
            claims.insert(name, claim);
        }
    }
    Ok(claims)
}

/// The mutex name a claim file's name gives, lowered, when it is a slot's or the critical gate's under the lowered
/// prefix.
fn mutex_name(path: &Path, prefix: &str) -> Option<String> {
    let base = json_base_name(path)?;
    let name = base.strip_prefix(prefix)?;
    if name == "gate-critical" || is_pooled_remote_slot(path, prefix) {
        return Some(base);
    }
    let digits = name
        .strip_prefix("gate-light-slot-")
        .or_else(|| name.strip_prefix("gate-remote-slot-"))
        .or_else(|| name.strip_prefix("gate-slot-"))?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then_some(base)
}

/// Whether the claim file at `path` is a `builders` entry's remote slot's under the lowered `prefix`,
/// `<prefix>gate-remote-<pool id>-slot-<n>.json`: the pool id exactly as a gate writes it ([`PoolId::parse`]), the rest
/// in any letter case.
fn is_pooled_remote_slot(path: &Path, prefix: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(rest) = strip_prefix_any_case(name, prefix).and_then(|rest| strip_prefix_any_case(rest, "gate-remote-")) else {
        return false;
    };
    let Some(at) = rest.to_ascii_lowercase().find("-slot-") else {
        return false;
    };
    let (pool, slot) = rest.split_at(at);
    let tail = slot["-slot-".len()..].to_ascii_lowercase();
    let digits = tail.strip_suffix(".json");
    PoolId::parse(pool).is_some() && digits.is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `text` less `prefix`, matched in any ASCII letter case; `None` when `text` does not start with it.
fn strip_prefix_any_case<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// A file's name lowered, without its `.json` extension; `None` for another extension.
pub(crate) fn json_base_name(path: &Path) -> Option<String> {
    let file = path.file_name()?.to_str()?.to_ascii_lowercase();
    file.strip_suffix(".json").map(str::to_owned)
}

/// The arrival tickets in `dir`, the `.json` files that hold every field a gate writes. A ticket that cannot be read
/// is left out: its gate took its slot and deleted it between the listing and the read, or no gate wrote it. A missing
/// folder holds none.
pub(crate) fn read_tickets(dir: &Path) -> Vec<Ticket> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut tickets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if json_base_name(&path).is_some()
            && path.is_file()
            && let Some(ticket) = read_ticket(&path)
        {
            tickets.push(ticket);
        }
    }
    tickets
}

/// The ticket at `path`; `None` when it cannot be read, is not JSON or lacks a field a gate writes.
pub(crate) fn read_ticket(path: &Path) -> Option<Ticket> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&read_text(path).ok()?) else {
        return None;
    };
    Some(Ticket {
        kind: powershell_text(object.get("kind")?),
        holds: object.get("holds")?.as_bool()?,
        pid: object.get("pid")?.as_u64().and_then(|pid| u32::try_from(pid).ok())?,
        start_ticks: object.get("startTicks")?.as_i64()?,
        arrived_ticks: object.get("arrived")?.as_i64()?,
        remote: object.get("remote").and_then(Value::as_bool).unwrap_or(false),
        pool: object.get("pool").and_then(Value::as_str).map(str::to_owned),
        pinned: object.get("pinned").and_then(Value::as_bool).unwrap_or(false),
        exact_start_ms: None,
        expected: None,
        reason: None,
    })
}

/// A file's text, without a UTF-8 byte order mark.
fn read_text(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).into_owned())
}

/// True when a process created at `created` is the PowerShell gate whose claim records `claim_process_start`: the two
/// are less than 2 s apart.
pub(crate) fn claim_started_by(claim_process_start: Timestamp, created: Timestamp) -> bool {
    (created.as_nanosecond() - claim_process_start.as_nanosecond()).abs() < CLAIM_START_TOLERANCE_NANOS
}

/// True when a process created at `created` is the PowerShell gate whose ticket records `start_ticks`: the two are less
/// than 2 s apart.
pub(crate) fn ticket_started_by(start_ticks: i64, created: Timestamp) -> bool {
    (ticks_of(created) - i128::from(start_ticks)).abs() < TICKET_START_TOLERANCE_TICKS
}

/// Whether a process created at `created` is the `gate.exe` gate whose ticket or covered marker records a start of
/// `start_ms` Unix epoch milliseconds: the two agree to the millisecond.
pub(crate) fn started_at_ms(created: Timestamp, start_ms: u64) -> bool {
    created.as_millisecond().unsigned_abs() == start_ms
}

/// A timestamp as .NET ticks, truncated to the tick.
pub(crate) fn ticks_of(timestamp: Timestamp) -> i128 {
    timestamp.as_nanosecond().div_euclid(100) + UNIX_EPOCH_TICKS
}

/// True when `ticks` is in the range a .NET `DateTime` holds, 0001-01-01 to 9999-12-31T23:59:59.9999999.
pub(crate) fn ticks_in_range(ticks: i64) -> bool {
    (0..=MAX_TICKS).contains(&ticks)
}

/// The moment .NET `ticks` name, or `None` outside the range a `DateTime` holds or a [`Timestamp`] holds.
pub(crate) fn timestamp_of_ticks(ticks: i64) -> Option<Timestamp> {
    if !ticks_in_range(ticks) {
        return None;
    }
    Timestamp::from_nanosecond((i128::from(ticks) - UNIX_EPOCH_TICKS) * 100).ok()
}

/// The claim at `path`, or `None` when the claim is gone: a read that fails with not-found is a claim its gate deleted
/// between the listing and this read, one that fails with access denied is a claim whose delete is pending and that
/// Windows refuses to open — each is going away, not unreadable. Every other read error is the claim as an unreadable
/// stale claim.
fn read_claim(path: &Path) -> Option<Claim> {
    let text = match read_text(path) {
        Ok(text) => text,
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => return None,
        Err(error) => return Some(unreadable(&error.to_string())),
    };
    Some(match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => parse_claim(object),
        Ok(_) => unreadable("the claim is not a JSON object"),
        Err(error) => unreadable(&error.to_string()),
    })
}

fn parse_claim(object: Map<String, Value>) -> Claim {
    let Some(pid) = object.get("pid").and_then(Value::as_u64).and_then(|pid| u32::try_from(pid).ok()) else {
        return unreadable("the claim has no pid that is a process id");
    };
    let Some(start) = object.get("processStart").and_then(Value::as_str) else {
        return unreadable("the claim has no processStart");
    };
    let process_start = match start.parse::<Timestamp>() {
        Ok(process_start) => process_start,
        Err(error) => return unreadable(&format!("the claim's processStart is not a timestamp: {error}")),
    };
    let log = claim_log(&object);
    Claim::Readable(ReadableClaim {
        value: Value::Object(object),
        pid,
        process_start,
        log,
    })
}

/// The claim's log path, a relative one joined to the claim's `cwd`; `None` when the claim names no log, or names a
/// relative one without a `cwd`.
fn claim_log(object: &Map<String, Value>) -> Option<PathBuf> {
    let log = Path::new(object.get("log")?.as_str().filter(|log| !log.is_empty())?);
    if log.is_absolute() {
        return Some(log.to_path_buf());
    }
    let cwd = object.get("cwd")?.as_str().filter(|cwd| !cwd.is_empty())?;
    Some(Path::new(cwd).join(log))
}

fn unreadable(message: &str) -> Claim {
    let mut object = Map::new();
    object.insert("unreadable".to_owned(), Value::String(message.to_owned()));
    Claim::Unreadable(Value::Object(object))
}

/// The last non-blank line of the log, read from its final 64 KB without locking out the gate that writes it, as its
/// text after its stamp and tag: a `gat` process line is passed over, and a line with no stamp is read whole. `None`
/// when there is no log or no such line, or `(could not read the log: <why>)`.
pub(crate) fn last_log_line(path: Option<&Path>) -> Option<String> {
    let path = path?;
    if !path.exists() {
        return None;
    }
    let text = match read_tail(path) {
        Ok(text) => text,
        Err(error) => return Some(format!("(could not read the log: {error})")),
    };
    let mut last = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let parts = unstamped(line);
        if parts.text.trim().is_empty() || (parts.tag == Some(Tag::Gat) && is_process_line(parts.text)) {
            continue;
        }
        last = Some(parts.text);
    }
    last.map(str::to_owned)
}

fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = open_shared(path)?;
    let length = file.metadata()?.len();
    let take = length.min(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(length - take))?;
    let mut buffer = Vec::new();
    file.take(take).read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(windows)]
fn open_shared(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

#[cfg(not(windows))]
fn open_shared(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose own moments do not parse must fail loudly")]
mod tests {
    use std::path::Path;

    use jiff::{SignedDuration, Timestamp};

    use super::{MAX_TICKS, UNIX_EPOCH_TICKS, claim_started_by, mutex_name, read_claim, ticket_started_by, ticks_of, timestamp_of_ticks};

    const START: &str = "2026-10-01T11:00:00.5Z";

    fn start() -> Timestamp {
        START.parse().expect("the test's start is a timestamp")
    }

    fn start_ticks() -> i64 {
        i64::try_from(ticks_of(start())).expect("the test's start fits in ticks")
    }

    fn after(nanos: i64) -> Timestamp {
        start()
            .checked_add(SignedDuration::from_nanos(nanos))
            .expect("the test's moment is a timestamp")
    }

    #[test]
    fn a_claim_is_started_by_a_process_created_less_than_2_s_from_it() {
        assert!(claim_started_by(start(), after(1_999_999_999)), "just under 2 s later");
        assert!(claim_started_by(start(), after(-1_999_999_999)), "just under 2 s earlier");
        assert!(!claim_started_by(start(), after(2_000_000_000)), "exactly 2 s later");
        assert!(!claim_started_by(start(), after(-2_000_000_000)), "exactly 2 s earlier");
    }

    #[test]
    fn a_ticket_is_started_by_a_process_created_less_than_2_s_from_it() {
        assert!(ticket_started_by(start_ticks(), after(1_999_999_900)), "one tick under 2 s later");
        assert!(ticket_started_by(start_ticks(), after(-1_999_999_900)), "one tick under 2 s earlier");
        assert!(!ticket_started_by(start_ticks(), after(2_000_000_000)), "exactly 2 s later");
        assert!(!ticket_started_by(start_ticks(), after(-2_000_000_000)), "exactly 2 s earlier");
    }

    #[test]
    fn ticks_of_counts_100_ns_from_year_one() {
        assert_eq!(ticks_of(Timestamp::UNIX_EPOCH), UNIX_EPOCH_TICKS);
        assert_eq!(ticks_of(after(99)), ticks_of(start()), "truncated to the tick");
        assert_eq!(
            timestamp_of_ticks(start_ticks()),
            Some(start()),
            "the ticks name the moment they came from"
        );
    }

    #[test]
    fn ticks_outside_what_a_date_time_holds_are_no_moment() {
        assert_eq!(timestamp_of_ticks(-1), None, "before 0001-01-01");
        assert_eq!(timestamp_of_ticks(MAX_TICKS + 1), None, "after 9999-12-31T23:59:59.9999999");
        assert_eq!(
            timestamp_of_ticks(0),
            Some("0001-01-01T00:00:00Z".parse().expect("year one is a timestamp")),
            "the first tick"
        );
    }

    #[test]
    fn a_builders_remote_claim_names_its_mutex_with_its_pool_id() {
        let name = |file: &str, prefix: &str| mutex_name(Path::new(file), prefix);

        assert_eq!(
            name("gate-remote-0a1b2c3d-slot-2.json", "").as_deref(),
            Some("gate-remote-0a1b2c3d-slot-2")
        );
        assert_eq!(
            name("ops225-gate-remote-0a1b2c3d-slot-2.json", "ops225-").as_deref(),
            Some("ops225-gate-remote-0a1b2c3d-slot-2"),
            "under a prefix"
        );
        assert_eq!(name("gate-remote-slot-2.json", "").as_deref(), Some("gate-remote-slot-2"), "host's pool");
        for refused in [
            "gate-remote-0A1B2C3D-slot-2.json",
            "gate-remote-0a1b2c3-slot-2.json",
            "gate-remote-0a1b2c3d4-slot-2.json",
            "gate-remote-0a1b2c3d-slot-.json",
            "gate-remote-0a1b2c3d-slot-x.json",
        ] {
            assert_eq!(name(refused, ""), None, "{refused}");
        }
    }

    #[test]
    fn a_claim_that_is_not_there_is_no_claim() {
        let path = crate::test_support::temp_dir().join("gate-state-no-such-claim.json");
        assert!(!path.exists(), "the test's path is free");

        assert!(read_claim(&path).is_none(), "a claim that is gone is no claim");
    }
}

//! The claim files gates write for the slots they hold, the arrival tickets they write as they start, and the last line
//! of a holding gate's log.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::counts::powershell_text;

/// How much of a log's end is read for its last line.
const LOG_TAIL_BYTES: u64 = 4096;

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
    /// For a `gate.exe` ticket, the gate's start in Unix epoch milliseconds, which its process's start must match to the
    /// millisecond as `gate.exe` checks it; `None` for a PowerShell ticket, whose start may be 2 s off.
    pub exact_start_ms: Option<u64>,
}

/// The claim files of the slots and of the critical gate in `dir`, keyed by mutex name in lower case: the files named
/// `<prefix>gate-slot-<n>`, `<prefix>gate-light-slot-<n>`, `<prefix>gate-remote-slot-<n>` or `<prefix>gate-critical`
/// with a `.json` extension, in any letter case. A claim without the prefix is left out; a missing folder holds none.
pub(crate) fn read_claims(dir: &Path, prefix: &str) -> HashMap<String, Claim> {
    let mut claims = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return claims;
    };
    let prefix = prefix.to_ascii_lowercase();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = mutex_name(&path, &prefix) else {
            continue;
        };
        if path.is_file() {
            claims.insert(name, read_claim(&path));
        }
    }
    claims
}

/// The mutex name a claim file's name gives, lowered, when it is a slot's or the critical gate's under the lowered
/// prefix.
fn mutex_name(path: &Path, prefix: &str) -> Option<String> {
    let base = json_base_name(path)?;
    let name = base.strip_prefix(prefix)?;
    if name == "gate-critical" {
        return Some(base);
    }
    let digits = name
        .strip_prefix("gate-light-slot-")
        .or_else(|| name.strip_prefix("gate-remote-slot-"))
        .or_else(|| name.strip_prefix("gate-slot-"))?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then_some(base)
}

/// A file's name lowered, without its `.json` extension; `None` for another extension.
fn json_base_name(path: &Path) -> Option<String> {
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

fn read_ticket(path: &Path) -> Option<Ticket> {
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
        exact_start_ms: None,
    })
}

/// A file's text, without a UTF-8 byte order mark.
fn read_text(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).into_owned())
}

fn read_claim(path: &Path) -> Claim {
    let text = match read_text(path) {
        Ok(text) => text,
        Err(error) => return unreadable(&error.to_string()),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => parse_claim(object),
        Ok(_) => unreadable("the claim is not a JSON object"),
        Err(error) => unreadable(&error.to_string()),
    }
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

/// The last non-blank line of the log, read from its final 4 KB without locking out the gate that writes it; `None`
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
        if !line.trim().is_empty() {
            last = Some(line);
        }
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

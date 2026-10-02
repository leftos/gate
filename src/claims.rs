//! The claim files gates write for the slots they hold, and the last line of a holding gate's log.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde_json::{Map, Value};

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

/// The claim files of real slots in `dir`, keyed by slot name in lower case. Self-test claims carry a prefix and are
/// left out; a missing folder holds none.
pub(crate) fn read_claims(dir: &Path) -> HashMap<String, Claim> {
    let mut claims = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return claims;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = slot_name(&path) else {
            continue;
        };
        if path.is_file() {
            claims.insert(name, read_claim(&path));
        }
    }
    claims
}

/// The slot name a claim file's name gives: `gate-slot-<n>` or `gate-light-slot-<n>` with a `.json` extension, in any
/// letter case, lowered.
fn slot_name(path: &Path) -> Option<String> {
    let file = path.file_name()?.to_str()?.to_ascii_lowercase();
    let base = file.strip_suffix(".json")?;
    let digits = base.strip_prefix("gate-light-slot-").or_else(|| base.strip_prefix("gate-slot-"))?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then(|| base.to_owned())
}

fn read_claim(path: &Path) -> Claim {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return unreadable(&error.to_string()),
    };
    let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes));
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

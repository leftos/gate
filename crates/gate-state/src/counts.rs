//! The slot counts file the gate reads: where it is, what it sets, the line the gate writes when it ignores it, and
//! writing it.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::model::{SlotCount, SlotCounts, SlotKind, StateInputs};

/// How many times a write tries to move its file into place, or to delete the file, before it fails.
const WRITE_ATTEMPTS: u32 = 10;
/// The pause between two of those tries.
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// The next `<n>` a temp file name takes, counted across every thread of this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Why the counts file could not be written.
#[derive(Debug, thiserror::Error)]
#[error("could not write {} ({source})", .path.display())]
pub struct WriteError {
    /// The counts file.
    pub path: PathBuf,
    /// The last error the write met.
    pub source: io::Error,
}

/// The counts file the gate reads: the override when the caller gives a non-empty one, else
/// `<local app data>\gate\slot-counts.json`.
#[must_use]
pub fn slot_count_path(inputs: &StateInputs) -> PathBuf {
    if let Some(path) = &inputs.slot_counts_override
        && !path.as_os_str().is_empty()
    {
        return path.clone();
    }
    inputs.local_app_data.join("gate").join("slot-counts.json")
}

/// The counts file parsed: `Ok(None)` when it does not exist, `Err` with the reason the gate gives when it cannot be
/// read, is not valid JSON, or is not a JSON object.
pub(crate) type CountsFile = Result<Option<Map<String, Value>>, String>;

pub(crate) fn read_counts_file(path: &Path) -> CountsFile {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("it could not be read: {}", prose(&error.to_string()))),
    };
    let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes));
    // The gate's JSON reader gives no value at all for blank text, which it then refuses as not an object.
    if text.trim().is_empty() {
        return Err("it is not a JSON object".to_owned());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(counts)) => Ok(Some(counts)),
        Ok(_) => Err("it is not a JSON object".to_owned()),
        Err(error) => Err(format!("it is not valid JSON: {}", prose(&error.to_string()))),
    }
}

/// One pool's counts as the gate reads them.
pub(crate) struct PoolCount {
    /// The count the processor count gives.
    pub default: u32,
    /// The counts file's value for the kind when it is a whole number 1 to 64.
    pub set: Option<SlotCount>,
    /// The line the gate writes for a counts file or a value it ignored.
    pub problem: Option<String>,
}

pub(crate) fn pool_count(file: &CountsFile, path: &Path, kind: SlotKind, processors: usize) -> PoolCount {
    let default = kind.default_count(processors);
    let ignored = |reason: &str| PoolCount {
        default,
        set: None,
        problem: Some(ignored_line(path, &reason, default, kind)),
    };
    let counts = match file {
        Ok(Some(counts)) => counts,
        Ok(None) => {
            return PoolCount {
                default,
                set: None,
                problem: None,
            };
        }
        Err(reason) => return ignored(reason),
    };
    let Some(value) = property(counts, kind.as_str()) else {
        return PoolCount {
            default,
            set: None,
            problem: None,
        };
    };
    match SlotCount::from_json(value) {
        Some(count) => PoolCount {
            default,
            set: Some(count),
            problem: None,
        },
        None => ignored(&format!(
            "{} must be a whole number 1 to 64, got {}",
            kind.as_str(),
            format_count_value(value)
        )),
    }
}

/// The line the gate says when it ignores the counts file or one kind's value in it and uses `count` slots of `kind`:
/// `gate: ignored <path>: <reason>; using <count> <kind> slots`.
pub(crate) fn ignored_line(path: &Path, reason: &dyn fmt::Display, count: u32, kind: SlotKind) -> String {
    format!("gate: ignored {}: {reason}; using {count} {} slots", path.display(), kind.as_str())
}

/// The value of an object's key, matched without regard to ASCII case as the gate's JSON reader does.
pub(crate) fn property<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    for (name, value) in object {
        if name.eq_ignore_ascii_case(key) {
            return Some(value);
        }
    }
    None
}

/// The counts the file sets that the gate would use, so a write keeps the other kind's value and drops any value the
/// gate itself would ignore.
#[must_use]
pub fn read_slot_count_setting(path: &Path) -> SlotCounts {
    let file = read_counts_file(path);
    let mut counts = SlotCounts::default();
    for kind in SlotKind::ALL {
        counts.set(kind, pool_count(&file, path, kind, 1).set);
    }
    counts
}

/// Writes the counts where the gate reads them, as compact JSON without a byte order mark: beside the file first, then
/// moved over it. Empty counts delete the file, so a machine back at its defaults holds none. A gate reading the file
/// blocks the move and the delete for that moment, so each is tried up to ten times, 100 ms apart.
///
/// # Errors
///
/// [`WriteError`] when the folder or the file beside it cannot be written, or the move or delete fails every try.
pub fn write_slot_counts(path: &Path, counts: &SlotCounts) -> Result<(), WriteError> {
    let fail = |source: io::Error| WriteError {
        path: path.to_path_buf(),
        source,
    };
    if counts.is_empty() {
        if !path.exists() {
            return Ok(());
        }
        return with_retry(|| remove_if_present(path)).map_err(fail);
    }
    let text = serde_json::to_string(counts).map_err(|error| fail(io::Error::other(error)))?;
    write_replacing(path, &text).map_err(fail)
}

/// Writes `text` to `path` beside it first, as `<path>.<pid>.<n>.tmp`, then moves that over `path`, so a reader never
/// sees a torn file; `<n>` is a per-process counter, so two threads writing one file never share a temp file. A missing
/// parent folder is made. A reader holding the file blocks the move for that moment, so the move
/// is tried up to ten times, 100 ms apart.
pub(crate) fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    if let Some(folder) = path.parent()
        && !folder.as_os_str().is_empty()
    {
        fs::create_dir_all(folder)?;
    }
    let temp = temp_path(path);
    if let Err(error) = fs::write(&temp, text) {
        return Err(remove_partial(&temp, error));
    }
    let moved = with_retry(|| fs::rename(&temp, path));
    if moved.is_err() {
        // The write has failed and says so; a temp file this cannot remove stays beside the target, since no later write uses its name.
        let _ = fs::remove_file(&temp);
    }
    moved
}

/// Removes the partial file a failed write left at `temp` and returns `error`, the write's failure; a file that is not
/// there is fine, and one that cannot be removed is added to the error.
fn remove_partial(temp: &Path, error: io::Error) -> io::Error {
    match fs::remove_file(temp) {
        Err(removal) if removal.kind() != io::ErrorKind::NotFound => io::Error::new(
            error.kind(),
            format!("{error}; the partial file {} could not be removed: {removal}", temp.display()),
        ),
        _ => error,
    }
}

/// Runs `operation` until it succeeds, up to [`WRITE_ATTEMPTS`] times, [`WRITE_RETRY_DELAY`] apart, returning the
/// last error when every try fails.
fn with_retry(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut failure = io::Error::other("no attempt was made");
    for attempt in 1..=WRITE_ATTEMPTS {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) => failure = error,
        }
        if attempt < WRITE_ATTEMPTS {
            thread::sleep(WRITE_RETRY_DELAY);
        }
    }
    Err(failure)
}

/// Sets or unsets one kind's count in the counts file, keeping the other kind's valid value.
///
/// # Errors
///
/// [`WriteError`] when the file cannot be written.
pub fn apply_slot_count(path: &Path, kind: SlotKind, count: Option<SlotCount>) -> Result<(), WriteError> {
    let mut counts = read_slot_count_setting(path);
    counts.set(kind, count);
    write_slot_counts(path, &counts)
}

/// `<path>.<pid>.<n>.tmp`, `<n>` taken from [`TEMP_COUNTER`] so no two writes of this process share one.
fn temp_path(path: &Path) -> PathBuf {
    let write = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(path.as_os_str());
    name.push(format!(".{}.{write}.tmp", std::process::id()));
    PathBuf::from(name)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Text as one line, so a problem line stays one line however the message it quotes was wrapped.
fn prose(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A value as a problem line or a refusal names it: `null`, or the value's text in single quotes.
pub(crate) fn format_count_value(value: &Value) -> String {
    if value.is_null() {
        return "null".to_owned();
    }
    format!("'{}'", powershell_text(value))
}

/// A JSON value as text the way PowerShell turns it into a string: null is empty, a boolean is
/// `True` or `False`, a whole number has no fraction, an array's items are joined by spaces, and an object is its JSON.
pub(crate) fn powershell_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number_text(number),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(powershell_text).collect::<Vec<_>>().join(" "),
        Value::Object(_) => value.to_string(),
    }
}

fn number_text(number: &serde_json::Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    match number.as_f64() {
        Some(float) if float.fract() == 0.0 && float.abs() < 1e15 => format!("{float:.0}"),
        Some(float) => float.to_string(),
        None => number.to_string(),
    }
}

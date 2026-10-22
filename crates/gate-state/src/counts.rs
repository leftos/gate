//! The slot counts file the gate reads: where it is, what it sets, the line the gate writes when it ignores it, and
//! writing it.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::model::{Generation, SlotCount, SlotCounts, SlotKind, StateInputs};
use crate::v1;

/// How many times a write tries to move its file into place, or to delete the file, before it fails.
const WRITE_ATTEMPTS: u32 = 10;
/// The pause between two of those tries.
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(100);
/// The next `<n>` a temp file name takes, counted across every thread of this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// The most temp names one write tries before it gives up on finding one nothing stands at.
const TEMP_NAMES: u64 = 16;
/// `FILE_FLAG_OPEN_REPARSE_POINT`: a link or junction is opened as itself. Kept here, as gate-state has no windows-sys.
#[cfg(windows)]
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Why the counts file could not be written.
#[derive(Debug, thiserror::Error)]
#[error("could not write {} ({source})", .path.display())]
pub struct WriteError {
    /// The counts file.
    pub path: PathBuf,
    /// The last error the write met.
    pub source: io::Error,
}

/// Why one kind's count could not be applied to the counts file.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The file exists but could not be read, so the other kind's count is unknown and nothing was written.
    #[error("could not read {} ({source})", .path.display())]
    Read {
        /// The counts file.
        path: PathBuf,
        /// The error the read met.
        source: io::Error,
    },
    /// The file could not be written.
    #[error(transparent)]
    Write(#[from] WriteError),
}

/// The counts file the PowerShell gate reads: the override when the caller gives a non-empty one, else
/// `<local app data>\gate\slot-counts.json`. It is the path [`apply_slot_count`] and [`write_slot_counts`] take, whatever
/// the generation: they write `gate.exe`'s `v1\slot-counts.json` beside it too.
#[must_use]
pub fn slot_count_path(inputs: &StateInputs) -> PathBuf {
    if let Some(path) = &inputs.slot_counts_override
        && !path.as_os_str().is_empty()
    {
        return path.clone();
    }
    inputs.local_app_data.join("gate").join("slot-counts.json")
}

/// `gate.exe`'s counts file beside the PowerShell gate's `path`: `<path's folder>\v1\slot-counts.json`, which for the
/// per-user file is the default state root's (`v1::counts_path`).
fn v1_counts_beside(path: &Path) -> PathBuf {
    v1::counts_path(&counts_folder(path).join("v1"))
}

/// The counts file the state reads for `inputs`' generation: [`slot_count_path`], or for [`Generation::V1`] the v1 file
/// beside it.
pub(crate) fn state_counts_path(inputs: &StateInputs) -> PathBuf {
    let path = slot_count_path(inputs);
    match inputs.generation {
        Generation::PowerShell => path,
        Generation::V1 => v1_counts_beside(&path),
    }
}

/// The counts file parsed: `Ok(None)` when it does not exist, `Err` with the reason the gate gives when it cannot be
/// read, is not valid JSON, or is not a JSON object.
pub(crate) type CountsFile = Result<Option<Map<String, Value>>, String>;

pub(crate) fn read_counts_file(path: &Path) -> CountsFile {
    match read_counts_bytes(path) {
        Ok(bytes) => counts_from_bytes(bytes),
        Err(error) => Err(format!("it could not be read: {}", prose(&error.to_string()))),
    }
}

/// The counts file a read gave: `bytes` parsed, or `Ok(None)` when there was no file.
fn counts_from_bytes(bytes: Option<Vec<u8>>) -> CountsFile {
    bytes.map(|bytes| parse_counts(&bytes)).transpose()
}

/// The counts file's bytes: `Ok(None)` when it does not exist, `Err` with the error when it exists but cannot be read.
fn read_counts_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The counts file's bytes parsed, as [`read_counts_file`] parses the bytes it reads: `Err` with the reason the gate
/// gives when they are not valid JSON or not a JSON object.
pub(crate) fn parse_counts(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes));
    // The gate's JSON reader gives no value at all for blank text, which it then refuses as not an object.
    if text.trim().is_empty() {
        return Err("it is not a JSON object".to_owned());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(counts)) => Ok(counts),
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

/// The counts the active generation's file sets that its gate would use, so a write keeps the other kind's value and
/// drops any value the gate itself would ignore: the PowerShell gate's file at `path` read leniently, or, once the
/// cutover flag beside it exists (`<path's folder>\cutover.flag`), `gate.exe`'s `v1\slot-counts.json` beside it read
/// strictly. A flag that cannot be looked for reads as absent.
#[must_use]
pub fn read_slot_count_setting(path: &Path) -> SlotCounts {
    match Generation::detect_in(&counts_folder(path)).unwrap_or(Generation::PowerShell) {
        Generation::PowerShell => kept_counts(&read_counts_file(path), path),
        Generation::V1 => {
            let v1_path = v1_counts_beside(path);
            v1_kept_counts(&v1::read_counts_file(&v1_path), &v1_path)
        }
    }
}

/// The counts in `gate.exe`'s counts file `file`, read from `path`, that it would use.
fn v1_kept_counts(file: &v1::CountsFile, path: &Path) -> SlotCounts {
    let [heavy, light] = v1::pool_counts_of(file, path, 1);
    SlotCounts {
        heavy: heavy.set,
        light: light.set,
    }
}

/// The folder of the counts file at `path`, which holds the cutover flag and the `v1` folder.
fn counts_folder(path: &Path) -> PathBuf {
    path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

/// The counts in `file`, read from `path`, that the gate would use.
pub(crate) fn kept_counts(file: &CountsFile, path: &Path) -> SlotCounts {
    let mut counts = SlotCounts::default();
    for kind in SlotKind::ALL {
        counts.set(kind, pool_count(file, path, kind, 1).set);
    }
    counts
}

/// Writes the counts where both gates read them, as compact JSON without a byte order mark: the PowerShell gate's file
/// at `path` (`{"heavy":4}`) and `gate.exe`'s `v1\slot-counts.json` beside it with `v` first (`{"v":1,"heavy":4}`),
/// making the `v1` folder when it is missing, whether or not the cutover flag exists. The active generation's file is
/// written first: `gate.exe`'s once the cutover flag beside `path` (`<path's folder>\cutover.flag`) exists, else the
/// PowerShell gate's. Each is written beside the file first, then moved over it. Empty counts delete both files, so a
/// machine back at its defaults holds none. A gate reading a file blocks the move and the delete for that moment, so
/// each is tried up to ten times, 100 ms apart.
///
/// # Errors
///
/// [`WriteError`] naming the cutover flag when whether it exists cannot be told, and nothing is written; else naming
/// the file when a folder or the file beside it cannot be written, or the move or delete fails every try. A failure on
/// the first file leaves the second as it was.
pub fn write_slot_counts(path: &Path, counts: &SlotCounts) -> Result<(), WriteError> {
    let folder = counts_folder(path);
    let generation = Generation::detect_in(&folder).map_err(|source| WriteError {
        path: Generation::flag_in(&folder),
        source,
    })?;
    write_both(path, *counts, generation)
}

/// Writes `counts` to the PowerShell gate's file at `path` and to `gate.exe`'s beside it, `generation`'s first.
fn write_both(path: &Path, counts: SlotCounts, generation: Generation) -> Result<(), WriteError> {
    let v1_path = v1_counts_beside(path);
    match generation {
        Generation::PowerShell => {
            write_counts_file(path, counts, powershell_counts_text)?;
            write_counts_file(&v1_path, counts, v1::counts_text)
        }
        Generation::V1 => {
            write_counts_file(&v1_path, counts, v1::counts_text)?;
            write_counts_file(path, counts, powershell_counts_text)
        }
    }
}

/// The PowerShell gate's counts file text, such as `{"heavy":4}`.
fn powershell_counts_text(counts: SlotCounts) -> serde_json::Result<String> {
    serde_json::to_string(&counts)
}

/// Writes `counts` at `path` as `text` gives it, or deletes the file when they are empty.
fn write_counts_file(path: &Path, counts: SlotCounts, text: fn(SlotCounts) -> serde_json::Result<String>) -> Result<(), WriteError> {
    let fail = |source: io::Error| WriteError {
        path: path.to_path_buf(),
        source,
    };
    if counts.is_empty() {
        if !path.exists() {
            return Ok(());
        }
        return with_retry(WRITE_ATTEMPTS, || remove_if_present(path)).map_err(fail);
    }
    let text = text(counts).map_err(|error| fail(io::Error::other(error)))?;
    write_replacing(path, &text).map_err(fail)
}

/// Writes `text` to `path` beside it first, as `<path>.<pid>.<n>.tmp`, then moves that over `path`, so a reader never
/// sees a torn file; `<n>` is a per-process counter, so two threads writing one file never share a temp file. A missing
/// parent folder is made. A reader holding the file blocks the move for that moment, so the move
/// is tried up to ten times, 100 ms apart.
pub(crate) fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    replace_with(path, WRITE_ATTEMPTS, |file| file.write_all(text.as_bytes()))
}

/// [`write_replacing`] with the move over `path` tried once: for a writer on a run's path, which says a failure and
/// goes on at once rather than wait out a reader.
pub(crate) fn write_replacing_once(path: &Path, text: &str) -> io::Result<()> {
    replace_with(path, 1, |file| file.write_all(text.as_bytes()))
}

/// [`write_replacing`] with the temp file written by `stage`, which is handed it open: the temp file is made new, a
/// link or junction standing at its name opened as itself; a name something already stands at is passed over for the
/// next, up to [`TEMP_NAMES`] names, and what stands there is left. A temp file `stage` fails to write is removed, and
/// the move over `path` is tried up to `attempts` times, [`WRITE_RETRY_DELAY`] apart.
fn replace_with(path: &Path, attempts: u32, stage: impl FnOnce(&mut fs::File) -> io::Result<()>) -> io::Result<()> {
    if let Some(folder) = path.parent()
        && !folder.as_os_str().is_empty()
    {
        fs::create_dir_all(folder)?;
    }
    let (temp, mut file) = new_temp(path)?;
    if let Err(error) = stage(&mut file) {
        drop(file);
        return Err(remove_partial(&temp, error));
    }
    drop(file);
    let moved = with_retry(attempts, || fs::rename(&temp, path));
    if moved.is_err() {
        // The write has failed and says so; a temp file this cannot remove stays beside the target, since no later write uses its name.
        let _ = fs::remove_file(&temp);
    }
    moved
}

/// A temp file made new beside `path` ([`temp_path`]) and its name: a name an entry already stands at is passed over,
/// the entry left as it is, up to [`TEMP_NAMES`] names.
fn new_temp(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..TEMP_NAMES {
        let temp = temp_path(path);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, OPEN_REPARSE_POINT);
        match options.open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("something already stands at each of {TEMP_NAMES} temp names beside {}", path.display()),
    ))
}

/// Removes the partial file a failed write left at `temp` and returns `error`, the write's failure; a file that is not
/// there is fine, and one that cannot be removed is added to the error.
pub(crate) fn remove_partial(temp: &Path, error: io::Error) -> io::Error {
    match fs::remove_file(temp) {
        Err(removal) if removal.kind() != io::ErrorKind::NotFound => io::Error::new(
            error.kind(),
            format!("{error}; the partial file {} could not be removed: {removal}", temp.display()),
        ),
        _ => error,
    }
}

/// Runs `operation` until it succeeds, up to `attempts` times, [`WRITE_RETRY_DELAY`] apart, returning the last error
/// when every try fails.
fn with_retry(attempts: u32, mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    let mut failure = io::Error::other("no attempt was made");
    for attempt in 1..=attempts {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) => failure = error,
        }
        if attempt < attempts {
            thread::sleep(WRITE_RETRY_DELAY);
        }
    }
    Err(failure)
}

/// Sets or unsets one kind's count, keeping the other kind's valid value in the active generation's file, and writes the
/// counts to both gates' files, the active one's first ([`write_slot_counts`]). The active file is the PowerShell gate's
/// at `path`, or once the cutover flag beside it exists, `gate.exe`'s `v1\slot-counts.json` beside it, read strictly;
/// the other file is only written. A file that is not valid JSON, not a JSON object, or (for `gate.exe`'s) not a v1
/// counts file keeps nothing and is replaced.
///
/// # Errors
///
/// [`ApplyError::Read`] when whether the cutover flag exists cannot be told, or the active file exists but cannot be
/// read; nothing is then written or deleted. [`ApplyError::Write`] when a file cannot be written.
pub fn apply_slot_count(path: &Path, kind: SlotKind, count: Option<SlotCount>) -> Result<(), ApplyError> {
    let folder = counts_folder(path);
    let generation = Generation::detect_in(&folder).map_err(|source| ApplyError::Read {
        path: Generation::flag_in(&folder),
        source,
    })?;
    let mut counts = match generation {
        Generation::PowerShell => {
            let bytes = read_counts_bytes(path).map_err(|source| ApplyError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            kept_counts(&counts_from_bytes(bytes), path)
        }
        Generation::V1 => {
            let v1_path = v1_counts_beside(path);
            let file = match v1::read_counts_file(&v1_path) {
                Err(v1::FormatError::Io(source)) => return Err(ApplyError::Read { path: v1_path, source }),
                file => file,
            };
            v1_kept_counts(&file, &v1_path)
        }
    };
    counts.set(kind, count);
    write_both(path, counts, generation)?;
    Ok(())
}

/// `<path>.<pid>.<n>.tmp`, `<n>` taken from [`TEMP_COUNTER`] so no two writes of this process share one.
pub(crate) fn temp_path(path: &Path) -> PathBuf {
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
pub(crate) fn prose(text: &str) -> String {
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::{TEMP_COUNTER, TEMP_NAMES, WRITE_ATTEMPTS, replace_with};

    /// The temp name the `n`th write of this process beside `path` takes.
    fn temp_name(path: &Path, n: u64) -> PathBuf {
        PathBuf::from(format!("{}.{}.{n}.tmp", path.display(), std::process::id()))
    }

    #[test]
    fn a_temp_name_something_stands_at_is_passed_over_and_what_stands_there_is_left() {
        let folder = crate::test_support::temp_dir().join(format!("gate-state-replace-{}", std::process::id()));
        fs::create_dir_all(&folder).expect("the folder can be made");
        let path = folder.join("record");
        let planted = temp_name(&path, TEMP_COUNTER.load(Ordering::Relaxed));
        fs::write(&planted, "planted").expect("an entry can be planted");

        replace_with(&path, WRITE_ATTEMPTS, |file| file.write_all(b"written")).expect("the next name is free");

        assert_eq!(fs::read_to_string(&path).expect("the file reads"), "written");
        assert_eq!(fs::read_to_string(&planted).expect("the planted entry stays"), "planted");

        let next = TEMP_COUNTER.load(Ordering::Relaxed);
        let names: Vec<PathBuf> = (next..next + TEMP_NAMES).map(|n| temp_name(&path, n)).collect();
        for name in &names {
            fs::write(name, "planted").expect("an entry can be planted");
        }
        let error = replace_with(&path, WRITE_ATTEMPTS, |file| file.write_all(b"other")).expect_err("no temp name is free");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(fs::read_to_string(&path).expect("the file reads"), "written", "the file stands");
        for name in &names {
            assert_eq!(
                fs::read_to_string(name).expect("the planted entry stays"),
                "planted",
                "{}",
                name.display()
            );
        }
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
}

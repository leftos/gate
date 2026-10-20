//! The reader the one-list declaration files share ([`read_list`]): `.gate/desktop.toml` and `.gate/local.toml`, each
//! a list of `patterns`, and `.gate/env.toml`, a list of `prefixes`. Nothing here reads a file; the caller hands over
//! its bytes.
//!
//! Such a file holds one key, an array of at most a cap of non-empty strings, each checked by its file's own rule; an
//! empty array declares none. Any other key is a problem. The reader stops at the first problem it finds: the count of
//! entries is checked against the cap before any entry, then the entries in file order.

use toml::{Table, Value};

use crate::like::Pattern;
use crate::warm::{parse_failure, spelled};

/// The `-like` pattern the `patterns` entry `text` at `index` gives ([`Pattern`]); `Err` with its problem when it is no
/// valid wildcard pattern. The entry rule of the desktop and local files.
pub(crate) fn pattern_of(index: usize, text: &str) -> Result<Pattern, String> {
    Pattern::parse(text).map_err(|_| format!("patterns[{index}] is not a valid wildcard pattern: {text:?}"))
}

/// The entries the file whose bytes are `bytes` declares under its one key `key`, in file order, a leading byte order
/// mark skipped: each a non-empty string that `entry` turns into an item, given its index, or refuses with its problem.
///
/// # Errors
///
/// The file's first problem, such as `patterns[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `key`, no `key`, a `key` that is not an array, more than `cap` entries, or an entry that is not a string, is empty or
/// is one `entry` refuses.
pub(crate) fn read_list<T>(bytes: &[u8], key: &str, cap: usize, entry: impl Fn(usize, &str) -> Result<T, String>) -> Result<Vec<T>, String> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|error| format!("not valid UTF-8 ({error})"))?;
    let table = text.parse::<Table>().map_err(|error| parse_failure(text, &error))?;
    let mut items = None;
    for (name, value) in table {
        if name != key {
            return Err(format!("{} is not a key ({key})", spelled(&name)));
        }
        items = Some(read_entries(value, key, cap, &entry)?);
    }
    items.ok_or_else(|| format!("declares no {key}"))
}

/// The items the `key` value `value` declares, or its first problem.
fn read_entries<T>(value: Value, key: &str, cap: usize, entry: &impl Fn(usize, &str) -> Result<T, String>) -> Result<Vec<T>, String> {
    let entries = match value {
        Value::Array(entries) => entries,
        other => return Err(format!("{key} is not an array (TOML {})", other.type_str())),
    };
    if entries.len() > cap {
        return Err(format!("declares more than {cap} {key}"));
    }
    entries
        .into_iter()
        .enumerate()
        .map(|(index, value)| match value {
            Value::String(text) if text.is_empty() => Err(format!("{key}[{index}] is empty")),
            Value::String(text) => entry(index, &text),
            other => Err(format!("{key}[{index}] is not a string (TOML {})", other.type_str())),
        })
        .collect()
}

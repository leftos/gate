//! The repo's desktop declaration, `.gate/desktop.toml`, read from its bytes ([`read_desktop`]): the `-like` patterns
//! of the routed commands that need the builder's desktop. Nothing here reads a file; the caller hands over its bytes.
//!
//! The file holds one key, `patterns`, an array of at most [`MOST_PATTERNS`] non-empty strings, each a valid `-like`
//! pattern ([`Pattern`]); an empty array declares none. Any other key is a problem. The reader stops at the first
//! problem it finds: the count of entries is checked against the cap before any entry, then the entries in file order.
//! A declaration the gate cannot read keeps every routed command of the repo here.

use toml::{Table, Value};

use crate::like::Pattern;
use crate::warm::{parse_failure, spelled};

/// The most patterns a desktop file may declare.
pub const MOST_PATTERNS: usize = 32;

/// The problem of a file that has no `patterns` key.
const NO_PATTERNS: &str = "declares no patterns";

/// The patterns the desktop file whose bytes are `bytes` declares, in file order, a leading byte order mark skipped.
///
/// # Errors
///
/// The file's first problem, such as `patterns[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `patterns`, no `patterns`, a `patterns` that is not an array, an entry that is not a string, is empty or is no
/// valid wildcard pattern, or more than [`MOST_PATTERNS`] entries.
pub fn read_desktop(bytes: &[u8]) -> Result<Vec<Pattern>, String> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|error| format!("not valid UTF-8 ({error})"))?;
    let table = text.parse::<Table>().map_err(|error| parse_failure(text, &error))?;
    let mut patterns = None;
    for (key, value) in table {
        if key != "patterns" {
            return Err(format!("{} is not a key (patterns)", spelled(&key)));
        }
        patterns = Some(read_patterns(value)?);
    }
    patterns.ok_or_else(|| NO_PATTERNS.to_owned())
}

/// The patterns the `patterns` value `value` declares, or its first problem.
fn read_patterns(value: Value) -> Result<Vec<Pattern>, String> {
    let entries = match value {
        Value::Array(entries) => entries,
        other => return Err(format!("patterns is not an array (TOML {})", other.type_str())),
    };
    if entries.len() > MOST_PATTERNS {
        return Err(format!("declares more than {MOST_PATTERNS} patterns"));
    }
    entries.into_iter().enumerate().map(|(index, entry)| pattern_of(index, entry)).collect()
}

/// The pattern the entry `entry` at `index` gives; `Err` with its problem when it is not a string, is empty or is no
/// valid wildcard pattern.
fn pattern_of(index: usize, entry: Value) -> Result<Pattern, String> {
    let text = match entry {
        Value::String(text) => text,
        other => return Err(format!("patterns[{index}] is not a string (TOML {})", other.type_str())),
    };
    if text.is_empty() {
        return Err(format!("patterns[{index}] is empty"));
    }
    Pattern::parse(&text).map_err(|_| format!("patterns[{index}] is not a valid wildcard pattern: {text:?}"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::read_desktop;

    fn texts(bytes: &[u8]) -> Vec<String> {
        read_desktop(bytes)
            .expect("the declaration reads")
            .iter()
            .map(|pattern| pattern.as_str().to_owned())
            .collect()
    }

    fn problem(bytes: &[u8]) -> String {
        read_desktop(bytes).expect_err("the declaration has a problem")
    }

    #[test]
    fn the_patterns_read_in_order_and_an_empty_list_declares_none() {
        assert_eq!(
            texts(b"patterns = ['dotnet test *', 'Tool*', '[ab]x']\n"),
            ["dotnet test *", "Tool*", "[ab]x"]
        );
        assert_eq!(texts(b"patterns = []\n"), Vec::<String>::new());
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(texts(b"\xef\xbb\xbfpatterns = ['a*']\n"), ["a*"]);
    }

    #[test]
    fn more_than_thirty_two_patterns_is_a_problem() {
        let list = |count: usize| (0..count).map(|index| format!("'p{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(texts(format!("patterns = [{}]\n", list(32)).as_bytes()).len(), 32);
        assert_eq!(
            problem(format!("patterns = [{}]\n", list(33)).as_bytes()),
            "declares more than 32 patterns"
        );
        assert_eq!(
            problem(format!("patterns = ['', {}]\n", list(32)).as_bytes()),
            "declares more than 32 patterns",
            "the cap is checked before any entry"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_or_utf8_is_a_problem() {
        let why = problem(b"patterns = ['a'\n");
        assert!(why.starts_with("line "), "{why}");
        let why = problem(b"\xef\xbb\xbfpatterns = 'a' x\n");
        assert!(why.starts_with("line 1, column 16: "), "{why}");
        let why = problem(b"patterns = ['\xff']\n");
        assert!(why.starts_with("not valid UTF-8 ("), "{why}");
    }

    #[test]
    fn each_bad_shape_or_entry_is_a_problem_and_the_first_wins() {
        for (text, why) in [
            ("", "declares no patterns"),
            ("# none\n", "declares no patterns"),
            ("patterns = 'a*'\n", "patterns is not an array (TOML string)"),
            ("patterns = ['a*', 3]\n", "patterns[1] is not a string (TOML integer)"),
            ("patterns = ['a*', ['b']]\n", "patterns[1] is not a string (TOML array)"),
            ("patterns = ['', 3]\n", "patterns[0] is empty"),
            ("patterns = ['a[']\n", "patterns[0] is not a valid wildcard pattern: \"a[\""),
            ("other = 1\npatterns = ['a']\n", "other is not a key (patterns)"),
            ("patterns = ['a']\n\"b c\" = 1\n", "\"b c\" is not a key (patterns)"),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
    }
}

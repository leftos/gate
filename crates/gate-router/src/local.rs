//! The repo's local declaration, `.gate/local.toml`, read from its bytes ([`read_local`]): the `-like` patterns of the
//! commands its repo's routing patterns match that must still run on this PC, such as tests that listen on its
//! interfaces. Nothing here reads a file; the caller hands over its bytes.
//!
//! The file holds one key, `patterns`, an array of at most [`MOST_PATTERNS`] non-empty strings, each a valid `-like`
//! pattern ([`Pattern`]); an empty array declares none. Any other key is a problem. The reader stops at the first
//! problem it finds: the count of entries is checked against the cap before any entry, then the entries in file order.
//! A declaration the gate cannot read keeps every routed command of the repo here.

use crate::like::Pattern;
use crate::list::{pattern_of, read_list};

/// The most patterns a local file may declare.
pub const MOST_PATTERNS: usize = 32;

/// The patterns the local file whose bytes are `bytes` declares, in file order, a leading byte order mark skipped.
///
/// # Errors
///
/// The file's first problem, such as `patterns[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `patterns`, no `patterns`, a `patterns` that is not an array, an entry that is not a string, is empty or is no
/// valid wildcard pattern, or more than [`MOST_PATTERNS`] entries.
pub fn read_local(bytes: &[u8]) -> Result<Vec<Pattern>, String> {
    read_list(bytes, "patterns", MOST_PATTERNS, pattern_of)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::read_local;

    fn texts(bytes: &[u8]) -> Vec<String> {
        read_local(bytes)
            .expect("the declaration reads")
            .iter()
            .map(|pattern| pattern.as_str().to_owned())
            .collect()
    }

    fn problem(bytes: &[u8]) -> String {
        read_local(bytes).expect_err("the declaration has a problem")
    }

    #[test]
    fn the_patterns_read_in_order_a_byte_order_mark_skipped() {
        assert_eq!(
            texts(b"\xef\xbb\xbfpatterns = ['dotnet test *Lan*', 'Tool*']\n"),
            ["dotnet test *Lan*", "Tool*"]
        );
        assert_eq!(texts(b"patterns = []\n"), Vec::<String>::new());
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
    fn each_bad_shape_or_entry_is_a_problem_and_the_first_wins() {
        for (text, why) in [
            ("", "declares no patterns"),
            ("patterns = 'a*'\n", "patterns is not an array (TOML string)"),
            ("patterns = ['a*', 3]\n", "patterns[1] is not a string (TOML integer)"),
            ("patterns = ['', 3]\n", "patterns[0] is empty"),
            ("patterns = ['a[']\n", "patterns[0] is not a valid wildcard pattern: \"a[\""),
            ("other = 1\npatterns = ['a']\n", "other is not a key (patterns)"),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
        assert!(problem(b"patterns = ['\xff']\n").starts_with("not valid UTF-8 ("));
        assert!(problem(b"patterns = ['a'\n").starts_with("line "));
    }
}

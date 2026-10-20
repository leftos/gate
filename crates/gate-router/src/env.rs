//! The repo's environment declaration, `.gate/env.toml`, read from its bytes ([`read_env`]): the prefixes of the
//! caller's variables the repo's commands read, which a routed run that does not send them names beside the build
//! variables it watches ([`crate::dropped_env`]). Nothing here reads a file; the caller hands over its bytes.
//!
//! The file holds one key, `prefixes`, an array of at most [`MOST_PREFIXES`] strings, each a name prefix (an exact name
//! is its own prefix): non-empty, of ASCII letters, digits and `_` alone, and not starting `GATE_` in any letter case,
//! since those names are the gate's. An empty array declares none. Any other key is a problem. The reader stops at the
//! first problem it finds: the count of entries is checked against the cap before any entry, then the entries in file
//! order. A declaration the gate cannot read is said ([`problem_line`]) and the run watches the build variables alone.

use crate::decide::fold_lines;
use crate::escape::escaped;
use crate::list::read_list;

/// The most prefixes an environment file may declare.
pub const MOST_PREFIXES: usize = 32;

/// The prefix of the names the gate keeps, which no declared prefix may start with, compared without regard to ASCII
/// case.
const GATE_PREFIX: &str = "GATE_";

/// The prefixes the environment file whose bytes are `bytes` declares, in file order, a leading byte order mark skipped.
///
/// # Errors
///
/// The file's first problem, such as `prefixes[1] is empty`: bytes that are not UTF-8 or not TOML, a key other than
/// `prefixes`, no `prefixes`, a `prefixes` that is not an array, an entry that is not a string, is empty, holds a
/// character other than an ASCII letter, digit or `_`, or starts `GATE_`, or more than [`MOST_PREFIXES`] entries.
pub fn read_env(bytes: &[u8]) -> Result<Vec<String>, String> {
    read_list(bytes, "prefixes", MOST_PREFIXES, prefix_of)
}

/// The line the gate says when the environment file the lines name `file` has the problem `problem`: both shown
/// [`escaped`], the problem's line breaks folded into one space.
#[must_use]
pub fn problem_line(file: &str, problem: &str) -> String {
    format!(
        "gate: {}: {}; only the build variables are watched",
        escaped(file),
        escaped(&fold_lines(problem))
    )
}

/// The prefix the non-empty entry `text` at `index` gives; `Err` with its problem when it holds a character other than
/// an ASCII letter, digit or `_`, or starts `GATE_`.
fn prefix_of(index: usize, text: &str) -> Result<String, String> {
    if !text.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err(format!("prefixes[{index}] is not ASCII letters, digits and _: {text:?}"));
    }
    if text.get(..GATE_PREFIX.len()).is_some_and(|start| start.eq_ignore_ascii_case(GATE_PREFIX)) {
        return Err(format!("prefixes[{index}] starts with GATE_, whose names are the gate's: {text:?}"));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::{problem_line, read_env};

    fn prefixes(bytes: &[u8]) -> Vec<String> {
        read_env(bytes).expect("the declaration reads")
    }

    fn problem(bytes: &[u8]) -> String {
        read_env(bytes).expect_err("the declaration has a problem")
    }

    #[test]
    fn the_prefixes_read_in_order_and_an_empty_list_declares_none() {
        assert_eq!(
            prefixes(b"prefixes = ['COLLIE_', 'ops_tower_lan_tests', 'A1_b']\n"),
            ["COLLIE_", "ops_tower_lan_tests", "A1_b"]
        );
        assert_eq!(prefixes(b"prefixes = []\n"), Vec::<String>::new());
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(prefixes(b"\xef\xbb\xbfprefixes = ['COLLIE_']\n"), ["COLLIE_"]);
    }

    #[test]
    fn more_than_thirty_two_prefixes_is_a_problem() {
        let list = |count: usize| (0..count).map(|index| format!("'P{index}_'")).collect::<Vec<_>>().join(", ");
        assert_eq!(prefixes(format!("prefixes = [{}]\n", list(32)).as_bytes()).len(), 32);
        assert_eq!(
            problem(format!("prefixes = [{}]\n", list(33)).as_bytes()),
            "declares more than 32 prefixes"
        );
        assert_eq!(
            problem(format!("prefixes = ['', {}]\n", list(32)).as_bytes()),
            "declares more than 32 prefixes",
            "the cap is checked before any entry"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_or_utf8_is_a_problem() {
        let why = problem(b"prefixes = ['A'\n");
        assert!(why.starts_with("line "), "{why}");
        let why = problem(b"prefixes = ['\xff']\n");
        assert!(why.starts_with("not valid UTF-8 ("), "{why}");
    }

    #[test]
    fn a_prefix_starting_gate_in_any_letter_case_is_a_problem() {
        for text in ["GATE_", "gate_x", "Gate_State_Dir"] {
            assert_eq!(
                problem(format!("prefixes = ['A_', '{text}']\n").as_bytes()),
                format!("prefixes[1] starts with GATE_, whose names are the gate's: {text:?}")
            );
        }
        assert_eq!(prefixes(b"prefixes = ['GATE', 'GATEX_', 'MY_GATE_']\n"), ["GATE", "GATEX_", "MY_GATE_"]);
    }

    #[test]
    fn each_bad_shape_or_entry_is_a_problem_and_the_first_wins() {
        for (text, why) in [
            ("", "declares no prefixes"),
            ("# none\n", "declares no prefixes"),
            ("prefixes = 'A_'\n", "prefixes is not an array (TOML string)"),
            ("prefixes = ['A_', 3]\n", "prefixes[1] is not a string (TOML integer)"),
            ("prefixes = ['A_', ['B']]\n", "prefixes[1] is not a string (TOML array)"),
            ("prefixes = ['', 3]\n", "prefixes[0] is empty"),
            ("prefixes = ['A-B']\n", "prefixes[0] is not ASCII letters, digits and _: \"A-B\""),
            ("prefixes = ['A B']\n", "prefixes[0] is not ASCII letters, digits and _: \"A B\""),
            ("prefixes = ['A*']\n", "prefixes[0] is not ASCII letters, digits and _: \"A*\""),
            (
                "prefixes = ['\u{e9}T\u{e9}']\n",
                "prefixes[0] is not ASCII letters, digits and _: \"\u{e9}T\u{e9}\"",
            ),
            ("other = 1\nprefixes = ['A']\n", "other is not a key (prefixes)"),
            ("prefixes = ['A']\n\"b c\" = 1\n", "\"b c\" is not a key (prefixes)"),
        ] {
            assert_eq!(problem(text.as_bytes()), why, "{text:?}");
        }
    }

    #[test]
    fn the_problem_line_folds_and_escapes_the_problem() {
        assert_eq!(
            problem_line(".gate/env.toml", "line 1, column 3:\r\n  bad\u{1b}[31m"),
            r"gate: .gate/env.toml: line 1, column 3: bad\u{1b}[31m; only the build variables are watched"
        );
    }
}

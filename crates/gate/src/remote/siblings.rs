//! The repo's siblings declaration, `.gate/siblings.toml`, read as data: the folders beside the worktree root that a
//! routed run carries with it. Nothing here reads a file; the caller hands over its text.
//!
//! The file holds one key, `siblings`, an array of folder names, each a name the builder can lay out
//! ([`is_layout_name`]), and at most [`MOST_SIBLINGS`] of them. A file that does not parse is unreadable whole; a bad
//! entry is skipped alone, with a problem naming it. Unlike the toolchain manifest's, any problem here keeps the run
//! local: a sibling left out would build something different. Whether a name is the worktree's own folder is the
//! routing decision's to tell.

use gate_router::{is_layout_name, parse_failure, spelled};
use gate_wire::MOST_SIBLINGS;
use toml::{Table, Value};

/// The problem of a file that has no `siblings` key.
const NO_SIBLINGS: &str = "declares no siblings";

/// What a `.gate/siblings.toml` declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Siblings {
    /// The folder names that read, in file order.
    pub names: Vec<String>,
    /// One text per problem, in file order, such as `siblings[1] is empty`.
    pub problems: Vec<String>,
}

/// The declaration whose file holds `bytes`; `Err` with the reason when they are not UTF-8 or do not parse.
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<Siblings, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("not valid UTF-8 ({error})"))?;
    read(text)
}

/// The declaration whose text is `text`, a leading byte order mark skipped as the TOML parser skips it; `Err` with the
/// line, column and message of the failure when it is not TOML.
pub(crate) fn read(text: &str) -> Result<Siblings, String> {
    let table = text.parse::<Table>().map_err(|error| parse_failure(text, &error))?;
    let mut siblings = Siblings::default();
    let mut declared = false;
    for (key, value) in table {
        if key == "siblings" {
            declared = true;
            read_names(value, &mut siblings);
        } else {
            siblings.problems.push(format!("{} is not a key (siblings)", spelled(&key)));
        }
    }
    if !declared {
        siblings.problems.push(NO_SIBLINGS.to_owned());
    }
    Ok(siblings)
}

/// Reads the `siblings` value `value` into `siblings`, each entry a name or a problem; at the entry past
/// [`MOST_SIBLINGS`] it says so and reads no further.
fn read_names(value: Value, siblings: &mut Siblings) {
    let entries = match value {
        Value::Array(entries) => entries,
        other => {
            siblings.problems.push(format!("siblings is not an array (TOML {})", other.type_str()));
            return;
        }
    };
    for (index, entry) in entries.into_iter().enumerate() {
        if index == MOST_SIBLINGS {
            siblings.problems.push(format!("siblings names more than {MOST_SIBLINGS} folders"));
            return;
        }
        match name_of(index, entry, &siblings.names) {
            Ok(name) => siblings.names.push(name),
            Err(problem) => siblings.problems.push(problem),
        }
    }
}

/// The name the entry `entry` at `index` gives; `Err` with its problem when it is not a string, is empty, is not a
/// name the builder can lay out or repeats one of `earlier`, ASCII letter case aside.
fn name_of(index: usize, entry: Value, earlier: &[String]) -> Result<String, String> {
    let name = match entry {
        Value::String(name) => name,
        other => return Err(format!("siblings[{index}] is not a string (TOML {})", other.type_str())),
    };
    if name.is_empty() {
        return Err(format!("siblings[{index}] is empty"));
    }
    if !is_layout_name(&name) {
        return Err(format!(
            "siblings[{index}] is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, \
             not a device name: {name:?}"
        ));
    }
    if earlier.iter().any(|before| before.eq_ignore_ascii_case(&name)) {
        return Err(format!("siblings[{index}] repeats an earlier name: {name:?}"));
    }
    Ok(name)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a fixture that does not read must fail loudly")]
mod tests {
    use super::{Siblings, read, read_bytes};

    fn declared(text: &str) -> Siblings {
        read(text).expect("the declaration parses")
    }

    fn owned(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn a_list_reads_in_order() {
        let read = declared("siblings = [\"yaat-server\", \"b.c\", \"D_e\"]\n");
        assert_eq!(read.names, owned(&["yaat-server", "b.c", "D_e"]));
        assert_eq!(read.problems, Vec::<String>::new());
    }

    #[test]
    fn a_file_without_the_siblings_key_declares_no_siblings_and_an_empty_list_is_none() {
        for text in ["", "# nothing yet\n"] {
            assert_eq!(declared(text).problems, ["declares no siblings"], "{text:?}");
        }
        assert_eq!(declared("siblings = []\n"), Siblings::default());
    }

    #[test]
    fn a_bad_name_is_skipped_with_a_problem_naming_it() {
        for bad in [
            "..",
            ".",
            r"a\b",
            "a/b",
            ".hidden",
            "a b",
            "x:",
            "a.",
            "server..",
            "nul",
            "CON",
            "Com1.x",
            "nul.txt",
            "LPT9",
            "aux.tar.gz",
        ] {
            let read = declared(&format!("siblings = ['ok', '{bad}']\n"));
            assert_eq!(read.names, owned(&["ok"]), "{bad:?}");
            assert_eq!(
                read.problems,
                [format!(
                    "siblings[1] is not a folder name of letters, digits, '.', '_' and '-' not starting or ending with a dot, not a \
                     device name: {bad:?}"
                )]
            );
        }
        let read = declared("siblings = ['', 'ok', 'yaat', 'COM10', 'nulx', 'lpt0', 3]\n");
        assert_eq!(
            read.names,
            owned(&["ok", "yaat", "COM10", "nulx", "lpt0"]),
            "the worktree's own folder is the router's"
        );
        assert_eq!(read.problems, ["siblings[0] is empty", "siblings[6] is not a string (TOML integer)"]);
        assert_eq!(
            declared("siblings = 'yaat-server'\n").problems,
            ["siblings is not an array (TOML string)"]
        );
    }

    #[test]
    fn a_name_given_twice_in_any_case_is_a_problem() {
        let read = declared("siblings = ['yaat-server', 'Yaat-Server']\n");
        assert_eq!(read.names, owned(&["yaat-server"]));
        assert_eq!(read.problems, ["siblings[1] repeats an earlier name: \"Yaat-Server\""]);
    }

    #[test]
    fn the_ninth_name_is_a_problem_that_stops_the_reading() {
        let names = |count: usize| (0..count).map(|index| format!("'s{index}'")).collect::<Vec<_>>().join(", ");
        assert_eq!(declared(&format!("siblings = [{}]\n", names(8))).problems, Vec::<String>::new());
        let read = declared(&format!("siblings = [{}, '..', 1]\n", names(8)));
        assert_eq!(read.names.len(), 8);
        assert_eq!(read.problems, ["siblings names more than 8 folders"], "nothing past the ninth is read");
    }

    #[test]
    fn a_file_of_nested_brackets_does_not_overflow_the_stack() {
        let depth = 64 * 1024 / 2 - 8;
        let text = format!("siblings = {}{}\n", "[".repeat(depth), "]".repeat(depth));
        assert!(text.len() <= 64 * 1024);
        match read(&text) {
            Ok(read) => assert_eq!(read.problems, ["siblings[0] is not a string (TOML array)"]),
            Err(why) => assert!(why.starts_with("line 1, column "), "{why}"),
        }
    }

    #[test]
    fn an_unknown_key_is_a_problem() {
        let read = declared("siblings = ['a']\noptional = ['a']\n\"b c\" = 1\n");
        assert_eq!(read.names, owned(&["a"]));
        assert_eq!(read.problems, ["optional is not a key (siblings)", "\"b c\" is not a key (siblings)",]);
    }

    #[test]
    fn a_byte_order_mark_is_skipped() {
        assert_eq!(declared("\u{feff}siblings = ['a']\n").names, owned(&["a"]));
    }

    #[test]
    fn a_file_that_is_not_toml_or_utf8_is_unreadable() {
        let why = read("siblings = [\"a\"\n").expect_err("an unclosed array is no TOML");
        assert!(why.starts_with("line "), "{why}");
        let why = read("\u{feff}siblings = 'a' x\n").expect_err("text after a value is no TOML");
        assert!(why.starts_with("line 1, column 16: "), "{why}");
        let why = read_bytes(b"siblings = ['\xff']\n").expect_err("no UTF-8");
        assert!(why.starts_with("not valid UTF-8 ("), "{why}");
    }
}

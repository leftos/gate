//! The repo's spare declaration, `.gate/spare.toml`: the folders it names and the problems it can have, and which
//! folders below the work folder a list of them spares.

use gate_router::spare::{Spared, problem_line, read_spare};

fn folders(bytes: &[u8]) -> Vec<String> {
    read_spare(bytes).expect("the declaration reads")
}

fn problem(bytes: &[u8]) -> String {
    read_spare(bytes).expect_err("the declaration has a problem")
}

#[test]
fn a_spare_file_lists_its_folders() {
    assert_eq!(
        folders(b"folders = ['.tmp/sweep-logs', 'logs', 'out/*/cache', 'b?n']\n"),
        [".tmp/sweep-logs", "logs", "out/*/cache", "b?n"]
    );
    assert_eq!(folders(b"folders = []\n"), Vec::<String>::new());
    assert_eq!(folders(b"\xef\xbb\xbffolders = ['logs']\n"), ["logs"]);
}

#[test]
fn a_spare_entry_that_is_no_folder_path_is_a_problem() {
    let why = r#"names separated by /, * and ? within a name, no . or .. name, no ** and none of \ : [ ] " < > | `"#;
    for entry in ["..", "a//b", "**", "a\\\\b", "[x]", ".", "a/./b", "/a", "a/", "c:/x", "a/**/b"] {
        let shown = entry.replace("\\\\", "\\");
        assert_eq!(
            problem(format!("folders = ['logs', \"{entry}\"]\n").as_bytes()),
            format!("entry 1 '{shown}' is not a folder path: {why}"),
            "{entry:?}"
        );
    }
    assert_eq!(problem(b"folders = ['']\n"), "folders[0] is empty");
    assert_eq!(problem(b"folders = [3]\n"), "folders[0] is not a string (TOML integer)");
    assert_eq!(problem(b"other = 1\n"), "other is not a key (folders)");
    assert_eq!(problem(b""), "declares no folders");
}

#[test]
fn a_spare_file_past_32_entries_is_a_problem() {
    let list = |count: usize| (0..count).map(|index| format!("'f{index}'")).collect::<Vec<_>>().join(", ");
    assert_eq!(folders(format!("folders = [{}]\n", list(32)).as_bytes()).len(), 32);
    assert_eq!(problem(format!("folders = [{}]\n", list(33)).as_bytes()), "declares more than 32 folders");
}

#[test]
fn a_spare_file_that_is_not_an_array_is_a_problem() {
    assert_eq!(problem(b"folders = 'logs'\n"), "folders is not an array (TOML string)");
    assert_eq!(
        problem_line(".gate/spare.toml", "folders is not an array (TOML string)"),
        "gate: .gate/spare.toml: folders is not an array (TOML string); the box's clear spares no folder"
    );
    assert_eq!(
        problem_line(".gate/spare.toml", "line 1:\r\n  bad\u{1b}[31m"),
        r"gate: .gate/spare.toml: line 1: bad\u{1b}[31m; the box's clear spares no folder"
    );
}

/// Whether `entries` spare the folder whose `/`-separated path below the tree is `path`.
fn spares(entries: &[&str], path: &str) -> bool {
    let entries: Vec<String> = entries.iter().map(|&entry| entry.to_owned()).collect();
    Spared::new(&entries).spares(&path.split('/').collect::<Vec<_>>())
}

#[test]
fn a_spare_entry_matches_a_folder_of_as_many_names_alone() {
    assert!(spares(&[".tmp/sweep-logs"], ".tmp/sweep-logs"));
    assert!(!spares(&[".tmp/sweep-logs"], ".tmp/sweep-logs-old"));
    assert!(!spares(&[".tmp/sweep-logs"], "x/.tmp/sweep-logs"));
    assert!(!spares(&[".tmp/sweep-logs"], ".tmp"));
    assert!(spares(&[".tmp/*"], ".tmp/a"));
    assert!(!spares(&[".tmp/*"], ".tmp"));
    assert!(spares(&["logs"], "logs"));
    assert!(!spares(&["logs"], "src/logs"));
    assert!(spares(&["b?n", "logs"], "bin"));
    assert!(!spares(&[], "logs"));
}

#[test]
fn a_spare_entry_matches_in_any_letter_case() {
    assert!(spares(&["Logs"], "logs"));
    assert!(spares(&["logs"], "LOGS"));
    assert!(spares(&[".TMP/Sweep-*"], ".tmp/sweep-logs"));
}

#[test]
fn a_spare_entry_that_is_no_folder_path_spares_nothing() {
    assert!(!spares(&[".."], ".."));
    assert!(!spares(&["a//b"], "a//b"));
    assert!(spares(&["..", "logs"], "logs"), "the good entries still spare");
}

//! `gate remote add|remove` on the case's own `remote.json`: what it writes, what it backs up, and what it leaves.

use std::fs;
use std::path::{Path, PathBuf};

use crate::support::{Case, Outcome, write_remote_json};

/// A routing file as the rewrite lays it out.
const ROUTING: &str =
    "{\n    \"host\": \"builder@example\",\n    \"repos\": {\n        \"gate\": [\n            \"cargo test*\"\n        ]\n    }\n}\n";

/// The names in the folder of `path`, sorted.
fn beside(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path.parent().expect("the file has a folder"))
        .expect("the folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The one backup beside `path`.
fn the_backup(path: &Path) -> PathBuf {
    let backups: Vec<String> = beside(path).into_iter().filter(|name| name.starts_with("remote.json.bak-")).collect();
    assert_eq!(backups.len(), 1, "one backup: {backups:?}");
    path.with_file_name(&backups[0])
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// Whether `gate check` says a problem of the routing file.
fn check_says_a_routing_problem(case: &Case) -> (bool, Outcome) {
    let outcome = case.run(&["check"]);
    let said = outcome.stderr.lines().any(|line| line.contains("remote.json:"));
    (said, outcome)
}

#[test]
fn remote_cmd_a_malformed_file_is_left_byte_identical_with_no_backup() {
    let case = Case::new("remote_cmd_malformed");
    let malformed = "{\"host\": \"builder@example\", \"repos\": ";
    let path = write_remote_json(&case, malformed);

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo build*"]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let prefix = format!("remote: ignored {}: ", path.display());
    assert!(outcome.stderr.lines().any(|line| line.starts_with(&prefix)), "{prefix}: {outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(fs::read(&path).expect("the file reads"), malformed.as_bytes());
    assert_eq!(beside(&path), ["remote.json"]);
}

#[test]
fn remote_cmd_an_add_backs_up_the_original_and_writes_a_file_the_gate_reads() {
    let case = Case::new("remote_cmd_add");
    let path = write_remote_json(&case, "{\"host\": \"builder@example\", \"repos\": ");
    let (said, outcome) = check_says_a_routing_problem(&case);
    assert!(said, "gate check reads the routing file and says a problem of it: {outcome:?}");
    write_remote_json(&case, ROUTING);

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo build*"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let backup = the_backup(&path);
    let backup_line = format!("remote: backup {}", backup.display());
    let wrote_line = format!("remote: wrote {}", path.display());
    assert_eq!(
        lines(&outcome.stdout),
        ["remote: repos.gate: + cargo build*", backup_line.as_str(), wrote_line.as_str()],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(fs::read(&backup).expect("the backup reads"), ROUTING.as_bytes());
    let written = fs::read_to_string(&path).expect("the file reads");
    assert!(written.contains("\"cargo build*\""), "{written}");
    let (said, outcome) = check_says_a_routing_problem(&case);
    assert!(!said, "the written file has no problem gate check says: {outcome:?}");
}

#[test]
fn remote_cmd_a_pattern_already_there_makes_no_backup_and_no_write() {
    let case = Case::new("remote_cmd_noop");
    let path = write_remote_json(&case, ROUTING);

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo test*"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(lines(&outcome.stdout), ["remote: repos.gate: unchanged (already has cargo test*)"]);
    assert_eq!(fs::read(&path).expect("the file reads"), ROUTING.as_bytes());
    assert_eq!(beside(&path), ["remote.json"]);
}

#[test]
fn remote_cmd_removing_an_absent_pattern_says_absent() {
    let case = Case::new("remote_cmd_absent");
    let path = write_remote_json(&case, ROUTING);

    let outcome = case.run(&["remote", "remove", "repos", "gate", "cargo bench*"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(lines(&outcome.stdout), ["remote: repos.gate: absent (no cargo bench*)"]);
    assert_eq!(fs::read(&path).expect("the file reads"), ROUTING.as_bytes());
    assert_eq!(beside(&path), ["remote.json"]);
}

#[test]
fn remote_cmd_a_dry_run_writes_nothing() {
    let case = Case::new("remote_cmd_dry_run");
    let path = write_remote_json(&case, ROUTING);

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo build*", "--dry-run"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        lines(&outcome.stdout),
        ["remote: repos.gate: + cargo build*", "remote: dry run; nothing written"]
    );
    assert_eq!(fs::read(&path).expect("the file reads"), ROUTING.as_bytes());
    assert_eq!(beside(&path), ["remote.json"]);
}

#[test]
fn remote_cmd_no_routing_file_is_a_failure() {
    let case = Case::new("remote_cmd_missing");
    let path = case.local_app_data().join("gate").join("remote.json");

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo build*"]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(lines(&outcome.stderr), [format!("remote: no remote.json at {}", path.display())]);
    assert_eq!(outcome.stdout, "");
    assert!(!path.exists(), "no file is made");
}

#[test]
fn remote_cmd_a_tag_names_the_backup() {
    let case = Case::new("remote_cmd_tag");
    let path = write_remote_json(&case, ROUTING);

    let outcome = case.run(&["remote", "add", "repos", "gate", "cargo build*", "--tag", "census"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let backup = the_backup(&path);
    let name = backup.file_name().expect("a name").to_string_lossy().into_owned();
    let stamp = name.strip_prefix("remote.json.bak-census-").expect("the backup's name carries the tag");
    let shape = stamp.len() == 16
        && stamp.char_indices().all(|(at, ch)| match at {
            8 => ch == 'T',
            15 => ch == 'Z',
            _ => ch.is_ascii_digit(),
        });
    assert!(shape, "a yyyyMMddTHHmmssZ stamp: {name}");
}

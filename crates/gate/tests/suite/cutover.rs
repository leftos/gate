//! `gate cutover` with no PowerShell gate to wait for: the counts copy, the flag, the lines it says and the words it
//! refuses. Every case writes under its own LOCALAPPDATA, never the machine's.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::{Case, USAGE, finish};

/// `gate cutover` with a short confirmation window, so a drain with nothing to wait for ends soon; no case asserts how
/// long it took.
const CUTOVER: [&str; 3] = ["cutover", "--test-confirm-ms", "50"];

/// The cutover flag under `case`'s LOCALAPPDATA.
fn flag(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("cutover.flag")
}

/// The PowerShell gate's counts file under `case`'s LOCALAPPDATA.
fn powershell_counts(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("slot-counts.json")
}

/// `gate.exe`'s counts file in the default root under `case`'s LOCALAPPDATA.
fn v1_counts(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("v1").join("slot-counts.json")
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("the file has a folder")).expect("the file's folder can be made");
    fs::write(path, text).expect("the file can be written");
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the file can be read")).expect("the file is JSON")
}

/// The three lines a cutover with nothing to wait for says, after any said before them.
fn handed_over(case: &Case) -> String {
    format!(
        "gate: cutover: flag written; new gates now start gate.exe\n\
         gate: cutover: done; every gate now runs gate.exe\n\
         gate: cutover: to go back to the PowerShell gate, delete {}\n",
        flag(case).display()
    )
}

#[test]
fn cutover_with_nothing_to_wait_for_writes_the_flag_and_exits_0() {
    let case = Case::new("cutover-empty");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert!(flag(&case).is_file(), "the flag is written");
    assert!(!v1_counts(&case).exists(), "no PowerShell counts, so no v1 counts");
}

#[test]
fn cutover_copies_the_powershell_slot_counts_when_the_v1_file_is_absent() {
    let case = Case::new("cutover-copy");
    write(&powershell_counts(&case), r#"{"heavy":3}"#);
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case), "a copy says nothing");
    assert_eq!(json_of(&v1_counts(&case)), json!({"v": 1, "heavy": 3}));
    assert_eq!(
        fs::read_to_string(powershell_counts(&case)).expect("the PowerShell counts can be read"),
        r#"{"heavy":3}"#,
        "the PowerShell file is untouched"
    );
}

#[test]
fn a_rerun_keeps_a_v1_counts_file_edited_since() {
    let case = Case::new("cutover-rerun");
    write(&powershell_counts(&case), r#"{"heavy":3,"light":2}"#);
    assert_eq!(case.run(&CUTOVER).code, Some(0));
    let edited = r#"{"v":1,"heavy":5}"#;
    write(&v1_counts(&case), edited);
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert_eq!(fs::read_to_string(v1_counts(&case)).expect("the v1 counts can be read"), edited);
    assert!(flag(&case).is_file());
}

#[test]
fn powershell_counts_gate_state_rejects_are_ignored_with_a_line() {
    let case = Case::new("cutover-ignored");
    write(&powershell_counts(&case), "{heavy: 4}");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let (first, rest) = outcome.stderr.split_once('\n').expect("more than one line");
    let prefix = format!("gate: cutover: ignored {}: ", powershell_counts(&case).display());
    assert!(first.starts_with(&prefix), "{first}");
    assert!(first.ends_with("; gates use the default counts"), "{first}");
    assert_eq!(rest, handed_over(&case));
    assert!(!v1_counts(&case).exists(), "nothing is copied");
    assert!(flag(&case).is_file());
}

#[test]
fn cutover_refuses_an_extra_word_and_state_dir_and_writes_nothing() {
    let case = Case::new("cutover-words");
    for (words, problems) in [
        (&["cutover", "now"][..], "gate: unexpected argument now\n"),
        (
            &["cutover", "--state-dir", "elsewhere"][..],
            "gate: unknown option --state-dir\ngate: unexpected argument elsewhere\n",
        ),
    ] {
        let outcome = case.run(words);
        assert_eq!(outcome.code, Some(2), "{words:?}: {outcome:?}");
        assert_eq!(outcome.stderr, format!("{problems}{USAGE}"), "{words:?}");
        assert!(!case.local_app_data().exists(), "{words:?} writes nothing");
    }
}

#[test]
fn cutover_ignores_gate_state_dir() {
    let case = Case::new("cutover-state-dir");
    write(&powershell_counts(&case), r#"{"light":2}"#);
    let mut command = case.gate(&CUTOVER);
    command.env("GATE_STATE_DIR", case.root());
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert!(flag(&case).is_file());
    assert_eq!(json_of(&v1_counts(&case)), json!({"v": 1, "light": 2}));
    assert_eq!(case.root_entries(), Vec::<String>::new(), "nothing lands in GATE_STATE_DIR");
}

#[test]
fn cutover_without_local_app_data_exits_2() {
    let case = Case::new("cutover-no-appdata");
    for value in [None, Some("")] {
        let mut command = case.gate(&["cutover"]);
        match value {
            None => command.env_remove("LOCALAPPDATA"),
            Some(value) => command.env("LOCALAPPDATA", value),
        };
        let outcome = finish(command);
        assert_eq!(outcome.code, Some(2), "{value:?}: {outcome:?}");
        assert_eq!(outcome.stderr, "gate: cutover needs LOCALAPPDATA\n", "{value:?}");
        assert!(!case.local_app_data().exists());
    }
}

#[test]
fn cutover_with_a_file_in_place_of_the_gate_folder_exits_1_without_a_flag() {
    let case = Case::new("cutover-unwritable");
    write(&case.local_app_data().join("gate"), "not a folder");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: cutover: cannot write the cutover flag: "),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.stderr.lines().count(), 1, "{}", outcome.stderr);
    assert!(case.local_app_data().join("gate").is_file(), "the file is left as it was");
}

#[test]
fn cutover_prints_its_request() {
    let case = Case::new("cutover-request");
    let outcome = case.run(&[
        "cutover",
        "--test-poll-ms",
        "20",
        "--test-heartbeat-ms=40",
        "--test-confirm-ms",
        "60",
        "--test-cutover-interrupt-after-list",
        "--test-print-request",
    ]);
    assert_eq!(
        outcome.request(),
        json!({
            "mode": "cutover",
            "localAppData": case.local_app_data().to_string_lossy(),
            "testSeams": {"pollMs": 20, "heartbeatMs": 40, "confirmMs": 60, "interruptAfterList": true},
        })
    );
    assert_eq!(
        case.run(&["cutover", "--test-print-request"]).request()["testSeams"],
        json!({"pollMs": null, "heartbeatMs": null, "confirmMs": null, "interruptAfterList": false})
    );
    assert!(!case.local_app_data().exists(), "printing the request writes nothing");
}

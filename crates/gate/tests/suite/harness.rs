//! The harness itself: each case's own root, the cleared gate environment, and the helper program.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, GATE, Outcome, finish, print_run, shown};

#[test]
fn each_case_gets_its_own_root() {
    let first = Case::new("each_case_gets_its_own_root");
    let second = Case::new("each_case_gets_its_own_root");
    let first_root = first.root();

    assert_ne!(first_root, second.root());
    for case in [&first, &second] {
        assert!(case.root().is_dir(), "the root exists while the case runs");
        assert!(
            case.root().starts_with(env!("CARGO_TARGET_TMPDIR")),
            "the root is under the target directory"
        );
    }
    let first_request = first.run(&print_run(&["cmd"])).request();
    let second_request = second.run(&print_run(&["cmd"])).request();
    assert_ne!(first_request["rootHash"], second_request["rootHash"]);

    drop(first);
    assert!(!first_root.exists(), "the root is removed when the case ends");
}

#[test]
fn case_clears_inherited_gate_environment() {
    let case = Case::new("case_clears_inherited_gate_environment");
    let other = Case::new("case_clears_inherited_gate_environment_other");
    let mut command = Command::new(GATE);
    command.env("GATE_STATE_DIR", other.root()).env("GATE_SLOT_HELD", "heavy:1:2");
    case.prepare(&mut command, &print_run(&["cmd"]));

    let removed: Vec<&OsStr> = command.get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name).collect();
    assert!(removed.contains(&OsStr::new("GATE_STATE_DIR")), "{removed:?}");
    assert!(removed.contains(&OsStr::new("GATE_SLOT_HELD")), "{removed:?}");
    assert_eq!(finish(command).request()["stateDir"], shown(&case.root()));

    let mut bare = Command::new(GATE);
    bare.env("GATE_STATE_DIR", other.root());
    case.prepare_without_state_dir(&mut bare, &["stop-tree", "--test-print-request", "1"]);
    let request = finish(bare).request();
    assert_eq!(
        request["stateDir"],
        shown(&case.local_app_data().join("gate").join("v1")),
        "the inherited GATE_STATE_DIR never reaches the gate, which resolves the case's default root"
    );
    assert_eq!(request["defaultRoot"], true);
    assert_eq!(request["rootHash"], Value::Null);
}

fn fake_tool(args: &[&str]) -> Outcome {
    let mut command = Command::new(FAKE_TOOL);
    command.current_dir(Path::new(env!("CARGO_TARGET_TMPDIR"))).args(args);
    finish(command)
}

#[test]
fn fake_tool_runs_its_steps_in_order() {
    let outcome = fake_tool(&["out", "hi", "err", "oops", "out", "there", "exit", "3"]);

    assert_eq!(outcome.code, Some(3));
    assert_eq!(outcome.stdout, "hi\nthere\n");
    assert_eq!(outcome.stderr, "oops\n");

    let outcome = fake_tool(&["out", "first", "argv", "out", "x", "exit", "3"]);

    assert_eq!(outcome.code, Some(0), "argv is the last step");
    assert_eq!(outcome.stdout, "first\nout\nx\nexit\n3\n");
}

#[test]
fn fake_tool_argv_prints_its_words_unchanged() {
    let outcome = fake_tool(&["argv", "-p:Name=Value", "a b", "", "--"]);

    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, "-p:Name=Value\na b\n\n--\n");
    assert_eq!(outcome.stderr, "");
}

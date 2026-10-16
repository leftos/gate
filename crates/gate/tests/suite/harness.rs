//! The harness itself: each case's own root, the cleared gate environment, and the helper programs.

use std::ffi::OsStr;
use std::io::Read;
use std::os::windows::io::AsHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use gate_desktop::HelperExit;
use serde_json::Value;

use crate::support::{Case, FAKE_DESKTOP_HELPER, FAKE_TOOL, GATE, Outcome, finish, print_run, shown};

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

/// `BELOW_NORMAL_PRIORITY_CLASS`: what the steps case gives its tool, so its `priority` line is a class the case set,
/// not the one the machine happened to hand down.
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

#[test]
fn fake_tool_prints_its_session_priority_and_cwd() {
    let case = Case::new("fake_tool_prints_its_session_priority_and_cwd");
    let folder = case.folder().join("there");
    std::fs::create_dir_all(&folder).expect("the case's folder can be made");

    let mut command = Command::new(FAKE_TOOL);
    command
        .current_dir(&folder)
        .args(["session", "priority", "cwd"])
        .stdout(Stdio::piped())
        .creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
    let mut child = command.spawn().expect("the tool starts");
    let mut printed = String::new();
    child
        .stdout
        .take()
        .expect("the tool's output is piped")
        .read_to_string(&mut printed)
        .expect("the tool's output is read");
    let session = gate_win32::session_of(child.as_handle()).expect("the tool's session is read");
    let status = child.wait().expect("the tool is waited on");

    assert_eq!(status.code(), Some(0), "{printed}");
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines.len(), 3, "one line for each of the three steps: {printed}");
    assert_eq!(lines[0], format!("session {session}"), "the session line names the tool's own session");
    assert_eq!(
        lines[1],
        format!("priority 0x{BELOW_NORMAL_PRIORITY_CLASS:x}"),
        "the priority line names the class the case gave the tool"
    );
    let cwd = lines[2].strip_prefix("cwd ").expect("the cwd line names a folder");
    assert!(
        cwd.eq_ignore_ascii_case(&shown(&folder)),
        "the cwd line names the folder the tool ran in: {cwd:?} is not {}",
        shown(&folder)
    );
}

#[test]
fn the_fake_desktop_helper_needs_an_argument() {
    let outcome = finish(Command::new(FAKE_DESKTOP_HELPER));

    assert_eq!(
        outcome.code,
        Some(i32::try_from(HelperExit::BadArgument.code()).expect("an exit code fits")),
        "the stand-in ends as the helper does with no argument: {outcome:?}"
    );
}

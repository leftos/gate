//! The box's `gate box desktop-hold`, run as the builder runs it over ssh: it holds the case's own desktop lock
//! (`--test-box-desktop-mutex`) with its holder record under a work root in the case's folder, until its input closes or
//! its ceiling passes. And `gate desktop hold` on the PC, driven through the stand-in ssh to that box on a builder root
//! in the case's folder, as `gate drain` reaches its builder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gate_wire::stamp::unstamped;
use serde_json::Value;

use crate::routing::CONTROL_C_EXIT;
use crate::support::{
    Case, FAKE_SSH, GATE, GATE_VARIABLE, HeldDesktop, KeptDesktop, Outcome, STEPS_VARIABLE, calls, desktop_unique, finish, shown, ssh_steps, start,
    start_with_stdin, wait_until, write_remote_json,
};

/// The holder every hold of a case names.
const HOLDER: &str = "builder-desktop MCP";
/// What the box's held line starts with.
const HELD: &str = "gate: holding the builder's desktop as builder-desktop MCP (ceiling ";
/// The box's line for a hold its input's end let go.
const RELEASED: &str = "gate: desktop hold released";
/// The box's line for a lock taken from a holder that ended without letting it go.
const ABANDONED: &str = "gate: remote took the builder's desktop from a run that ended without releasing it";

/// The case's work root, `builder\work` under its folder, made.
fn work_root(case: &Case) -> PathBuf {
    let root = case.folder().join("builder").join("work");
    fs::create_dir_all(&root).expect("the work root can be made");
    root
}

/// The holder record under `root`.
fn record_in(root: &Path) -> PathBuf {
    root.join("desktop-holder.json")
}

/// `gate box desktop-hold` of the case's own desktop lock under `root` by [`HOLDER`], with `seams` before the verb and
/// `options` after the holder; not started.
fn box_hold(case: &Case, root: &Path, options: &[&str], seams: &[&str]) -> Command {
    let (unique, root) = (desktop_unique(case), shown(root));
    let mut words = vec!["box", "--test-box-desktop-mutex", &unique];
    words.extend_from_slice(seams);
    words.extend_from_slice(&["desktop-hold", "--root", &root, "--holder", HOLDER]);
    words.extend_from_slice(options);
    case.gate_without_state_dir(&words)
}

/// The lines of `said` without their stamps.
fn texts(said: &str) -> Vec<&str> {
    said.lines().map(|line| unstamped(line).text).collect()
}

/// Whether `outcome` said `line` on stderr.
fn said_err(outcome: &Outcome, line: &str) -> bool {
    texts(&outcome.stderr).contains(&line)
}

/// The line of a hold whose input ended before it took the lock.
const ENDED_BEFORE: &str = "gate: desktop hold ended before it held the desktop";

/// The case's desktop lock taken now, with no wait, and let go: whether it came back abandoned, a holder having ended
/// holding it; `None` while another holds it. The case keeps the lock in being across its holders ([`KeptDesktop`]).
fn lock_taken_now(case: &Case) -> Option<bool> {
    let sid = gate_win32::current_user_sid().expect("the suite's user is read");
    let name = gate_win32::PrivateName::for_user(&sid, &desktop_unique(case)).expect("the case's desktop lock has a name");
    let mutex = gate_win32::PrivateMutex::open(&name).expect("the case's desktop lock opens");
    mutex.acquire(Duration::ZERO).ok().map(|guard| guard.abandoned())
}

/// The case's desktop lock is free, and was let go, not left by a holder that ended holding it.
fn lock_is_free(case: &Case) {
    assert_eq!(lock_taken_now(case), Some(false), "the lock is free and was let go");
}

#[test]
fn a_desktop_hold_releases_the_lock_and_removes_its_record_when_its_input_closes() {
    let case = Case::new("desktop_hold_released");
    let root = work_root(&case);
    let record = record_in(&root);
    // Open across the hold, so a lock it left held would still be there to come back abandoned.
    let _kept = KeptDesktop::open(&case);

    let (running, input) = start_with_stdin(box_hold(&case, &root, &["--ceiling", "600"], &[]));
    running.wait_for_stdout(HELD);
    let text = fs::read_to_string(&record).expect("the hold keeps its holder record");
    let held: Value = serde_json::from_str(&text).expect("the record is JSON");
    drop(input);
    let outcome = running.finish();

    assert_eq!(held["repo"], HOLDER, "the record names the holder: {text}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        texts(&outcome.stdout).contains(&"gate: holding the builder's desktop as builder-desktop MCP (ceiling 600 s)"),
        "{outcome:?}"
    );
    assert!(said_err(&outcome, RELEASED), "{outcome:?}");
    assert!(!record.exists(), "the hold removed its record: {outcome:?}");
    lock_is_free(&case);
}

#[test]
fn a_desktop_hold_ends_at_its_ceiling_with_124_and_frees_the_lock() {
    let case = Case::new("desktop_hold_ceiling");
    let root = work_root(&case);
    let _kept = KeptDesktop::open(&case);

    let (running, input) = start_with_stdin(box_hold(&case, &root, &["--ceiling", "1"], &[]));
    let outcome = running.finish();
    drop(input);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(said_err(&outcome, "gate: desktop hold ended at its ceiling of 1 s"), "{outcome:?}");
    assert!(!record_in(&root).exists(), "the hold removed its record: {outcome:?}");
    lock_is_free(&case);
}

#[test]
fn a_desktop_hold_behind_a_held_desktop_is_refused_75_after_its_wait() {
    let case = Case::new("desktop_hold_busy");
    let root = work_root(&case);
    let held = HeldDesktop::hold(&case);

    // Its input stays open, so only the wait ends the hold.
    let (running, input) = start_with_stdin(box_hold(
        &case,
        &root,
        &["--ceiling", "600", "--wait", "1"],
        &["--test-box-desktop-ms", "200"],
    ));
    let outcome = running.finish();
    drop(input);
    drop(held);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(
        said_err(&outcome, "gate: desktop hold: the builder's desktop stayed busy for 1 s"),
        "{outcome:?}"
    );
    assert!(
        !texts(&outcome.stdout).iter().any(|line| line.starts_with(HELD)),
        "never held: {outcome:?}"
    );
    assert!(!record_in(&root).exists(), "no record without the lock: {outcome:?}");
}

#[test]
fn a_killed_desktop_hold_leaves_an_abandoned_lock_the_next_run_takes() {
    let case = Case::new("desktop_hold_killed");
    let root = work_root(&case);
    // Open across both holds, so the lock the killed hold held is still there for the next to find abandoned.
    let _kept = KeptDesktop::open(&case);

    let (first, _first_input) = start_with_stdin(box_hold(&case, &root, &["--ceiling", "600"], &[]));
    first.wait_for_stdout(HELD);
    let killed = first.kill();
    let (next, input) = start_with_stdin(box_hold(&case, &root, &["--ceiling", "600"], &[]));
    next.wait_for_stdout(HELD);
    drop(input);
    let outcome = next.finish();

    assert_eq!(outcome.code, Some(0), "the next hold took the lock: {killed:?} {outcome:?}");
    assert!(said_err(&outcome, ABANDONED), "the abandoned lock is said: {outcome:?}");
    assert!(said_err(&outcome, RELEASED), "{outcome:?}");
    lock_is_free(&case);
}

#[test]
fn a_desktop_hold_whose_input_ends_while_it_waits_ends_without_taking_the_desktop() {
    let case = Case::new("desktop_hold_input_ends_waiting");
    let root = work_root(&case);
    let _kept = KeptDesktop::open(&case);
    let held = HeldDesktop::hold(&case);

    let (running, input) = start_with_stdin(box_hold(
        &case,
        &root,
        &["--ceiling", "600", "--wait", "600"],
        &["--test-box-desktop-ms", "200"],
    ));
    running.wait_for_stderr("gate: remote waiting for the builder's desktop: ");
    drop(input);
    let outcome = running.finish();
    drop(held);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(said_err(&outcome, ENDED_BEFORE), "{outcome:?}");
    assert!(
        !texts(&outcome.stdout).iter().any(|line| line.starts_with(HELD)),
        "never held: {outcome:?}"
    );
    assert!(!record_in(&root).exists(), "no record without the lock: {outcome:?}");
    lock_is_free(&case);
}

/// The routing file of a PC case: `builder-1` its host, and the macOS builder `mac-1`.
const ROUTING: &str = r#"{"host": "builder-1", "repos": {"app": ["*"]},
    "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/b/gate-remote", "repos": {"app": ["*"]}}]}"#;
/// The steps the stand-in answers a PC case's calls with.
const ANSWERS: [&str; 4] = ["ok"; 4];

/// A PC case: its routing file, a folder for the stand-in's files, and the builder's work root, made.
fn pc_case(name: &str) -> (Case, PathBuf) {
    let case = Case::new(name);
    write_remote_json(&case, ROUTING);
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    let root = work_root(&case);
    (case, root)
}

/// `gate desktop hold` of `builder` by [`HOLDER`] for a ceiling of 600 s with `extra` after its seams, through the
/// stand-in of the steps file `steps`, to the case's builder root, its box holding the case's own desktop lock.
fn gate_hold(case: &Case, builder: &str, extra: &[&str], steps: &Path) -> Command {
    let transport = format!("local:{}", case.folder().join("builder").display());
    let mut args = vec![
        "desktop",
        "hold",
        "--builder",
        builder,
        "--holder",
        HOLDER,
        "--ceiling",
        "600",
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        FAKE_SSH,
    ];
    args.extend_from_slice(extra);
    let mut command = case.gate(&args);
    command
        .env(STEPS_VARIABLE, steps)
        .env(GATE_VARIABLE, GATE)
        .env("GATE_FAKE_SSH_BOX_SEAMS", format!("--test-box-desktop-mutex {}", desktop_unique(case)));
    command
}

#[test]
fn gate_desktop_hold_holds_through_the_builder_until_ctrl_c_and_then_frees_the_lock() {
    let (case, root) = pc_case("desktop_hold_pc_ctrl_c");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);
    let _kept = KeptDesktop::open(&case);

    let outcome = finish(gate_hold(&case, "builder-1", &["--test-desktop-hold-interrupt-after-held"], &steps));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(
        outcome
            .stdout
            .lines()
            .any(|line| line == "gate: holding the builder's desktop as builder-desktop MCP (ceiling 600 s)"),
        "the box's held line is passed on: {outcome:?}"
    );
    assert!(
        outcome.stderr.lines().any(|line| line == RELEASED),
        "the box let the desktop go: {outcome:?}"
    );
    assert_eq!(calls(&steps), ["desktop-hold"], "one call: {outcome:?}");
    assert!(!record_in(&root).exists(), "the hold removed its record: {outcome:?}");
    lock_is_free(&case);
}

#[test]
fn gate_desktop_hold_writes_its_record_where_this_pc_s_locked_runs_look() {
    let (case, root) = pc_case("desktop_hold_pc_record");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let _kept = KeptDesktop::open(&case);

    let running = start(gate_hold(&case, "builder-1", &[], &steps));
    running.wait_for_stdout(HELD);
    let text = fs::read_to_string(record_in(&root)).expect("the hold keeps its holder record in the work root");
    let ended = running.kill();
    // The gate's kill-on-close job ends ssh and the box with it, which leaves the lock abandoned.
    let mut came_back = None;
    wait_until("the killed hold's lock coming back", || {
        came_back = lock_taken_now(&case);
        came_back.is_some()
    });

    let held: Value = serde_json::from_str(&text).expect("the record is JSON");
    assert_eq!(held["repo"], HOLDER, "the record names the holder: {text} {ended:?}");
    assert_eq!(came_back, Some(true), "the box ended holding the lock: {ended:?}");
}

#[test]
fn gate_desktop_hold_s_ctrl_c_exits_control_c_even_when_ssh_ended_first() {
    let (case, _) = pc_case("desktop_hold_pc_ctrl_c_ssh_ended");
    // The held line comes, which raises the interrupt, and the call then ends as a dropped connection does.
    let dropped = "out-exit 255 gate: holding the builder's desktop as builder-desktop MCP (ceiling 600 s)";
    let steps = ssh_steps(&case, "ssh", &[dropped]);

    let outcome = finish(gate_hold(&case, "builder-1", &["--test-desktop-hold-interrupt-after-held"], &steps));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(!outcome.stderr.contains("could not be reached"), "{outcome:?}");
}

#[test]
fn gate_desktop_hold_says_a_box_failure_with_no_gate_line() {
    let (case, _) = pc_case("desktop_hold_pc_plain_failure");
    let steps = ssh_steps(&case, "ssh", &["say-exit 1 the box broke"]);

    let outcome = finish(gate_hold(&case, "builder-1", &[], &steps));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate desktop hold: the builder could not be reached: the box broke\n",
        "{outcome:?}"
    );
}

#[test]
fn gate_desktop_hold_refuses_a_builder_remote_json_does_not_list() {
    let (case, _) = pc_case("desktop_hold_pc_unlisted");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = finish(gate_hold(&case, "builder-3", &[], &steps));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate desktop hold: builder-3 is not a builder remote.json lists\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn gate_desktop_hold_refuses_a_macos_builder() {
    let (case, _) = pc_case("desktop_hold_pc_macos");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = finish(gate_hold(&case, "mac-1", &[], &steps));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate desktop hold: mac-1 is a macOS builder, which has no desktop\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn gate_desktop_hold_says_run_gate_push_to_a_box_without_the_verb() {
    let (case, _) = pc_case("desktop_hold_pc_too_old");
    let usage = "say-exit 2 usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs|drain --stdin";
    let steps = ssh_steps(&case, "ssh", &[usage]);

    let outcome = finish(gate_hold(&case, "builder-1", &[], &steps));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate desktop hold: builder-1 runs a gate without desktop-hold; run gate push\n",
        "{outcome:?}"
    );
}

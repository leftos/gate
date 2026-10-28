//! The builder's own slot pool: a routed run's preflight takes a slot for its drain session and its run lets it go, and
//! `gate box hold` holds one while its command runs, all under a root in the case's folder whose `slots.json` the case
//! writes, every session judged from the fixed clock `--test-box-now`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use proc_table::{ProcessTable, Win32ProcessTable};
use serde_json::{Value, json};

use super::{ReadBack, bare_box_command, box_call, let_go, payload, refused_line, renamed, snapshot};
use crate::support::{Case, FAKE_TOOL, Outcome, UNTIL_KILLED, box_payload, build_version, shown, start_with_stdin, wait_until};

/// The fixed time every call of a case starts from, in Unix seconds.
const T: u64 = 1_800_000_000;
/// The holder text of every hold of the suite.
const HOLDER: &str = "github-actions repo ci/test run 7";

/// The case's builder root, `remote` under its folder.
fn root_of(case: &Case) -> PathBuf {
    case.folder().join("remote")
}

/// The payload of a run of `fake-tool out ran` in the work folder `k`, its root made and its snapshot pushed by a
/// preflight with no session, which takes no slot.
fn rooted(case: &Case) -> Value {
    let mut base = box_payload(case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(case, &mut base, &[("a.txt", "a\n")]);
    base
}

/// Writes the root's count file giving `slots` slots.
fn counted(root: &Path, slots: u32) {
    fs::write(root.join("slots.json"), json!({ "v": 1, "slots": slots }).to_string()).expect("the count file can be written");
}

/// `payload` naming the drain session `id`.
fn sessioned(payload: &Value, id: &str) -> Value {
    let mut sessioned = payload.clone();
    sessioned["Session"] = json!(id);
    sessioned
}

/// `gate box <verb>` of `payload` at the Unix second `now` with `seams` before the verb, to its end.
fn verb_at(case: &Case, verb: &str, payload: &Value, now: u64, seams: &[&str]) -> Outcome {
    let now = now.to_string();
    let mut args = vec!["box", "--test-box-now", &now, "--test-box-drain-readable-only", "true"];
    args.extend_from_slice(seams);
    args.push(verb);
    box_call(case, &args, &self::payload(payload)).finish()
}

/// The JSON of slot `n`'s claim under `root`; `None` when it has none.
fn claim(root: &Path, n: usize) -> Option<Value> {
    let text = fs::read_to_string(root.join("slots").join(format!("{n}.json"))).ok()?;
    Some(serde_json::from_str(&text).expect("a claim is JSON"))
}

/// The session slot `n`'s claim under `root` names.
fn claimed_by(root: &Path, n: usize) -> Option<String> {
    claim(root, n).and_then(|claim| claim["session"].as_str().map(str::to_owned))
}

/// `gate box hold` under `root` at the Unix second `now`, with `seams` before the verb, `options` after the holder
/// and its ceiling of `ceiling` s, then `--` and `command`; not started.
fn hold(case: &Case, root: &Path, now: u64, seams: &[&str], options: &[&str], command: &[&str]) -> Command {
    let (stop, now, root) = (shown(&case.folder()), now.to_string(), shown(root));
    let mut words = vec![
        "box",
        "--test-box-root-walk-stop",
        &stop,
        "--test-box-now",
        &now,
        "--test-box-drain-readable-only",
        "true",
    ];
    words.extend_from_slice(seams);
    words.extend_from_slice(&["hold", "--root", &root]);
    words.extend_from_slice(options);
    words.push("--");
    words.extend_from_slice(command);
    bare_box_command(case, &words)
}

/// The usual options of a hold: the suite's holder and a ceiling of 600 s.
const OPTIONS: [&str; 4] = ["--holder", HOLDER, "--ceiling", "600"];

/// A hold run to its end.
fn held(command: Command) -> Outcome {
    let (running, stdin) = start_with_stdin(command);
    drop(stdin);
    running.finish_read()
}

/// What a refused preflight says first.
fn version_line(case: &Case) -> String {
    format!("gate: remote version {}\n", build_version(case))
}

#[test]
fn with_no_count_file_a_preflight_and_its_run_take_no_slot() {
    let case = Case::new("with_no_count_file_a_preflight_and_its_run_take_no_slot");
    let root = root_of(&case);
    let base = sessioned(&rooted(&case), "s1");

    let preflight = verb_at(&case, "preflight", &base, T, &[]);
    let run = verb_at(&case, "run", &base, T + 1, &[]);

    assert_eq!(preflight.code, Some(0), "{preflight:?}");
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(!root.join("slots").exists(), "no slot folder is made: {preflight:?}");
}

#[test]
fn a_full_builder_refuses_a_second_session_until_the_first_run_ends() {
    let case = Case::new("a_full_builder_refuses_a_second_session_until_the_first_run_ends");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &[]);
    let refused = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 1, &[]);
    let claim_while_held = claim(&root, 0);
    let run = verb_at(&case, "run", &sessioned(&base, "s1"), T + 2, &[]);
    let after_run = claim(&root, 0);
    let second = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 3, &[]);

    assert_eq!(first.code, Some(0), "{first:?}");
    let held = claim_while_held.expect("the first session holds slot 0");
    assert_eq!(held["kind"], json!("session"), "{held}");
    assert_eq!(held["session"], json!("s1"), "{held}");
    assert_eq!(refused.code, Some(75), "{refused:?}");
    assert_eq!(refused.stdout, version_line(&case), "{refused:?}");
    assert_eq!(
        refused.stderr,
        format!("{}\ngate: remote builder is full: 1 of 1 slots held\n", refused_line(&base, "full")),
        "{refused:?}"
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(after_run, None, "the run's end lets its slot go");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s2"));
}

#[test]
fn a_session_s_slot_is_free_once_its_lease_lapses() {
    let case = Case::new("a_session_s_slot_is_free_once_its_lease_lapses");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let lease = ["--test-box-drain-lease-ms", "500"];

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &lease);
    let second = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 1, &lease);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "s1's lease lapsed: {second:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s2"));
}

#[test]
fn a_run_whose_lapsed_slot_another_session_took_is_refused_full() {
    let case = Case::new("a_run_whose_lapsed_slot_another_session_took_is_refused_full");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let lease = ["--test-box-drain-lease-ms", "500"];

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &lease);
    let second = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 1, &lease);
    let run = verb_at(&case, "run", &sessioned(&base, "s1"), T + 2, &[]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(second.code, Some(0), "s1's lease lapsed: {second:?}");
    assert_eq!(run.code, Some(75), "{run:?}");
    assert!(
        run.stderr.ends_with(&format!(
            "{}\ngate: remote builder is full: 1 of 1 slots held\n",
            refused_line(&base, "full")
        )),
        "{run:?}"
    );
    assert!(!run.stdout.contains("ran"), "the command never ran: {run:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s2"), "s2 keeps its slot");
}

#[test]
fn a_preflight_refused_full_leaves_no_work_tree_and_no_bare_repository() {
    let case = Case::new("a_preflight_refused_full_leaves_no_work_tree_and_no_bare_repository");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let cold = sessioned(&box_payload(&case, "k2", &[FAKE_TOOL, "out", "ran"]), "s2");

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &[]);
    let refused = verb_at(&case, "preflight", &cold, T + 1, &[]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(refused.code, Some(75), "{refused:?}");
    assert_eq!(refused.stdout, version_line(&case), "{refused:?}");
    assert!(!root.join("work").join("k2").exists(), "no work tree is made: {refused:?}");
    assert!(!root.join("k2.git").exists(), "no bare repository is made: {refused:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s1"), "s1 keeps its slot");
}

#[test]
fn a_preflight_that_fails_after_its_slot_take_leaves_no_claim() {
    let case = Case::new("a_preflight_that_fails_after_its_slot_take_leaves_no_claim");
    let root = root_of(&case);
    rooted(&case);
    counted(&root, 1);
    let broken = sessioned(&box_payload(&case, "k2", &[FAKE_TOOL, "out", "ran"]), "s1");
    // A `.git` folder holding only an empty `HEAD` is no repository, so the preflight's first git step on it fails.
    let git_folder = root.join("work").join("k2").join(".git");
    fs::create_dir_all(&git_folder).expect("an empty .git folder can be made");
    fs::write(git_folder.join("HEAD"), "").expect("an empty HEAD can be written");

    let failed = verb_at(&case, "preflight", &broken, T, &[]);

    assert!(failed.code.is_some_and(|code| code != 0 && code != 75), "{failed:?}");
    assert!(failed.stderr.contains("gate: remote git "), "the git step's line: {failed:?}");
    assert_eq!(claim(&root, 0), None, "the failed preflight's slot is let go: {failed:?}");
}

#[test]
fn a_preflight_that_makes_the_root_and_fails_with_no_count_file_says_no_slot_release() {
    let case = Case::new("a_preflight_that_makes_the_root_and_fails_with_no_count_file_says_no_slot_release");
    let fresh = sessioned(&box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]), "s1");
    // With no git on its PATH the preflight's first git step, the bare repository's init, cannot start.
    let empty = case.folder().join("empty");
    fs::create_dir_all(&empty).expect("the empty folder can be made");
    let mut call = box_call(&case, &["box", "preflight"], &payload(&fresh));
    call.env("PATH", &empty);

    let failed = call.finish_read();

    assert_eq!(failed.code, Some(1), "{failed:?}");
    assert!(failed.stderr.starts_with("gate: remote git init --bare failed: "), "{failed:?}");
    assert!(!failed.stderr.contains("slot release failed"), "no slot was taken to let go: {failed:?}");
}

#[test]
fn a_second_run_call_of_a_session_refused_busy_leaves_the_live_run_s_claim() {
    let case = Case::new("a_second_run_call_of_a_session_refused_busy_leaves_the_live_run_s_claim");
    let root = root_of(&case);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut base = box_payload(&case, "k", &[FAKE_TOOL, "err", "started", "wait", &go_word]);
    snapshot(&case, &mut base, &[("a.txt", "a\n")]);
    counted(&root, 1);
    let live = sessioned(&base, "s1");
    let first = verb_at(&case, "preflight", &live, T, &[]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let now = (T + 1).to_string();
    let running = box_call(
        &case,
        &["box", "--test-box-now", &now, "--test-box-drain-readable-only", "true", "run"],
        &payload(&live),
    )
    .start();
    running.wait_for_stderr("started");
    let held = claim(&root, 0);

    let again = renamed(&live, 1);
    let second = verb_at(&case, "run", &again, T + 2, &[]);
    let after = claim(&root, 0);
    let_go(&go);
    let outcome = running.finish_read();

    assert_eq!(second.code, Some(75), "{second:?}");
    assert_eq!(
        second.stderr,
        format!("{}\ngate: remote work folder k is busy\n", refused_line(&again, "busy")),
        "{second:?}"
    );
    assert!(held.as_ref().is_some_and(|held| held["session"] == json!("s1")), "{held:?}");
    assert_eq!(after, held, "the live run's claim is still there");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn a_session_s_second_preflight_takes_no_second_slot() {
    let case = Case::new("a_session_s_second_preflight_takes_no_second_slot");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 2);

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &[]);
    let again = verb_at(&case, "preflight", &sessioned(&base, "s1"), T + 1, &[]);
    let after_again = claim(&root, 1);
    let other = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 2, &[]);

    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(after_again, None, "the second preflight takes no slot");
    assert_eq!(other.code, Some(0), "a second session finds slot 1 free: {other:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s1"));
    assert_eq!(claimed_by(&root, 1).as_deref(), Some("s2"));
}

#[test]
fn a_hold_runs_its_command_passes_its_output_through_and_exits_with_its_status() {
    let case = Case::new("a_hold_runs_its_command_passes_its_output_through_and_exits_with_its_status");
    let root = root_of(&case);
    rooted(&case);
    counted(&root, 1);

    let outcome = held(hold(
        &case,
        &root,
        T,
        &[],
        &OPTIONS,
        &[FAKE_TOOL, "out", "held", "err", "oops", "exit", "3"],
    ));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(outcome.stdout, "held\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "oops\n", "{outcome:?}");
    assert_eq!(claim(&root, 0), None, "the hold's claim goes with it");
    assert!(!root.join("slots").join("0.lock").exists(), "and its lock");
}

#[test]
fn a_hold_with_no_free_slot_exits_full_at_once() {
    let case = Case::new("a_hold_with_no_free_slot_exits_full_at_once");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &[]);
    assert_eq!(first.code, Some(0), "{first:?}");

    let outcome = held(hold(&case, &root, T + 1, &[], &OPTIONS, &[FAKE_TOOL, "out", "never"]));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran: {outcome:?}");
    assert_eq!(
        outcome.stderr, "gate-remote-refused: hold full\ngate: remote builder is full: 1 of 1 slots held\n",
        "{outcome:?}"
    );
}

#[test]
fn a_waiting_hold_takes_the_slot_when_the_session_s_run_ends() {
    let case = Case::new("a_waiting_hold_takes_the_slot_when_the_session_s_run_ends");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let poll = ["--test-box-hold-poll-ms", "50"];
    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T, &[]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let none = [&OPTIONS[..], &["--wait", "0"]].concat();
    let long = [&OPTIONS[..], &["--wait", "600"]].concat();

    let gave_up = held(hold(&case, &root, T + 1, &poll, &none, &[FAKE_TOOL, "out", "never"]));
    let (waiting, stdin) = start_with_stdin(hold(&case, &root, T + 1, &poll, &long, &[FAKE_TOOL, "out", "took"]));
    drop(stdin);
    waiting.wait_for_stderr("gate: waiting for a builder slot: 1 of 1 held\n");
    let run = verb_at(&case, "run", &sessioned(&base, "s1"), T + 2, &[]);
    let took = waiting.finish_read();

    assert_eq!(gave_up.code, Some(75), "{gave_up:?}");
    assert_eq!(
        gave_up.stderr, "gate-remote-refused: hold full\ngate: remote builder is full: 1 of 1 slots held\n",
        "a wait of 0 refuses at once, never waiting: {gave_up:?}"
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(took.code, Some(0), "{took:?}");
    assert_eq!(took.stdout, "took\n", "{took:?}");
}

#[test]
fn a_killed_hold_frees_its_slot_for_the_next_taker() {
    let case = Case::new("a_killed_hold_frees_its_slot_for_the_next_taker");
    let root = root_of(&case);
    let base = rooted(&case);
    counted(&root, 1);
    let (running, stdin) = start_with_stdin(hold(&case, &root, T, &[], &OPTIONS, &[FAKE_TOOL, "sleep", UNTIL_KILLED]));
    drop(stdin);
    wait_until("the hold claims slot 0", || {
        claim(&root, 0).is_some_and(|claim| claim["kind"] == json!("hold"))
    });
    let holding = claim(&root, 0).expect("the hold's claim");

    let killed = running.kill();
    let next = verb_at(&case, "preflight", &sessioned(&base, "s1"), T + 1, &[]);

    assert_eq!(holding["holder"], json!(HOLDER), "{holding}");
    assert_eq!(holding["ceiling"], json!(600), "{holding}");
    assert!(killed.code != Some(0), "{killed:?}");
    assert_eq!(next.code, Some(0), "the killed hold's slot is free: {next:?}");
    assert_eq!(claimed_by(&root, 0).as_deref(), Some("s1"));
}

#[test]
fn a_hold_past_its_ceiling_exits_124_and_ends_its_command() {
    let case = Case::new("a_hold_past_its_ceiling_exits_124_and_ends_its_command");
    let root = root_of(&case);
    rooted(&case);
    counted(&root, 1);
    let pid_file = case.folder().join("pid");
    let pid_path = shown(&pid_file);

    let outcome = held(hold(
        &case,
        &root,
        T,
        &[],
        &["--holder", HOLDER, "--ceiling", "1"],
        &[FAKE_TOOL, "pid-to", &pid_path, "sleep", UNTIL_KILLED],
    ));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(outcome.stderr.ends_with("gate: hold ended at its ceiling of 1 s\n"), "{outcome:?}");
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("the command wrote its pid")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        !Win32ProcessTable
            .snapshot()
            .iter()
            .any(|process| process.pid == pid && process.name.contains("fake-tool")),
        "the command was ended"
    );
    assert_eq!(claim(&root, 0), None, "the slot is let go");
}

#[test]
fn a_holder_with_a_control_character_is_a_usage_error() {
    let case = Case::new("a_holder_with_a_control_character_is_a_usage_error");
    let root = root_of(&case);
    rooted(&case);
    counted(&root, 1);

    let outcome = held(hold(
        &case,
        &root,
        T,
        &[],
        &["--holder", "ci\u{1b}[2J", "--ceiling", "60"],
        &[FAKE_TOOL, "out", "never"],
    ));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate box: the holder 'ci\\u{1b}[2J' holds a control character\n",
        "{outcome:?}"
    );
    assert!(!root.join("slots").exists(), "no slot is taken");
}

#[test]
fn a_hold_on_a_builder_with_no_count_file_exits_2_with_its_line() {
    let case = Case::new("a_hold_on_a_builder_with_no_count_file_exits_2_with_its_line");
    let root = root_of(&case);
    rooted(&case);

    let outcome = held(hold(&case, &root, T, &[], &OPTIONS, &[FAKE_TOOL, "out", "never"]));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "gate box: this builder has no slot count; run gate push\n", "{outcome:?}");
}

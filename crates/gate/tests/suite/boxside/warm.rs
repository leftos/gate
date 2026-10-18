//! A warm-up on the box: a `gate box run` whose payload holds `WarmCommands` and `WarmHash` instead of a `Command`,
//! the payload's refusals, the commands run in order under one budget with a stop looked at before each, the
//! `{Work}.warm` record it writes once every command passed, and the record's removal when the folder state changes.

use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

use serde_json::{Value, json};

use regex::Regex;

use super::{
    BUN_LOCKFILE, BUN_LOCKFILE_PATH, ENDLESS, Folders, NODE_MODULES_IGNORED, PNPM_LOCKFILE, PNPM_LOCKFILE_PATH, STALE_UE_ROOT, box_call,
    bun_install_call, command_call, fake_godot, fake_unreal, field, gate_box, godot_root, has_ref, install_call, installs, let_go, lines_starting,
    marks, pair_payload, pair_snapshot, pair_tree, passed, path_for_bun, path_for_pnpm, pnpm_box, pnpm_calls, refused_line, renamed, run_box,
    run_box_with_godot, run_box_with_seams, run_box_with_unreal, snapshot, started_line, stopped_line_for, stub_bun, stub_pnpm, unreal_root,
};
use crate::support::{Case, FAKE_TOOL, GATE, Outcome, box_payload, shown, wait_until, wait_until_closed};

/// The warm record of `payload`'s work folder, `{Work}.warm`.
fn work_warm(payload: &Value) -> String {
    format!("{}.warm", field(payload, "Work"))
}

/// The line a warm-up says last once every command passed and its record is written.
fn warm_said() -> String {
    format!("gate: remote warm {WARM_HASH}")
}

/// Checks a warm-up that did not pass left no warm record and did not say it is warm.
fn not_warm(outcome: &Outcome, payload: &Value) {
    assert!(!Path::new(&work_warm(payload)).exists(), "no warm record: {outcome:?}");
    assert!(lines_starting(outcome, "gate: remote warm ").is_empty(), "{outcome:?}");
}

#[test]
fn box_warm_up_runs_every_command_in_order_and_writes_warm() {
    let case = Case::new("box_warm_up_in_order");
    let (first, second) = (case.folder().join("first"), case.folder().join("second"));
    let (first_word, second_word) = (shown(&first), shown(&second));
    let mut payload = warm_payload(
        &case,
        &[
            &[FAKE_TOOL, "mark", &first_word, "out", "first"],
            &[FAKE_TOOL, "mark", &second_word, "write", "a.txt", "changed", "out", "second"],
        ],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "first\nsecond\n", "the commands ran in order: {outcome:?}");
    assert_eq!((marks(&first), marks(&second)), ("run\n".to_owned(), "run\n".to_owned()), "each ran once");
    assert_eq!(
        lines_starting(&outcome, "gate-remote-started:"),
        [started_line(&payload)],
        "one start marker: {outcome:?}"
    );
    assert_eq!(outcome.stderr.lines().last(), Some(warm_said().as_str()), "said last: {outcome:?}");
    assert_eq!(
        fs::read_to_string(work_warm(&payload)).ok().as_deref(),
        Some(WARM_HASH),
        "the record holds the hash alone"
    );
    assert!(
        !has_ref(&payload, "refs/gate-back/k"),
        "a warm-up commits nothing back, though it changed a tracked file: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_stops_at_a_failing_command() {
    let case = Case::new("box_warm_up_failing");
    let marked: Vec<_> = (1..=3).map(|n| case.folder().join(format!("m{n}"))).collect();
    let words: Vec<String> = marked.iter().map(|path| shown(path)).collect();
    let planted = work_warm(&box_payload(&case, "k", &[]));
    // The first command plants a record holding the right hash where the box keeps it; the second fails.
    let mut payload = warm_payload(
        &case,
        &[
            &[FAKE_TOOL, "mark", &words[0], "write", &planted, WARM_HASH],
            &[FAKE_TOOL, "mark", &words[1], "exit", "3"],
            &[FAKE_TOOL, "mark", &words[2]],
        ],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(3), "the failing command's status: {outcome:?}");
    assert_eq!(
        lines_starting(&outcome, "gate: remote warm-up "),
        ["gate: remote warm-up stopped at command 2 of 3"],
        "{outcome:?}"
    );
    assert_eq!(
        (marks(&marked[0]), marks(&marked[1])),
        ("run\n".to_owned(), "run\n".to_owned()),
        "{outcome:?}"
    );
    assert_eq!(marks(&marked[2]), String::new(), "no later command ran");
    not_warm(&outcome, &payload);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_deletes_an_old_warm_before_its_first_command() {
    let case = Case::new("box_warm_up_deletes_old_warm");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "3"]]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    fs::write(work_warm(&payload), WARM_HASH).expect("the record can be written");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    not_warm(&outcome, &payload);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_shares_one_budget_across_its_commands() {
    let case = Case::new("box_warm_up_one_budget");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "sleep", "1500"], ENDLESS]);
    payload["Timeout"] = json!(3);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    // The whole machine free, so the clock runs with wall time however loaded the machine running the case is.
    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let ran_out = "gate: remote warm-up ran out of its 3 s budget before command 2 of 2";
    let kill = Regex::new(r"^gate: TIMED OUT on builder after (\d+) s of load-adjusted time").expect("the pattern compiles");
    let killed_after: Vec<u64> = outcome
        .stderr
        .lines()
        .filter_map(|line| kill.captures(line).map(|captures| captures[1].parse().expect("the seconds are a number")))
        .collect();
    if outcome.stderr.lines().any(|line| line == ran_out) {
        // A machine slow enough that the first command spent the whole budget: the second never started.
        assert!(killed_after.is_empty(), "{outcome:?}");
    } else {
        assert_eq!(killed_after.len(), 1, "one kill: {outcome:?}");
        assert!(killed_after[0] < 3, "the second command's ceiling is what the first left: {outcome:?}");
        assert_eq!(
            lines_starting(&outcome, "gate: remote warm-up "),
            ["gate: remote warm-up stopped at command 2 of 2"],
            "{outcome:?}"
        );
    }
    not_warm(&outcome, &payload);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_stopped_between_commands_runs_no_more() {
    let case = Case::new("box_warm_up_stopped_between");
    let (park, parked) = (case.folder().join("park"), case.folder().join("park.parked"));
    let (first, second, go) = (case.folder().join("m1"), case.folder().join("m2"), case.folder().join("go"));
    let (park_word, first_word, second_word, go_word) = (shown(&park), shown(&first), shown(&second), shown(&go));
    let mut payload = warm_payload(
        &case,
        &[&[FAKE_TOOL, "mark", &first_word, "wait", &go_word], &[FAKE_TOOL, "mark", &second_word]],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let job = field(&payload, "Job").to_owned();
    let payload_word = super::payload(&payload);
    let running = box_call(&case, &["box", "--test-box-park-launch", &park_word, "run"], &payload_word).start();
    // The first command parks, then runs once let go; the park is taken back while it runs, so the second parks.
    wait_until("the first command parked", || parked.exists());
    fs::remove_file(&parked).expect("the parked file can be removed");
    let_go(&park);
    wait_until("the first command running", || first.exists());
    fs::remove_file(&park).expect("the park file can be removed");
    let_go(&go);
    wait_until("the second command parked", || parked.exists());

    let stopped = gate_box(&case, "stop", &payload_word, None);
    let_go(&park);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop before the second command fails the warm-up: {outcome:?}");
    let said = stopped_line_for(&job);
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert_eq!(marks(&first), "run\n", "the first command ran: {outcome:?}");
    assert!(!second.exists(), "the second command never ran: {outcome:?}");
    not_warm(&outcome, &payload);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_lay_out_deletes_warm_when_the_layout_changes() {
    let case = Case::new("box_lay_out_deletes_warm");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let record = work_warm(&payload);
    fs::write(&record, WARM_HASH).expect("the record can be written");

    let kept = run_box(&case, &payload);

    assert_eq!(kept.code, Some(0), "{kept:?}");
    assert!(Path::new(&record).exists(), "a run of the same layout keeps the record: {kept:?}");
    fs::write(format!("{}.layout", field(&payload, "Work")), "changing").expect("the layout record can be written");

    let changed = run_box(&case, &renamed(&payload, 1));

    assert_eq!(changed.code, Some(0), "{changed:?}");
    assert!(!Path::new(&record).exists(), "a layout change removes the record: {changed:?}");
    fs::write(&record, WARM_HASH).expect("the record can be written");
    fs::remove_dir_all(field(&payload, "Work")).expect("the work folder can be removed");

    let made = run_box(&case, &renamed(&payload, 2));

    assert_eq!(made.code, Some(0), "{made:?}");
    assert!(!Path::new(&record).exists(), "a work folder made again removes the record: {made:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_restores_pnpm_packages_before_its_marker() {
    let case = Case::new("box_warm_up_pnpm");
    let mut payload = warm_payload(&case, &[&["pnpm", "run", "a"], &[FAKE_TOOL, "exit", "0"], &["pnpm", "run", "b"]]);
    snapshot(
        &case,
        &mut payload,
        &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE), ("package.json", "{}\n")],
    );
    let stub = stub_pnpm(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case),
        [install_call(work), command_call(work, "run a"), command_call(work, "run b")],
        "one install for the one lockfile, then the commands: {outcome:?}"
    );
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let took = said.iter().position(|line| line.starts_with("gate: pnpm install took "));
    let marker = said.iter().position(|line| *line == started_line(&payload));
    assert!(took.is_some() && took < marker, "the install before the marker: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_restores_bun_packages_before_its_marker() {
    let case = Case::new("box_warm_up_bun");
    let mut payload = warm_payload(&case, &[&["bunx", "vitest", "run"], &[FAKE_TOOL, "exit", "0"], &["bun", "test"]]);
    snapshot(
        &case,
        &mut payload,
        &[NODE_MODULES_IGNORED, (BUN_LOCKFILE_PATH, BUN_LOCKFILE), ("package.json", "{}\n")],
    );
    let stub = stub_bun(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_bun(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case),
        [
            bun_install_call(work),
            command_call(work, "bunx vitest run"),
            command_call(work, "bun test")
        ],
        "one install for the one lockfile, then the commands: {outcome:?}"
    );
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let took = said.iter().position(|line| line.starts_with("gate: bun install took "));
    let marker = said.iter().position(|line| *line == started_line(&payload));
    assert!(took.is_some() && took < marker, "the install before the marker: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_refuses_commands_that_need_pnpm_and_bun_in_one_folder() {
    let case = Case::new("box_warm_up_pnpm_and_bun");
    let mut payload = warm_payload(&case, &[&["pnpm", "run", "a"], &["bun", "test"]]);
    snapshot(
        &case,
        &mut payload,
        &[
            NODE_MODULES_IGNORED,
            (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE),
            (BUN_LOCKFILE_PATH, BUN_LOCKFILE),
            ("package.json", "{}\n"),
        ],
    );
    let path = path_for_bun(&[stub_pnpm(&case, 0, ""), stub_bun(&case, 0, "")]);

    let outcome = pnpm_box(&case, &payload, &path).finish();

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let tag = refused_line(&payload, "tools");
    let tagged = said.iter().position(|line| *line == tag);
    assert!(
        tagged.and_then(|at| said.get(at + 1)) == Some(&"gate: remote work folder k uses both pnpm and bun in ."),
        "the tag, then the line naming the folder: {outcome:?}"
    );
    assert!(pnpm_calls(&case).is_empty(), "no install and no command: {outcome:?}");
    assert!(!said.contains(&started_line(&payload).as_str()), "no marker: {outcome:?}");
    assert!(!Path::new(&format!("{}.packages", field(&payload, "Work"))).exists(), "no record");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

/// A warm-up file hash, as the PC sends it: 64 lower-case hex digits.
const WARM_HASH: &str = "5f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// The payload of a warm-up of `commands` for the work folder `k`: [`box_payload`] with no `Command`, the commands as
/// `WarmCommands`, [`WARM_HASH`] as `WarmHash` and a [`WARM_BUDGET`] `Timeout` no case reaches.
fn warm_payload(case: &Case, commands: &[&[&str]]) -> Value {
    warmed(box_payload(case, "k", &[]), commands)
}

/// The budget of every warm-up case but the one about the budget: long enough that no command of a case reaches it
/// however slow a loaded machine makes the run.
const WARM_BUDGET: u64 = 600;

/// `payload` made a warm-up of `commands`: its `Command` removed, the commands as `WarmCommands`, [`WARM_HASH`] as
/// `WarmHash` and [`WARM_BUDGET`] as `Timeout`.
fn warmed(mut payload: Value, commands: &[&[&str]]) -> Value {
    payload.as_object_mut().expect("the payload is an object").remove("Command");
    payload["WarmCommands"] = json!(commands);
    payload["WarmHash"] = json!(WARM_HASH);
    payload["Timeout"] = json!(WARM_BUDGET);
    payload
}

#[test]
fn box_run_refuses_a_warm_up_without_a_timeout() {
    let case = Case::new("box_warm_no_timeout");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "0"]]);
    payload.as_object_mut().expect("the payload is an object").remove("Timeout");

    refused_before_the_lock(&case, payload, "gate: box payload lacks Timeout");
}

#[test]
fn box_preflight_says_warm_none_for_a_work_folder_made_again() {
    let case = Case::new("box_preflight_warm_made_again");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    fs::write(warm_record(&folders), WARM_HASH).expect("the record can be written");
    fs::remove_dir_all(&folders.work).expect("the work folder can be removed");

    let made = gate_box(&case, "preflight", &folders.payload(), None);
    let again = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(after_version(&made), "gate: remote warm none", "no work folder: {made:?}");
    assert_eq!(
        after_version(&again),
        "gate: remote warm none",
        "a work folder made again is not warm: {again:?}"
    );
}

#[test]
fn box_preflight_readies_no_tree_whose_warm_record_cannot_be_removed() {
    let case = Case::new("box_preflight_warm_held");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    let record = warm_record(&folders);
    fs::write(&record, WARM_HASH).expect("the record can be written");
    fs::remove_dir_all(Path::new(&folders.work).join(".git")).expect("the work folder's .git can be removed");
    // Held without delete sharing, so the preflight cannot remove it.
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&record)
        .expect("the record can be held");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with("gate: ")).collect();
    assert_eq!(said.len(), 1, "one line says why: {outcome:?}");
    assert!(said[0].starts_with(&format!("gate: remote could not remove {record}: ")), "{outcome:?}");
    let printed: Vec<&str> = outcome.stdout.lines().collect();
    let version = printed.iter().position(|line| line.starts_with("gate: remote version "));
    assert_eq!(
        version.and_then(|at| printed.get(at + 1)).copied(),
        Some("gate: remote warm none"),
        "{outcome:?}"
    );
    assert!(
        !Path::new(&folders.work).join(".git").exists(),
        "the tree is left to the run: {outcome:?}"
    );
}

#[test]
fn box_lay_out_deletes_warm_when_a_pair_sibling_git_is_repaired() {
    let case = Case::new("box_lay_out_warm_pair_repair");
    let mut payload = warmed(pair_payload(&case, &["lib"], &[]), &[&[FAKE_TOOL, "exit", "0"]]);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);
    let warmed_up = run_box(&case, &payload);
    assert_eq!(warmed_up.code, Some(0), "{warmed_up:?}");
    assert!(Path::new(&work_warm(&payload)).exists(), "{warmed_up:?}");
    let info = Path::new(&pair_tree(&payload, "lib")).join(".git").join("objects").join("info");
    fs::remove_dir_all(&info).expect("the sibling's objects\\info can be removed");
    let mut run = renamed(&payload, 1);
    let object = run.as_object_mut().expect("the payload is an object");
    object.remove("WarmCommands");
    object.remove("WarmHash");
    run["Command"] = json!([FAKE_TOOL, "exit", "0"]);

    let repaired = run_box(&case, &run);
    let preflight = gate_box(&case, "preflight", &super::payload(&run), None);

    assert_eq!(repaired.code, Some(0), "{repaired:?}");
    assert!(info.is_dir(), "the run repaired the sibling's .git: {repaired:?}");
    assert!(!Path::new(&work_warm(&payload)).exists(), "a repaired tree is no warm tree: {repaired:?}");
    assert_eq!(after_version(&preflight), "gate: remote warm none", "{preflight:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_installs_once_for_a_lockfile_folder_named_in_two_cases() {
    let case = Case::new("box_warm_up_pnpm_case");
    let mut payload = warm_payload(&case, &[&["pnpm", "--dir", "App", "run", "a"], &["pnpm", "--dir", "app", "run", "b"]]);
    snapshot(
        &case,
        &mut payload,
        &[NODE_MODULES_IGNORED, ("App/pnpm-lock.yaml", PNPM_LOCKFILE), ("App/package.json", "{}\n")],
    );
    let stub = stub_pnpm(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(installs(&case), 1, "one install for the one folder: {:?} {outcome:?}", pnpm_calls(&case));
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_says_a_pnpm_skip_once_for_commands_that_share_it() {
    let case = Case::new("box_warm_up_pnpm_skip_once");
    let outside = ["pnpm", "--dir", "..", "run", "a"];
    let mut payload = warm_payload(&case, &[&outside, &outside]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let stub = stub_pnpm(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        lines_starting(&outcome, "gate: remote pnpm install skipped:").len(),
        1,
        "one line for the one place both commands lead to: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_preflight_says_warm_none_for_a_pair_whose_sibling_git_is_missing() {
    let case = Case::new("box_preflight_warm_pair_sibling");
    let payload = pair_payload(&case, &["lib"], &[FAKE_TOOL]);
    let work = Path::new(field(&payload, "Work"));
    let plain_git = |tree: &str| {
        fs::create_dir_all(work.join(tree).join(".git").join("objects").join("info")).expect("a tree's .git can be made");
    };
    plain_git("app");
    fs::write(format!("{}.layout", work.display()), "pair app lib").expect("the layout record can be written");
    fs::write(work_warm(&payload), WARM_HASH).expect("the record can be written");
    let word = super::payload(&payload);

    let missing = gate_box(&case, "preflight", &word, None);
    plain_git("lib");
    let present = gate_box(&case, "preflight", &word, None);

    assert_eq!(after_version(&missing), "gate: remote warm none", "the sibling has no .git: {missing:?}");
    assert_eq!(after_version(&present), warm_said(), "every tree of the layout has its .git: {present:?}");
}

#[test]
fn box_warm_up_of_a_pair_runs_in_the_main_tree_and_says_no_back_commits() {
    let case = Case::new("box_warm_up_pair");
    let mut payload = warmed(
        pair_payload(&case, &["lib"], &[]),
        &[&[FAKE_TOOL, "write", "here.txt", "x"], &[FAKE_TOOL, "exit", "0"]],
    );
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        Path::new(&pair_tree(&payload, "app")).join("here.txt").is_file(),
        "ran in the main tree: {outcome:?}"
    );
    assert!(lines_starting(&outcome, "gate: remote back commits").is_empty(), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().last(), Some(warm_said().as_str()), "{outcome:?}");
    assert_eq!(fs::read_to_string(work_warm(&payload)).ok().as_deref(), Some(WARM_HASH));
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_warm_up_ends_the_job_of_a_command_before_the_next() {
    let case = Case::new("box_warm_up_ends_each_job");
    let (pid, release, log, second) = (
        case.folder().join("pid"),
        case.folder().join("release"),
        case.folder().join("left.log"),
        case.folder().join("m2"),
    );
    let_go(&release);
    let (pid_word, release_word, log_word, second_word) = (shown(&pid), shown(&release), shown(&log), shown(&second));
    // The first command leaves a gate of the box's own state root running in its job, which the end of a command
    // spares; only ending the whole job frees the job's name for the second command.
    let left = [
        FAKE_TOOL,
        "spawn-pid",
        &pid_word,
        &release_word,
        GATE,
        "run",
        "--local",
        "--slot",
        "light",
        "--timeout",
        "300",
        "--log",
        &log_word,
        "--",
        FAKE_TOOL,
        "sleep",
        "600000",
    ];
    let mut payload = warm_payload(&case, &[&left, &[FAKE_TOOL, "mark", &second_word]]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(marks(&second), "run\n", "the second command ran: {outcome:?}");
    let left_pid: u32 = fs::read_to_string(&pid).expect("the pid file").trim().parse().expect("a pid");
    assert!(
        gate_win32::process_start_of(left_pid).expect("the process can be looked for").is_none(),
        "the first command's leftover gate is ended: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

/// Runs the warm-up `payload`, given a well-formed `Snapshot` and never readied, and checks the box refused it before
/// the lock with `line` alone and status 2.
fn refused_before_the_lock(case: &Case, mut payload: Value, line: &str) {
    payload["Snapshot"] = json!("0123456789abcdef0123456789abcdef01234567");

    let outcome = run_box(case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{line}\n"), "{outcome:?}");
    let lock = payload["Lock"].as_str().expect("the payload holds its lock");
    assert!(!Path::new(lock).exists(), "no lock is taken: {outcome:?}");
}

/// The line a passed preflight says right after its version line.
fn after_version(outcome: &Outcome) -> String {
    let printed = passed(outcome);
    let version = printed.iter().position(|line| line.starts_with("gate: remote version "));
    let next = version.and_then(|at| printed.get(at + 1));
    assert!(next.is_some(), "a line follows the version line: {outcome:?}");
    next.map_or_else(String::new, |line| (*line).to_owned())
}

/// The warm record of the preflight folders `folders`, `{Work}.warm`.
fn warm_record(folders: &Folders) -> String {
    format!("{}.warm", folders.work)
}

#[test]
fn box_preflight_says_warm_none_for_a_missing_folder() {
    let case = Case::new("box_preflight_warm_missing_folder");
    let folders = Folders::new(&case);
    let record = warm_record(&folders);
    fs::create_dir_all(Path::new(&record).parent().expect("the record has a folder")).expect("the root can be made");
    fs::write(&record, WARM_HASH).expect("the record can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(after_version(&outcome), "gate: remote warm none", "no work folder: {outcome:?}");
    fs::remove_dir_all(Path::new(&folders.work).join(".git")).expect("the work folder's .git can be removed");
    fs::write(&record, WARM_HASH).expect("the record can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(after_version(&outcome), "gate: remote warm none", "no .git: {outcome:?}");
    assert!(
        Path::new(&folders.work).join(".git").is_dir(),
        "the preflight readied the tree after: {outcome:?}"
    );
}

#[test]
fn box_preflight_says_warm_none_for_a_malformed_record() {
    let case = Case::new("box_preflight_warm_malformed");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    let record = warm_record(&folders);
    let malformed = [
        format!("{WARM_HASH}\n"),
        format!("{WARM_HASH}\r\n"),
        format!("\u{feff}{WARM_HASH}"),
        format!(" {WARM_HASH}"),
        WARM_HASH.to_uppercase(),
        WARM_HASH[..63].to_owned(),
        format!("{WARM_HASH}0"),
        String::new(),
    ];
    for text in malformed {
        fs::write(&record, &text).expect("the record can be written");

        let outcome = gate_box(&case, "preflight", &folders.payload(), None);

        assert_eq!(after_version(&outcome), "gate: remote warm none", "{text:?}: {outcome:?}");
    }
    fs::remove_file(&record).expect("the record can be removed");
    fs::create_dir(&record).expect("a folder can stand at the record's name");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(after_version(&outcome), "gate: remote warm none", "a folder: {outcome:?}");
}

#[test]
fn box_preflight_says_the_warm_hash_of_a_warm_folder() {
    let case = Case::new("box_preflight_warm_hash");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    fs::write(warm_record(&folders), WARM_HASH).expect("the record can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(after_version(&outcome), format!("gate: remote warm {WARM_HASH}"), "{outcome:?}");
}

#[test]
fn box_run_refuses_a_payload_with_both_command_and_warm_commands() {
    let case = Case::new("box_warm_both_commands");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "0"]]);
    payload["Command"] = json!([FAKE_TOOL, "exit", "0"]);

    refused_before_the_lock(&case, payload, "gate: box payload sets both Command and WarmCommands");
}

#[test]
fn box_run_refuses_warm_commands_without_a_warm_hash() {
    let case = Case::new("box_warm_no_hash");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "0"]]);
    payload.as_object_mut().expect("the payload is an object").remove("WarmHash");

    refused_before_the_lock(&case, payload, "gate: box payload lacks WarmHash");
}

#[test]
fn box_run_refuses_a_warm_up_that_asks_for_outputs() {
    let case = Case::new("box_warm_outputs");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "0"]]);
    payload["Outputs"] = json!(["bin/**"]);

    refused_before_the_lock(&case, payload, "gate: box payload asks a warm-up for outputs");
}

/// A warm-up's `GodotVersion` is taken, and its Godot console word runs the install of that version, `GODOT_PATH`
/// naming it, as a run's does.
#[test]
fn box_warm_up_runs_the_godot_install_of_its_godot_version() {
    let case = Case::new("box_warm_godot");
    let root = godot_root(&case);
    fake_godot(&root.join("4.3.0"), "4.3.stable.mono.official.y\n");
    fake_godot(&root.join("4.7.2"), "4.7.2.stable.mono.official.x\n");
    let mut payload = warm_payload(&case, &[&["Godot_console.exe", "--version", "env", "GODOT_PATH"]]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    payload["GodotVersion"] = json!("4.7.2");

    let outcome = run_box_with_godot(&case, &payload, Some(&root), &[], None);

    let install = root.join("4.7.2").join("Godot_console.exe");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("4.7.2.stable.mono.official.x\n{}\n", install.display()),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.lines().last(), Some(warm_said().as_str()), "{outcome:?}");
    assert!(!root.join("4.3.0").join("ran").exists(), "the other install ran");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

/// A warm-up's `UnrealVersion` is taken, and its commands see `UE_ROOT` naming the engine of that version over the one
/// the box inherited, as a run's do.
#[test]
fn box_warm_up_sets_ue_root_for_its_unreal_version() {
    let case = Case::new("box_warm_unreal");
    let root = unreal_root(&case);
    fake_unreal(&root.join("5.7.3"), "5.7.3");
    fake_unreal(&root.join("5.8.0"), "5.8.0");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "env", "UE_ROOT"]]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    payload["UnrealVersion"] = json!("5.8.0");

    let outcome = run_box_with_unreal(&case, &payload, Some(&root), Some(STALE_UE_ROOT));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, format!("{}\n", root.join("5.8.0").display()), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().last(), Some(warm_said().as_str()), "{outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_refuses_a_warm_up_that_asks_for_env() {
    let case = Case::new("box_warm_env");
    let mut payload = warm_payload(&case, &[&[FAKE_TOOL, "exit", "0"]]);
    payload["Env"] = json!({"NuGetAudit": "false"});

    refused_before_the_lock(&case, payload, "gate: box payload asks a warm-up for Env");
}

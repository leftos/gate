//! The learned durations: the expected-time line a gate says as it takes its slot, and `durations.json`, the store
//! each run's release is filed in, keyed by where the run ran, its class, its repo and its command.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;

use regex::Regex;
use serde_json::Value;

use crate::routing::{heavy_words_with_limit, routed_case};
use crate::support::{
    Case, FAKE_TOOL, Outcome, assert_in_order, critical_words, events, finish, git, git_init, heavy_words, none_words, run_words, shown,
    wait_for_file,
};

/// `FILE_SHARE_READ`: a handle that lets others read the file, but not write, move or delete it.
const FILE_SHARE_READ: u32 = 1;

/// The expected-time line of a key with history in seconds, its run count captured.
const EXPECTED: &str = r"gate: expected about \d+ s \(usual \d+ s, p90 \d+ s, (\d+) runs\)";
/// The command every case runs unless it needs another.
const QUICK: [&str; 3] = [FAKE_TOOL, "exit", "0"];
/// The command part of [`QUICK`]'s key.
const QUICK_KEY: &str = " | fake-tool exit 0";

/// Runs `command` as `case`'s gate, asserting it passed.
fn passing(command: Command) -> Outcome {
    let outcome = finish(command);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    outcome
}

/// Runs [`QUICK`] in a light slot of `case` `times` times, asserting each passed and said nothing on stderr.
fn quick_runs(case: &Case, times: usize) {
    for _ in 0..times {
        let outcome = passing(case.gate(&run_words("out.log", &[], &QUICK)));
        assert_eq!(outcome.stderr, "", "a key with fewer than 5 runs says nothing: {outcome:?}");
    }
}

/// The run count of the one expected-time line `stderr` holds, the line alone on its own line.
fn said_runs(stderr: &str) -> u64 {
    let line = Regex::new(&format!("(?m)^{EXPECTED}$")).expect("the pattern is valid");
    let found: Vec<u64> = line
        .captures_iter(stderr)
        .map(|captures| captures[1].parse().expect("the run count is a number"))
        .collect();
    assert_eq!(found.len(), 1, "one expected-time line in:\n{stderr}");
    found[0]
}

/// The store `case`'s gates wrote, `None` when there is none.
fn store(case: &Case) -> Option<Value> {
    match fs::read_to_string(case.root().join("durations.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(serde_json::from_str(&read.expect("the store can be read")).expect("the store is JSON")),
    }
}

/// The keys of `case`'s store.
fn keys(case: &Case) -> Vec<String> {
    let store = store(case).expect("the store was written");
    store["keys"].as_object().expect("the store holds its keys").keys().cloned().collect()
}

/// The history of the one key of `case`'s store that ends with `command`.
fn history_ending(case: &Case, command: &str) -> Value {
    let store = store(case).expect("the store was written");
    let matching: Vec<&Value> = store["keys"]
        .as_object()
        .expect("the store holds its keys")
        .iter()
        .filter(|(key, _)| key.ends_with(command))
        .map(|(_, history)| history)
        .collect();
    assert_eq!(matching.len(), 1, "one key ends with {command:?} in {store:#}");
    matching[0].clone()
}

/// The samples of a key's history.
fn samples(history: &Value) -> &Vec<Value> {
    history["samples"].as_array().expect("a history holds its samples")
}

#[test]
fn a_gate_says_the_expected_time_when_it_takes_its_slot() {
    let case = Case::new("durations-says");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let command: [&str; 5] = [FAKE_TOOL, "wait", &go_word, "out", "ran"];
    fs::write(&go, "").expect("the go file can be made");
    for _ in 0..5 {
        let outcome = passing(case.gate(&run_words("out.log", &[], &command)));
        assert_eq!(outcome.stderr, "", "a key with fewer than 5 runs says nothing: {outcome:?}");
    }
    fs::remove_file(&go).expect("the go file can be removed");

    let mut running = case.start(&run_words("out.log", &[], &command));
    // The command writes nothing until the go file comes, so a line said while it waits came before its output.
    running.wait_for_stderr_while_running("the expected-time line", |stderr| stderr.contains("gate: expected"));
    let pid = running.id();
    let took = events(&case).iter().any(|event| event["event"] == "take" && event["pid"] == pid);
    fs::write(&go, "").expect("the go file can be made");
    let outcome = running.finish();

    assert!(took, "the take is journalled before the line is said");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stdout.starts_with("ran\n"), "{outcome:?}");
    let line = Regex::new(&format!(r"\A{EXPECTED}\n\z")).expect("the pattern is valid");
    assert!(line.is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(said_runs(&outcome.stderr), 5);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_eq!(said_runs(&log), 5, "the line is in the log too");
}

#[test]
fn a_command_without_history_says_nothing() {
    let case = Case::new("durations-no-history");
    quick_runs(&case, 5);

    let other = passing(case.gate(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"])));

    assert_eq!(other.stderr, "", "another command has no history of its own: {other:?}");
}

#[test]
fn a_failed_run_is_counted_not_timed() {
    let case = Case::new("durations-failed");
    let command = [FAKE_TOOL, "env", "GATE_SUITE_SAY", "exit", "0"];
    let saying = |said: &str| {
        let mut gate = case.gate(&run_words("out.log", &[], &command));
        gate.env("GATE_SUITE_SAY", said);
        finish(gate)
    };
    for _ in 0..5 {
        assert_eq!(saying("fine").code, Some(0));
    }
    let failed = saying("error CS1002: x");
    assert_eq!(failed.code, Some(1), "the marker fails the run: {failed:?}");

    let after = saying("fine");

    assert_eq!(said_runs(&after.stderr), 5, "the failure is no run of the estimate: {after:?}");
    let history = history_ending(&case, " | fake-tool env GATE_SUITE_SAY exit 0");
    assert_eq!(history["failed"], 1, "{history:#}");
    assert_eq!(
        samples(&history).len(),
        6,
        "five passing runs before the failure and one after: {history:#}"
    );
}

#[test]
fn the_store_is_written_after_the_release_line() {
    let case = Case::new("durations-after-release");

    quick_runs(&case, 2);

    let releases: Vec<Value> = events(&case).into_iter().filter(|event| event["event"] == "release").collect();
    let history = history_ending(&case, QUICK_KEY);
    let samples = samples(&history);
    assert_eq!(samples.len(), 2, "{history:#}");
    for (sample, release) in samples.iter().zip(&releases) {
        assert_eq!(
            sample["time"], release["time"],
            "a sample is filed at its release line's time: {history:#}"
        );
        let wall = sample["wall"].as_f64().expect("a sample has its wall time");
        let journalled = release["wallSeconds"].as_f64().expect("a release has its wall time");
        assert!((wall - journalled).abs() < 0.001, "{wall} against the journal's {journalled}");
    }
    assert_eq!(history["last"], releases[1]["time"], "{history:#}");
}

#[test]
fn a_critical_gate_says_it_and_keys_crit() {
    let case = Case::new("durations-critical");
    for _ in 0..5 {
        let outcome = passing(case.gate(&critical_words("out.log", &[], &QUICK)));
        assert!(!outcome.stderr.contains("gate: expected"), "{outcome:?}");
    }

    let outcome = passing(case.gate(&critical_words("out.log", &[], &QUICK)));

    assert_in_order(&outcome.stderr, &["gate: holds the machine (critical)", "gate: expected about "]);
    assert_eq!(said_runs(&outcome.stderr), 5);
    let keys = keys(&case);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("local:crit:") && keys[0].ends_with(QUICK_KEY), "{keys:?}");
}

#[test]
fn a_routed_run_keys_remote_host() {
    let case = routed_case("durations-routed");
    let steps = case.folder().join("steps.txt");
    fs::write(&steps, "status 0").expect("the step file can be written");
    let steps = shown(&steps);
    let words = heavy_words_with_limit("out.log", &["--test-remote-client", &steps], &[r".\tool.exe", "exit", "0"]);
    for _ in 0..5 {
        let outcome = passing(case.gate(&words));
        assert!(!outcome.stderr.contains("gate: expected"), "{outcome:?}");
    }

    let outcome = passing(case.gate(&words));

    assert_eq!(said_runs(&outcome.stderr), 5, "{outcome:?}");
    let keys = keys(&case);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("remote:builder::") && keys[0].ends_with(" | tool exit 0"), "{keys:?}");
}

#[test]
fn a_routed_run_that_falls_back_says_each_take_and_files_here() {
    let case = routed_case("durations-fallback");
    let steps = |name: &str, step: &str| {
        let path = case.folder().join(name);
        fs::write(&path, step).expect("the step file can be written");
        shown(&path)
    };
    let (passed, refused) = (steps("passed.txt", "status 0"), steps("refused.txt", "refuse unreachable"));
    let command = [r".\tool.exe", "exit", "0"];
    let passing_there = heavy_words_with_limit("out.log", &["--test-remote-client", &passed], &command);
    let falling_back = heavy_words_with_limit("out.log", &["--test-remote-client", &refused], &command);
    for _ in 0..6 {
        passing(case.gate(&passing_there));
    }
    for _ in 0..5 {
        passing(case.gate(&falling_back));
    }

    let outcome = passing(case.gate(&falling_back));

    let expected = Regex::new(&format!("(?m)^{EXPECTED}$")).expect("the pattern is valid");
    let said: Vec<(usize, u64)> = expected
        .captures_iter(&outcome.stderr)
        .map(|captures| {
            let at = captures.get(0).expect("a match has its whole text").start();
            (at, captures[1].parse().expect("the run count is a number"))
        })
        .collect();
    let counts: Vec<u64> = said.iter().map(|(_, runs)| *runs).collect();
    assert_eq!(
        counts,
        [6, 5],
        "the remote key's runs at the remote take, the local key's at the local take: {outcome:?}"
    );
    let fallback = outcome
        .stderr
        .find("gate: remote builder unreachable; running here")
        .expect("the run falls back");
    assert!(said[0].0 < fallback && fallback < said[1].0, "{outcome:?}");
    let store = store(&case).expect("the store was written");
    let mut filed: Vec<(String, usize)> = store["keys"]
        .as_object()
        .expect("the store holds its keys")
        .iter()
        .map(|(key, history)| (key.split(':').next().expect("a key names its place").to_owned(), samples(history).len()))
        .collect();
    filed.sort();
    assert_eq!(
        filed,
        [("local".to_owned(), 6), ("remote".to_owned(), 6)],
        "the runs that fell back are filed here: {store:#}"
    );
}

#[test]
fn a_store_that_cannot_be_written_is_said_once_and_keeps_the_runs_status() {
    let case = Case::new("durations-unwritten");
    quick_runs(&case, 1);
    let path = case.root().join("durations.json");
    // Held open with its delete not shared, the store still reads, but the write's move over it fails.
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .expect("the store can be opened");

    let failed = finish(case.gate(&run_words("out.log", &[], &[FAKE_TOOL, "exit", "3"])));
    drop(held);

    assert_eq!(failed.code, Some(3), "the store's write never changes the run's status: {failed:?}");
    let line = format!(r"(?m)^gate: could not write {} \(.+\)$", regex::escape(&shown(&path)));
    let said = Regex::new(&line).expect("the pattern is valid").find_iter(&failed.stderr).count();
    assert_eq!(said, 1, "{failed:?}");
    assert_eq!(keys(&case).len(), 1, "the store is as it was");
}

#[test]
fn a_rebuild_leaves_out_none_slot_runs_as_live_filing_does() {
    let case = Case::new("durations-rebuild-none");
    for _ in 0..3 {
        passing(case.gate(&none_words("out.log", &[], &QUICK)));
    }

    quick_runs(&case, 1);

    assert_eq!(
        samples(&history_ending(&case, QUICK_KEY)).len(),
        1,
        "the store the light run rebuilt holds it alone"
    );
    fs::remove_file(case.root().join("durations.json")).expect("the store can be removed");
    passing(case.gate(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"])));
    assert_eq!(
        samples(&history_ending(&case, QUICK_KEY)).len(),
        1,
        "a rebuild by another command's run leaves them out too"
    );
}

#[test]
fn two_runs_that_read_a_missing_store_rebuild_it_once() {
    let case = Case::new("durations-rebuild-once");
    // A release line that does not parse, which every rebuild says it skipped: the lines count the rebuilds.
    fs::write(case.root().join("events.jsonl"), "{\"event\":\"release\"}\n").expect("the journal can be seeded");
    let (started, go) = (case.folder().join("started"), case.folder().join("go"));
    let (started_word, go_word) = (shown(&started), shown(&go));
    let held = case.start(&heavy_words("held.log", &[], &[FAKE_TOOL, "mark", &started_word, "wait", &go_word]));
    // Its command runs only once it has taken its slot and read the missing store.
    wait_for_file(&started);

    let quick = passing(case.gate(&run_words("out.log", &[], &QUICK)));
    fs::write(&go, "").expect("the go file can be made");
    let held = held.finish();

    let skipped = "release line(s) of ";
    assert_eq!(quick.stderr.matches(skipped).count(), 1, "the first run to release rebuilds: {quick:?}");
    assert_eq!(held.code, Some(0), "{held:?}");
    assert!(
        !held.stderr.contains(skipped),
        "the second files into the rebuilt store and rebuilds nothing: {held:?}"
    );
    assert_eq!(keys(&case).len(), 2, "both runs are filed");
}

#[test]
fn worktrees_of_one_repo_share_a_key() {
    let case = Case::new("durations-worktrees");
    git_init(&case);
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "start"]);
    git(&case, &["worktree", "add", "--quiet", "wt"]);
    quick_runs(&case, 3);
    for _ in 0..2 {
        let outcome = passing(case.gate_in("wt", &run_words("out.log", &[], &QUICK)));
        assert_eq!(outcome.stderr, "", "{outcome:?}");
    }

    let outcome = passing(case.gate_in("wt", &run_words("out.log", &[], &QUICK)));

    assert_eq!(
        said_runs(&outcome.stderr),
        5,
        "the main checkout's runs count in the worktree: {outcome:?}"
    );
    let name = case
        .folder()
        .file_name()
        .expect("the case folder has a name")
        .to_string_lossy()
        .into_owned();
    assert_eq!(keys(&case), [format!("local::{name}{QUICK_KEY}")]);
}

/// A folder of its own in the system temp folder, outside any repository, removed when dropped.
struct Outside(PathBuf);

impl Drop for Outside {
    fn drop(&mut self) {
        // A folder left in the temp folder harms no later case, and a panic in drop would abort the run.
        drop(fs::remove_dir_all(&self.0));
    }
}

#[test]
fn a_cwd_outside_git_keys_on_its_full_path() {
    let case = Case::new("durations-outside-git");
    let outside = Outside(std::env::temp_dir().join(format!("gate-durations-outside-{}", std::process::id())));
    drop(fs::remove_dir_all(&outside.0));
    fs::create_dir_all(&outside.0).expect("the folder outside git can be made");
    let ceiling = outside.0.parent().expect("the temp folder has a parent");

    let mut gate = case.gate(&run_words("out.log", &[], &QUICK));
    gate.current_dir(&outside.0).env("GIT_CEILING_DIRECTORIES", ceiling);
    passing(gate);

    let path = shown(&outside.0).replace('\\', "/").to_ascii_lowercase();
    assert_eq!(keys(&case), [format!("local::{path}{QUICK_KEY}")]);
}

#[test]
fn a_torn_store_says_once_and_runs_on() {
    let case = Case::new("durations-torn");
    let path = case.root().join("durations.json");
    fs::write(&path, r#"{"v":1,"keyVersion":1,"keys":{"#).expect("a torn store can be written");

    let outcome = passing(case.gate(&run_words("out.log", &[], &QUICK)));

    let line = format!(
        r"\Agate: cannot read {} \(.+\); expected times are unavailable\n\z",
        regex::escape(&shown(&path))
    );
    assert!(Regex::new(&line).expect("the pattern is valid").is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(
        samples(&history_ending(&case, QUICK_KEY)).len(),
        1,
        "the store is rebuilt from the journal"
    );
}

#[test]
fn a_none_slot_run_says_nothing_and_records_nothing() {
    let case = Case::new("durations-none");
    quick_runs(&case, 5);

    let outcome = passing(case.gate(&none_words("out.log", &[], &QUICK)));

    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(samples(&history_ending(&case, QUICK_KEY)).len(), 5, "the none run is not filed");
}

#[test]
fn a_missing_store_is_rebuilt_from_the_journal() {
    let case = Case::new("durations-rebuilt");
    quick_runs(&case, 5);
    fs::remove_file(case.root().join("durations.json")).expect("the store can be removed");

    let silent = passing(case.gate(&run_words("out.log", &[], &QUICK)));

    assert_eq!(silent.stderr, "", "a missing store is no history, said by nothing: {silent:?}");
    assert_eq!(
        samples(&history_ending(&case, QUICK_KEY)).len(),
        6,
        "every release in the journal is filed"
    );
    assert!(!case.root().join("durations.rebuilding").exists(), "the rebuild marker is gone");
    let after = passing(case.gate(&run_words("out.log", &[], &QUICK)));
    assert_eq!(said_runs(&after.stderr), 6, "{after:?}");
}

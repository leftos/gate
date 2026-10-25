//! The learned durations: the expected-time line a gate says as it takes its slot, and `durations.json`, the store
//! each run's release is filed in, keyed by where the run ran, its class, its repo and its command.

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gate_state::{KernelObject, StateRoot};
use gate_win32::NamedMutex;
use regex::Regex;
use serde_json::Value;

use crate::routing::{heavy_words_with_limit, routed_case};
use crate::support::{
    Case, FAKE_TOOL, LOCAL_REASON, Outcome, USAGE, assert_in_order, critical_words, events, finish, git, git_init, heavy_words, log_texts,
    none_words, run_words, shown, wait_for_file, wait_until, write_counts,
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
    // The line is written to the log as it is said, so the tail shows it before the command's output.
    assert_in_order(&outcome.stdout, &["gate: expected", "\nran\n"]);
    let line = Regex::new(&format!(r"\A{EXPECTED}\n\z")).expect("the pattern is valid");
    assert!(line.is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(said_runs(&outcome.stderr), 5);
    let log = log_texts(&case.folder().join("out.log"));
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

/// `gate expect` of [`QUICK`] in `case`'s folder, `options` before `--`.
fn expect_quick(case: &Case, options: &[&str]) -> Outcome {
    let words: Vec<&str> = ["expect"].iter().chain(options).chain(&["--"]).chain(&QUICK).copied().collect();
    finish(case.gate(&words))
}

/// The run count of the one line `gate expect` printed, the expected-time line for here, with status 0 and nothing on
/// stderr.
fn expected_here(outcome: &Outcome) -> u64 {
    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let line = Regex::new(&format!(r"\A{EXPECTED} here\n\z")).expect("the pattern is valid");
    let captures = line.captures(&outcome.stdout);
    assert!(captures.is_some(), "one line for here: {outcome:?}");
    captures.expect("checked above")[1].parse().expect("the run count is a number")
}

/// Asserts `gate expect` printed `no history` alone, with status 0 and nothing on stderr.
fn assert_no_history(outcome: &Outcome) {
    assert_eq!(
        (outcome.code, outcome.stdout.as_str(), outcome.stderr.as_str()),
        (Some(0), "no history\n", ""),
        "{outcome:?}"
    );
}

#[test]
fn gate_expect_prints_the_figure_here() {
    let case = Case::new("expect-here");
    quick_runs(&case, 5);

    let outcome = expect_quick(&case, &[]);

    assert_eq!(expected_here(&outcome), 5);
}

#[test]
fn gate_expect_prints_each_builder_that_has_history() {
    let case = Case::new("expect-builders");
    quick_runs(&case, 5);
    let mut seeded = store(&case).expect("the store was written");
    let keys = seeded["keys"].as_object_mut().expect("the store holds its keys");
    let (local, history) = keys
        .iter()
        .next()
        .map(|(key, history)| (key.clone(), history.clone()))
        .expect("the runs are filed");
    // The key after `local`: its empty class, its folder and its command.
    let rest = local.strip_prefix("local").expect("the runs were filed here").to_owned();
    keys.insert(format!("remote:me@beta{rest}"), history.clone());
    keys.insert(format!("remote:alpha{rest}"), history.clone());
    keys.insert(format!("remote:gamma:crit{}", &rest[1..]), history.clone());
    keys.insert(format!("remote:delta{}", rest.replace(QUICK_KEY, " | fake-tool exit 1")), history);
    let path = case.root().join("durations.json");
    fs::write(&path, seeded.to_string()).expect("the store can be seeded");

    let outcome = expect_quick(&case, &[]);

    assert_eq!((outcome.code, outcome.stderr.as_str()), (Some(0), ""), "{outcome:?}");
    let here = outcome
        .stdout
        .lines()
        .next()
        .and_then(|line| line.strip_suffix(" here"))
        .expect("here comes first");
    assert!(
        Regex::new(&format!(r"\A{EXPECTED}\z")).expect("the pattern is valid").is_match(here),
        "{outcome:?}"
    );
    assert_eq!(
        outcome.stdout,
        format!("{here} here\n{here} on alpha\n{here} on me@beta\n"),
        "here, then each builder with history of this command and class, in host order"
    );

    seeded["keys"].as_object_mut().expect("the store holds its keys").remove(&local);
    fs::write(&path, seeded.to_string()).expect("the store can be seeded");
    let builders_only = expect_quick(&case, &[]);
    assert_eq!(
        builders_only.stdout,
        format!("{here} on alpha\n{here} on me@beta\n"),
        "a place without history has no line: {builders_only:?}"
    );
}

#[test]
fn gate_expect_without_history_prints_no_history_and_exits_0() {
    let case = Case::new("expect-no-history");

    assert_no_history(&expect_quick(&case, &[]));
    assert_eq!(case.root_entries(), Vec::<String>::new(), "a missing store is not rebuilt");

    quick_runs(&case, 4);
    assert_no_history(&expect_quick(&case, &[]));
    quick_runs(&case, 1);
    assert_no_history(&finish(case.gate(&["expect", "--", FAKE_TOOL, "out", "hi"])));

    fs::write(case.root().join("durations.json"), r#"{"v":1,"keyVersion":999,"keys":{}}"#).expect("a newer store can be written");
    assert_no_history(&expect_quick(&case, &[]));

    let older = r#"{"v":1,"keyVersion":0,"keys":{}}"#;
    fs::write(case.root().join("durations.json"), older).expect("an older store can be written");
    let entries = sorted_entries(&case);
    assert_no_history(&expect_quick(&case, &[]));
    assert_eq!(sorted_entries(&case), entries, "an older store is not rebuilt");
    assert_eq!(
        fs::read_to_string(case.root().join("durations.json")).expect("the store reads"),
        older,
        "an older store is left as it was"
    );
}

#[test]
fn gate_expect_with_a_torn_store_prints_no_history_and_says_why() {
    let case = Case::new("expect-torn");
    let path = case.root().join("durations.json");
    let torn = r#"{"v":1,"keyVersion":1,"keys":{"#;
    fs::write(&path, torn).expect("a torn store can be written");

    let outcome = expect_quick(&case, &[]);

    assert_eq!((outcome.code, outcome.stdout.as_str()), (Some(0), "no history\n"), "{outcome:?}");
    let line = format!(
        r"\Agate: cannot read {} \(.+\); expected times are unavailable\n\z",
        regex::escape(&shown(&path))
    );
    assert!(Regex::new(&line).expect("the pattern is valid").is_match(&outcome.stderr), "{outcome:?}");
    assert_eq!(fs::read_to_string(&path).expect("the store reads"), torn, "the store is left as it was");
    assert_eq!(case.root_entries(), ["durations.json"], "nothing is rebuilt");
}

/// The entries of `case`'s state root, sorted.
fn sorted_entries(case: &Case) -> Vec<String> {
    let mut entries = case.root_entries();
    entries.sort();
    entries
}

#[test]
fn gate_expect_queues_nothing_and_writes_nothing() {
    let case = Case::new("expect-reader");
    write_counts(&case.root(), r#"{"v":1,"light":1}"#);
    quick_runs(&case, 5);
    let entries = sorted_entries(&case);
    let journal = fs::read(case.root().join("events.jsonl")).expect("the journal was written");
    let durations = fs::read(case.root().join("durations.json")).expect("the store was written");
    let root = StateRoot::new(&shown(&case.root()), None);
    let mutexes =
        [KernelObject::LightSlot(0), KernelObject::Journal].map(|object| NamedMutex::open(&root.object_name(object)).expect("the mutex opens"));
    // With the one light slot and the journal mutex held, a gate that queued would wait and one that journalled would
    // say it could not write the journal.
    let held = mutexes
        .each_ref()
        .map(|mutex| mutex.acquire(Duration::from_secs(5)).expect("the mutex is free"));

    let outcome = expect_quick(&case, &["--slot", "light"]);
    drop(held);

    assert_eq!(expected_here(&outcome), 5);
    assert_eq!(sorted_entries(&case), entries, "no entry of the state root is made or removed");
    assert_eq!(
        fs::read(case.root().join("events.jsonl")).expect("the journal reads"),
        journal,
        "no journal line"
    );
    assert_eq!(
        fs::read(case.root().join("durations.json")).expect("the store reads"),
        durations,
        "no store write"
    );
    assert!(!case.local_app_data().exists(), "nor is the default state root made");
}

#[test]
fn gate_expect_needs_a_command() {
    let case = Case::new("expect-no-command");
    for words in [&["expect"][..], &["expect", "--"], &["expect", "--slot", "light", "--"]] {
        let outcome = finish(case.gate(words));

        assert_eq!(
            (outcome.code, outcome.stdout.as_str(), outcome.stderr),
            (Some(2), "", format!("gate: missing the command after --\n{USAGE}")),
            "{words:?}"
        );
    }
    assert_eq!(case.root_entries(), Vec::<String>::new());
}

#[test]
fn gate_expect_keys_crit_with_slot_critical() {
    let case = Case::new("expect-critical");
    for _ in 0..5 {
        passing(case.gate(&critical_words("out.log", &[], &QUICK)));
    }

    let plain = expect_quick(&case, &[]);
    let critical = expect_quick(&case, &["--slot", "critical"]);
    let spelled = expect_quick(&case, &["--slot=CRITICAL"]);

    assert_no_history(&plain);
    assert_eq!(expected_here(&critical), 5);
    assert_eq!(spelled.stdout, critical.stdout, "the slot is read as a run reads it: {spelled:?}");
}

#[test]
fn gate_expect_matches_the_run_line() {
    let case = Case::new("expect-matches-run");
    quick_runs(&case, 5);

    let expected = expect_quick(&case, &[]);
    let run = passing(case.gate(&run_words("out.log", &[], &QUICK)));

    assert_eq!(expected_here(&expected), 5);
    assert_eq!(
        expected.stdout,
        format!("{} here\n", run.stderr.trim_end_matches('\n')),
        "the figure is the line the next run says: {run:?}"
    );
}

#[test]
fn gate_expect_keys_a_worktree_as_the_run_does() {
    let case = Case::new("expect-worktree");
    git_init(&case);
    git(&case, &["commit", "--quiet", "--allow-empty", "-m", "start"]);
    git(&case, &["worktree", "add", "--quiet", "wt"]);
    // Git places `wt` as its own repo's worktree, where its path's shape places it under this checkout. A word under the
    // worktree, one under `.tmp/` and one under the system temp folder each key as the run keys them only when expect
    // places the folder by git and knows the temp root.
    let under_worktree = shown(&case.folder().join("wt").join("x"));
    let under_temp = shown(&std::env::temp_dir().join("y"));
    let command = [FAKE_TOOL, "out", under_worktree.as_str(), "out", ".tmp/y", "out", under_temp.as_str()];
    for _ in 0..5 {
        let outcome = passing(case.gate_in("wt", &run_words("out.log", &[], &command)));
        assert_eq!(outcome.stderr, "", "a key with fewer than 5 runs says nothing: {outcome:?}");
    }
    let words: Vec<&str> = ["expect", "--"].iter().chain(&command).copied().collect();

    let expected = finish(case.gate_in("wt", &words));
    let run = passing(case.gate_in("wt", &run_words("out.log", &[], &command)));

    assert_eq!(expected_here(&expected), 5);
    assert_eq!(
        expected.stdout,
        format!("{} here\n", run.stderr.trim_end_matches('\n')),
        "the figure is the line the next run in the worktree says: {run:?}"
    );
}

/// The one claim or ticket file in the folder `dir` of `case`'s state root, as JSON.
fn only_file(case: &Case, dir: &str) -> Value {
    let files: Vec<PathBuf> = fs::read_dir(case.root().join(dir))
        .expect("the folder can be listed")
        .map(|entry| entry.expect("the folder's entry can be read").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    assert_eq!(files.len(), 1, "one file in {dir}: {files:?}");
    serde_json::from_str(&fs::read_to_string(&files[0]).expect("the file can be read")).expect("the file is JSON")
}

/// What a holder and a waiter wrote as the waiter queued behind it and then took the slot: the holder's claim and the
/// waiter's ticket as both ran, the waiter's stderr up to its waiting line, and the waiter's own claim once it held the
/// slot.
struct Queued {
    claim: Value,
    ticket: Value,
    waiting: String,
    waiter_claim: Value,
}

/// A light run of `command`, which waits for the file `go`.
struct Waiting<'c> {
    command: &'c [&'c str],
    go: &'c Path,
}

/// Runs a light `holder` on the case's one light slot and a light `waiter` behind it, reads what [`Queued`] holds as
/// both run, then lets the holder end, reads the waiter's claim once it holds the slot, and lets the waiter end.
fn hold_and_queue(case: &Case, holder: &Waiting<'_>, waiter: &Waiting<'_>) -> Queued {
    write_counts(&case.root(), r#"{"v":1,"light":1}"#);
    let claim_path = case.root().join("slots").join("gate-light-slot-0.json");
    let holding = case.start(&run_words("holder.log", &[], holder.command));
    // A claim is written whole, through a file moved over it.
    wait_for_file(&claim_path);
    let queued = case.start(&run_words("waiter.log", &[], waiter.command));
    let waiting = queued.wait_for_stderr_where("the waiting line", |stderr| stderr.contains("gate: waiting for a light slot"));
    let (claim, ticket) = (only_file(case, "slots"), only_file(case, "waiting"));
    fs::write(holder.go, "").expect("the holder's go file can be made");
    let pid = queued.id();
    wait_until("the waiter's claim", || {
        fs::read_to_string(&claim_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|claim| claim["pid"] == pid)
    });
    let waiter_claim = only_file(case, "slots");
    fs::write(waiter.go, "").expect("the waiter's go file can be made");
    for outcome in [holding.finish(), queued.finish()] {
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
    }
    Queued {
        claim,
        ticket,
        waiting,
        waiter_claim,
    }
}

/// Runs `run` 5 times in a light slot of `case` with its go file there, then removes it.
fn five_runs(case: &Case, run: &Waiting<'_>) {
    fs::write(run.go, "").expect("the go file can be made");
    for _ in 0..5 {
        passing(case.gate(&run_words("out.log", &[], run.command)));
    }
    fs::remove_file(run.go).expect("the go file can be removed");
}

/// The one key of `case`'s store that ends with `ending`.
fn key_ending(case: &Case, ending: &str) -> String {
    let matching: Vec<String> = keys(case).into_iter().filter(|key| key.ends_with(ending)).collect();
    assert_eq!(matching.len(), 1, "one key ends with {ending:?}: {matching:?}");
    matching[0].clone()
}

/// Asserts `expected` is the field of a key with history: `p50`, `p90`, `n` and `key`, in that order, `n` runs of `key`.
fn assert_expected(expected: &Value, key: &str, runs: u64) {
    let field = expected.as_object().expect("expected is an object");
    let names: Vec<&str> = field.keys().map(String::as_str).collect();
    assert_eq!(names, ["p50", "p90", "n", "key"], "{expected}");
    let (p50, p90) = (expected["p50"].as_f64(), expected["p90"].as_f64());
    assert!(p50.is_some_and(|p50| p50 >= 0.0) && p90 >= p50, "seconds: {expected}");
    assert_eq!((&expected["n"], &expected["key"]), (&Value::from(runs), &Value::from(key)), "{expected}");
}

#[test]
fn the_claim_and_the_ticket_carry_what_the_store_expects() {
    let case = Case::new("durations-expected-field");
    let (go_held, go_ran) = (case.folder().join("go-held"), case.folder().join("go-ran"));
    let (held_word, ran_word) = (shown(&go_held), shown(&go_ran));
    let held: [&str; 5] = [FAKE_TOOL, "wait", &held_word, "out", "held"];
    let ran: [&str; 5] = [FAKE_TOOL, "wait", &ran_word, "out", "ran"];
    let (holder, waiter) = (
        Waiting {
            command: &held,
            go: &go_held,
        },
        Waiting { command: &ran, go: &go_ran },
    );
    five_runs(&case, &waiter);
    let key = key_ending(&case, " out ran");

    let queued = hold_and_queue(&case, &holder, &waiter);

    assert_eq!(
        queued.claim.get("expected"),
        None,
        "the holder's command has no history: {}",
        queued.claim
    );
    assert_expected(&queued.ticket["expected"], &key, 5);
    assert_eq!(
        queued.waiter_claim["expected"], queued.ticket["expected"],
        "the waiter's ticket and claim carry one estimate of one store read"
    );
}

#[test]
fn without_history_neither_the_claim_nor_the_ticket_has_expected() {
    let case = Case::new("durations-no-expected-field");
    let (go_held, go_ran) = (case.folder().join("go-held"), case.folder().join("go-ran"));
    let (held_word, ran_word) = (shown(&go_held), shown(&go_ran));
    let held: [&str; 5] = [FAKE_TOOL, "wait", &held_word, "out", "held"];
    let ran: [&str; 5] = [FAKE_TOOL, "wait", &ran_word, "out", "ran"];
    let (holder, waiter) = (
        Waiting {
            command: &held,
            go: &go_held,
        },
        Waiting { command: &ran, go: &go_ran },
    );

    let queued = hold_and_queue(&case, &holder, &waiter);

    for file in [&queued.claim, &queued.ticket, &queued.waiter_claim] {
        assert_eq!(file.get("expected"), None, "{file}");
    }
    assert!(
        queued.waiting.contains("gate: waiting for a light slot (1 busy)\n"),
        "no ETA without the holder's history: {}",
        queued.waiting
    );
}

#[test]
fn a_waiter_says_when_the_first_holder_frees_its_slot() {
    let case = Case::new("durations-eta");
    let (go_held, go_ran) = (case.folder().join("go-held"), case.folder().join("go-ran"));
    let (held_word, ran_word) = (shown(&go_held), shown(&go_ran));
    let held: [&str; 5] = [FAKE_TOOL, "wait", &held_word, "out", "held"];
    let ran: [&str; 5] = [FAKE_TOOL, "wait", &ran_word, "out", "ran"];
    let (holder, waiter) = (
        Waiting {
            command: &held,
            go: &go_held,
        },
        Waiting { command: &ran, go: &go_ran },
    );
    five_runs(&case, &holder);
    // Every run of the holder's key took 240 s, so its usual run time is 4 min.
    let mut seeded = store(&case).expect("the store was written");
    for history in seeded["keys"].as_object_mut().expect("the store holds its keys").values_mut() {
        for sample in history["samples"].as_array_mut().expect("a history holds its samples") {
            sample["wall"] = Value::from(240.0);
        }
    }
    fs::write(case.root().join("durations.json"), seeded.to_string()).expect("the store can be seeded");

    let queued = hold_and_queue(&case, &holder, &waiter);

    let line = Regex::new(r"(?m)^gate: waiting for a light slot \(1 busy, the first frees in about \d+ min\)$").expect("the pattern is valid");
    assert!(line.is_match(&queued.waiting), "{}", queued.waiting);
    assert_eq!(queued.claim["expected"]["p50"], 240.0, "{}", queued.claim);
}

#[test]
fn a_critical_gates_claim_and_ticket_carry_the_crit_key() {
    let case = Case::new("durations-critical-field");
    let (go_held, go_ran) = (case.folder().join("go-held"), case.folder().join("go-ran"));
    let (held_word, ran_word) = (shown(&go_held), shown(&go_ran));
    let held = [FAKE_TOOL, "wait", held_word.as_str()];
    let ran: [&str; 5] = [FAKE_TOOL, "wait", &ran_word, "out", "ran"];
    fs::write(&go_ran, "").expect("the go file can be made");
    for _ in 0..5 {
        passing(case.gate(&critical_words("out.log", &[], &ran)));
    }
    fs::remove_file(&go_ran).expect("the go file can be removed");
    let key = key_ending(&case, " out ran");
    assert!(key.starts_with("local:crit:"), "{key}");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let holder = case.start(&heavy_words("holder.log", &[], &held));
    wait_for_file(&case.root().join("slots").join("gate-slot-0.json"));

    let mut critical = case.start(&critical_words("critical.log", &[], &ran));
    critical.wait_for_stderr("gate: waiting for the machine (critical)");
    let ticket = only_file(&case, "waiting");
    fs::write(&go_held, "").expect("the holder's go file can be made");
    // The expected-time line comes once the critical claim is written.
    critical.wait_for_stderr_while_running("the expected-time line", |stderr| stderr.contains("gate: expected about"));
    let claim = only_file(&case, "slots");
    fs::write(&go_ran, "").expect("the go file can be made");
    let (holder, critical) = (holder.finish(), critical.finish());

    assert_eq!((holder.code, critical.code), (Some(0), Some(0)), "{holder:?}\n{critical:?}");
    assert_eq!((&ticket["kind"], &claim["kind"]), (&Value::from("critical"), &Value::from("critical")));
    assert_expected(&ticket["expected"], &key, 5);
    assert_expected(&claim["expected"], &key, 5);
}

#[test]
fn a_routed_run_that_falls_back_claims_and_queues_here_with_the_local_key() {
    let case = routed_case("durations-fallback-field");
    let steps = |name: &str, line: &str| {
        let path = case.folder().join(name);
        fs::write(&path, line).expect("the step file can be written");
        shown(&path)
    };
    let (passed, refused) = (steps("passed.txt", "status 0"), steps("refused.txt", "refuse unreachable"));
    let (go_local, go_ran) = (case.folder().join("go-local"), case.folder().join("go-ran"));
    let (local_word, ran_word) = (shown(&go_local), shown(&go_ran));
    let command = [r".\tool.exe", "wait", ran_word.as_str()];
    fs::write(&go_ran, "").expect("the go file can be made");
    for extra in [&["--test-remote-client", passed.as_str()], &["--test-remote-client", refused.as_str()]] {
        for _ in 0..5 {
            passing(case.gate(&heavy_words_with_limit("out.log", extra, &command)));
        }
    }
    fs::remove_file(&go_ran).expect("the go file can be removed");
    let mut keys = keys(&case);
    keys.sort();
    assert!(keys[0].starts_with("local:") && keys[1].starts_with("remote:builder:"), "{keys:?}");
    write_counts(&case.root(), r#"{"v":1,"heavy":1}"#);
    let holder = case.start(&heavy_words_with_limit(
        "local.log",
        &["--local", LOCAL_REASON[0], LOCAL_REASON[1]],
        &[r".\tool.exe", "wait", &local_word],
    ));
    wait_for_file(&case.root().join("slots").join("gate-slot-0.json"));

    let mut routed = case.start(&heavy_words_with_limit("routed.log", &["--test-remote-client", &refused], &command));
    routed.wait_for_stderr("gate: waiting for a heavy slot");
    let ticket = only_file(&case, "waiting");
    fs::write(&go_local, "").expect("the local go file can be made");
    // The local take's expected-time line, the second of the run, comes once its claim is written.
    routed.wait_for_stderr_while_running("the local take's expected-time line", |stderr| {
        stderr.matches("gate: expected about").count() == 2
    });
    let claim = only_file(&case, "slots");
    fs::write(&go_ran, "").expect("the go file can be made");
    let (holder, routed) = (holder.finish(), routed.finish());

    assert_eq!((holder.code, routed.code), (Some(0), Some(0)), "{holder:?}\n{routed:?}");
    assert_eq!((ticket.get("remote"), &claim["kind"]), (None, &Value::from("heavy")), "{ticket}\n{claim}");
    assert_expected(&ticket["expected"], &keys[0], 5);
    assert_expected(&claim["expected"], &keys[0], 5);
}

#[test]
fn a_routed_runs_claim_and_ticket_carry_the_builders_key() {
    let case = routed_case("durations-routed-field");
    let steps = |name: &str, lines: &[String]| {
        let path = case.folder().join(name);
        fs::write(&path, lines.join("\n")).expect("the step file can be written");
        shown(&path)
    };
    let (call, go) = (case.folder().join("call.json"), case.folder().join("go"));
    let passed = steps("passed.txt", &["status 0".to_owned()]);
    let held = steps("held.txt", &[format!("record {}", shown(&call)), format!("wait {}", shown(&go))]);
    let command = [r".\tool.exe", "exit", "0"];
    for _ in 0..5 {
        passing(case.gate(&heavy_words_with_limit("out.log", &["--test-remote-client", &passed], &command)));
    }
    let key = keys(&case).pop().expect("the runs are filed");
    assert!(key.starts_with("remote:builder:"), "{key}");

    let holder = case.start(&heavy_words_with_limit("holder.log", &["--test-remote-client", &held], &command));
    // The client runs once the claim is written.
    wait_for_file(&call);
    let waiter = case.start(&heavy_words_with_limit("waiter.log", &["--test-remote-client", &passed], &command));
    waiter.wait_for_stderr("gate: waiting for a remote slot on builder");
    let (claim, ticket) = (only_file(&case, "slots"), only_file(&case, "waiting"));
    fs::write(&go, "").expect("the go file can be made");
    let (holder, waiter) = (holder.finish(), waiter.finish());

    assert_eq!((holder.code, waiter.code), (Some(0), Some(0)), "{holder:?}\n{waiter:?}");
    assert_eq!(claim["kind"], "remote", "{claim}");
    assert_eq!(ticket["remote"], true, "{ticket}");
    assert_expected(&claim["expected"], &key, 5);
    assert_expected(&ticket["expected"], &key, 5);
}

/// Rewrites every sample of the one key of `case`'s store ending with `ending` to `wall` wall seconds and `adjusted`
/// load-adjusted seconds, none when `adjusted` is `None`.
pub(crate) fn seed(case: &Case, ending: &str, wall: f64, adjusted: Option<f64>) {
    let mut seeded = store(case).expect("the store was written");
    let keys = seeded["keys"].as_object_mut().expect("the store holds its keys");
    let matching: Vec<&mut Value> = keys
        .iter_mut()
        .filter(|(key, _)| key.ends_with(ending))
        .map(|(_, history)| history)
        .collect();
    assert_eq!(matching.len(), 1, "one key ends with {ending:?}");
    for history in matching {
        for sample in history["samples"].as_array_mut().expect("a history holds its samples") {
            let sample = sample.as_object_mut().expect("a sample is an object");
            sample.insert("wall".to_owned(), Value::from(wall));
            match adjusted {
                Some(adjusted) => sample.insert("adjusted".to_owned(), Value::from(adjusted)),
                None => sample.remove("adjusted"),
            };
        }
    }
    fs::write(case.root().join("durations.json"), seeded.to_string()).expect("the store can be seeded");
}

/// The command [`held_run`] runs, held on the go file `go`, and the ending of its key.
const HELD_KEY: &str = " out ran";

/// A light run with `--timeout timeout` of a command held on a go file, once five runs of it made its key's history
/// and [`seed`] rewrote it to `seeded` wall and adjusted seconds; with no history when `seeded` is `None`. The run's
/// claim as it held its slot, and its outcome. The run is given `extra` before `--`, and is let go once its stderr
/// holds what `said_first` looks for while the command still waits, so a line it finds came before the command's output.
fn held_run(case: &Case, timeout: &str, seeded: Option<(f64, Option<f64>)>, extra: &[&str], said_first: impl Fn(&str) -> bool) -> (Value, Outcome) {
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let command: [&str; 5] = [FAKE_TOOL, "wait", &go_word, "out", "ran"];
    if let Some((wall, adjusted)) = seeded {
        fs::write(&go, "").expect("the go file can be made");
        for _ in 0..5 {
            passing(case.gate(&run_words("out.log", &[], &command)));
        }
        fs::remove_file(&go).expect("the go file can be removed");
        seed(case, HELD_KEY, wall, adjusted);
    }
    let mut words = run_words("out.log", extra, &command);
    words[4] = timeout;
    let mut running = case.start(&words);
    wait_for_file(&case.root().join("slots").join("gate-light-slot-0.json"));
    let claim = only_file(case, "slots");
    running.wait_for_stderr_while_running("the lines said before the command's output", said_first);
    fs::write(&go, "").expect("the go file can be made");
    (claim, running.finish())
}

/// The log of `case`'s runs.
fn log(case: &Case) -> String {
    log_texts(&case.folder().join("out.log"))
}

#[test]
fn an_auto_ceiling_with_history_is_raised_and_said() {
    let case = Case::new("durations-auto-raised");

    let (claim, outcome) = held_run(&case, "auto:900", Some((1.0, Some(840.0))), &[], |_| true);

    let raised = "gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)\n";
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(claim["timeoutSeconds"], 2520.0, "{claim}");
    for said in [&outcome.stderr, &log(&case)] {
        assert_eq!(said.matches(raised).count(), 1, "{said}");
        assert_in_order(said, &["gate: expected about 1 s (usual 1 s, p90 1 s, 5 runs)\n", raised]);
    }
    assert!(outcome.stdout.contains(", ceiling 2520 s). Full output: "), "{outcome:?}");
}

#[test]
fn an_auto_ceiling_without_history_runs_on_its_floor() {
    let case = Case::new("durations-auto-floor");

    let (claim, outcome) = held_run(&case, "auto:900", None, &[], |_| true);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(claim["timeoutSeconds"], 900.0, "{claim}");
    assert_eq!(outcome.stderr, "", "neither the raised ceiling nor a hint: {outcome:?}");
    assert!(outcome.stdout.contains(", ceiling 900 s). Full output: "), "{outcome:?}");
}

#[test]
fn an_auto_floor_never_lowers_the_ceiling() {
    let case = Case::new("durations-auto-never-lowers");

    let (claim, outcome) = held_run(&case, "auto:900", Some((1.0, Some(1.0))), &[], |_| true);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(claim["timeoutSeconds"], 900.0, "3x 1 s is below the floor: {claim}");
    assert_eq!(said_runs(&outcome.stderr), 5, "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: ceiling"), "{outcome:?}");
    assert!(outcome.stdout.contains(", ceiling 900 s). Full output: "), "{outcome:?}");
}

#[test]
fn a_ceiling_below_the_load_adjusted_p90_is_hinted_once_after_the_expected_line() {
    let case = Case::new("durations-ceiling-hint");

    let hint = "gate: the ceiling 5 min is below this command's load-adjusted p90 of 10 min\n";

    // The command writes nothing until the go file comes, so the hint said while it waits came before its output.
    let (claim, outcome) = held_run(&case, "300", Some((600.0, Some(600.0))), &[], |stderr| {
        stderr.contains("gate: the ceiling")
    });

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(claim["timeoutSeconds"], 300.0, "a given ceiling is never resolved: {claim}");
    assert_eq!(outcome.stderr.matches(hint).count(), 1, "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &["gate: expected about 10 min (usual 10 min, p90 10 min, 5 runs)\n", hint],
    );
    let log = log(&case);
    assert_eq!(log.matches(hint).count(), 1, "{log}");
    assert_in_order(&log, &["gate: expected about 10 min", hint]);
}

#[test]
fn a_ceiling_below_a_wall_p90_is_not_hinted_when_the_load_adjusted_p90_is_below_it() {
    let case = Case::new("durations-ceiling-hint-adjusted");

    // The reported bug: a wall p90 of 10 min under load, whose load-adjusted p90 is 1 min, flags a 5 min ceiling.
    let (_claim, outcome) = held_run(&case, "300", Some((600.0, Some(60.0))), &[], |_| true);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: expected about 10 min (usual 10 min, p90 10 min, 5 runs)"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("gate: the ceiling"), "{outcome:?}");
    assert!(!log(&case).contains("gate: the ceiling"), "{}", log(&case));
}

#[test]
fn no_ceiling_hint_at_or_above_the_load_adjusted_p90_or_without_history_or_without_a_slot() {
    let case = Case::new("durations-no-ceiling-hint");
    quick_runs(&case, 5);
    seed(&case, QUICK_KEY, 600.0, Some(600.0));
    let with_timeout = |words: Vec<&'static str>, timeout: &'static str| {
        let mut words = words;
        words[4] = timeout;
        passing(case.gate(&words))
    };

    let at_p90 = with_timeout(run_words("out.log", &[], &QUICK), "600");
    let no_slot = with_timeout(none_words("out.log", &[], &QUICK), "300");
    let fresh = Case::new("durations-no-ceiling-hint-fresh");
    let mut words = run_words("out.log", &[], &QUICK);
    words[4] = "1";
    let no_history = passing(fresh.gate(&words));

    assert!(at_p90.stderr.contains("gate: expected about 10 min"), "{at_p90:?}");
    for outcome in [&at_p90, &no_slot, &no_history] {
        assert!(!outcome.stderr.contains("gate: the ceiling"), "{outcome:?}");
    }
    assert_eq!(no_slot.stderr, "", "{no_slot:?}");
    assert_eq!(no_history.stderr, "", "{no_history:?}");
}

#[test]
fn a_run_past_3x_its_usual_p90_is_hinted_once_and_not_killed() {
    let case = Case::new("durations-hang-hint");
    let hint = Regex::new(r"gate: still running after \d+ s, \d+x its usual p90 of 1 s; running here for \d+ s\n").expect("the pattern is valid");

    // Sampled every 100 ms, the run passes 3x its p90 of 1 s while its command waits.
    let (_, outcome) = held_run(&case, "3600", Some((1.0, None)), &["--test-sample-ms", "100"], |stderr| {
        hint.is_match(stderr)
    });

    assert_eq!(outcome.code, Some(0), "the hint kills nothing: {outcome:?}");
    assert_eq!(hint.find_iter(&outcome.stderr).count(), 1, "{outcome:?}");
    let log = log(&case);
    assert_eq!(hint.find_iter(&log).count(), 1, "{log}");
    assert!(!outcome.stderr.contains("TIMED OUT") && !log.contains("TIMED OUT"), "{outcome:?}");
}

#[test]
fn a_run_that_ends_before_3x_its_p90_gets_no_hang_hint() {
    let case = Case::new("durations-no-hang-hint");

    let (_, outcome) = held_run(&case, "3600", Some((600.0, None)), &["--test-sample-ms", "100"], |stderr| {
        stderr.contains("gate: expected about 10 min")
    });

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: still running"), "{outcome:?}");
    assert!(!log(&case).contains("gate: still running"), "{outcome:?}");
}

#[test]
fn a_routed_auto_run_without_builder_adjusted_seconds_runs_on_its_floor() {
    let case = routed_case("durations-routed-auto");
    let steps = |name: &str, lines: &[String]| {
        let path = case.folder().join(name);
        fs::write(&path, lines.join("\n")).expect("the step file can be written");
        shown(&path)
    };
    let (call, go) = (case.folder().join("call.json"), case.folder().join("go"));
    let passed = steps("passed.txt", &["status 0".to_owned()]);
    let held = steps("held.txt", &[format!("record {}", shown(&call)), format!("wait {}", shown(&go))]);
    let command = [r".\tool.exe", "exit", "0"];
    for _ in 0..5 {
        passing(case.gate(&heavy_words_with_limit("out.log", &["--test-remote-client", &passed], &command)));
    }
    // A wall p99 of 14 min would raise the floor were it read in place of the adjusted p99 the builder's runs lack.
    seed(&case, " | tool exit 0", 840.0, None);
    let mut words = heavy_words_with_limit("held.log", &["--test-remote-client", &held], &command);
    words[4] = "auto:900";

    let running = case.start(&words);
    wait_for_file(&call);
    let claim = only_file(&case, "slots");
    fs::write(&go, "").expect("the go file can be made");
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        (&claim["kind"], &claim["timeoutSeconds"]),
        (&Value::from("remote"), &Value::from(900.0)),
        "{claim}"
    );
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&call).expect("the client recorded its call")).expect("the record is JSON");
    assert_eq!(recorded["timeout"], 900, "{recorded}");
    assert!(outcome.stderr.contains("gate: expected about 14 min"), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate: ceiling"), "{outcome:?}");
    assert!(outcome.stdout.contains(", ceiling 900 s; command "), "{outcome:?}");
}

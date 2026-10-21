//! The journal a run writes: its v1 lines in order with their verdict and quoted command, the state root's own journal
//! mutex, and the rollover and archive while other gates append.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::Duration;

use flate2::read::GzDecoder;
use gate_args::command_text;
use gate_state::{JOURNAL_CAP_BYTES, KernelObject, StateRoot};
use gate_win32::NamedMutex;
use regex::Regex;
use serde_json::Value;

use crate::support::{
    CORE_SECOND, Case, FAKE_TOOL, Outcome, assert_in_order, cpu_ticks, finish, none_words, run_words, shown, wait_for_journal_line, wait_until,
};

/// The words of a light run logging to `log` whose command exits with `status`.
fn exiting<'a>(log: &'a str, status: &'a str) -> Vec<&'a str> {
    run_words(log, &[], &[FAKE_TOOL, "exit", status])
}

/// The journal's `command` text of `words`, each quoted as `Format-CommandText` quotes it.
fn quoted(words: &[&str]) -> String {
    command_text(&words.iter().map(OsString::from).collect::<Vec<_>>())
}

/// JSON lines of `total` bytes in all, each ending in a line feed, every thousandth a `release` and the rest `queue`: a
/// handful of release lines near the cap, so the archive written inside the journal mutex's hold is little work. A
/// release line has the time, folder and command a durations rebuild reads, and no verdict, so a rebuild reads it
/// without a word and files nothing of it.
fn seed_lines(total: u64) -> Vec<String> {
    const LINE: usize = 1000;
    const RELEASE_EVERY: usize = 1000;
    const RELEASE_FIELDS: &str = r#","time":"2026-10-03T12:00:00.0000000Z","cwd":"seed","command":"seed""#;
    let mut left = usize::try_from(total).expect("a seed size fits in memory");
    let mut lines = Vec::new();
    while left > 0 {
        let size = if left >= 2 * LINE { LINE } else { left };
        let (event, fields) = if lines.len().is_multiple_of(RELEASE_EVERY) {
            ("release", RELEASE_FIELDS)
        } else {
            ("queue", "")
        };
        let base = format!(r#"{{"v":1,"event":"{event}"{fields},"seed":{},"pad":""}}"#, lines.len());
        let pad = "x".repeat(size - 1 - base.len());
        lines.push(format!(r#"{{"v":1,"event":"{event}"{fields},"seed":{},"pad":"{pad}"}}"#, lines.len()));
        left -= size;
    }
    lines
}

fn text(lines: &[String]) -> String {
    lines.iter().flat_map(|line| [line.as_str(), "\n"]).collect()
}

fn lines_of(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("the file can be read")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn parsed(line: &str) -> Value {
    serde_json::from_str(line).expect("a journal line is JSON")
}

fn release_lines(lines: &[String]) -> Vec<String> {
    lines.iter().filter(|line| parsed(line)["event"] == "release").cloned().collect()
}

fn gunzip(path: &Path) -> String {
    let mut text = String::new();
    GzDecoder::new(File::open(path).expect("the archive can be opened"))
        .read_to_string(&mut text)
        .expect("the archive is gzip text");
    text
}

/// Whether `name` is `events-<yyyyMMddTHHmmssZ>.jsonl.gz`.
fn is_archive_name(name: &str) -> bool {
    name.strip_prefix("events-")
        .and_then(|rest| rest.strip_suffix(".jsonl.gz"))
        .is_some_and(|stamp| {
            stamp.len() == 16
                && stamp.bytes().enumerate().all(|(index, byte)| match index {
                    8 => byte == b'T',
                    15 => byte == b'Z',
                    _ => byte.is_ascii_digit(),
                })
        })
}

/// The one archive in the case root, by name.
fn only_archive(case: &Case) -> String {
    let names: Vec<String> = fs::read_dir(case.root().join("archive"))
        .expect("the archive folder exists")
        .map(|entry| entry.expect("an archive entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

/// A journal at one byte under the cap, in the case root.
fn seed_near_cap(case: &Case) -> Vec<String> {
    let seed = seed_lines(JOURNAL_CAP_BYTES - 1);
    fs::write(case.root().join("events.jsonl"), text(&seed)).expect("the journal can be seeded");
    seed
}

fn assert_quiet(outcome: &Outcome, code: i32) {
    assert_eq!(outcome.code, Some(code), "{outcome:?}");
    let unexpected: Vec<&str> = outcome.stderr.lines().filter(|line| !is_expected_line(line)).collect();
    assert!(
        unexpected.is_empty(),
        "stderr holds nothing but expected-time lines, yet it says {unexpected:?} in {outcome:?}"
    );
}

/// Whether `line` is the expected-time line a gate says as it takes its slot for a key with history: `gate: expected
/// about {mean} (usual {p50}, p90 {p90}, {runs} runs)`, every figure whole seconds or whole minutes, the estimate of a
/// family key ending `runs of similar commands)`. A case that fills a key's history makes a later run say one.
fn is_expected_line(line: &str) -> bool {
    let pattern = Regex::new(r"^gate: expected about \d+ (?:s|min) \(usual \d+ (?:s|min), p90 \d+ (?:s|min), \d+ runs(?: of similar commands)?\)$")
        .expect("the expected-time pattern is valid");
    pattern.is_match(line)
}

#[test]
fn a_run_journals_queue_take_release_in_order() {
    let case = Case::new("a_run_journals_queue_take_release_in_order");
    let slot = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::LightSlot(0));
    let slots = format!("\"slots\":[{}],", Value::from(slot.as_str()));
    let command = quoted(&[FAKE_TOOL, "exit", "0"]);

    let outcome = case.run(&exiting("out.log", "0"));

    assert_quiet(&outcome, 0);
    let journal = fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written");
    assert_in_order(
        &journal,
        &[
            r#"{"v":1,"time":""#,
            r#""event":"queue""#,
            "\n",
            r#"{"v":1,"time":""#,
            r#""event":"take""#,
            &format!(r#"{slots}"waitedSeconds":"#),
            "\n",
            r#"{"v":1,"time":""#,
            r#""event":"release""#,
            &slots,
            r#""status":0,"#,
            r#""adjustedSeconds":"#,
            "\n",
        ],
    );
    assert!(
        regex::Regex::new(concat!(
            r#""event":"release".*"adjustedSeconds":\d+(\.\d+)?"#,
            r#"(,"avgCores":[\d.]+,"peakCores":[\d.]+,"freeCores":[\d.]+)?,"verdict":"passed"(,"cpuSeconds":[^\n]*)?\}\n"#
        ))
        .expect("the pattern is valid")
        .is_match(&journal),
        "the release's load-adjusted seconds are a number, then the verdict, then any trace: {journal}"
    );
    let lines: Vec<Value> = journal.lines().map(parsed).collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert_eq!(
            (&line["pid"], line["kind"].as_str(), line["cwd"].as_str(), line["command"].as_str()),
            (
                &lines[0]["pid"],
                Some("light"),
                Some(shown(&case.folder()).as_str()),
                Some(command.as_str())
            )
        );
    }
    assert_eq!(lines[0]["log"], shown(&case.folder().join("out.log")));
    assert!(lines[1]["waitedSeconds"].is_f64(), "the take says how long it waited: {}", lines[1]);
    assert!(lines[1].get("abandoned").is_none(), "a clean take names no abandoned slot: {}", lines[1]);
}

/// The release line of a run, sampled every `sample_ms`, whose command starts a child that spins until it has used
/// `burned` core-seconds, then exits while the command still runs.
fn release_of_a_spinning_child(case: &Case, sample_ms: &str, burned: u64) -> Value {
    let (pid_file, release_file, stop_file) = (case.folder().join("child.pid"), case.folder().join("release"), case.folder().join("stop"));
    let (pid_word, release_word, stop_word) = (shown(&pid_file), shown(&release_file), shown(&stop_file));
    let command = [FAKE_TOOL, "spawn-pid", &pid_word, &release_word, FAKE_TOOL, "spin", "1", &stop_word];

    let mut running = case.start(&run_words("out.log", &["--test-sample-ms", sample_ms], &command));
    let read_pid = || fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok());
    wait_until("the spinning child's pid written", || read_pid().is_some() || !running.is_running());
    let child = read_pid().expect("the command wrote its child's pid");
    wait_until(&format!("the child using {burned} core-seconds"), || {
        cpu_ticks(child) >= burned * CORE_SECOND
    });
    fs::write(&stop_file, "").expect("the stop file can be written");
    wait_until("the child exited", || gate_win32::process_times(child).ok().flatten().is_none());
    fs::write(&release_file, "").expect("the release file can be written");
    let outcome = running.finish();

    assert_quiet(&outcome, 0);
    let lines: Vec<Value> = lines_of(&case.root().join("events.jsonl")).iter().map(|line| parsed(line)).collect();
    lines.into_iter().find(|line| line["event"] == "release").expect("the run released")
}

/// The entry of `release`'s `children` that names fake-tool's spinning child.
fn spinner_of(release: &Value) -> &Value {
    let children = release["children"].as_array().expect("the release names the busy child");
    let spinner = children.iter().find(|entry| {
        entry["cmd"]
            .as_str()
            .is_some_and(|cmd| cmd.contains("fake-tool") && cmd.contains(" spin "))
    });
    assert!(spinner.is_some(), "a child entry names the spinning child: {release}");
    spinner.expect("checked above")
}

#[test]
fn a_release_says_where_the_run_s_time_went() {
    // The core-seconds the child burns before the case stops it: the gate's samples, every 100 ms, see it long before.
    const BURNED: u64 = 2;
    let case = Case::new("a_release_says_where_the_run_s_time_went");

    let release = release_of_a_spinning_child(&case, "100", BURNED);

    let cpu = release["cpuSeconds"].as_f64().expect("the release has the job's CPU");
    assert!(cpu >= 1.9, "the job's CPU counts the child's {BURNED} core-seconds: {release}");
    let processes = release["processes"].as_u64().expect("the release counts the job's processes");
    assert!(processes >= 2, "the command and its child: {release}");
    assert!(release["peakProcesses"].as_u64().is_some_and(|peak| peak >= 1), "{release}");
    assert_eq!(release["priority"], "belowNormal", "{release}");
    let share = release["loadFreeShare"].as_f64().expect("a sampled run has the machine's free share");
    assert!((0.0..=1.0).contains(&share), "{release}");
    let spinner = spinner_of(&release);
    assert!(spinner["cpu"].as_f64().is_some_and(|cpu| cpu >= 1.9), "{spinner}");
    assert!(spinner["start"].as_f64().is_some_and(|start| start >= 0.0), "{spinner}");
    assert!(spinner["end"].as_f64().is_some(), "the child exited before the release: {spinner}");
}

#[test]
fn a_child_that_lives_between_two_samples_is_named_with_its_image() {
    // The samples are ten minutes apart, so none falls inside the child's life: the job's own memory of its members
    // names it, not a sample's listing.
    const BURNED: u64 = 1;
    let case = Case::new("a_child_that_lives_between_two_samples_is_named_with_its_image");

    let release = release_of_a_spinning_child(&case, "600000", BURNED);

    let spinner = spinner_of(&release);
    assert!(spinner["cpu"].as_f64().is_some_and(|cpu| cpu >= 0.9), "{spinner}");
    assert!(spinner["end"].as_f64().is_some(), "the child exited before the release: {spinner}");
    let image = spinner["image"].as_str().expect("the child's image path is given");
    let tool = Path::new(FAKE_TOOL)
        .file_name()
        .expect("fake-tool's path has a file name")
        .to_string_lossy()
        .to_lowercase();
    assert!(image.to_lowercase().ends_with(&tool), "the image is fake-tool's own path: {spinner}");
    assert!(Path::new(image).is_absolute(), "the image is a full path: {spinner}");
}

#[test]
fn a_run_of_more_than_256_short_children_counts_them_all_and_passes() {
    const CHILDREN: u64 = 300;
    let case = Case::new("a_run_of_more_than_256_short_children_counts_them_all_and_passes");
    let count = CHILDREN.to_string();

    let outcome = case.run(&run_words("out.log", &["--test-sample-ms", "100"], &[FAKE_TOOL, "run-each", &count]));

    assert_quiet(&outcome, 0);
    let lines: Vec<Value> = lines_of(&case.root().join("events.jsonl")).iter().map(|line| parsed(line)).collect();
    let release = lines.iter().find(|line| line["event"] == "release").expect("the run released");
    assert_eq!(release["verdict"], "passed", "{release}");
    let processes = release["processes"].as_u64().expect("the release counts the job's processes");
    assert!(processes > CHILDREN, "the command and each of its {CHILDREN} children: {release}");
}

#[test]
fn a_no_slot_run_is_journalled_with_kind_none_and_no_slots() {
    let case = Case::new("a_no_slot_run_is_journalled_with_kind_none_and_no_slots");

    let outcome = case.run(&none_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]));

    assert_quiet(&outcome, 0);
    let lines: Vec<Value> = lines_of(&case.root().join("events.jsonl")).iter().map(|line| parsed(line)).collect();
    let events: Vec<&str> = lines.iter().map(|line| line["event"].as_str().expect("a line names its event")).collect();
    assert_eq!(events, ["queue", "take", "release"]);
    for line in &lines {
        assert_eq!((&line["pid"], line["kind"].as_str()), (&lines[0]["pid"], Some("none")), "{line}");
    }
    let (take, release) = (&lines[1], &lines[2]);
    assert_eq!(
        (&take["slots"], &take["waitedSeconds"]),
        (&serde_json::json!([]), &serde_json::json!(0.0))
    );
    for absent in ["abandoned", "counts", "otherHeld", "otherClaims", "parentPid"] {
        assert!(take.get(absent).is_none(), "a no-slot take has no {absent}: {take}");
    }
    assert_eq!(
        (&release["slots"], &release["status"], &release["verdict"]),
        (&serde_json::json!([]), &serde_json::json!(0), &serde_json::json!("passed"))
    );
}

#[test]
fn journal_command_is_quoted_like_format_command_text() {
    let case = Case::new("journal_command_is_quoted_like_format_command_text");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    // `Format-CommandText` of these words, as the PowerShell gate writes it: the fake tool's path and the go file's are
    // quoted the same way, so only their spelling is built.
    let expected = format!(r#"{} argv "a b" "x\"y""#, quoted(&[FAKE_TOOL, "wait", &go_word]));

    let running = case.start(&run_words("out.log", &[], &[FAKE_TOOL, "wait", &go_word, "argv", "a b", "x\"y"]));
    // The claim is written (by rename) before the take is journalled, so it is complete once the take line is there.
    let claim_path = case.root().join("slots").join("gate-light-slot-0.json");
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, running.id()));
    let claim = fs::read_to_string(&claim_path).expect("the held slot's claim can be read");
    fs::write(&go, "").expect("the go file can be made");
    let outcome = running.finish();

    assert_quiet(&outcome, 0);
    assert_eq!(parsed(&claim)["command"].as_str(), Some(expected.as_str()), "the claim: {claim}");
    let lines = lines_of(&case.root().join("events.jsonl"));
    let commands: Vec<(Value, Value)> = lines
        .iter()
        .map(|line| {
            let line = parsed(line);
            (line["event"].clone(), line["command"].clone())
        })
        .collect();
    assert_eq!(
        commands,
        ["queue", "take", "release"].map(|event| (Value::from(event), Value::from(expected.as_str()))),
        "every journal line carries the quoted command"
    );
}

#[test]
fn a_run_with_status_3_is_journalled_failed_and_exits_3() {
    let case = Case::new("a_run_with_status_3_is_journalled_failed_and_exits_3");

    let outcome = case.run(&exiting("out.log", "3"));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: FAILED (status 3). Full output: {}\n", shown(&case.folder().join("out.log")))
    );
    let lines = lines_of(&case.root().join("events.jsonl"));
    let release = parsed(lines.last().expect("the journal has lines"));
    assert_eq!(
        (&release["event"], &release["status"], &release["verdict"]),
        (&Value::from("release"), &Value::from(3), &Value::from("failed"))
    );
}

#[test]
fn journal_lines_carry_the_case_root_mutex_not_the_machine_s() {
    let case = Case::new("journal_lines_carry_the_case_root_mutex");
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Journal);
    assert_ne!(name, r"Local\gate-journal");
    let mutex = NamedMutex::open(&name).expect("the case root's journal mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("the case root's journal mutex is free");

    let blocked = case.run(&exiting("out.log", "0"));
    drop(held);

    assert_eq!(blocked.code, Some(0), "the gate runs on without its journal: {blocked:?}");
    assert_eq!(
        blocked.stderr,
        format!(
            "gate: could not write the journal {} ({name} was not free within 2 s)\n",
            shown(&case.root().join("events.jsonl"))
        ),
        "the gate waits on the case root's mutex, and says so once for its three lines"
    );
    assert!(!case.root().join("events.jsonl").exists());
    assert_quiet(&case.run(&exiting("out.log", "0")), 0);
    assert_eq!(lines_of(&case.root().join("events.jsonl")).len(), 3);
}

#[test]
fn rollover_while_another_gate_appends_loses_no_line() {
    const GATES: usize = 6;
    let case = Case::new("rollover_while_another_gate_appends_loses_no_line");
    let seed = seed_near_cap(&case);
    fs::write(case.root().join("slot-counts.json"), format!(r#"{{"v":1,"light":{GATES}}}"#)).expect("the counts can be written");
    let seeded_releases = release_lines(&seed);
    assert!(
        (1..=10).contains(&seeded_releases.len()),
        "a handful of seeded release lines: {}",
        seeded_releases.len()
    );
    // Each gate logs to a file of its own, since a gate refuses a log another gate is writing.
    let logs: Vec<String> = (0..GATES).map(|number| format!("out-{number}.log")).collect();
    let commands: Vec<_> = logs.iter().map(|log| case.gate(&exiting(log, "0"))).collect();

    let runs: Vec<_> = commands.into_iter().map(|command| thread::spawn(move || finish(command))).collect();
    let outcomes: Vec<Outcome> = runs.into_iter().map(|run| run.join().expect("a gate's runner ends")).collect();

    for outcome in &outcomes {
        assert_quiet(outcome, 0);
    }
    let rolled = lines_of(&case.root().join("events.1.jsonl"));
    let current = lines_of(&case.root().join("events.jsonl"));
    assert_eq!(rolled[..seed.len()], seed[..], "the seed was rolled whole");
    assert!(current.iter().all(|line| parsed(line)["seed"].is_null()), "exactly one roll");
    let mut events: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for line in rolled[seed.len()..].iter().chain(&current) {
        let value = parsed(line);
        let pid = value["pid"].as_u64().expect("a gate's line has its pid");
        events
            .entry(pid)
            .or_default()
            .push(value["event"].as_str().expect("a line names its event").to_owned());
    }
    assert_eq!(events.len(), GATES, "{events:?}");
    for (pid, lines) in &events {
        assert_eq!(lines, &["queue", "take", "release"], "gate {pid}'s lines, each once and in order");
    }
    let archive = only_archive(&case);
    assert!(is_archive_name(&archive), "{archive}");
    let mut archived = seeded_releases;
    archived.extend(release_lines(&rolled[seed.len()..]));
    assert_eq!(
        gunzip(&case.root().join("archive").join(&archive)),
        text(&archived),
        "the archive holds the seeded release lines and any run's release that went in before the roll"
    );
}

#[test]
fn archive_is_written_on_roll() {
    let case = Case::new("archive_is_written_on_roll");
    let seed = seed_near_cap(&case);

    let outcome = case.run(&exiting("out.log", "0"));

    assert_quiet(&outcome, 0);
    let rolled = lines_of(&case.root().join("events.1.jsonl"));
    assert_eq!(
        rolled.len(),
        seed.len() + 1,
        "the queue line went in under the cap; the take rolled the journal"
    );
    let archive = only_archive(&case);
    assert!(is_archive_name(&archive), "{archive}");
    let archived = gunzip(&case.root().join("archive").join(&archive));
    assert!(archived.lines().all(|line| parsed(line)["event"] == "release"), "release lines only");
    assert_eq!(archived, text(&release_lines(&seed)));
    assert_eq!(lines_of(&case.root().join("events.jsonl")).len(), 2, "the take and the release");
}

#[test]
fn a_quiet_run_may_say_its_expected_time() {
    let case = Case::new("a_quiet_run_may_say_its_expected_time");
    let command = exiting("out.log", "0");
    let mut said = 0;

    // The sixth run's key holds five runs, so it says its expected time as it takes its slot; the five before it, with
    // four runs behind them at most, say nothing. A case that fills a key's history this way must not read the line as
    // a problem.
    for _ in 0..6 {
        let outcome = case.run(&command);
        said += outcome.stderr.matches("gate: expected about ").count();
        assert_quiet(&outcome, 0);
    }

    assert_eq!(said, 1, "only the run with five runs behind it says its expected time");
}

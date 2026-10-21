//! Nesting under a parent gate's slot marker, `GATE_SLOT_HELD`: a gate a live parent's slot covers takes no slot and
//! journals `covered`, a critical gate under a heavy or light marker is refused, a heavy gate under a light marker takes
//! a heavy slot, and a marker that is malformed, names a gate no longer running or belongs to another root is ignored. A
//! marker naming a live process whose gate's command this gate does not run inside is ignored too, unless the
//! `--test-parent-job-trusted` seam trusts it, as the cases naming the test process do. A covered gate keeps its
//! covered marker while its command runs, and a gate that queues deletes a dead gate's. A covered gate whose parent gate
//! ends while its command still runs takes a slot of its own kind for the rest of the run, waiting for one without
//! pausing its command, and ends that wait silently when its command ends first.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::Duration;

use gate_state::v1::{self, CoveredMarker};
use gate_state::{KernelObject, StateRoot};
use gate_win32::{Job, Priority, StoppableProcess, priority_of};
use jiff::Timestamp;
use serde_json::Value;

use crate::support::{
    Case, Escapee, FAKE_TOOL, GATE, Outcome, POLL, Running, assert_in_order, critical_words, events, events_of, finish, heavy_words, none_words,
    run_words, said_pid, shown, start, take_of, tool, wait_for_file, wait_for_journal_line, wait_until, write_counts,
};

/// A heavy pool of one slot.
const ONE_HEAVY: &str = r#"{"v":1,"heavy":1}"#;

/// The start of the running process `pid` in Unix epoch milliseconds, as a gate's marker gives it.
fn start_ms(pid: u32) -> i64 {
    gate_win32::process_start_of(pid)
        .expect("a running process's start can be read")
        .expect("the process runs")
        .as_millisecond()
}

/// The case root's hash, which the marker of a gate under that root ends with.
fn root_hash(case: &Case) -> String {
    StateRoot::new(&shown(&case.root()), None)
        .hash()
        .expect("a case root is not the default root")
        .to_owned()
}

/// The marker a gate of `kind` that wrote the case's journal line `line` hands its command.
fn marker_of(case: &Case, kind: &str, line: &Value) -> String {
    let start: Timestamp = line["processStart"]
        .as_str()
        .expect("a line has its gate's start")
        .parse()
        .expect("the start is a time");
    format!("{kind}:{}:{}:{}", line["pid"], start.as_millisecond(), root_hash(case))
}

/// A marker of a `held` slot naming this test process, which runs as a gate would, under the case's root.
pub(crate) fn test_marker(case: &Case, held: &str) -> String {
    let pid = process::id();
    format!("{held}:{pid}:{}:{}", start_ms(pid), root_hash(case))
}

/// The seam that has a gate trust a marker naming the test process, which is no gate and has no job.
const TRUSTED: &str = "--test-parent-job-trusted";

/// The gate with `args` as the case readies it, started under `marker`, which it trusts as a real parent gate's
/// ([`TRUSTED`] given before the command).
pub(crate) fn marked(case: &Case, args: &[&str], marker: &str) -> Command {
    let mut words = args.to_vec();
    let command = words.iter().position(|word| *word == "--").expect("the words have a command");
    words.insert(command, TRUSTED);
    forged(case, &words, marker)
}

/// The gate with `args` as the case readies it, started under `marker`, which it judges as any gate does.
fn forged(case: &Case, args: &[&str], marker: &str) -> Command {
    let mut command = case.gate(args);
    command.env("GATE_SLOT_HELD", marker);
    command
}

/// The journal's lines as their event and pid, in order.
fn sequence(case: &Case) -> Vec<(String, u64)> {
    events(case)
        .iter()
        .map(|event| {
            let name = event["event"].as_str().expect("a line names its event").to_owned();
            (name, event["pid"].as_u64().expect("a line has a pid"))
        })
        .collect()
}

/// The journal's event names of every gate but `other`, in order.
fn events_besides(case: &Case, other: u32) -> Vec<String> {
    sequence(case)
        .into_iter()
        .filter(|(_, pid)| *pid != u64::from(other))
        .map(|(name, _)| name)
        .collect()
}

/// The journal's one `covered` line.
fn covered(case: &Case) -> Value {
    let lines: Vec<Value> = events(case).into_iter().filter(|event| event["event"] == "covered").collect();
    assert_eq!(lines.len(), 1, "one covered line: {lines:?}");
    lines.into_iter().next().expect("checked above")
}

/// Whether any gate has a ticket in the case root.
fn has_tickets(case: &Case) -> bool {
    match fs::read_dir(case.root().join("waiting")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        listed => listed.expect("the tickets folder can be listed").next().is_some(),
    }
}

/// Starts a heavy gate whose command holds its slot until `go` exists, and returns once it has taken the slot.
fn heavy_holder(case: &Case, go: &Path) -> Running {
    let go = shown(go);
    let holder = case.start(&heavy_words("holder.log", &[], &[FAKE_TOOL, "out", "holding", "wait", &go]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    holder
}

pub(crate) fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

fn assert_passed(outcome: &Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn a_gate_under_a_covering_parent_takes_no_slot() {
    let case = Case::new("a_gate_under_a_covering_parent_takes_no_slot");
    let mut inner = vec![GATE];
    inner.extend(run_words("inner.log", &[], &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    let outer = case.start(&heavy_words("outer.log", &[], &inner));
    let outer_pid = outer.id();
    assert_passed(&outer.finish());

    let line = covered(&case);
    let inner_pid = line["pid"].as_u64().expect("the covered line has a pid");
    let outer_pid = u64::from(outer_pid);
    let expected: Vec<(String, u64)> = [("queue", outer_pid), ("take", outer_pid), ("covered", inner_pid), ("release", outer_pid)]
        .into_iter()
        .map(|(name, pid)| (name.to_owned(), pid))
        .collect();
    assert_eq!(
        sequence(&case),
        expected,
        "the inner gate journals only its covered line, inside the outer's hold"
    );
    assert_eq!((&line["parentPid"], &line["kind"]), (&Value::from(outer_pid), &Value::from("light")));

    let outer_start: Timestamp = events(&case)[0]["processStart"]
        .as_str()
        .expect("a line has its gate's start")
        .parse()
        .expect("the start is a time");
    let marker = format!("heavy:{outer_pid}:{}:{}", outer_start.as_millisecond(), root_hash(&case));
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the inner gate wrote its log");
    assert_in_order(
        &inner_log,
        &[&format!("{marker}\n"), &format!("gate: running under the heavy slot of gate {outer_pid}")],
    );
}

/// The names of the files in the case root's covered folder, none when it is missing.
fn covered_files(case: &Case) -> Vec<String> {
    match fs::read_dir(v1::covered_dir(&case.root())) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        listed => listed
            .expect("the covered folder can be listed")
            .map(|entry| entry.expect("the entry lists").file_name().to_string_lossy().into_owned())
            .collect(),
    }
}

/// The covered markers in the case root, as gate-state reads them.
fn covered_markers(case: &Case) -> Vec<CoveredMarker> {
    v1::read_covered(&v1::covered_dir(&case.root()))
        .expect("the covered folder can be listed")
        .into_iter()
        .map(|(_, marker)| marker)
        .collect()
}

#[test]
fn a_covered_gate_keeps_its_marker_while_its_command_runs() {
    let case = Case::new("a_covered_gate_keeps_its_marker_while_its_command_runs");
    let started = shown(&case.folder().join("started"));
    let go = case.folder().join("go");
    let go_text = shown(&go);
    let words = run_words("inner.log", &[], &[FAKE_TOOL, "mark", &started, "wait", &go_text]);
    let inner = start(marked(&case, &words, &test_marker(&case, "heavy")));
    // The command starts only after the marker is written, so its mark is the moment to look.
    wait_for_file(Path::new(&started));

    let pid = inner.id();
    let markers: Vec<(u32, u64, u32)> = covered_markers(&case)
        .iter()
        .map(|marker| (marker.pid, marker.process_start, marker.parent_pid))
        .collect();
    let child_start = u64::try_from(start_ms(pid)).expect("a start is after the epoch");
    assert_eq!(
        markers,
        [(pid, child_start, process::id())],
        "the covered gate's marker names it and its parent"
    );
    assert_eq!(covered_files(&case), [format!("{pid}-{child_start}.json")]);

    let_go(&go);
    assert_passed(&inner.finish());
    assert_eq!(covered_files(&case), Vec::<String>::new(), "the marker is gone once the run ends");
    assert_eq!(events_besides(&case, 0), ["covered"], "the marker changes nothing in the journal");
}

#[test]
fn a_dead_gates_covered_marker_is_deleted_by_the_next_gate_that_queues() {
    let case = Case::new("a_dead_gates_covered_marker_is_deleted_by_the_next_gate_that_queues");
    let root = case.root();
    // A pid no process runs as: Windows gives out multiples of 4 far below this one.
    let dead = CoveredMarker {
        pid: u32::MAX - 3,
        process_start: 1_790_852_400_500,
        parent_pid: 52000,
        command: "cargo fmt --check".to_owned(),
        cwd: shown(&case.folder()),
        since: 1_790_852_402_000,
        kind: Some("light".to_owned()),
    };
    v1::write_covered(&v1::covered_path(&root, dead.pid, dead.process_start), &dead).expect("the dead marker can be written");
    let own = process::id();
    let own_start = u64::try_from(start_ms(own)).expect("a start is after the epoch");
    let live = CoveredMarker {
        pid: own,
        process_start: own_start,
        ..dead.clone()
    };
    v1::write_covered(&v1::covered_path(&root, own, own_start), &live).expect("the live marker can be written");

    assert_passed(&case.run(&run_words("out.log", &[], &[FAKE_TOOL, "exit", "0"])));

    assert_eq!(
        covered_markers(&case),
        [live],
        "the dead gate's marker is deleted; the live gate's is kept"
    );
}

#[test]
fn a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine() {
    let case = Case::new("a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine");
    write_counts(&case.root(), ONE_HEAVY);
    let go = case.folder().join("go");
    let holder = heavy_holder(&case, &go);
    let holder_pid = holder.id();

    let words = heavy_words("inner.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let outcome = finish(marked(&case, &words, &test_marker(&case, "critical")));
    assert_passed(&outcome);
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: running under the critical slot of gate {}", process::id())),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("waiting for"), "{outcome:?}");
    assert!(!has_tickets(&case), "the covered gate wrote no ticket");
    assert_eq!(events_besides(&case, holder_pid), ["covered"]);
    assert_eq!(covered(&case)["parentPid"], Value::from(process::id()));

    let_go(&go);
    assert_passed(&holder.finish());
}

/// Runs a critical gate under a live `held` marker in a case of its own, `command` readying the gate with its words.
fn refused(name: &str, held: &str, command: impl FnOnce(&Case) -> Command) -> (Outcome, Case) {
    let case = Case::new(name);
    let log = case.folder().join("refused.log");
    fs::write(&log, "earlier\n").expect("an earlier log can be written");
    let mut gate = command(&case);
    gate.env("GATE_SLOT_HELD", test_marker(&case, held));
    let outcome = finish(gate);
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(fs::read_to_string(&log).expect("the earlier log is still there"), "earlier\n");
    assert!(!case.root().join("events.jsonl").exists(), "the refused gate journals nothing");
    assert!(!has_tickets(&case), "the refused gate queued nothing");
    (outcome, case)
}

/// The refusal line of a critical gate asked for with `flag`, under the `held` slot of this test process.
fn refusal(flag: &str, held: &str) -> String {
    format!(
        "gate: {flag} critical cannot run under the {held} slot of gate {}: it would wait for that slot for good; run the critical \
         command outside that gate\n",
        process::id()
    )
}

#[test]
fn a_critical_gate_under_a_heavy_or_light_marker_is_refused() {
    for held in ["heavy", "light"] {
        let name = format!("a_critical_gate_under_a_heavy_or_light_marker_is_refused_{held}");
        let (outcome, _case) = refused(&name, held, |case| {
            case.gate(&critical_words("refused.log", &[TRUSTED], &[FAKE_TOOL, "exit", "0"]))
        });
        assert_eq!(outcome.stderr, refusal("--slot", held));
    }
}

#[test]
fn a_critical_gate_refused_through_the_powershell_form_names_its_flag_so() {
    let name = "a_critical_gate_refused_through_the_powershell_form_names_its_flag_so";
    let (outcome, _case) = refused(name, "heavy", |case| {
        let words = [
            "-Log",
            "refused.log",
            "-TimeoutSeconds",
            "60",
            "-Slot",
            "critical",
            "-Reason",
            "the suite's critical run",
            TRUSTED,
            "--",
            FAKE_TOOL,
            "exit",
            "0",
        ];
        let mut command = case.gate_without_state_dir(&words);
        command.env("GATE_STATE_DIR", case.root());
        command
    });
    assert_eq!(outcome.stderr, refusal("-Slot", "heavy"));
}

#[test]
fn a_critical_gate_under_a_critical_marker_is_covered() {
    let case = Case::new("a_critical_gate_under_a_critical_marker_is_covered");
    let words = critical_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let outcome = finish(marked(&case, &words, &test_marker(&case, "critical")));
    assert_passed(&outcome);
    assert!(
        outcome
            .stderr
            .contains(&format!("gate: running under the critical slot of gate {}", process::id())),
        "{outcome:?}"
    );
    assert_eq!(events_besides(&case, 0), ["covered"]);
    assert_eq!(covered(&case)["parentPid"], Value::from(process::id()));
}

/// Runs a light gate under `marker` that exits 0, and asserts that it took its own slot; what it said.
fn own_slot_under(case: &Case, log: &str, marker: &str) -> String {
    let before = events_or_none(case);
    let outcome = finish(marked(case, &run_words(log, &[], &[FAKE_TOOL, "exit", "0"]), marker));
    assert_passed(&outcome);
    let names: Vec<String> = sequence(case).into_iter().skip(before).map(|(name, _)| name).collect();
    assert_eq!(names, ["queue", "take", "release"], "the gate took a slot of its own");
    outcome.stderr
}

/// How many lines the case's journal holds, none when it was not written yet.
fn events_or_none(case: &Case) -> usize {
    if case.root().join("events.jsonl").exists() {
        events(case).len()
    } else {
        0
    }
}

#[test]
fn a_marker_naming_a_gate_that_is_not_running_is_ignored() {
    let case = Case::new("a_marker_naming_a_gate_that_is_not_running_is_ignored");
    let pid = process::id();
    let marker = format!("light:{pid}:{}:{}", start_ms(pid) + 1, root_hash(&case));
    let stderr = own_slot_under(&case, "out.log", &marker);
    assert!(
        stderr.contains(&format!(
            "gate: ignored GATE_SLOT_HELD='{marker}': gate {pid} is no longer running; taking a light slot"
        )),
        "{stderr}"
    );
}

#[test]
fn a_malformed_marker_is_ignored_with_its_line() {
    let case = Case::new("a_malformed_marker_is_ignored_with_its_line");
    let stderr = own_slot_under(&case, "out.log", "light:not-a-pid:1");
    assert!(
        stderr.contains("gate: ignored GATE_SLOT_HELD='light:not?a?pid:1', which is not <kind>:<pid>:<start>; taking a light slot"),
        "{stderr}"
    );
}

#[test]
fn a_marker_of_another_state_root_is_not_honoured() {
    let case = Case::new("a_marker_of_another_state_root_is_not_honoured");
    let pid = process::id();
    let own = root_hash(&case);
    let other = if own == "00000000" { "11111111" } else { "00000000" };
    let markers = [format!("light:{pid}:{}:{other}", start_ms(pid)), format!("light:{pid}:{}", start_ms(pid))];
    for (index, marker) in markers.iter().enumerate() {
        let stderr = own_slot_under(&case, &format!("out-{index}.log"), marker);
        assert!(
            !stderr.contains("GATE_SLOT_HELD") && !stderr.contains("slot of gate"),
            "{marker}: {stderr}"
        );
    }
}

#[test]
fn a_gate_under_a_no_slot_run_takes_its_own_slot_and_says_nothing() {
    let case = Case::new("a_gate_under_a_no_slot_run_takes_its_own_slot_and_says_nothing");
    let mut inner = vec![GATE];
    inner.extend(heavy_words("inner.log", &[], &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    let outer = case.start(&none_words("outer.log", &[], &inner));
    let outer_pid = outer.id();
    assert_passed(&outer.finish());

    let take = events(&case)
        .into_iter()
        .find(|event| event["event"] == "take" && event["kind"] == "heavy")
        .expect("the inner gate took a heavy slot");
    let slot = StateRoot::new(&shown(&case.root()), None).object_name(gate_state::KernelObject::HeavySlot(0));
    assert_eq!((&take["slots"], &take["parentPid"]), (&Value::from(vec![slot]), &Value::from(outer_pid)));
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the inner gate wrote its log");
    assert!(inner_log.starts_with(&format!("{}\n", marker_of(&case, "heavy", &take))), "{inner_log}");
    let outer_log = fs::read_to_string(case.folder().join("outer.log")).expect("the outer gate wrote its log");
    assert!(
        !outer_log.contains("GATE_SLOT_HELD") && !outer_log.contains("slot of gate"),
        "the inner gate said nothing of its parent: {outer_log}"
    );
}

#[test]
fn a_critical_gate_under_a_no_slot_run_takes_the_machine() {
    let case = Case::new("a_critical_gate_under_a_no_slot_run_takes_the_machine");
    let words = critical_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let outcome = finish(marked(&case, &words, &test_marker(&case, "none")));

    assert_passed(&outcome);
    assert!(
        !outcome.stderr.contains("GATE_SLOT_HELD") && !outcome.stderr.contains("cannot run under"),
        "{outcome:?}"
    );
    assert!(outcome.stderr.contains("gate: holds the machine (critical) at "), "{outcome:?}");
    assert_eq!(events_besides(&case, 0), ["queue", "take", "release"]);
    let take = &events(&case)[1];
    assert!(!take["slots"].as_array().expect("slots is a list").is_empty(), "{take}");
    assert_eq!(take["parentPid"], Value::from(process::id()));
}

#[test]
fn a_no_slot_run_marks_its_command_with_kind_none() {
    let case = Case::new("a_no_slot_run_marks_its_command_with_kind_none");
    let outcome = case.run(&none_words("out.log", &[], &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    assert_passed(&outcome);

    let log = fs::read_to_string(case.folder().join("out.log")).expect("the gate wrote its log");
    let marker = marker_of(&case, "none", &events(&case)[0]);
    assert!(log.starts_with(&format!("{marker}\n")), "{log}");
}

#[test]
fn a_no_slot_gate_under_a_heavy_gate_is_covered_and_hands_the_marker_on() {
    let case = Case::new("a_no_slot_gate_under_a_heavy_gate_is_covered_and_hands_the_marker_on");
    let mut inner = vec![GATE];
    inner.extend(none_words("inner.log", &[], &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    let outer = case.start(&heavy_words("outer.log", &[], &inner));
    let outer_pid = outer.id();
    assert_passed(&outer.finish());

    let line = covered(&case);
    assert_eq!((&line["parentPid"], &line["kind"]), (&Value::from(outer_pid), &Value::from("none")));
    let marker = marker_of(&case, "heavy", &events(&case)[0]);
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the inner gate wrote its log");
    assert_in_order(
        &inner_log,
        &[&format!("{marker}\n"), &format!("gate: running under the heavy slot of gate {outer_pid}")],
    );
}

#[test]
fn a_gate_under_a_no_slot_run_is_not_lent_a_slot_during_a_critical_request() {
    let case = Case::new("a_gate_under_a_no_slot_run_is_not_lent_a_slot_during_a_critical_request");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let holder = heavy_holder(&case, &go);
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr("gate: waiting for the machine (critical)");

    let words = heavy_words("child.log", &POLL, &[FAKE_TOOL, "exit", "0"]);
    let child = start(marked(&case, &words, &test_marker(&case, "none")));
    let child_pid = child.id();
    child.wait_for_stderr(&format!(
        "gate: waiting for a heavy slot behind the critical request of gate {critical_pid}\n"
    ));
    assert_eq!(events_of(&case, child_pid), ["queue"], "the child waits for a slot of its own");

    let_go(&go);
    assert_passed(&holder.finish());
    assert_passed(&critical.finish());
    let outcome = child.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("taking no slot"), "{outcome:?}");
    assert_eq!(events_of(&case, child_pid), ["queue", "take", "release"]);
    assert_eq!(take_of(&case, child_pid)["parentPid"], Value::from(process::id()));
}

#[test]
fn a_gate_lent_its_light_parents_slot_hands_its_command_its_own_heavy_marker() {
    let case = Case::new("a_gate_lent_its_light_parents_slot_hands_its_command_its_own_heavy_marker");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let holder = heavy_holder(&case, &go);
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    critical.wait_for_stderr("gate: waiting for the machine (critical)");

    let words = heavy_words("child.log", &POLL, &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]);
    let child = start(marked(&case, &words, &test_marker(&case, "light")));
    let child_pid = child.id();
    assert_passed(&child.finish());
    let_go(&go);
    assert_passed(&holder.finish());
    assert_passed(&critical.finish());

    assert_eq!(events_of(&case, child_pid), ["parent"], "the child ran on its parent's lent slot");
    let parent_line = events(&case)
        .into_iter()
        .find(|event| event["event"] == "parent" && event["pid"] == child_pid)
        .expect("checked above");
    let marker = marker_of(&case, "heavy", &parent_line);
    let child_log = fs::read_to_string(case.folder().join("child.log")).expect("the child wrote its log");
    assert!(
        child_log.contains(&format!("{marker}\n")),
        "the lent child hands its command its own heavy marker {marker}: {child_log}"
    );
}

/// The line of a gate that ignores `marker`, naming this test process, because it does not run inside that process's
/// gate's command, then goes on as `going_on` says.
fn unbound_line(marker: &str, going_on: &str) -> String {
    format!(
        "gate: ignored GATE_SLOT_HELD='{marker}': this gate does not run inside the command of gate {}; {going_on}",
        process::id()
    )
}

/// Runs a light gate under a heavy marker naming this test process, the marker judged as any gate judges it, and
/// asserts that the gate ignored it with its line and took a light slot of its own under no parent.
fn ignores_the_test_processs_marker(case: &Case) {
    let marker = test_marker(case, "heavy");
    let outcome = finish(forged(case, &run_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]), &marker));
    assert_passed(&outcome);
    assert!(outcome.stderr.contains(&unbound_line(&marker, "taking a light slot")), "{outcome:?}");
    assert_eq!(
        events_besides(case, 0),
        ["queue", "take", "release"],
        "the gate took a slot of its own and journalled no covered line"
    );
    assert_eq!(events(case)[1]["parentPid"], Value::Null, "the gate names no parent");
}

#[test]
fn a_marker_naming_a_live_process_that_is_not_a_gate_is_not_honoured() {
    ignores_the_test_processs_marker(&Case::new("a_marker_naming_a_live_process_that_is_not_a_gate_is_not_honoured"));
}

#[test]
fn a_marker_naming_a_gate_whose_job_does_not_hold_this_gate_is_not_honoured() {
    let case = Case::new("a_marker_naming_a_gate_whose_job_does_not_hold_this_gate_is_not_honoured");
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::Job { gate_pid: process::id() });
    let _job = Job::create(&name).expect("the job of the test process's name is created, empty");
    ignores_the_test_processs_marker(&case);
}

#[test]
fn a_critical_gate_under_a_forged_heavy_marker_takes_the_machine_instead_of_being_refused() {
    let case = Case::new("a_critical_gate_under_a_forged_heavy_marker_takes_the_machine_instead_of_being_refused");
    let marker = test_marker(&case, "heavy");
    let outcome = finish(forged(&case, &critical_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]), &marker));

    assert_passed(&outcome);
    assert!(outcome.stderr.contains(&unbound_line(&marker, "taking a critical slot")), "{outcome:?}");
    assert!(!outcome.stderr.contains("cannot run under"), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: holds the machine (critical) at "), "{outcome:?}");
    assert_eq!(events_besides(&case, 0), ["queue", "take", "release"]);
}

/// The `pwsh.exe` the first folder on `PATH` that has one holds, an app execution alias included.
fn pwsh_on_path() -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|folder| folder.join("pwsh.exe"))
        .find(|path| fs::symlink_metadata(path).is_ok())
}

#[test]
#[expect(clippy::print_stderr, reason = "a case this machine cannot run says why it skipped")]
fn a_gate_under_cmd_and_pwsh_in_its_parents_command_is_covered() {
    if pwsh_on_path().is_none() {
        eprintln!("skipped: no pwsh on PATH");
        return;
    }
    let case = Case::new("a_gate_under_cmd_and_pwsh_in_its_parents_command_is_covered");
    let inner: Vec<String> = std::iter::once(GATE)
        .chain(run_words("inner.log", &[], &[FAKE_TOOL, "exit", "0"]))
        .map(|word| format!("'{word}'"))
        .collect();
    let script = format!("& {}", inner.join(" "));
    let outer = case.start(&heavy_words(
        "outer.log",
        &[],
        &["cmd", "/d", "/c", "pwsh", "-NoProfile", "-Command", &script],
    ));
    let outer_pid = u64::from(outer.id());
    assert_passed(&outer.finish());

    let line = covered(&case);
    let inner_pid = line["pid"].as_u64().expect("the covered line has a pid");
    let expected: Vec<(String, u64)> = [("queue", outer_pid), ("take", outer_pid), ("covered", inner_pid), ("release", outer_pid)]
        .into_iter()
        .map(|(name, pid)| (name.to_owned(), pid))
        .collect();
    assert_eq!(sequence(&case), expected, "the gate under cmd and pwsh runs under the outer's slot");
    assert_eq!((&line["parentPid"], &line["kind"]), (&Value::from(outer_pid), &Value::from("light")));
}

/// The seams of a covered gate whose parent ends as its command runs: it looks at its parent every 100 ms, polls for a
/// free slot every 20 ms, and says it still waits only once an hour, so no case's lines depend on how long it took.
pub(crate) const LOOK_OFTEN: [&str; 6] = ["--test-sample-ms", "100", "--test-poll-ms", "20", "--test-heartbeat-ms", "3600000"];

/// A covered gate the outer gate's command started and left running: its process, opened while that command still
/// held it, so its pid is pinned, and its pid. Dropping it while the gate still runs ends it, so no failure leaves it
/// waiting.
pub(crate) struct Orphan {
    process: StoppableProcess,
    pub(crate) pid: u32,
}

impl Orphan {
    /// Whether the gate has ended.
    pub(crate) fn has_ended(&self) -> bool {
        self.process.wait(Duration::ZERO).expect("the covered gate can be waited for")
    }

    /// Waits for the covered gate to end; its exit code.
    pub(crate) fn ended(&self) -> u32 {
        wait_until("the covered gate ending", || {
            self.process.wait(Duration::ZERO).expect("the covered gate can be waited for")
        });
        self.process.exit_code().expect("the covered gate's exit code can be read")
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        if !self.process.wait(Duration::ZERO).unwrap_or(true) {
            // A gate that is stopping already refuses this, which is no failure of its case.
            let _ = self.process.terminate(1);
        }
    }
}

/// Starts a heavy outer gate whose command starts the gate of `inner` words through fake-tool's `spawn-pid` and holds
/// it until the file `release` comes, and returns once the covered gate has journalled its `covered` line.
fn orphaned(case: &Case, inner: &[&str], release: &Path) -> (Running, Orphan) {
    orphaned_as(case, "heavy", inner, release)
}

/// [`orphaned`], the outer gate taking a slot of `outer_kind`.
pub(crate) fn orphaned_as(case: &Case, outer_kind: &str, inner: &[&str], release: &Path) -> (Running, Orphan) {
    orphaned_by(case, outer_kind, inner, release, None)
}

/// [`orphaned_as`], the covered gate's stderr sent to the file `stderr` when there is one, where a case reads what it
/// says while it runs: its log takes its own lines only at its end, and the outer gate's pipe it would inherit is gone
/// with the parent.
fn orphaned_by(case: &Case, outer_kind: &str, inner: &[&str], release: &Path, stderr: Option<&Path>) -> (Running, Orphan) {
    let pid_file = case.folder().join("inner.pid");
    let (pid_text, release_text) = (shown(&pid_file), shown(release));
    let stderr_text = stderr.map(shown);
    let mut command = match &stderr_text {
        Some(stderr_text) => vec![FAKE_TOOL, "spawn-pid-err", stderr_text.as_str()],
        None => vec![FAKE_TOOL, "spawn-pid"],
    };
    command.extend([pid_text.as_str(), release_text.as_str(), GATE]);
    command.extend(inner);
    let mut extra = Vec::new();
    if outer_kind == "critical" {
        extra.extend(["--reason", "the suite's outer critical gate"]);
    }
    let mut words = heavy_words("outer.log", &extra, &command);
    words[2] = outer_kind;
    let outer = case.start(&words);
    (outer, spawned(case, &pid_file))
}

/// The gate a fake-tool `spawn-pid` step started and still holds, its pid read from `pid_file`, once it has journalled
/// its `covered` line.
pub(crate) fn spawned(case: &Case, pid_file: &Path) -> Orphan {
    let orphan = started_gate(pid_file);
    wait_for_journal_line(&case.root(), &format!(r#""event":"covered","pid":{},"#, orphan.pid));
    orphan
}

/// The gate a fake-tool `spawn-pid` step started and still holds, its pid read from `pid_file`, once it has written it.
fn started_gate(pid_file: &Path) -> Orphan {
    let read_pid = || fs::read_to_string(pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok());
    wait_until("the spawned gate's pid written", || read_pid().is_some());
    let pid = read_pid().expect("checked above");
    let process = StoppableProcess::open(pid)
        .expect("the spawned gate can be opened")
        .expect("the spawned gate its starter holds is running");
    Orphan { process, pid }
}

/// The gate a fake-tool `spawn-pid` step started, its pid read from `pid_file`, and the first event it journalled,
/// once it has journalled one: `covered` when it runs under a parent's slot, `queue` when it queues for its own.
pub(crate) fn first_event(case: &Case, pid_file: &Path) -> (Orphan, Value) {
    let orphan = started_gate(pid_file);
    let first = || events_or_none_of(case).into_iter().find(|event| event["pid"] == orphan.pid);
    wait_until("the spawned gate's first journal line", || first().is_some());
    let event = first().expect("checked above");
    (orphan, event)
}

/// The case's journal lines, none when it was not written yet.
fn events_or_none_of(case: &Case) -> Vec<Value> {
    if case.root().join("events.jsonl").exists() {
        events(case)
    } else {
        Vec::new()
    }
}

/// The pids of the gates that journalled a `take`, in order, but `but`'s.
fn takers_but(case: &Case, but: u32) -> Vec<u64> {
    events(case)
        .into_iter()
        .filter(|event| event["event"] == "take" && event["pid"] != but)
        .filter_map(|event| event["pid"].as_u64())
        .collect()
}

/// Waits until the file at `path` holds a line containing `needle`, and returns that line.
fn line_with(path: &Path, needle: &str) -> String {
    line_with_unless_ended(path, needle, || false)
}

/// [`line_with`], failing at once when `ended` says the process that writes the file has ended without writing the
/// line. `ended` is asked before the file is read, so a line written just before the end is still found. A missing
/// file holds no line yet; any other failure to read it fails the case. The bytes are read as UTF-8 with U+FFFD for a
/// character torn by a write still under way.
fn line_with_unless_ended(path: &Path, needle: &str, ended: impl Fn() -> bool) -> String {
    let found = || {
        let bytes = match fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, String::new()),
            read => read.expect("a file a case waits on can be read"),
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (text.lines().find(|line| line.contains(needle)).map(str::to_owned), text)
    };
    let mut line = None;
    wait_until(&format!("a line with {needle:?} in {}", path.display()), || {
        let had_ended = ended();
        let (found, text) = found();
        assert!(
            found.is_some() || !had_ended,
            "it ended without writing {needle:?} to {}:\n{text}",
            path.display()
        );
        line = found;
        line.is_some()
    });
    line.expect("checked above")
}

#[test]
fn a_gate_started_after_two_parents_ended_runs_covered_by_the_gate_that_took_the_slot() {
    let case = Case::new("a_gate_started_after_two_parents_ended_runs_covered_by_the_gate_that_took_the_slot");
    write_counts(&case.root(), ONE_HEAVY);
    let paths = ["a-go", "c.pid", "c.release", "c-go", "e.pid", "e.release", "outer.release"].map(|name| case.folder().join(name));
    let [a_go, c_pid, c_release, c_go, e_pid, e_release, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let e_words = heavy_words("e.log", &LOOK_OFTEN, &[FAKE_TOOL, "exit", "0"]);
    let mut c_command = vec![FAKE_TOOL, "wait", &texts[3], "spawn-pid", &texts[4], &texts[5], GATE];
    c_command.extend(e_words);
    let c_words = heavy_words("c.log", &LOOK_OFTEN, &c_command);
    let mut a_command = vec![FAKE_TOOL, "wait", &texts[0], "spawn-pid", &texts[1], &texts[2], GATE];
    a_command.extend(c_words);
    let a_words = heavy_words("a.log", &LOOK_OFTEN, &a_command);

    // The outer gate ends, so A takes the one heavy slot; then A's command starts C, covered by A.
    let (outer, a) = orphaned(&case, &a_words, release);
    let outer_pid = end_parent(outer, release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, a.pid));
    let_go(a_go);
    let c = spawned(&case, c_pid);
    // A's command ends, leaving C running, so C takes the slot A lets go.
    let_go(c_release);
    assert_eq!(a.ended(), 0);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, c.pid));

    // C's command starts E, still under the outer gate's marker, while C holds the one slot.
    let_go(c_go);
    let (e, first) = first_event(&case, e_pid);
    assert_eq!(
        (&first["event"], &first["parentPid"]),
        (&Value::from("covered"), &Value::from(c.pid)),
        "E runs under C, which holds the slot, rather than queueing behind it: {first}"
    );
    assert_eq!(e.ended(), 0, "E ran without waiting for a slot");
    let_go(e_release);
    assert_eq!(c.ended(), 0);
    assert_eq!(events_of(&case, e.pid), ["covered"]);
    assert_eq!(events_of(&case, c.pid), ["covered", "take", "release"]);
    assert_eq!(takers_but(&case, outer_pid), [u64::from(a.pid), u64::from(c.pid)], "one slot at a time");
}

#[test]
fn a_critical_gate_under_a_gate_standing_in_for_an_ended_critical_parent_is_refused() {
    let case = Case::new("a_critical_gate_under_a_gate_standing_in_for_an_ended_critical_parent_is_refused");
    write_counts(&case.root(), r#"{"v":1,"heavy":2}"#);
    let paths = ["a-go", "d.pid", "d.release", "outer.release"].map(|name| case.folder().join(name));
    let [a_go, d_pid, d_release, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let mut a_command = vec![FAKE_TOOL, "wait", &texts[0], "spawn-pid", &texts[1], &texts[2], GATE];
    a_command.extend(critical_words("d.log", &[], &[FAKE_TOOL, "exit", "0"]));
    let a_words = heavy_words("a.log", &LOOK_OFTEN, &a_command);

    // Pc (critical) > A (heavy): Pc's command ends, so A takes a heavy slot of its own for the rest of its run.
    let (outer, a) = orphaned_as(&case, "critical", &a_words, release);
    end_parent(outer, release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, a.pid));

    // A's command starts D, a critical gate under Pc's marker, inside A's command, whose heavy slot would never free
    // for D's request for the machine.
    let_go(a_go);
    let d = started_gate(d_pid);
    let journalled = || events_or_none_of(&case).into_iter().any(|event| event["pid"] == d.pid);
    wait_until("D ending or journalling", || d.has_ended() || journalled());
    assert!(!journalled(), "D never queues for the machine behind A's slot");
    assert_eq!(d.ended(), 2, "D is refused");
    assert_eq!(
        line_with(&case.folder().join("a.log.err"), "critical cannot run under"),
        format!(
            "gate: --slot critical cannot run under the heavy slot of gate {}: it would wait for that slot for good; run the critical \
             command outside that gate",
            a.pid
        )
    );
    // No critical request is left pending, so another heavy gate takes the free slot and runs.
    assert_passed(&finish(case.gate(&heavy_words("other.log", &[], &[FAKE_TOOL, "exit", "0"]))));

    let_go(d_release);
    assert_eq!(a.ended(), 0);
    assert_eq!(events_of(&case, a.pid), ["covered", "take", "release"]);
}

#[test]
fn a_nested_chain_whose_stand_in_ends_too_holds_one_slot() {
    let case = Case::new("a_nested_chain_whose_stand_in_ends_too_holds_one_slot");
    write_counts(&case.root(), ONE_HEAVY);
    let paths = ["b.pid", "b.release", "c.pid", "c.release", "c-started", "c-go", "outer.release"].map(|name| case.folder().join(name));
    let [b_pid, b_release, c_pid, c_release, c_started, c_go, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let c_words = heavy_words("c.log", &LOOK_OFTEN, &[FAKE_TOOL, "mark", &texts[4], "wait", &texts[5]]);
    let mut b_command = vec![FAKE_TOOL, "spawn-pid", &texts[2], &texts[3], GATE];
    b_command.extend(c_words);
    let b_words = heavy_words("b.log", &LOOK_OFTEN, &b_command);
    let mut a_command = vec![FAKE_TOOL, "spawn-pid", &texts[0], &texts[1], GATE];
    a_command.extend(b_words);
    let a_words = heavy_words("a.log", &LOOK_OFTEN, &a_command);

    // O > A > B > C, all three under O's marker. O ends: A takes the one slot, and B and C stand in under A.
    let (outer, a) = orphaned(&case, &a_words, release);
    let b = spawned(&case, b_pid);
    let c = spawned(&case, c_pid);
    wait_for_file(c_started);
    let outer_pid = end_parent(outer, release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, a.pid));
    // Each gate's lines reach the `.err` file of the gate whose command it runs as.
    let (a_err, b_err) = (case.folder().join("a.log.err"), case.folder().join("b.log.err"));
    let under_a = format!("gate: the parent gate {outer_pid} ended; running under the heavy slot of gate {}", a.pid);
    assert_eq!(line_with(&a_err, &format!("gate: the parent gate {outer_pid} ended;")), under_a, "B");
    assert_eq!(line_with(&b_err, &format!("gate: the parent gate {outer_pid} ended;")), under_a, "C");

    // A's command ends, leaving B and C running: B takes the slot A lets go, and C stands in under B.
    let_go(b_release);
    assert_eq!(a.ended(), 0);
    let c_line = line_with(&b_err, &format!("gate: the parent gate {} ended;", a.pid));
    assert_eq!(
        c_line,
        format!("gate: the parent gate {} ended; running under the heavy slot of gate {}", a.pid, b.pid),
        "C finds B, though B's marker names O and not A"
    );
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, b.pid));
    assert_eq!(claimant(&case, KernelObject::HeavySlot(0)), b.pid, "one claim: B's");

    let_go(c_go);
    assert_eq!(c.ended(), 0);
    let_go(c_release);
    assert_eq!(b.ended(), 0);
    assert_eq!(takers_but(&case, outer_pid), [u64::from(a.pid), u64::from(b.pid)], "one slot at a time");
    assert_eq!(events_of(&case, c.pid), ["covered"], "C took no slot of its own");
}

#[test]
fn a_gate_started_as_its_parent_gate_ends_runs_covered_by_the_gate_whose_command_holds_it() {
    let case = Case::new("a_gate_started_as_its_parent_gate_ends_runs_covered_by_the_gate_whose_command_holds_it");
    let paths = ["go", "e.pid", "e.release"].map(|name| case.folder().join(name));
    let [go, e_pid, e_release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    // E judges its marker as any gate does: the marker names this test process, which is live but whose job is gone,
    // as a parent gate's is between its command's end and its release.
    let mut a_command = vec![FAKE_TOOL, "wait", &texts[0], "spawn-pid", &texts[1], &texts[2], GATE];
    a_command.extend(heavy_words("e.log", &[], &[FAKE_TOOL, "exit", "0"]));
    let a = start(marked(
        &case,
        &heavy_words("a.log", &LOOK_OFTEN, &a_command),
        &test_marker(&case, "heavy"),
    ));
    wait_for_journal_line(&case.root(), &format!(r#""event":"covered","pid":{},"#, a.id()));

    let_go(go);
    let (e, first) = first_event(&case, e_pid);
    assert_eq!(
        (&first["event"], &first["parentPid"]),
        (&Value::from("covered"), &Value::from(a.id())),
        "E runs under A rather than queueing before its command: {first}"
    );
    assert_eq!(e.ended(), 0);
    let e_log = fs::read_to_string(case.folder().join("e.log")).expect("E wrote its log");
    assert!(
        e_log.contains(&format!("gate: running under the heavy slot of gate {}", a.id())) && !e_log.contains("ignored GATE_SLOT_HELD"),
        "{e_log}"
    );
    let_go(e_release);
    assert_passed(&a.finish());
    assert_eq!(events_of(&case, e.pid), ["covered"]);
}

#[test]
fn a_heavy_gate_under_a_light_covered_gate_takes_its_own_slot_when_their_parent_ends() {
    let case = Case::new("a_heavy_gate_under_a_light_covered_gate_takes_its_own_slot_when_their_parent_ends");
    let paths = ["c.pid", "c.release", "c-started", "c-go", "outer.release"].map(|name| case.folder().join(name));
    let [c_pid, c_release, c_started, c_go, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let mut a_command = vec![FAKE_TOOL, "spawn-pid", &texts[0], &texts[1], GATE];
    a_command.extend(heavy_words("c.log", &LOOK_OFTEN, &[FAKE_TOOL, "mark", &texts[2], "wait", &texts[3]]));
    let a_words = run_words("a.log", &LOOK_OFTEN, &a_command);

    // O (heavy) > A (light) > C (heavy): O's slot covers both.
    let (outer, a) = orphaned(&case, &a_words, release);
    let c = spawned(&case, c_pid);
    wait_for_file(c_started);
    let outer_pid = end_parent(outer, release);
    let c_line = line_with(&case.folder().join("a.log.err"), &format!("gate: the parent gate {outer_pid} ended;"));
    assert_eq!(c_line, parent_ended_line(outer_pid, "heavy"), "A's light slot does not cover C");
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, c.pid));
    assert_eq!(
        take_of(&case, c.pid)["slots"],
        Value::from(vec![slot_name(&case, KernelObject::HeavySlot(0))])
    );
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, a.pid));
    assert_eq!(
        take_of(&case, a.pid)["slots"],
        Value::from(vec![slot_name(&case, KernelObject::LightSlot(0))])
    );

    let_go(c_go);
    assert_eq!(c.ended(), 0);
    let_go(c_release);
    assert_eq!(a.ended(), 0);
    assert_eq!(events_of(&case, c.pid), ["covered", "take", "release"]);
    assert_eq!(events_of(&case, a.pid), ["covered", "take", "release"]);
}

#[test]
fn a_gate_started_after_the_parent_ended_runs_covered_by_the_late_gate() {
    let case = Case::new("a_gate_started_after_the_parent_ended_runs_covered_by_the_late_gate");
    write_counts(&case.root(), ONE_HEAVY);
    let started = case.folder().join("started");
    let child_go = case.folder().join("child-go");
    let child_pid = case.folder().join("child.pid");
    let child_release = case.folder().join("child.release");
    let release = case.folder().join("outer.release");
    let texts = [&started, &child_go, &child_pid, &child_release].map(|path| shown(path));
    let mut command = vec![FAKE_TOOL, "mark", &texts[0], "wait", &texts[1], "spawn-pid", &texts[2], &texts[3], GATE];
    command.extend(heavy_words("child.log", &[], &[FAKE_TOOL, "exit", "0"]));
    let inner = heavy_words("inner.log", &LOOK_OFTEN, &command);
    let (outer, late) = orphaned(&case, &inner, &release);
    wait_for_file(&started);
    let outer_pid = end_parent(outer, &release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, late.pid));

    // The late gate's command now starts a heavy gate under the ended outer gate's marker, and waits for it to end.
    let_go(&child_go);
    let child = spawned(&case, &child_pid);
    assert_eq!(
        child.ended(),
        0,
        "the gate the late gate's command started ran without waiting for a slot"
    );
    assert_eq!(events_of(&case, child.pid), ["covered"], "it took no slot of its own");
    let line = events(&case)
        .into_iter()
        .find(|event| event["event"] == "covered" && event["pid"] == child.pid)
        .expect("checked above");
    assert_eq!(line["parentPid"], Value::from(late.pid), "it runs under the late gate, not the ended one");
    assert_eq!(claimant(&case, KernelObject::HeavySlot(0)), late.pid, "one claim: the late gate's");
    let child_log = fs::read_to_string(case.folder().join("child.log")).expect("the child gate wrote its log");
    assert!(
        child_log.contains(&format!("gate: running under the heavy slot of gate {}", late.pid)),
        "{child_log}"
    );
    assert!(
        !child_log.contains(&format!("gate {outer_pid} is no longer running")),
        "it does not ignore the marker as an ended gate's: {child_log}"
    );

    let_go(&child_release);
    assert_eq!(late.ended(), 0);
    assert_eq!(events_of(&case, late.pid), ["covered", "take", "release"]);
}

#[test]
fn a_nested_chain_whose_parent_ended_holds_one_slot() {
    let case = Case::new("a_nested_chain_whose_parent_ended_holds_one_slot");
    write_counts(&case.root(), ONE_HEAVY);
    let started = case.folder().join("started");
    let go = case.folder().join("go");
    let release = case.folder().join("outer.release");
    let (started_text, go_text) = (shown(&started), shown(&go));
    let mut command = vec![GATE];
    command.extend(heavy_words(
        "nested.log",
        &LOOK_OFTEN,
        &[FAKE_TOOL, "mark", &started_text, "wait", &go_text],
    ));
    let inner = heavy_words("inner.log", &LOOK_OFTEN, &command);
    let (outer, late) = orphaned(&case, &inner, &release);
    wait_for_file(&started);
    let nested = events(&case)
        .into_iter()
        .find(|event| event["event"] == "covered" && event["pid"] != late.pid)
        .and_then(|event| event["pid"].as_u64())
        .expect("the nested gate is covered too");

    let outer_pid = end_parent(outer, &release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, late.pid));
    // The nested gate runs as the late gate's command, so its lines reach the late gate's `.err` file as it says them.
    let err = case.folder().join("inner.log.err");
    let taken_up = format!(
        "gate: the parent gate {outer_pid} ended; running under the heavy slot of gate {}",
        late.pid
    );
    wait_until("the nested gate staying covered by the late gate", || {
        fs::read_to_string(&err).is_ok_and(|text| text.contains(&taken_up))
    });
    assert!(!has_tickets(&case), "the nested gate never queued for a slot");
    assert_eq!(claimant(&case, KernelObject::HeavySlot(0)), late.pid, "one claim: the late gate's");

    let_go(&go);
    assert_eq!(late.ended(), 0);
    let takes: Vec<u64> = events(&case)
        .into_iter()
        .filter(|event| event["event"] == "take" && event["pid"] != outer_pid)
        .filter_map(|event| event["pid"].as_u64())
        .collect();
    assert_eq!(takes, [u64::from(late.pid)], "one take after the outer gate's: the late gate's");
    let nested_events: Vec<String> = events(&case)
        .into_iter()
        .filter(|event| event["pid"] == nested)
        .map(|event| event["event"].as_str().expect("a line names its event").to_owned())
        .collect();
    assert_eq!(nested_events, ["covered"]);
}

/// Lets the outer gate's command end, which leaves the covered gate running, and waits for the outer gate to pass;
/// the outer gate's pid.
pub(crate) fn end_parent(outer: Running, release: &Path) -> u32 {
    let pid = outer.id();
    let_go(release);
    assert_passed(&outer.finish());
    pid
}

/// The full name of the slot `object` under the case root.
fn slot_name(case: &Case, object: KernelObject) -> String {
    StateRoot::new(&shown(&case.root()), None).object_name(object)
}

/// The pid the claim of the slot `object` under the case root names, once it is written: a gate writes its claim before
/// it journals its `take`, so a caller that waited for the take line finds it.
fn claimant(case: &Case, object: KernelObject) -> u32 {
    let claim = v1::claim_path(&case.root(), object).expect("a pool's slot has a claim");
    v1::read_claim(&claim).expect("the slot's claim can be read").pid
}

/// The line a covered gate of `kind` says when its parent gate `pid` ends as its command runs.
fn parent_ended_line(pid: u32, kind: &str) -> String {
    format!("gate: the parent gate {pid} ended; taking a {kind} slot for the rest of this run")
}

/// Runs a `kind` gate covered by a heavy outer gate that ends while the covered gate's command still runs, and asserts
/// that the covered gate says so, takes the slot `object` of its kind naming the ended parent, claims it while its
/// command runs and releases it at its end.
fn a_covered_gate_takes_a_slot_when_its_parent_gate_ends_as(name: &str, kind: &str, object: KernelObject) {
    let case = Case::new(name);
    let started = case.folder().join("started");
    let go = case.folder().join("go");
    let release = case.folder().join("outer.release");
    let (started_text, go_text) = (shown(&started), shown(&go));
    let mut inner = run_words("inner.log", &LOOK_OFTEN, &[FAKE_TOOL, "mark", &started_text, "wait", &go_text]);
    inner[2] = kind;
    let (outer, orphan) = orphaned(&case, &inner, &release);
    wait_for_file(&started);

    let outer_pid = end_parent(outer, &release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, orphan.pid));
    let take = take_of(&case, orphan.pid);
    assert_eq!(
        (&take["slots"], &take["parentPid"]),
        (&Value::from(vec![slot_name(&case, object)]), &Value::from(outer_pid)),
        "{take}"
    );
    assert_eq!(
        claimant(&case, object),
        orphan.pid,
        "the covered gate claims its slot while its command runs"
    );

    let_go(&go);
    assert_eq!(orphan.ended(), 0);
    let claim = v1::claim_path(&case.root(), object).expect("a pool's slot has a claim");
    assert!(!claim.exists(), "the claim goes with the release");
    assert_eq!(events_of(&case, orphan.pid), ["covered", "take", "release"]);
    assert_eq!(covered_files(&case), Vec::<String>::new(), "the covered marker goes with the release");
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the covered gate wrote its log");
    assert_in_order(
        &inner_log,
        &[
            &format!("gate: running under the heavy slot of gate {outer_pid}"),
            &parent_ended_line(outer_pid, kind),
        ],
    );
}

#[test]
fn a_covered_gate_takes_a_slot_when_its_parent_gate_ends() {
    a_covered_gate_takes_a_slot_when_its_parent_gate_ends_as(
        "a_covered_gate_takes_a_slot_when_its_parent_gate_ends",
        "heavy",
        KernelObject::HeavySlot(0),
    );
}

#[test]
fn a_parent_s_run_end_ends_its_plain_child_and_spares_its_covered_gate() {
    let case = Case::new("a_parent_s_run_end_ends_its_plain_child_and_spares_its_covered_gate");
    let started = case.folder().join("started");
    let go = case.folder().join("go");
    let plain_go = case.folder().join("plain.go");
    let release = case.folder().join("outer.release");
    let pid_file = case.folder().join("inner.pid");
    let texts = [&started, &go, &plain_go, &release, &pid_file].map(|path| shown(path));
    let mut command = vec![FAKE_TOOL, "spawn-wait", &texts[2], "spawn-pid", &texts[4], &texts[3], GATE];
    let mut inner = run_words("inner.log", &LOOK_OFTEN, &[FAKE_TOOL, "mark", &texts[0], "wait", &texts[1]]);
    inner[2] = "heavy";
    command.extend(inner);
    let outer = case.start(&heavy_words("outer.log", &[], &command));
    let orphan = spawned(&case, &pid_file);
    wait_for_file(&started);
    let outer_log = fs::read_to_string(case.folder().join("outer.log")).expect("the outer gate's log is written");
    let plain_pid = said_pid(&outer_log, "spawned ");
    let plain = StoppableProcess::open(plain_pid)
        .expect("the plain child is opened")
        .expect("the plain child runs: it waits for a go file no one has written");

    let outer_pid = end_parent(outer, &release);
    let plain_ended = Escapee::exit_code_once_ended(&plain);
    // The plain child is let go before any assertion, so a failing case leaves no helper waiting.
    let_go(&plain_go);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, orphan.pid));
    let take = take_of(&case, orphan.pid);
    let_go(&go);

    assert_eq!(plain_ended, Some(124), "the parent's run end ended its plain child");
    assert_eq!(take["parentPid"], Value::from(outer_pid), "the covered gate took its late slot: {take}");
    assert_eq!(orphan.ended(), 0, "the covered gate lived on to its own command's end");
    assert_eq!(events_of(&case, orphan.pid), ["covered", "take", "release"]);
}

#[test]
fn a_covered_light_gate_takes_a_light_slot() {
    a_covered_gate_takes_a_slot_when_its_parent_gate_ends_as("a_covered_light_gate_takes_a_light_slot", "light", KernelObject::LightSlot(0));
}

/// A light pool of one slot.
const ONE_LIGHT: &str = r#"{"v":1,"light":1}"#;

/// Starts a light gate whose command holds its slot until `go` exists, and returns once it has taken the slot.
fn light_holder(case: &Case, go: &Path) -> Running {
    let go = shown(go);
    let holder = case.start(&run_words("holder.log", &[], &[FAKE_TOOL, "out", "holding", "wait", &go]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    holder
}

/// The size of the file at `path`.
fn size(path: &Path) -> u64 {
    fs::metadata(path).expect("the file's size can be read").len()
}

#[test]
fn a_covered_gate_waits_for_a_slot_without_pausing_its_command() {
    let case = Case::new("a_covered_gate_waits_for_a_slot_without_pausing_its_command");
    write_counts(&case.root(), ONE_LIGHT);
    let holder_go = case.folder().join("holder-go");
    let holder = light_holder(&case, &holder_go);
    let go = case.folder().join("go");
    let release = case.folder().join("outer.release");
    let go_text = shown(&go);
    let inner = run_words("inner.log", &LOOK_OFTEN, &[FAKE_TOOL, "chatter", &go_text]);
    let stderr = case.folder().join("inner.stderr");
    let (outer, orphan) = orphaned_by(&case, "heavy", &inner, &release, Some(&stderr));

    let outer_pid = end_parent(outer, &release);
    let log = case.folder().join("inner.log");
    wait_until_said(&orphan, &stderr, WAITING_FOR_THE_LIGHT_SLOT);
    let queued_size = size(&log);
    wait_until("the covered gate's command writing on as the gate waits", || size(&log) > queued_size);
    assert_eq!(events_of(&case, orphan.pid), ["covered"], "no take while the one light slot is held");

    let_go(&holder_go);
    assert_passed(&holder.finish());
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, orphan.pid));
    let take = take_of(&case, orphan.pid);
    assert_eq!(take["parentPid"], Value::from(outer_pid));
    assert!(take["waitedSeconds"].as_f64().expect("a take has its wait") > 0.0, "{take}");

    let_go(&go);
    assert_eq!(orphan.ended(), 0);
    assert_eq!(events_of(&case, orphan.pid), ["covered", "take", "release"]);
    let order = [parent_ended_line(outer_pid, "light"), WAITING_FOR_THE_LIGHT_SLOT.to_owned()];
    let order: Vec<&str> = order.iter().map(String::as_str).collect();
    let said = fs::read_to_string(&stderr).expect("the covered gate's stderr can be read");
    assert_in_order(&said, &order);
    let inner_log = fs::read_to_string(&log).expect("the covered gate wrote its log");
    assert_in_order(&inner_log, &order);
}

/// What a covered gate that finds the one light slot held says, once.
const WAITING_FOR_THE_LIGHT_SLOT: &str = "gate: waiting for a light slot (1 busy)";

/// Returns once the covered gate `orphan` has said `line` on its stderr, sent to the file `stderr`; fails at once when
/// the gate ended without saying it. A ticket shows only that the gate queued: a gate held back before its first pass
/// finds the slot free once the holder lets go, and rightly says nothing, so a case that needs the gate to have found
/// the slot held waits for the line that says so.
fn wait_until_said(orphan: &Orphan, stderr: &Path, line: &str) {
    line_with_unless_ended(stderr, line, || orphan.has_ended());
}

#[test]
fn a_covered_gate_whose_command_ends_while_waiting_says_nothing_more() {
    let case = Case::new("a_covered_gate_whose_command_ends_while_waiting_says_nothing_more");
    write_counts(&case.root(), ONE_LIGHT);
    let holder_go = case.folder().join("holder-go");
    let holder = light_holder(&case, &holder_go);
    let go = case.folder().join("go");
    let release = case.folder().join("outer.release");
    let go_text = shown(&go);
    // A still-waiting line is due on every pass of the wait, so a pass after the command's end would say one.
    let seams = ["--test-sample-ms", "100", "--test-poll-ms", "20", "--test-heartbeat-ms", "1"];
    let inner = run_words("inner.log", &seams, &[FAKE_TOOL, "wait", &go_text]);
    let stderr = case.folder().join("inner.stderr");
    let (outer, orphan) = orphaned_by(&case, "heavy", &inner, &release, Some(&stderr));

    let outer_pid = end_parent(outer, &release);
    wait_until_said(&orphan, &stderr, WAITING_FOR_THE_LIGHT_SLOT);
    let_go(&go);
    assert_eq!(orphan.ended(), 0);

    assert_eq!(events_of(&case, orphan.pid), ["covered"], "no take and no release");
    assert!(!has_tickets(&case), "the covered gate's ticket is deleted");
    assert_eq!(claimant(&case, KernelObject::LightSlot(0)), holder.id(), "only the holder claims a slot");
    assert_eq!(covered_files(&case), Vec::<String>::new(), "the covered marker goes with the run");
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the covered gate wrote its log");
    assert_in_order(&inner_log, &[&parent_ended_line(outer_pid, "light"), WAITING_FOR_THE_LIGHT_SLOT]);
    let (waiting, ended) = inner_log.split_once("gate: passed in").expect("the covered gate passed");
    assert!(
        !waiting.contains("gate: interrupted") && !waiting.contains("gate: took"),
        "the wait ends silently: {inner_log}"
    );
    assert!(
        !ended.contains("gate: still waiting") && !ended.contains("gate: waiting"),
        "no line of the wait follows the run's end: {inner_log}"
    );

    let_go(&holder_go);
    assert_passed(&holder.finish());
}

#[test]
fn a_heavy_gate_under_a_light_parent_takes_a_heavy_slot() {
    let case = Case::new("a_heavy_gate_under_a_light_parent_takes_a_heavy_slot");
    write_counts(&case.root(), ONE_HEAVY);
    let go = case.folder().join("go");
    let holder = heavy_holder(&case, &go);
    let holder_pid = holder.id();

    let words = heavy_words("under.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let under = start(marked(&case, &words, &test_marker(&case, "light")));
    under.wait_for_stderr("gate: waiting for a heavy slot (");
    let_go(&go);
    let outcome = under.finish();
    assert_passed(&outcome);
    assert_passed(&holder.finish());

    let line = format!(
        "gate: the light slot of gate {} does not cover a heavy gate; taking a heavy slot",
        process::id()
    );
    assert_in_order(&outcome.stderr, &[&line, "gate: waiting for a heavy slot ("]);
    assert_eq!(events_besides(&case, holder_pid), ["queue", "take", "release"]);
}

/// The priority class of the process `pid`, read by the case itself: what its waits and assertions are ordered on.
/// `None` once the process has ended.
fn class_of(pid: u32) -> Option<Priority> {
    priority_of(pid).expect("a process's class can be read")
}

/// The pid of the command that wrote `pid <n>` to the log at `log`, once it has.
fn command_pid(log: &Path) -> u32 {
    wait_until("the command to say its pid", || {
        fs::read_to_string(log).is_ok_and(|text| text.lines().any(|line| line.starts_with("pid ")))
    });
    said_pid(&fs::read_to_string(log).expect("the log can be read"), "pid ")
}

#[test]
fn a_covered_gate_whose_critical_parent_ended_lowers_its_command() {
    let case = Case::new("a_covered_gate_whose_critical_parent_ended_lowers_its_command");
    let go = shown(&case.folder().join("go"));
    let release = case.folder().join("outer.release");
    let inner = heavy_words("inner.log", &LOOK_OFTEN, &[FAKE_TOOL, "pid", "wait", &go]);
    // The heavy gate runs under the critical outer gate's slot, as critical: its command at High.
    let (outer, orphan) = orphaned_as(&case, "critical", &inner, &release);
    let command = command_pid(&case.folder().join("inner.log"));
    assert_eq!(class_of(command), Some(Priority::High), "the command runs as critical");

    end_parent(outer, &release);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, orphan.pid));
    wait_until("the command lowered from High at the take", || {
        class_of(command) == Some(Priority::BelowNormal)
    });

    fs::write(&go, "").expect("the go file can be written");
    assert_eq!(orphan.ended(), 0);
}

#[test]
fn a_light_gate_under_a_critical_parent_that_ends_runs_as_light_under_a_heavy_stand_in() {
    let case = Case::new("a_light_gate_under_a_critical_parent_that_ends_runs_as_light_under_a_heavy_stand_in");
    let paths = ["c.pid", "c.release", "c-go", "outer.release"].map(|name| case.folder().join(name));
    let [c_pid, c_release, c_go, release] = &paths;
    let texts = paths.each_ref().map(|path| shown(path));
    let mut a_command = vec![FAKE_TOOL, "spawn-pid", &texts[0], &texts[1], GATE];
    a_command.extend(run_words("c.log", &LOOK_OFTEN, &[FAKE_TOOL, "pid", "wait", &texts[2]]));
    let a_words = heavy_words("a.log", &LOOK_OFTEN, &a_command);

    // O (critical) > A (heavy) > C (light): both run as critical, C's command at High.
    let (outer, a) = orphaned_as(&case, "critical", &a_words, release);
    let c = spawned(&case, c_pid);
    let command = command_pid(&case.folder().join("c.log"));
    assert_eq!(class_of(command), Some(Priority::High), "C's command runs as critical");

    let outer_pid = end_parent(outer, release);
    let c_line = line_with(&case.folder().join("a.log.err"), &format!("gate: the parent gate {outer_pid} ended;"));
    assert_eq!(
        c_line,
        format!("gate: the parent gate {outer_pid} ended; running under the heavy slot of gate {}", a.pid),
        "A stands in with the heavy slot it takes, not the critical one O held"
    );
    wait_until("C's command lowered from High as a light gate's runs", || {
        class_of(command) == Some(Priority::BelowNormal)
    });

    let_go(c_go);
    assert_eq!(c.ended(), 0);
    let_go(c_release);
    assert_eq!(a.ended(), 0);
    assert_eq!(events_of(&case, c.pid), ["covered"]);
}

#[test]
fn a_gate_given_a_box_job_it_does_not_run_inside_ignores_it() {
    let case = Case::new("a_gate_given_a_box_job_it_does_not_run_inside_ignores_it");
    let mut command = case.gate(&heavy_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]));
    // A box run's job name no job has: no box run holds this gate.
    command.env("GATE_BOX_JOB", format!(r"Local\gate-remote-nowhere-{:012x}", process::id()));

    let outcome = finish(command);

    assert_passed(&outcome);
    let stderr: Vec<&str> = outcome.stderr.lines().collect();
    assert!(
        stderr.contains(&"gate: ignored GATE_BOX_JOB: this gate does not run inside its job"),
        "{outcome:?}"
    );
    assert!(!outcome.stderr.contains("inside a routed run"), "{outcome:?}");
    let take = events(&case)
        .into_iter()
        .find(|event| event["event"] == "take")
        .expect("the gate journalled its take");
    let slot = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::HeavySlot(0));
    assert_eq!(take["slots"], Value::from(vec![slot]), "its heavy slot, as usual: {take}");
}

/// The line fake-tool's `priority` step prints at `BELOW_NORMAL_PRIORITY_CLASS`.
const BELOW_NORMAL_LINE: &str = "priority 0x4000";

/// The lines of the log `name` in the case folder.
fn log_lines(case: &Case, name: &str) -> Vec<String> {
    let text = fs::read_to_string(case.folder().join(name)).expect("the gate wrote its log");
    text.lines().map(str::to_owned).collect()
}

/// The line a commit run asked for on no slot says once, before it waits for the light slot it takes.
const NO_SLOT_COMMIT_LINE: &str = "gate: a commit's hooks take a slot: pass --slot heavy or --slot light";

/// The line a critical or `--alone` gate inside a commit's hooks says as it is refused.
const IN_COMMIT_LINE: &str = "gate: inside a git commit's hooks; a critical or --alone gate cannot run there";

/// The words of a run on a slot of some kind, as [`run_words`] and its siblings give them.
type Words = for<'a> fn(&'a str, &[&'a str], &[&'a str]) -> Vec<&'a str>;

/// How many times `line` is among `lines`.
fn count_of<'a>(lines: impl IntoIterator<Item = &'a str>, line: &str) -> usize {
    lines.into_iter().filter(|said| *said == line).count()
}

/// Asserts that the one gate journalled in `case` is a commit run: its `queue`, `take` and `release` carry `kind` and
/// `commit: true`, and its take names the slot `slot`.
fn assert_commit_lines(case: &Case, kind: &str, slot: KernelObject) {
    let lines = events(case);
    let names: Vec<&str> = lines.iter().filter_map(|line| line["event"].as_str()).collect();
    assert_eq!(names, ["queue", "take", "release"], "{lines:#?}");
    for line in &lines {
        assert_eq!((&line["kind"], &line["commit"]), (&Value::from(kind), &Value::Bool(true)), "{line}");
    }
    let slot = slot_name(case, slot);
    assert_eq!(lines[1]["slots"], Value::from(vec![slot]), "its slot taken: {}", lines[1]);
}

#[test]
fn a_no_slot_git_commit_takes_a_light_slot_says_so_once_and_runs_its_command_below_normal() {
    let case = Case::new("a_no_slot_git_commit_takes_a_light_slot_says_so_once_and_runs_its_command_below_normal");
    let git = tool(&case, "git.exe");
    let outcome = case.run(&none_words("commit.log", &[], &[&git, "commit", "priority"]));
    assert_passed(&outcome);

    assert_eq!(count_of(outcome.stderr.lines(), NO_SLOT_COMMIT_LINE), 1, "{outcome:?}");
    let log = log_lines(&case, "commit.log");
    assert_eq!(count_of(log.iter().map(String::as_str), NO_SLOT_COMMIT_LINE), 1, "{log:#?}");
    assert!(log.iter().any(|line| line == BELOW_NORMAL_LINE), "the commit runs below normal: {log:#?}");
    assert_commit_lines(&case, "light", KernelObject::LightSlot(0));
}

#[test]
fn a_light_or_heavy_git_commit_takes_the_slot_it_asked_for_and_says_nothing_of_it() {
    let kinds: [(&str, KernelObject, Words); 2] = [
        ("light", KernelObject::LightSlot(0), run_words),
        ("heavy", KernelObject::HeavySlot(0), heavy_words),
    ];
    for (kind, slot, words) in kinds {
        let case = Case::new(&format!("a_{kind}_git_commit_takes_the_slot_it_asked_for_and_says_nothing_of_it"));
        let git = tool(&case, "git.exe");
        let outcome = case.run(&words("commit.log", &[], &[&git, "commit", "priority"]));
        assert_passed(&outcome);

        assert!(!outcome.stderr.contains(NO_SLOT_COMMIT_LINE), "{kind}: {outcome:?}");
        let log = log_lines(&case, "commit.log");
        assert_eq!(count_of(log.iter().map(String::as_str), NO_SLOT_COMMIT_LINE), 0, "{kind}: {log:#?}");
        assert!(
            log.iter().any(|line| line == BELOW_NORMAL_LINE),
            "the {kind} commit runs below normal: {log:#?}"
        );
        assert_commit_lines(&case, kind, slot);
    }
}

#[test]
fn a_no_slot_run_that_is_no_commit_takes_no_slot_and_runs_below_normal() {
    let case = Case::new("a_no_slot_run_that_is_no_commit_takes_no_slot_and_runs_below_normal");
    let git = tool(&case, "git.exe");
    let other = case.run(&none_words("other.log", &[], &[FAKE_TOOL, "priority"]));
    assert_passed(&other);
    // git whose first word after it is no `commit`.
    let not_commit = case.run(&none_words("git.log", &[], &[&git, "priority"]));
    assert_passed(&not_commit);

    for outcome in [&other, &not_commit] {
        assert!(!outcome.stderr.contains(NO_SLOT_COMMIT_LINE), "{outcome:?}");
    }
    for log in ["other.log", "git.log"] {
        let lines = log_lines(&case, log);
        assert!(lines.iter().any(|line| line == BELOW_NORMAL_LINE), "{log} runs below normal");
        assert_eq!(count_of(lines.iter().map(String::as_str), NO_SLOT_COMMIT_LINE), 0, "{log}: {lines:#?}");
    }
    let lines = events(&case);
    let names: Vec<&str> = lines.iter().filter_map(|line| line["event"].as_str()).collect();
    assert_eq!(names, ["queue", "take", "release", "queue", "take", "release"], "{lines:#?}");
    for line in &lines {
        assert_eq!((&line["kind"], line.get("commit")), (&Value::from("none"), None), "{line}");
    }
    assert_eq!((&lines[1]["slots"], &lines[4]["slots"]), (&json_empty(), &json_empty()), "no slot taken");
}

#[test]
fn a_no_slot_git_commit_under_a_covering_parent_says_nothing_of_its_slot() {
    let case = Case::new("a_no_slot_git_commit_under_a_covering_parent_says_nothing_of_its_slot");
    let git = tool(&case, "git.exe");
    let mut inner = vec![GATE];
    inner.extend(none_words("commit.log", &[], &[&git, "commit", "exit", "0"]));
    let outcome = case.run(&heavy_words("outer.log", &[], &inner));
    assert_passed(&outcome);

    let log = log_lines(&case, "commit.log");
    assert_eq!(count_of(log.iter().map(String::as_str), NO_SLOT_COMMIT_LINE), 0, "{log:#?}");
}

/// An empty JSON array, as a `take` of no slot lists its slots.
fn json_empty() -> Value {
    Value::Array(Vec::new())
}

/// A commit run of `outer` words in `case` whose hooks start a gate of `inner` words, logging to `inner.log`, and wait
/// for it to end: the commit's pid, the nested gate's pid and the nested gate's exit status, once the commit passed.
fn commit_with_hook_gate(case: &Case, outer: Words, inner: &[&str]) -> (u32, u32, u32) {
    let git = tool(case, "git.exe");
    let (pid_file, release) = (case.folder().join("inner.pid"), case.folder().join("outer.release"));
    let (pid_text, release_text) = (shown(&pid_file), shown(&release));
    let mut command = vec![git.as_str(), "commit", "spawn-pid", &pid_text, &release_text, GATE];
    command.extend(inner);
    let running = case.start(&outer("outer.log", &[], &command));
    let outer_pid = running.id();

    let nested = started_gate(&pid_file);
    let status = nested.ended();
    let_go(&release);
    assert_passed(&running.finish());
    (outer_pid, nested.pid, status)
}

#[test]
fn a_heavy_gate_in_a_light_commit_run_s_hooks_takes_a_heavy_slot_of_its_own() {
    let case = Case::new("a_heavy_gate_in_a_light_commit_run_s_hooks_takes_a_heavy_slot_of_its_own");
    let inner = heavy_words("inner.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let (outer_pid, inner_pid, status) = commit_with_hook_gate(&case, none_words, &inner);

    assert_eq!(status, 0, "the nested gate passed");
    assert_eq!(
        events_of(&case, inner_pid),
        ["queue", "take", "release"],
        "the nested gate takes its own slot"
    );
    let inner_take = take_of(&case, inner_pid);
    assert_eq!(
        (&inner_take["slots"], &inner_take["parentPid"]),
        (&Value::from(vec![slot_name(&case, KernelObject::HeavySlot(0))]), &Value::from(outer_pid)),
        "{inner_take}"
    );
    let outer_take = take_of(&case, outer_pid);
    assert_eq!(
        outer_take["slots"],
        Value::from(vec![slot_name(&case, KernelObject::LightSlot(0))]),
        "{outer_take}"
    );
}

#[test]
fn a_heavy_gate_in_a_heavy_commit_run_s_hooks_runs_under_its_slot_without_waiting_for_another() {
    let case = Case::new("a_heavy_gate_in_a_heavy_commit_run_s_hooks_runs_under_its_slot_without_waiting_for_another");
    // The commit holds the heavy pool's one slot: a nested gate that waited for a heavy slot would never end.
    write_counts(&case.root(), ONE_HEAVY);
    let inner = heavy_words("inner.log", &[], &[FAKE_TOOL, "exit", "0"]);
    let (outer_pid, inner_pid, status) = commit_with_hook_gate(&case, heavy_words, &inner);

    assert_eq!(status, 0, "the nested gate ran under the commit's slot");
    assert_eq!(events_of(&case, inner_pid), ["covered"]);
    assert_eq!(covered(&case)["parentPid"], Value::from(outer_pid));
    assert_eq!(events_of(&case, outer_pid), ["queue", "take", "release"]);
}

#[test]
fn a_critical_gate_in_a_commit_run_s_hooks_is_refused_naming_the_commit() {
    let rows: [(&str, &[&str]); 2] = [("critical", &[]), ("alone", &["--alone"])];
    for (name, extra) in rows {
        let case = Case::new(&format!("a_{name}_gate_in_a_commit_run_s_hooks_is_refused_naming_the_commit"));
        fs::write(case.folder().join("inner.log"), "earlier\n").expect("an earlier log can be written");
        let inner = critical_words("inner.log", extra, &[FAKE_TOOL, "exit", "0"]);
        let (outer_pid, inner_pid, status) = commit_with_hook_gate(&case, none_words, &inner);

        assert_eq!(status, 2, "{name}: refused");
        assert_eq!(log_lines(&case, "inner.log"), ["earlier"], "{name}: the earlier log is left in place");
        // The hook gate's stderr is the commit's command's, which the commit's log holds.
        let outer_log = log_lines(&case, "outer.log");
        assert_eq!(
            count_of(outer_log.iter().map(String::as_str), IN_COMMIT_LINE),
            1,
            "{name}: {outer_log:#?}"
        );
        assert!(events_of(&case, inner_pid).is_empty(), "{name}: the refused gate journals nothing");
        assert_eq!(events_of(&case, outer_pid), ["queue", "take", "release"], "{name}");
    }
}

#[test]
fn a_critical_gate_under_a_commit_run_s_marker_says_its_refusal_on_stderr() {
    let rows: [(&str, &[&str]); 2] = [("critical", &[TRUSTED]), ("alone", &[TRUSTED, "--alone"])];
    for (name, extra) in rows {
        let case_name = format!("a_{name}_gate_under_a_commit_run_s_marker_says_its_refusal_on_stderr");
        let (outcome, _case) = refused(&case_name, "light", |case| {
            let mut gate = case.gate(&critical_words("refused.log", extra, &[FAKE_TOOL, "exit", "0"]));
            // The marker a commit run hands its command, naming this test process's light slot as the commit's.
            gate.env("GATE_COMMIT_SLOT", test_marker(case, "light"));
            gate
        });
        assert_eq!(outcome.stderr, format!("{IN_COMMIT_LINE}\n"), "{name}");
    }
}

#[test]
fn a_critical_gate_under_a_light_gate_that_is_no_commit_keeps_today_s_refusal() {
    let case = Case::new("a_critical_gate_under_a_light_gate_that_is_no_commit_keeps_today_s_refusal");
    let mut inner = vec![GATE];
    inner.extend(critical_words("inner.log", &[], &[FAKE_TOOL, "exit", "0"]));
    let outcome = case.run(&run_words("outer.log", &[], &inner));

    assert_eq!(outcome.code, Some(2), "the nested gate's refusal fails the run: {outcome:?}");
    let said = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(
        said.contains("gate: --slot critical cannot run under the light slot of gate "),
        "{outcome:?}"
    );
    assert!(!said.contains(IN_COMMIT_LINE), "{outcome:?}");
}

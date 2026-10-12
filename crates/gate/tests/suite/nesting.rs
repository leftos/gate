//! Nesting under a parent gate's slot marker, `GATE_SLOT_HELD`: a gate a live parent's slot covers takes no slot and
//! journals `covered`, a critical gate under a heavy or light marker is refused, a heavy gate under a light marker takes
//! a heavy slot, and a marker that is malformed, names a gate no longer running or belongs to another root is ignored. A
//! marker naming a live process whose gate's command this gate does not run inside is ignored too, unless the
//! `--test-parent-job-trusted` seam trusts it, as the cases naming the test process do. A covered gate keeps its
//! covered marker while its command runs, and a gate that queues deletes a dead gate's.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use gate_state::v1::{self, CoveredMarker};
use gate_state::{KernelObject, StateRoot};
use gate_win32::Job;
use jiff::Timestamp;
use serde_json::Value;

use crate::support::{
    Case, FAKE_TOOL, GATE, Outcome, POLL, Running, assert_in_order, critical_words, events, events_of, finish, heavy_words, none_words, run_words,
    shown, start, take_of, wait_for_file, wait_for_journal_line, write_counts,
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

fn let_go(go: &Path) {
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

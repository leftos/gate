//! Nesting under a parent gate's slot marker, `GATE_SLOT_HELD`: a gate a live parent's slot covers takes no slot and
//! journals `covered`, a critical gate under a heavy or light marker is refused, a heavy gate under a light marker takes
//! a heavy slot, and a marker that is malformed, names a gate no longer running or belongs to another root is ignored.

use std::fs;
use std::path::Path;
use std::process::{self, Command};

use gate_state::StateRoot;
use jiff::Timestamp;
use serde_json::Value;

use crate::support::{
    Case, FAKE_TOOL, GATE, Outcome, Running, assert_in_order, events, finish, heavy_words, run_words, shown, start, wait_for_journal_line,
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

/// A marker of a `held` slot naming this test process, which runs as a gate would, under the case's root.
pub(crate) fn test_marker(case: &Case, held: &str) -> String {
    let pid = process::id();
    format!("{held}:{pid}:{}:{}", start_ms(pid), root_hash(case))
}

/// The gate with `args` as the case readies it, started under `marker`.
pub(crate) fn marked(case: &Case, args: &[&str], marker: &str) -> Command {
    let mut command = case.gate(args);
    command.env("GATE_SLOT_HELD", marker);
    command
}

/// The words of a critical run, as [`run_words`] gives those of a light one.
fn critical_words<'a>(log: &'a str, command: &[&'a str]) -> Vec<&'a str> {
    let mut words = run_words(log, &[], command);
    words[2] = "critical";
    words
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

#[test]
fn a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine() {
    let case = Case::new("a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine");
    crate::support::write_counts(&case.root(), ONE_HEAVY);
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
        let (outcome, _case) = refused(&name, held, |case| case.gate(&critical_words("refused.log", &[FAKE_TOOL, "exit", "0"])));
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
    let words = critical_words("out.log", &[FAKE_TOOL, "exit", "0"]);
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
fn a_heavy_gate_under_a_light_parent_takes_a_heavy_slot() {
    let case = Case::new("a_heavy_gate_under_a_light_parent_takes_a_heavy_slot");
    crate::support::write_counts(&case.root(), ONE_HEAVY);
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

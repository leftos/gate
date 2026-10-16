//! The critical gate: it takes the critical mutex in arrival order, waits for the heavy and light gates that hold a slot
//! or arrived before it while more light gates run than may run beside it, takes every heavy slot and the free light
//! slots but those left for light gates beside it, says and journals what it found, and releases it all; a Ctrl+C while
//! it waits leaves nothing behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use gate_state::v1::{self, Claim, ClaimKind, Ticket, TicketKind};
use gate_state::{KernelObject, StateRoot};
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::durations::seed;
use crate::nesting::{marked, test_marker};
use crate::support::{
    Case, FAKE_TOOL, GATE, POLL, Running, assert_in_order, critical_words, events, events_of, finish, heavy_words, none_words, run_words, shown,
    start, take_of, wait_for_journal_line, wait_until, write_counts,
};

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();
/// The prefix of a critical gate's waiting line.
const WAITING: &str = "gate: waiting for the machine (critical)";
/// The prefix of a critical gate's holds line.
const HOLDS: &str = "gate: holds the machine (critical) at ";

/// The full name of `object` under the case's root.
fn name(case: &Case, object: KernelObject) -> String {
    StateRoot::new(&shown(&case.root()), None).object_name(object)
}

/// The journal's lines as their event and pid, in order.
fn sequence(case: &Case) -> Vec<(String, u32)> {
    events(case)
        .iter()
        .map(|event| {
            let name = event["event"].as_str().expect("a line names its event").to_owned();
            let pid = event["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()).expect("a line has a pid");
            (name, pid)
        })
        .collect()
}

/// Where the line of `event` by the gate `pid` stands in the journal; fails when there is none.
fn at(case: &Case, event: &str, pid: u32) -> usize {
    let sequence = sequence(case);
    let found = sequence.iter().position(|(name, of)| name == event && *of == pid);
    assert!(found.is_some(), "no {event} line of gate {pid} in {sequence:?}");
    found.expect("checked above")
}

/// The names of a `slots` list, sorted: a critical gate lists its slots in the order it took them.
fn sorted(slots: &Value) -> Vec<String> {
    let list = slots.as_array().expect("slots is a list");
    let mut names: Vec<String> = list.iter().map(|name| name.as_str().expect("a slot name").to_owned()).collect();
    names.sort();
    names
}

/// Starts the gate with `words` and returns once it has journalled its take.
fn holder(case: &Case, words: &[&str]) -> Running {
    let running = case.start(words);
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, running.id()));
    running
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

fn assert_passed(outcome: &crate::support::Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The start of this test process in Unix epoch milliseconds, which a ticket or claim naming it as a live gate carries.
fn own_start() -> Timestamp {
    gate_win32::process_start_of(process::id())
        .expect("this process's start can be read")
        .expect("this process runs")
}

/// Writes a ticket of `kind` naming the gate `pid` started at `start`, arrived at `arrived` ms, in the case's root.
fn write_ticket(case: &Case, kind: TicketKind, (pid, start): (u32, Timestamp), arrived: u64, remote: bool) {
    let start_ms = start.as_millisecond().unsigned_abs();
    let ticket = Ticket {
        kind,
        arrived,
        pid,
        start_ms,
        holds: false,
        lends_closed: false,
        remote: remote.then_some(true),
        expected: None,
    };
    v1::write_ticket(&v1::ticket_path(&case.root(), pid, start_ms), &ticket).expect("the ticket can be written");
}

/// Whether the case's root holds a ticket of the gate `pid`.
fn has_ticket(case: &Case, pid: u32) -> bool {
    match fs::read_dir(v1::tickets_dir(&case.root())) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        listed => listed.expect("the tickets folder can be listed").any(|entry| {
            entry
                .expect("a ticket entry can be read")
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{pid}-"))
        }),
    }
}

#[test]
fn a_critical_gate_starts_only_after_every_earlier_holder_has_released() {
    let case = Case::new("a_critical_gate_starts_only_after_every_earlier_holder_has_released");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let held_pid = held.id();

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(&format!(
        "{WAITING}: 1 heavy and 0 light slots held (2 light slots beside it), 0 earlier waiters\n"
    ));
    assert_eq!(events_of(&case, critical_pid), ["queue"], "no take while the holder holds");

    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
    assert!(at(&case, "release", held_pid) < at(&case, "take", critical_pid));
    assert!(at(&case, "take", critical_pid) < at(&case, "release", critical_pid));
    let names = [name(&case, KernelObject::HeavySlot(0)), name(&case, KernelObject::LightSlot(0))];
    assert_eq!(sorted(&take_of(&case, critical_pid)["slots"]), sorted(&json!(names)));
    let release = events(&case)
        .into_iter()
        .find(|event| event["event"] == "release" && event["pid"] == critical_pid)
        .expect("the critical gate journalled its release");
    assert_eq!(
        release["slots"],
        take_of(&case, critical_pid)["slots"],
        "the release lists the same slots"
    );
}

#[test]
fn a_critical_gate_waits_for_an_earlier_waiting_heavy_gate() {
    let case = Case::new("a_critical_gate_waits_for_an_earlier_waiting_heavy_gate");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let held_pid = held.id();
    let waiter = case.start(&heavy_words("waiter.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let waiter_pid = waiter.id();
    waiter.wait_for_stderr("gate: waiting for a heavy slot (");

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(&format!(
        "{WAITING}: 1 heavy and 0 light slots held (1 light slot beside it), 1 earlier waiters\n"
    ));

    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&waiter.finish());
    assert_passed(&held.finish());
    assert!(at(&case, "release", held_pid) < at(&case, "take", waiter_pid));
    assert!(at(&case, "release", waiter_pid) < at(&case, "take", critical_pid));
}

#[test]
fn a_critical_request_does_not_wait_for_a_gate_that_arrived_after_it() {
    let case = Case::new("a_critical_request_does_not_wait_for_a_gate_that_arrived_after_it");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let after = Timestamp::now().as_millisecond().unsigned_abs() + 3_600_000;
    write_ticket(&case, TicketKind::Heavy, (process::id(), own_start()), after, false);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(HOLDS);
    assert!(has_ticket(&case, process::id()), "the later gate's ticket stands");

    let later = case.start(&heavy_words("later.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let later_pid = later.id();
    later.wait_for_stderr(&behind(critical_pid));

    let_go(&go);
    let outcome = critical.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert_passed(&later.finish());
    assert!(
        at(&case, "release", critical_pid) < at(&case, "take", later_pid),
        "the later gate takes the slot only once the critical gate let it go"
    );
}

/// The first line of a heavy gate held back by the critical request of gate `pid`.
fn behind(pid: u32) -> String {
    format!("gate: waiting for a heavy slot behind the critical request of gate {pid}\n")
}

/// The first line of a light gate kept off the light slots by the critical request of gate `pid`.
fn beside(pid: u32) -> String {
    format!("gate: waiting for a light slot beside the critical request of gate {pid}\n")
}

/// The line of a gate lent the light slot of its parent gate `pid`.
fn lent(pid: u32) -> String {
    format!("gate: running under the light slot of gate {pid} while a critical request drains; taking no slot")
}

/// `POLL` with a still-waiting line every 50 ms.
fn with_heartbeat() -> Vec<&'static str> {
    let mut seams = POLL.to_vec();
    seams.extend(["--test-heartbeat-ms", "50"]);
    seams
}

/// The pid of the gate whose `parent` line names `parent`.
fn lent_to(case: &Case, parent: u32) -> u32 {
    let line = events(case)
        .into_iter()
        .find(|event| event["event"] == "parent" && event["parentPid"] == parent)
        .expect("a gate journalled its parent line");
    line["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()).expect("a line has a pid")
}

#[test]
fn a_later_heavy_gate_starts_only_after_the_critical_gate_releases() {
    let case = Case::new("a_later_heavy_gate_starts_only_after_the_critical_gate_releases");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(HOLDS);

    let later = case.start(&heavy_words("later.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let later_pid = later.id();
    later.wait_for_stderr(&behind(critical_pid));
    later.wait_for_stderr("gate: still waiting for a heavy slot (");
    assert_eq!(events_of(&case, later_pid), ["queue"], "no take while the critical gate holds");

    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&later.finish());
    assert!(at(&case, "release", critical_pid) < at(&case, "take", later_pid));
}

#[test]
fn a_later_heavy_gate_waits_behind_a_draining_critical_request_even_with_a_slot_free() {
    let case = Case::new("a_later_heavy_gate_waits_behind_a_draining_critical_request_even_with_a_slot_free");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let start = own_start();
    write_ticket(&case, TicketKind::Critical, (process::id(), start), 0, false);

    let later = case.start(&heavy_words("later.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let later_pid = later.id();
    later.wait_for_stderr(&behind(process::id()));
    assert_eq!(events_of(&case, later_pid), ["queue"], "no take behind the critical request");
    let slot = name(&case, KernelObject::HeavySlot(0));
    assert!(!gate_win32::mutex_exists(&slot).expect("the slot can be probed"), "the slot is free");

    let ticket = v1::ticket_path(&case.root(), process::id(), start.as_millisecond().unsigned_abs());
    fs::remove_file(&ticket).expect("the critical ticket can be deleted");
    let outcome = later.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("busy)"), "{outcome:?}");
    assert_eq!(events_of(&case, later_pid), ["queue", "take", "release"]);
}

#[test]
fn light_gates_keep_working_beside_a_holding_critical_gate() {
    let case = Case::new("light_gates_keep_working_beside_a_holding_critical_gate");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();

    let light = case.start(&run_words("light.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let light_pid = light.id();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("waiting for"), "{outcome:?}");
    assert_eq!(take_of(&case, light_pid)["slots"], json!([name(&case, KernelObject::LightSlot(1))]));
    assert_eq!(events_of(&case, critical_pid), ["queue", "take"], "the critical gate holds on");
    let_go(&go);
    assert_passed(&critical.finish());
}

/// What a journal line of the release of the gate `pid` contains.
fn release_needle(pid: u32) -> String {
    format!(r#""event":"release","pid":{pid},"#)
}

#[test]
fn a_no_slot_run_starts_while_a_critical_gate_holds_the_machine() {
    let case = Case::new("a_no_slot_run_starts_while_a_critical_gate_holds_the_machine");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();

    let none = case.start(&none_words("none.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let none_pid = none.id();
    wait_for_journal_line(&case.root(), &release_needle(none_pid));
    let outcome = none.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("waiting for"), "{outcome:?}");
    assert_eq!(events_of(&case, none_pid), ["queue", "take", "release"]);
    assert_eq!(take_of(&case, none_pid)["slots"], json!([]), "the no-slot run took no slot");
    assert_eq!(events_of(&case, critical_pid), ["queue", "take"], "the critical gate holds on");
    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn a_critical_gate_does_not_wait_for_a_running_no_slot_run() {
    let case = Case::new("a_critical_gate_does_not_wait_for_a_running_no_slot_run");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let none = holder(&case, &none_words("none.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let none_pid = none.id();

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    wait_for_journal_line(&case.root(), &release_needle(critical_pid));
    let outcome = critical.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert_eq!(events_of(&case, none_pid), ["queue", "take"], "the no-slot run still runs");
    assert_eq!(take_of(&case, critical_pid)["otherClaims"], json!([]));
    let_go(&go);
    assert_passed(&none.finish());
}

#[test]
fn a_third_light_gate_waits_for_a_light_slot_beside_the_critical_request_of_gate_n() {
    let case = Case::new("a_third_light_gate_waits_for_a_light_slot_beside_the_critical_request_of_gate_n");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let second = holder(&case, &run_words("second.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let (first_pid, second_pid) = (first.id(), second.id());

    let third = case.start(&run_words("third.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let third_pid = third.id();
    third.wait_for_stderr(&beside(critical_pid));
    third.wait_for_stderr(&format!(
        "gate: still waiting for a light slot beside the critical request of gate {critical_pid} ("
    ));
    assert_eq!(
        events_of(&case, third_pid),
        ["queue"],
        "two light gates run beside the critical gate already"
    );

    let_go(&light_go);
    assert_passed(&third.finish());
    assert_passed(&first.finish());
    assert_passed(&second.finish());
    let released = at(&case, "release", first_pid).min(at(&case, "release", second_pid));
    assert!(released < at(&case, "take", third_pid));
    assert_eq!(events_of(&case, critical_pid), ["queue", "take"], "the critical gate holds on");
    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn a_count_raised_during_the_critical_run_lets_no_more_light_in() {
    let case = Case::new("a_count_raised_during_the_critical_run_lets_no_more_light_in");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let second = holder(&case, &run_words("second.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let (first_pid, second_pid) = (first.id(), second.id());
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":5}"#);

    let third = case.start(&run_words("third.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let third_pid = third.id();
    third.wait_for_stderr(&beside(critical_pid));
    assert_eq!(
        events_of(&case, third_pid),
        ["queue"],
        "two light gates run beside the critical gate already"
    );
    let free = name(&case, KernelObject::LightSlot(3));
    assert!(
        !gate_win32::mutex_exists(&free).expect("the slot can be probed"),
        "a slot the raised count added is free, and the third gate still waits"
    );

    let_go(&light_go);
    assert_passed(&third.finish());
    assert_passed(&first.finish());
    assert_passed(&second.finish());
    let released = at(&case, "release", first_pid).min(at(&case, "release", second_pid));
    assert!(
        released < at(&case, "take", third_pid),
        "the third gate takes a slot only once a light gate beside the critical gate let one go"
    );
    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn a_heavy_child_of_a_light_parent_is_lent_the_parents_slot_while_a_critical_request_drains() {
    let case = Case::new("a_heavy_child_of_a_light_parent_is_lent_the_parents_slot_while_a_critical_request_drains");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let held_pid = held.id();
    let mut child = vec![GATE];
    child.extend(heavy_words("child.log", &POLL, &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    let parent = holder(&case, &run_words("parent.log", &POLL, &child));
    let parent_pid = parent.id();
    wait_until("the child gate queueing", || {
        sequence(&case)
            .iter()
            .any(|(event, pid)| event == "queue" && ![held_pid, parent_pid].contains(pid))
    });

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    assert_passed(&parent.finish());

    let child_pid = lent_to(&case, parent_pid);
    assert_eq!(
        events_of(&case, child_pid),
        ["queue", "parent"],
        "the queued child left the queue for its parent's slot"
    );
    assert_eq!(events_of(&case, critical_pid), ["queue"], "the child ran while the critical gate drains");
    let child_log = fs::read_to_string(case.folder().join("child.log")).expect("the child wrote its log");
    assert!(child_log.contains(&lent(parent_pid)), "{child_log}");
    assert!(
        child_log.contains(&format!("light:{parent_pid}:")),
        "the child's command has its parent's marker: {child_log}"
    );

    let marker = test_marker(&case, "light");
    let early = finish(marked(&case, &heavy_words("early.log", &POLL, &[FAKE_TOOL, "exit", "0"]), &marker));
    assert_passed(&early);
    assert!(early.stderr.contains(&lent(process::id())), "{early:?}");
    let early_pid = lent_to(&case, process::id());
    assert_eq!(events_of(&case, early_pid), ["parent"], "lent before it queued, it journals no queue");

    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
    assert!(at(&case, "parent", early_pid) < at(&case, "take", critical_pid));
}

#[test]
fn a_lent_auto_run_uses_its_history_for_the_ceiling() {
    let case = Case::new("a_lent_auto_run_uses_its_history_for_the_ceiling");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let command = [FAKE_TOOL, "env", "GATE_SLOT_HELD"];
    for _ in 0..5 {
        assert_passed(&finish(case.gate(&heavy_words("history.log", &POLL, &command))));
    }
    // 3x an adjusted p99 of 14 min raises the floor of 15 min to 42 min.
    seed(&case, " | fake-tool env GATE_SLOT_HELD", 1.0, Some(840.0));
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let held_pid = held.id();
    let mut child = vec![GATE];
    child.extend(heavy_words("child.log", &POLL, &command));
    assert_eq!(child[4], "--timeout", "the ceiling's value follows it");
    child[5] = "auto:900";
    let parent = holder(&case, &run_words("parent.log", &POLL, &child));
    let parent_pid = parent.id();
    wait_until("the child gate queueing", || {
        sequence(&case)
            .iter()
            .any(|(event, pid)| event == "queue" && ![held_pid, parent_pid].contains(pid))
    });

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    critical.wait_for_stderr(WAITING);
    let parent_outcome = parent.finish();

    assert_passed(&parent_outcome);
    let child_pid = lent_to(&case, parent_pid);
    assert_eq!(events_of(&case, child_pid), ["queue", "parent"], "the child was lent as it waited");
    assert!(
        parent_outcome.stdout.contains(", ceiling 2520 s). Full output: "),
        "the lent child's passed line names the ceiling its history resolved: {parent_outcome:?}"
    );
    // The parent shows the child's stderr as it runs, up to the child's passed line, and again in its own log's tail.
    let (child_stream, _) = parent_outcome
        .stdout
        .split_once("child.log")
        .expect("the child's passed line names its log");
    assert_eq!(
        child_stream
            .matches("gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)")
            .count(),
        1,
        "the lent child says its raised ceiling once: {parent_outcome:?}"
    );
    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
}

#[test]
fn a_parent_on_the_highest_light_slot_lends_while_a_critical_request_drains() {
    let case = Case::new("a_parent_on_the_highest_light_slot_lends_while_a_critical_request_drains");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));

    let marker = test_marker(&case, "light");
    let outcome = finish(marked(&case, &heavy_words("child.log", &POLL, &[FAKE_TOOL, "exit", "0"]), &marker));

    assert_passed(&outcome);
    assert!(outcome.stderr.contains(&lent(process::id())), "{outcome:?}");
    let child_pid = lent_to(&case, process::id());
    assert_eq!(
        events_of(&case, child_pid),
        ["parent"],
        "a parent lends whatever light slot it holds, so the child journals no queue"
    );
    assert_eq!(events_of(&case, critical_pid), ["queue"], "the child ran while the critical gate drains");
    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
}

#[test]
fn a_holding_critical_gate_lends_nothing() {
    let case = Case::new("a_holding_critical_gate_lends_nothing");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();

    let marker = test_marker(&case, "light");
    let child = start(marked(&case, &heavy_words("child.log", &POLL, &[FAKE_TOOL, "exit", "0"]), &marker));
    let child_pid = child.id();
    child.wait_for_stderr(&behind(critical_pid));
    assert_eq!(events_of(&case, child_pid), ["queue"], "the child waits behind the holding critical gate");

    let_go(&go);
    assert_passed(&critical.finish());
    let outcome = child.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("while a critical request drains"), "{outcome:?}");
    assert!(at(&case, "release", critical_pid) < at(&case, "take", child_pid));
}

#[test]
fn critical_requests_run_one_at_a_time_in_arrival_order() {
    let case = Case::new("critical_requests_run_one_at_a_time_in_arrival_order");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let first = holder(&case, &critical_words("first.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let first_pid = first.id();
    let second = case.start(&critical_words("second.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let second_pid = second.id();
    second.wait_for_stderr(WAITING);
    let third = case.start(&critical_words("third.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let third_pid = third.id();
    third.wait_for_stderr(WAITING);

    let_go(&go);
    assert_passed(&third.finish());
    assert_passed(&second.finish());
    assert_passed(&first.finish());
    assert!(at(&case, "release", first_pid) < at(&case, "take", second_pid));
    assert!(at(&case, "release", second_pid) < at(&case, "take", third_pid));
}

#[test]
fn with_one_light_slot_the_critical_gate_does_not_wait_for_a_light_holder() {
    let case = Case::new("with_one_light_slot_the_critical_gate_does_not_wait_for_a_light_holder");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let light_pid = light.id();

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    let outcome = critical.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(light.is_running(), "the light gate runs on beside the critical gate");
    assert_eq!(take_of(&case, critical_pid)["slots"], json!([name(&case, KernelObject::HeavySlot(0))]));
    let_go(&go);
    assert_passed(&light.finish());
    assert!(at(&case, "release", critical_pid) < at(&case, "release", light_pid));
}

#[test]
fn one_light_gate_on_slot_0_does_not_hold_the_drain_and_the_critical_gate_leaves_one_light_slot_for_a_later_one() {
    let case = Case::new("one_light_gate_on_slot_0_does_not_hold_the_drain_and_the_critical_gate_leaves_one_light_slot_for_a_later_one");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let light_pid = light.id();
    assert_eq!(take_of(&case, light_pid)["slots"], json!([name(&case, KernelObject::LightSlot(0))]));

    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    let names = json!([name(&case, KernelObject::HeavySlot(0)), name(&case, KernelObject::LightSlot(1))]);
    assert_eq!(sorted(&take_of(&case, critical_pid)["slots"]), sorted(&names));
    assert_eq!(
        events_of(&case, light_pid),
        ["queue", "take"],
        "the light gate runs on beside the critical gate"
    );

    let later = holder(&case, &run_words("later.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let later_pid = later.id();
    assert_eq!(take_of(&case, later_pid)["slots"], json!([name(&case, KernelObject::LightSlot(2))]));
    let third = case.start(&run_words("third.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let third_pid = third.id();
    third.wait_for_stderr(&beside(critical_pid));
    third.wait_for_stderr(&format!(
        "gate: still waiting for a light slot beside the critical request of gate {critical_pid} ("
    ));
    assert_eq!(
        events_of(&case, third_pid),
        ["queue"],
        "two light gates run beside the critical gate already"
    );

    let_go(&light_go);
    assert_passed(&third.finish());
    assert_passed(&light.finish());
    assert_passed(&later.finish());
    let_go(&go);
    let outcome = critical.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
}

#[test]
fn two_light_gates_on_the_lowest_slots_run_on_while_the_critical_gate_takes_the_rest() {
    let case = Case::new("two_light_gates_on_the_lowest_slots_run_on_while_the_critical_gate_takes_the_rest");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let second = holder(&case, &run_words("second.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let lowest = json!([name(&case, KernelObject::LightSlot(0)), name(&case, KernelObject::LightSlot(1))]);
    let mut held = take_of(&case, first.id())["slots"].as_array().expect("slots is a list").clone();
    held.extend(take_of(&case, second.id())["slots"].as_array().expect("slots is a list").clone());
    assert_eq!(sorted(&json!(held)), sorted(&lowest));

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    let outcome = critical.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    let rest = json!([
        name(&case, KernelObject::HeavySlot(0)),
        name(&case, KernelObject::LightSlot(2)),
        name(&case, KernelObject::LightSlot(3))
    ]);
    assert_eq!(sorted(&take_of(&case, critical_pid)["slots"]), sorted(&rest));
    assert_eq!(events_of(&case, first.id()), ["queue", "take"], "the first light gate runs on");
    assert_eq!(events_of(&case, second.id()), ["queue", "take"], "the second light gate runs on");
    let_go(&light_go);
    assert_passed(&first.finish());
    assert_passed(&second.finish());
}

/// The seams of a critical gate that only a wake event moves on: a fallback poll and still-waiting period of an hour,
/// far past the hang guard of a case's waits.
const WAKE_ONLY: [&str; 4] = ["--test-poll-ms", "3600000", "--test-heartbeat-ms", "3600000"];

#[test]
fn a_critical_gate_waits_while_more_than_two_light_gates_run_and_a_release_on_the_highest_slot_wakes_it() {
    let case = Case::new("a_critical_gate_waits_while_more_than_two_light_gates_run_and_a_release_on_the_highest_slot_wakes_it");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let last_go = case.folder().join("last-go");
    let last_go_word = shown(&last_go);
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let second = holder(&case, &run_words("second.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let last = holder(&case, &run_words("last.log", &POLL, &[FAKE_TOOL, "wait", &last_go_word, "exit", "0"]));
    let last_pid = last.id();
    assert_eq!(take_of(&case, last_pid)["slots"], json!([name(&case, KernelObject::LightSlot(2))]));

    let critical = case.start(&critical_words("critical.log", &WAKE_ONLY, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(&format!(
        "{WAITING}: 0 heavy and 3 light slots held (2 light slots beside it), 0 earlier waiters\n"
    ));
    assert_eq!(
        events_of(&case, critical_pid),
        ["queue"],
        "three light gates run, one more than may run beside it"
    );

    let_go(&last_go);
    assert_passed(&last.finish());
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{critical_pid},"#));
    let names = json!([name(&case, KernelObject::HeavySlot(0)), name(&case, KernelObject::LightSlot(2))]);
    assert_eq!(
        sorted(&take_of(&case, critical_pid)["slots"]),
        sorted(&names),
        "the slot freed, whichever index"
    );
    assert!(at(&case, "release", last_pid) < at(&case, "take", critical_pid));
    assert_passed(&critical.finish());
    let_go(&light_go);
    assert_passed(&first.finish());
    assert_passed(&second.finish());
}

/// The seams of a light gate that waits between journalling its take and writing its claim until the file `go` exists.
fn slow_claim(go: &str) -> [&str; 4] {
    ["--test-poll-ms", "20", "--test-beside-claim-wait", go]
}

/// Whether the gate `gate` journalled a take in the case's root.
fn took(case: &Case, gate: &Running) -> bool {
    events_of(case, gate.id()).contains(&"take".to_owned())
}

#[test]
fn a_light_gate_between_its_take_and_its_claim_lets_no_third_light_gate_beside_the_critical_gate() {
    let case = Case::new("a_light_gate_between_its_take_and_its_claim_lets_no_third_light_gate_beside_the_critical_gate");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":6}"#);
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let claim_go = case.folder().join("claim-go");
    let claim_go_word = shown(&claim_go);

    let slow = holder(
        &case,
        &run_words("slow.log", &slow_claim(&claim_go_word), &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]),
    );
    let slow_claim = v1::claim_path(&case.root(), KernelObject::LightSlot(1)).expect("a light slot has a claim");
    assert!(!slow_claim.exists(), "the slow gate has journalled its take and not yet claimed its slot");
    let later = [
        case.start(&run_words(
            "second.log",
            &with_heartbeat(),
            &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
        )),
        case.start(&run_words(
            "third.log",
            &with_heartbeat(),
            &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
        )),
    ];
    for gate in &later {
        gate.wait_for_stderr(&beside(critical_pid));
    }
    assert!(
        !later.iter().any(|gate| took(&case, gate)),
        "no later gate took a slot while the slow gate's claim was missing"
    );

    let_go(&claim_go);
    wait_until("the slow gate's claim", || slow_claim.exists());
    wait_until("one later gate taking a slot beside the slow one", || {
        later.iter().any(|gate| took(&case, gate))
    });
    let waiting = later.iter().find(|gate| !took(&case, gate)).expect("one later gate still waits");
    let heartbeat = format!("gate: still waiting for a light slot beside the critical request of gate {critical_pid} (");
    let before = waiting.wait_for_stderr_where("its lines so far", |_| true).matches(&heartbeat).count();
    waiting.wait_for_stderr_where("two more still-waiting lines after the other's take", |stderr| {
        stderr.matches(&heartbeat).count() >= before + 2
    });
    assert_eq!(
        events_of(&case, waiting.id()),
        ["queue"],
        "two light gates run beside the critical gate already"
    );

    let_go(&light_go);
    assert_passed(&slow.finish());
    for gate in later {
        assert_passed(&gate.finish());
    }
    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn a_light_gate_no_request_holds_back_is_counted_between_its_take_and_its_claim() {
    let case = Case::new("a_light_gate_no_request_holds_back_is_counted_between_its_take_and_its_claim");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let claim_go = case.folder().join("claim-go");
    let claim_go_word = shown(&claim_go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let early = holder(
        &case,
        &run_words(
            "early.log",
            &slow_claim(&claim_go_word),
            &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
        ),
    );
    let early_claim = v1::claim_path(&case.root(), KernelObject::LightSlot(0)).expect("a light slot has a claim");
    assert!(
        !early_claim.exists(),
        "the early gate has journalled its take and not yet claimed its slot"
    );

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    let later = [
        case.start(&run_words(
            "second.log",
            &with_heartbeat(),
            &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
        )),
        case.start(&run_words(
            "third.log",
            &with_heartbeat(),
            &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
        )),
    ];
    for gate in &later {
        gate.wait_for_stderr(&beside(critical_pid));
    }
    assert!(
        !later.iter().any(|gate| took(&case, gate)),
        "no gate held back took a slot while the early gate, held back by no request, had not claimed its slot"
    );

    let_go(&claim_go);
    wait_until("the early gate's claim", || early_claim.exists());
    wait_until("one later gate taking a slot beside the early one", || {
        later.iter().any(|gate| took(&case, gate))
    });
    let waiting = later.iter().find(|gate| !took(&case, gate)).expect("one later gate still waits");
    let heartbeat = format!("gate: still waiting for a light slot beside the critical request of gate {critical_pid} (");
    let before = waiting.wait_for_stderr_where("its lines so far", |_| true).matches(&heartbeat).count();
    waiting.wait_for_stderr_where("two more still-waiting lines after the other's take", |stderr| {
        stderr.matches(&heartbeat).count() >= before + 2
    });
    assert_eq!(
        events_of(&case, waiting.id()),
        ["queue"],
        "two light gates run beside the critical request already"
    );

    let_go(&light_go);
    assert_passed(&early.finish());
    for gate in later {
        assert_passed(&gate.finish());
    }
    let_go(&go);
    assert_passed(&held.finish());
    assert_passed(&critical.finish());
}

#[test]
fn a_critical_gate_does_not_take_the_machine_while_a_lent_heavy_child_runs() {
    let case = Case::new("a_critical_gate_does_not_take_the_machine_while_a_lent_heavy_child_runs");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let child_go = case.folder().join("child-go");
    let child_go_word = shown(&child_go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));

    let marker = test_marker(&case, "light");
    let child = start(marked(
        &case,
        &heavy_words("child.log", &POLL, &[FAKE_TOOL, "wait", &child_go_word, "exit", "0"]),
        &marker,
    ));
    let child_pid = child.id();
    child.wait_for_stderr(&lent(process::id()));
    let lend_marker = format!("gate-lent-{}-{child_pid}-", process::id());
    let has_lend_marker = || {
        fs::read_dir(v1::slots_dir(&case.root()))
            .expect("the claims folder can be listed")
            .any(|entry| {
                entry
                    .expect("a claims entry can be read")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&lend_marker)
            })
    };
    assert!(has_lend_marker(), "the lent child keeps a lend marker naming its parent");

    let_go(&go);
    assert_passed(&held.finish());
    let heartbeat = "gate: still waiting for the machine (critical): ";
    let before = critical.wait_for_stderr_where("its lines so far", |_| true).matches(heartbeat).count();
    critical.wait_for_stderr_where("a still-waiting line after the heavy holder's end naming the lent child", |stderr| {
        stderr
            .split(heartbeat)
            .skip(before + 1)
            .any(|line| line.starts_with("1 heavy and 0 light slots held (2 light slots beside it), 0 earlier waiters"))
    });
    assert_eq!(
        events_of(&case, critical_pid),
        ["queue"],
        "the critical gate waits for the heavy child its parent lent its slot"
    );

    let_go(&child_go);
    assert_passed(&child.finish());
    assert!(!has_lend_marker(), "the lent child's marker is gone with it");
    assert_passed(&critical.finish());
    assert_eq!(events_of(&case, child_pid), ["parent"]);
    assert!(events_of(&case, critical_pid).contains(&"take".to_owned()));
}

/// The ticket file of the gate `pid` in the case's root; fails when there is none.
fn ticket_file(case: &Case, pid: u32) -> PathBuf {
    fs::read_dir(v1::tickets_dir(&case.root()))
        .expect("the tickets folder can be listed")
        .map(|entry| entry.expect("a ticket entry can be read").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&format!("{pid}-")))
        })
        .expect("the gate has a ticket")
}

/// Whether the ticket of the gate `pid` refuses every new lend, as its `lendsClosed` says.
fn lends_closed(case: &Case, pid: u32) -> bool {
    fs::read_to_string(ticket_file(case, pid))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|ticket| ticket["lendsClosed"] == json!(true))
}

/// Waits until the critical gate `critical` says a still-waiting line that starts with `status` after the lines it said
/// so far.
fn wait_for_heartbeat_after(critical: &Running, status: &str) {
    let heartbeat = "gate: still waiting for the machine (critical): ";
    let before = critical.wait_for_stderr_where("its lines so far", |_| true).matches(heartbeat).count();
    critical.wait_for_stderr_where("a later still-waiting line", |stderr| {
        stderr.split(heartbeat).skip(before + 1).any(|line| line.starts_with(status))
    });
}

/// The status of a critical gate that holds its slots of a count of heavy 1 and light 4 and waits for one lent child.
const ONE_LENT: &str = "1 heavy and 0 light slots held (2 light slots beside it), 0 earlier waiters";

#[test]
fn a_light_parent_lends_no_more_once_the_critical_gate_holds_its_slots_and_waits_for_its_lent_child() {
    let case = Case::new("a_light_parent_lends_no_more_once_the_critical_gate_holds_its_slots_and_waits_for_its_lent_child");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let child_go = case.folder().join("child-go");
    let child_go_word = shown(&child_go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));
    let marker = test_marker(&case, "light");
    let first = start(marked(
        &case,
        &heavy_words("first.log", &POLL, &[FAKE_TOOL, "wait", &child_go_word, "exit", "0"]),
        &marker,
    ));
    first.wait_for_stderr(&lent(process::id()));

    let_go(&go);
    assert_passed(&held.finish());
    wait_until("the critical gate closing its lends", || lends_closed(&case, critical_pid));
    let second = start(marked(&case, &heavy_words("second.log", &POLL, &[FAKE_TOOL, "exit", "0"]), &marker));
    let second_pid = second.id();
    second.wait_for_stderr(&format!(
        "gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"
    ));
    assert_eq!(events_of(&case, second_pid), ["queue"], "the second child queues behind the request");

    let_go(&child_go);
    assert_passed(&first.finish());
    assert_passed(&critical.finish());
    let second = second.finish();
    assert_passed(&second);
    assert!(!second.stderr.contains(&lent(process::id())), "{second:?}");
    assert!(at(&case, "take", critical_pid) < at(&case, "take", second_pid));
    assert!(!events_of(&case, second_pid).contains(&"parent".to_owned()));
}

/// Writes a lend marker in the case's root naming this process as parent and child, started at `start_ms`, whose
/// content is no claim.
fn write_lend_marker(case: &Case, start_ms: u64) -> PathBuf {
    let dir = v1::slots_dir(&case.root());
    fs::create_dir_all(&dir).expect("the claims folder can be made");
    let path = dir.join(format!("gate-lent-{0}-{0}-{start_ms}.json", process::id()));
    fs::write(&path, "not a claim").expect("the lend marker can be written");
    path
}

#[test]
fn an_unreadable_lend_marker_named_for_a_live_gate_keeps_the_critical_gate_from_holding_the_machine() {
    let case = Case::new("an_unreadable_lend_marker_named_for_a_live_gate_keeps_the_critical_gate_from_holding_the_machine");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let marker = write_lend_marker(&case, own_start().as_millisecond().unsigned_abs());

    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(&format!("{WAITING}: {ONE_LENT}\n"));
    wait_for_heartbeat_after(&critical, ONE_LENT);
    assert_eq!(events_of(&case, critical_pid), ["queue"], "the marker's live gate is waited for");

    fs::remove_file(&marker).expect("the marker can be deleted");
    assert_passed(&critical.finish());
    assert!(events_of(&case, critical_pid).contains(&"take".to_owned()));
}

#[test]
fn a_lend_marker_named_for_a_dead_gate_is_deleted_and_does_not_hold_the_critical_gate_back() {
    let case = Case::new("a_lend_marker_named_for_a_dead_gate_is_deleted_and_does_not_hold_the_critical_gate_back");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let marker = write_lend_marker(&case, 1);

    let outcome = finish(case.gate(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"])));

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(!marker.exists(), "the dead gate's marker is deleted");
}

#[test]
fn a_critical_gate_whose_ticket_cannot_be_rewritten_as_holding_does_not_hold_the_machine() {
    let case = Case::new("a_critical_gate_whose_ticket_cannot_be_rewritten_as_holding_does_not_hold_the_machine");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    let ticket = ticket_file(&case, critical_pid);
    let set_read_only = |read_only: bool| {
        let mut permissions = fs::metadata(&ticket).expect("the ticket's metadata can be read").permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(&ticket, permissions).expect("the ticket's read-only flag can be set");
    };
    set_read_only(true);

    let_go(&go);
    assert_passed(&held.finish());
    let cannot = "gate: cannot write the ticket ";
    critical.wait_for_stderr(cannot);
    wait_for_heartbeat_after(&critical, "0 heavy and 0 light slots held");
    assert_eq!(events_of(&case, critical_pid), ["queue"], "a ticket not rewritten as holding is no hold");

    set_read_only(false);
    let outcome = critical.finish();
    assert_passed(&outcome);
    assert_eq!(outcome.stderr.matches(cannot).count(), 1, "said once: {outcome:?}");
    assert!(events_of(&case, critical_pid).contains(&"take".to_owned()));
}

#[test]
fn a_heavy_child_that_cannot_read_the_critical_ticket_is_not_lent_its_parents_slot() {
    let case = Case::new("a_heavy_child_that_cannot_read_the_critical_ticket_is_not_lent_its_parents_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    fs::write(ticket_file(&case, critical_pid), "not a ticket").expect("the ticket can be overwritten");
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));

    let marker = test_marker(&case, "light");
    let child = start(marked(&case, &heavy_words("child.log", &POLL, &[FAKE_TOOL, "exit", "0"]), &marker));
    let child_pid = child.id();
    child.wait_for_stderr("gate: waiting for a heavy slot");
    assert_eq!(events_of(&case, child_pid), ["queue"], "the child queues as an ordinary heavy gate");

    let_go(&go);
    assert_passed(&critical.finish());
    let outcome = child.finish();
    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(&lent(process::id())), "{outcome:?}");
    assert!(at(&case, "release", critical_pid) < at(&case, "take", child_pid));
}

#[test]
fn a_critical_gate_does_not_take_the_machine_while_a_child_lent_as_it_waited_runs() {
    let case = Case::new("a_critical_gate_does_not_take_the_machine_while_a_child_lent_as_it_waited_runs");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let child_go = case.folder().join("child-go");
    let child_go_word = shown(&child_go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));
    let marker = test_marker(&case, "light");
    let child = start(marked(
        &case,
        &heavy_words("child.log", &POLL, &[FAKE_TOOL, "wait", &child_go_word, "exit", "0"]),
        &marker,
    ));
    let child_pid = child.id();
    child.wait_for_stderr("gate: waiting for a heavy slot");

    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    child.wait_for_stderr(&lent(process::id()));
    assert_eq!(events_of(&case, child_pid), ["queue", "parent"], "lent as it waited");

    let_go(&go);
    assert_passed(&held.finish());
    wait_for_heartbeat_after(&critical, ONE_LENT);
    assert_eq!(
        events_of(&case, critical_pid),
        ["queue"],
        "the critical gate waits for the child lent as it waited"
    );

    let_go(&child_go);
    assert_passed(&child.finish());
    assert_passed(&critical.finish());
    assert!(at(&case, "parent", child_pid) < at(&case, "take", critical_pid));
}

#[test]
fn the_drain_ignores_remote_tickets_and_claims() {
    let case = Case::new("the_drain_ignores_remote_tickets_and_claims");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let start = own_start();
    write_ticket(&case, TicketKind::Heavy, (process::id(), start), 0, true);
    let claim = Claim {
        kind: ClaimKind::Remote,
        index: 0,
        pid: process::id(),
        process_start: start,
        started: start,
        cwd: shown(&case.folder()),
        command: "cargo build".to_owned(),
        log: shown(&case.folder().join("remote.log")),
        timeout_seconds: 60.0,
        remote: Some(true),
        host: Some("builder@build-box".to_owned()),
        key: Some("repo-1".to_owned()),
        expected: None,
    };
    let path = v1::claim_path(&case.root(), KernelObject::RemoteSlot(0)).expect("a remote slot has a claim");
    v1::write_claim(&path, &claim).expect("the remote claim can be written");

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    let outcome = critical.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(outcome.stderr.contains("; other live claims: none\n"), "{outcome:?}");
    assert_eq!(take_of(&case, critical_pid)["otherClaims"], json!([]));
}

#[test]
fn a_critical_take_journals_the_snapshot_and_says_the_holds_line() {
    let case = Case::new("a_critical_take_journals_the_snapshot_and_says_the_holds_line");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let light_pid = light.id();
    let light_command = take_of(&case, light_pid)["command"].as_str().expect("a line has its command").to_owned();

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    let outcome = critical.finish();

    assert_passed(&outcome);
    let take = take_of(&case, critical_pid);
    let keys: Vec<&str> = take.as_object().expect("a line is an object").keys().map(String::as_str).collect();
    assert_eq!(keys[8..], ["slots", "waitedSeconds", "counts", "otherHeld", "otherClaims"]);
    assert_eq!(take["counts"], json!({"heavy": 1, "light": 1}));
    assert_eq!(take["otherHeld"], json!({"heavy": 0, "light": 1}));
    let claim = json!({"pid": light_pid, "kind": "light", "index": 0, "command": light_command, "file": "gate-light-slot-0.json"});
    assert_eq!(take["otherClaims"], json!([claim]));
    let said = format!(
        ": counts heavy 1, light 1; other gates hold 0 heavy and 1 light slots (1 light slot beside it); \
         other live claims: {light_pid} light 0 {light_command}\n"
    );
    assert_in_order(&outcome.stderr, &[HOLDS, " s", &said]);
    let log = fs::read_to_string(case.folder().join("critical.log")).expect("the critical gate wrote its log");
    assert_in_order(&log, &[HOLDS, &said]);

    let_go(&go);
    assert_passed(&light.finish());
    let alone = case.start(&critical_words("alone.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let alone_pid = alone.id();
    let outcome = alone.finish();
    assert_passed(&outcome);
    assert!(
        outcome.stderr.contains(
            ": counts heavy 1, light 1; other gates hold 0 heavy and 0 light slots (1 light slot beside it); \
             other live claims: none\n"
        ),
        "{outcome:?}"
    );
    let take = take_of(&case, alone_pid);
    assert_eq!((&take["otherHeld"], &take["otherClaims"]), (&json!({"heavy": 0, "light": 0}), &json!([])));
}

#[test]
fn a_dead_critical_ticket_does_not_block_a_later_critical_gate() {
    let case = Case::new("a_dead_critical_ticket_does_not_block_a_later_critical_gate");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let ended = case.start(&run_words("ended.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let ended_pid = ended.id();
    assert_passed(&ended.finish());
    let ended_start: Timestamp = take_of(&case, ended_pid)["processStart"]
        .as_str()
        .expect("a line has its gate's start")
        .parse()
        .expect("the start is a time");
    write_ticket(&case, TicketKind::Critical, (ended_pid, ended_start), 0, false);

    let outcome = case.run(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(!has_ticket(&case, ended_pid), "the dead gate's ticket is deleted");
}

/// Writes a claim of `kind` on the slot `object` naming the gate `pid` started at `start`, in the case's root.
fn write_claim(case: &Case, object: KernelObject, kind: ClaimKind, (pid, start): (u32, Timestamp)) {
    let index = match object {
        KernelObject::HeavySlot(index) | KernelObject::LightSlot(index) | KernelObject::RemoteSlot(index) => index,
        _ => 0,
    };
    let claim = Claim {
        kind,
        index,
        pid,
        process_start: start,
        started: start,
        cwd: shown(&case.folder()),
        command: "cargo build".to_owned(),
        log: shown(&case.folder().join("claimed.log")),
        timeout_seconds: 60.0,
        remote: None,
        host: None,
        key: None,
        expected: None,
    };
    let path = v1::claim_path(&case.root(), object).expect("a slot has a claim");
    v1::write_claim(&path, &claim).expect("the claim can be written");
}

#[test]
fn a_critical_gate_does_not_wait_for_an_earlier_ticket_whose_gate_holds_a_slot() {
    let case = Case::new("a_critical_gate_does_not_wait_for_an_earlier_ticket_whose_gate_holds_a_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let own = (process::id(), own_start());
    write_ticket(&case, TicketKind::Heavy, own, 0, false);
    write_claim(&case, KernelObject::HeavySlot(5), ClaimKind::Heavy, own);

    let outcome = case.run(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(has_ticket(&case, process::id()), "the holder's ticket stands");
}

#[test]
fn a_critical_gate_clears_the_claim_a_killed_gate_left_on_a_slot() {
    let case = Case::new("a_critical_gate_clears_the_claim_a_killed_gate_left_on_a_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let ended = case.start(&run_words("ended.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let ended_pid = ended.id();
    assert_passed(&ended.finish());
    let ended_start: Timestamp = take_of(&case, ended_pid)["processStart"]
        .as_str()
        .expect("a line has its gate's start")
        .parse()
        .expect("the start is a time");
    write_claim(&case, KernelObject::HeavySlot(0), ClaimKind::Heavy, (ended_pid, ended_start));
    let killed = "which a killed gate had left held";

    let outcome = case.run(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    assert_passed(&outcome);
    let slot = name(&case, KernelObject::HeavySlot(0));
    assert!(outcome.stderr.contains(&format!("gate: took {slot}, {killed}")), "{outcome:?}");
    let claim = v1::claim_path(&case.root(), KernelObject::HeavySlot(0)).expect("a slot has a claim");
    assert!(!claim.exists(), "the killed gate's claim is gone");

    let next = case.run(&heavy_words("next.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    assert_passed(&next);
    assert!(!next.stderr.contains(killed), "the takeover is counted once: {next:?}");
}

#[test]
fn an_interrupted_critical_wait_leaves_nothing_behind() {
    let case = Case::new("an_interrupted_critical_wait_leaves_nothing_behind");
    write_counts(&case.root(), r#"{"v":1,"heavy":2,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));

    let mut seams = POLL.to_vec();
    seams.push("--test-interrupt-queued");
    let critical = case.start(&critical_words("critical.log", &seams, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    let outcome = critical.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[WAITING, "\ngate: interrupted while waiting for the machine\n"]);
    assert_eq!(events_of(&case, critical_pid), ["queue", "abandon"]);
    assert!(!has_ticket(&case, critical_pid), "the interrupted gate's ticket is gone");
    for object in [KernelObject::Critical, KernelObject::HeavySlot(1)] {
        let object_name = name(&case, object);
        assert!(
            !gate_win32::mutex_exists(&object_name).expect("the mutex can be probed"),
            "{object_name} is let go"
        );
    }
    let claim = v1::claim_path(&case.root(), KernelObject::Critical).expect("the critical mutex has a claim");
    assert!(!claim.exists(), "no critical claim is left");
    assert!(held.is_running(), "the holder runs on");
    let_go(&go);
    assert_passed(&held.finish());
}

#[test]
fn a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine() {
    let case = Case::new("a_nested_gate_under_a_critical_parent_runs_without_waiting_for_the_machine");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let mut inner = vec![GATE];
    inner.extend(heavy_words("inner.log", &POLL, &[FAKE_TOOL, "exit", "0"]));

    let outer = case.start(&critical_words("outer.log", &POLL, &inner));
    let outer_pid = outer.id();
    assert_passed(&outer.finish());

    let sequence = sequence(&case);
    let inner_pid = sequence
        .iter()
        .find(|(event, _)| event == "covered")
        .map(|(_, pid)| *pid)
        .expect("the inner gate journalled its covered line");
    let expected: Vec<(String, u32)> = [("queue", outer_pid), ("take", outer_pid), ("covered", inner_pid), ("release", outer_pid)]
        .into_iter()
        .map(|(event, pid)| (event.to_owned(), pid))
        .collect();
    assert_eq!(sequence, expected, "the inner gate runs inside the critical gate's hold");
    let covered = events(&case)
        .into_iter()
        .find(|event| event["event"] == "covered")
        .expect("checked above");
    assert_eq!(covered["parentPid"], Value::from(outer_pid));
    let inner_log = fs::read_to_string(case.folder().join("inner.log")).expect("the inner gate wrote its log");
    assert!(
        inner_log.contains(&format!("gate: running under the critical slot of gate {outer_pid}")),
        "{inner_log}"
    );
    assert!(!inner_log.contains("waiting for"), "{inner_log}");
}

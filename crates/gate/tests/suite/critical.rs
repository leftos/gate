//! The critical gate: it takes the critical mutex in arrival order, waits for the heavy and light gates that hold a slot
//! or arrived before it, takes every heavy and unreserved light slot, says and journals what it found, and releases it
//! all; a Ctrl+C while it waits leaves nothing behind.

use std::fs;
use std::path::Path;
use std::process;

use gate_state::v1::{self, Claim, ClaimKind, Ticket, TicketKind};
use gate_state::{KernelObject, StateRoot};
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::support::{
    Case, FAKE_TOOL, GATE, Running, assert_in_order, critical_words, events, heavy_words, run_words, shown, wait_for_journal_line, write_counts,
};

/// The fallback poll of every gate a case starts, so a wake missed costs a case 20 ms, not 2 s.
const POLL: [&str; 2] = ["--test-poll-ms", "20"];
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

/// The journal's events of the gate `pid`, in order.
fn events_of(case: &Case, pid: u32) -> Vec<String> {
    sequence(case).into_iter().filter(|(_, of)| *of == pid).map(|(name, _)| name).collect()
}

/// The `take` line of the gate `pid`.
fn take_of(case: &Case, pid: u32) -> Value {
    events(case)
        .into_iter()
        .find(|event| event["event"] == "take" && event["pid"] == pid)
        .expect("the gate journalled its take")
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
        "{WAITING}: 1 heavy and 0 light slots held (2 light slots reserved for light gates beside it), 0 earlier waiters\n"
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
        "{WAITING}: 1 heavy and 0 light slots held (1 light slot reserved for light gates beside it), 1 earlier waiters\n"
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
    let later = Timestamp::now().as_millisecond().unsigned_abs() + 3_600_000;
    write_ticket(&case, TicketKind::Heavy, (process::id(), own_start()), later, false);

    let outcome = case.run(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains(WAITING), "{outcome:?}");
    assert!(outcome.stderr.contains(HOLDS), "{outcome:?}");
    assert!(has_ticket(&case, process::id()), "the later gate's ticket stands");
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
fn a_critical_gate_waits_for_an_earlier_light_gate_on_an_unreserved_slot() {
    let case = Case::new("a_critical_gate_waits_for_an_earlier_light_gate_on_an_unreserved_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let light_pid = light.id();

    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(&format!(
        "{WAITING}: 0 heavy and 1 light slots held (2 light slots reserved for light gates beside it), 0 earlier waiters\n"
    ));

    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&light.finish());
    assert!(at(&case, "release", light_pid) < at(&case, "take", critical_pid));
    let names = json!([name(&case, KernelObject::HeavySlot(0)), name(&case, KernelObject::LightSlot(0))]);
    assert_eq!(sorted(&take_of(&case, critical_pid)["slots"]), sorted(&names));
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
        ": counts heavy 1, light 1; other gates hold 0 heavy and 1 light slots (1 light slot reserved for light gates beside it); \
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
            ": counts heavy 1, light 1; other gates hold 0 heavy and 0 light slots (1 light slot reserved for light gates beside it); \
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

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
use regex::Regex;
use serde_json::{Value, json};

use crate::durations::seed;
use crate::nesting::{marked, test_marker};
use crate::routing::{
    BUSY_LINE, GIT_HANG_GUARDS, HOST, balance_case, has_no_remote_fields, has_remote_tag, names, own, repo, routed_case, routed_words, stub,
};
use crate::support::{
    Case, FAKE_TOOL, GATE, LOCAL_REASON, POLL, Running, UNTIL_KILLED, USAGE, assert_in_order, critical_words, events, events_of, finish, git,
    heavy_words, log_texts, none_words, run_words, shown, start, take_of, wait_for_file, wait_for_journal_line, wait_until, write_counts,
    write_remote_json,
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

/// Whether the gate logging to `log` has journalled its own queue line, the one naming that log: a gate its parent lent
/// its slot before it queued writes none.
fn has_queued(case: &Case, log: &str) -> bool {
    events(case)
        .iter()
        .any(|event| event["event"] == "queue" && event["log"].as_str().is_some_and(|line| line.ends_with(log)))
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
        alone: false,
        remote: remote.then_some(true),
        pool: None,
        pinned: None,
        reason: None,
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

/// A critical run records its reason in its arrival ticket, its claim and its queue line, and a ticket rewrite keeps it.
#[test]
fn a_critical_run_records_its_reason_in_its_ticket_claim_and_journal() {
    let case = Case::new("a_critical_run_records_its_reason_in_its_ticket_claim_and_journal");
    let reason = "the suite's critical run";
    let (held_go, critical_go) = (case.folder().join("held-go"), case.folder().join("critical-go"));
    let (held_word, critical_word) = (shown(&held_go), shown(&critical_go));
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &held_word, "exit", "0"]));

    let critical = case.start(&critical_words(
        "critical.log",
        &with_heartbeat(),
        &[FAKE_TOOL, "wait", &critical_word, "exit", "0"],
    ));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    let ticket = ticket_json(&case, critical_pid);
    assert_eq!(ticket["reason"], reason, "{ticket}");

    let_go(&held_go);
    critical.wait_for_stderr(HOLDS);
    let claim_path = case.root().join("slots").join("gate-critical.json");
    wait_for_file(&claim_path);
    let claim = json_file(&claim_path);
    assert_eq!(claim["reason"], reason, "{claim}");

    let queue = events(&case)
        .into_iter()
        .find(|event| event["event"] == "queue" && event["pid"] == critical_pid)
        .expect("the critical gate journalled its queue line");
    assert_eq!(queue["reason"], reason, "{queue}");

    let rewritten = ticket_json(&case, critical_pid);
    assert_eq!(rewritten["holds"], true, "the ticket says it holds: {rewritten}");
    assert_eq!(rewritten["reason"], reason, "a rewrite keeps the reason: {rewritten}");

    let_go(&critical_go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
}

/// The ticket of the gate `pid` as JSON.
fn ticket_json(case: &Case, pid: u32) -> Value {
    serde_json::from_str(&fs::read_to_string(ticket_file(case, pid)).expect("the ticket can be read")).expect("the ticket is JSON")
}

/// The JSON at `path`.
fn json_file(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the file can be read")).expect("the file is JSON")
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

    // The third gate waited with a raised slot free (above); which journal line comes first once the light gates end
    // is not pinned: a gate deletes its claim before it journals its release.
    let_go(&light_go);
    assert_passed(&third.finish());
    assert_passed(&first.finish());
    assert_passed(&second.finish());
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
    let child_log = log_texts(&case.folder().join("child.log"));
    assert!(child_log.contains(&lent(parent_pid)), "{child_log}");
    assert!(
        child_log.contains(&format!("heavy:{child_pid}:")),
        "the lent child hands its command its own heavy marker: {child_log}"
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
    let mut child = vec![GATE];
    child.extend(heavy_words("child.log", &POLL, &command));
    assert_eq!(child[4], "--timeout", "the ceiling's value follows it");
    child[5] = "auto:900";
    let parent = holder(&case, &run_words("parent.log", &POLL, &child));
    let parent_pid = parent.id();
    wait_until("the child gate queueing", || has_queued(&case, "child.log"));

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
    let raised = "gate: ceiling 42 min (3x this command's p99 of 14 min, above the given 15 min)";
    let child_log = log_texts(&case.folder().join("child.log"));
    assert_eq!(
        child_log.matches(raised).count(),
        1,
        "the lent child says its raised ceiling once: {child_log}"
    );
    // The parent's log holds the child's stderr line as the child said it, and the child's tail on its stdout says it
    // again; the parent's tail shows both, before the child's passed line.
    let (child_stream, _) = parent_outcome
        .stdout
        .split_once("child.log")
        .expect("the child's passed line names its log");
    assert_eq!(child_stream.matches(raised).count(), 2, "{parent_outcome:?}");
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

/// A run pinned with `--builder` asks its light parent for no lend: while a critical request drains, it routes to its
/// builder as it does at any other time.
#[test]
fn a_pinned_run_under_a_light_parent_routes_while_a_critical_request_drains() {
    let case = routed_case("a_pinned_run_under_a_light_parent_routes_while_a_critical_request_drains");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let local = ["--local", "--reason", "holds the heavy slot here"];
    let held = holder(&case, &heavy_words("holder.log", &local, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    critical.wait_for_stderr(WAITING);
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));

    let call = case.folder().join("call.json");
    let steps = shown(&stub(&case, &[&format!("record {}", shown(&call)), "status 0"]));
    let words = routed_words("child.log", &steps, &["--builder", HOST], &[r".\tool.exe", "out", "ran there"]);
    let outcome = finish(marked(&case, &words, &test_marker(&case, "light")));

    assert_passed(&outcome);
    assert!(
        !outcome.stderr.contains(&lent(process::id())),
        "the pinned run is lent nothing: {outcome:?}"
    );
    assert!(call.exists(), "the builder ran the command: {outcome:?}");
    let_go(&go);
    assert_passed(&critical.finish());
    assert_passed(&held.finish());
}

/// The prefix of the still-waiting line of a light gate held back by the critical request of gate `pid`.
fn still_beside(pid: u32) -> String {
    format!("gate: still waiting for a light slot beside the critical request of gate {pid} (")
}

/// The line of a light gate the critical request of gate `pid` held back as it sends its run to the builder.
fn sent_to_builder(pid: u32) -> String {
    format!("gate: held back by the critical request of gate {pid}; sending this light run to {HOST}")
}

/// A routed case of `name` whose builder has `slots` remote slots, with three light slots here.
fn reroute_case(name: &str, slots: u32) -> Case {
    let case = routed_case(name);
    if slots != 1 {
        let routing = json!({ "host": HOST, "slots": slots, "repos": { repo(&case): ["*tool*"] } });
        write_remote_json(&case, &routing.to_string());
    }
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    case
}

/// A linked worktree of the case's repo beside its folder, as `git worktree add` makes one, and its path.
fn other_worktree(case: &Case) -> PathBuf {
    git(case, &["commit", "--quiet", "--allow-empty", "-m", "a"]);
    let mut name = case.folder().file_name().expect("the case folder has a name").to_owned();
    name.push(".other");
    let other = case.folder().with_file_name(name);
    if other.exists() {
        fs::remove_dir_all(&other).expect("a folder left by an earlier run can be removed");
    }
    fs::create_dir_all(&other).expect("the folder beside the case can be made");
    git(case, &["worktree", "add", "--quiet", "--detach", &shown(&other)]);
    other
}

/// The gates that hold a light gate back: a critical gate holding until `go`, and light gates beside it holding until
/// `light_go`.
struct HeldBack {
    critical: Running,
    go: PathBuf,
    lights: Vec<Running>,
    light_go: PathBuf,
}

impl HeldBack {
    /// Starts a critical gate, asked for `--alone` when `alone`, then `lights` light gates beside it, each once the one
    /// before it has taken its slot.
    fn start(case: &Case, alone: bool, lights: usize) -> HeldBack {
        let go = case.folder().join("go");
        let go_word = shown(&go);
        let command = [FAKE_TOOL, "wait", go_word.as_str(), "exit", "0"];
        let words = if alone {
            alone_words("critical.log", &POLL, &command)
        } else {
            critical_words("critical.log", &POLL, &command)
        };
        let critical = holder(case, &words);
        let (lights, light_go) = light_holders(case, lights);
        HeldBack {
            critical,
            go,
            lights,
            light_go,
        }
    }

    fn critical_pid(&self) -> u32 {
        self.critical.id()
    }

    fn let_lights_go(&self) {
        let_go(&self.light_go);
    }

    /// Lets every holder go and asserts each passed.
    fn finish(self) {
        let_go(&self.light_go);
        for light in self.lights {
            assert_passed(&light.finish());
        }
        let_go(&self.go);
        assert_passed(&self.critical.finish());
    }
}

/// Starts `count` light gates, each once the one before it has taken its slot, holding until the file `light-go` of the
/// case folder exists; them and that file's path.
fn light_holders(case: &Case, count: usize) -> (Vec<Running>, PathBuf) {
    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let lights = (0..count)
        .map(|index| {
            let log = format!("light-{index}.log");
            holder(case, &run_words(&log, &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]))
        })
        .collect();
    (lights, light_go)
}

/// Starts a light run logging to `log` that calls the scripted client of the step file `steps`, with the routing git
/// guards and `extra` before `--` and `command` after it.
fn light_with_client(case: &Case, log: &str, steps: &Path, extra: &[&str], command: &[&str]) -> Running {
    let steps = shown(steps);
    let mut before = vec!["--test-remote-client", steps.as_str()];
    before.extend(GIT_HANG_GUARDS);
    before.extend(extra);
    case.start(&run_words(log, &before, command))
}

/// The command of a light run that would route on heavy.
const ROUTED_COMMAND: [&str; 3] = [r".\tool.exe", "out", "hi"];

/// The scripted client's steps that record the call at `call` and pass.
fn recording_stub(case: &Case, call: &Path) -> PathBuf {
    stub(case, &[&format!("record {}", shown(call)), "status 0"])
}

/// Asserts the journal lines of a light run sent to the builder by the critical request of gate `critical_pid`: a
/// `queue` with no remote field, then a `take` and a `release` with the remote tag naming that request.
fn assert_rerouted(events: &[Value], critical_pid: u32) {
    assert_eq!(names(events), ["queue", "take", "release"], "{events:#?}");
    assert!(has_no_remote_fields(&events[0]), "{events:#?}");
    for event in &events[1..] {
        assert!(has_remote_tag(event), "{events:#?}");
        assert_eq!(event["routedFor"], "critical", "{event}");
        assert_eq!(event["criticalPid"], critical_pid, "{event}");
    }
}

/// Asserts the journal lines of a light run that ran here all along: `queue`, `take` and `release` with no remote field.
fn assert_stayed_here(events: &[Value]) {
    assert_eq!(names(events), ["queue", "take", "release"], "{events:#?}");
    assert!(events.iter().all(has_no_remote_fields), "{events:#?}");
}

#[test]
fn a_light_gate_the_critical_barrier_holds_back_routes_to_a_free_builder() {
    let case = reroute_case("a_light_gate_the_critical_barrier_holds_back_routes_to_a_free_builder", 2);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, &POLL, &ROUTED_COMMAND);
    let pid = light.id();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert!(outcome.stderr.contains(&sent_to_builder(critical_pid)), "{outcome:?}");
    assert!(call.exists(), "the builder ran the command: {outcome:?}");
    assert_rerouted(&own(&case, pid), critical_pid);
    held.finish();
}

#[test]
fn a_light_gate_sent_to_a_builder_says_why_before_its_route_lines() {
    let case = reroute_case("a_light_gate_sent_to_a_builder_says_why_before_its_route_lines", 2);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    // The outside path's rewrite says a route line, `gate: routed <path> as fake-tool`.
    let outcome = light_with_client(&case, "light.log", &steps, &POLL, &[FAKE_TOOL, "out", "hi"]).finish();

    assert_passed(&outcome);
    assert_in_order(&outcome.stderr, &[&sent_to_builder(critical_pid), "gate: routed "]);
    held.finish();
}

/// A light gate the critical request holds back whose log lies in another worktree of its repository is not sent to the
/// builder: it keeps waiting here, says the refusal line in its log, and runs here once a light slot frees.
#[test]
fn a_light_gate_held_back_logging_into_another_worktree_stays_here_saying_so() {
    let case = reroute_case("a_light_gate_held_back_logging_into_another_worktree_stays_here_saying_so", 2);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);
    let other = other_worktree(&case);
    let log = shown(&other.join("light.log"));
    let line = format!("run it with --cd {}", shown(&other).replace('/', "\\"));

    let light = light_with_client(&case, &log, &steps, &with_heartbeat(), &ROUTED_COMMAND);
    let pid = light.id();
    // A still-waiting line beside the critical request is only said once the run's reroute came back here.
    light.wait_for_stderr(&still_beside(critical_pid));
    held.let_lights_go();
    let outcome = light.finish();
    held.finish();

    assert_passed(&outcome);
    assert!(!call.exists(), "nothing went to the builder: {outcome:?}");
    assert!(
        !outcome.stderr.contains("sending this light run to"),
        "a run whose log is elsewhere keeps waiting here: {outcome:?}"
    );
    assert_stayed_here(&own(&case, pid));
    assert!(
        log_texts(&other.join("light.log")).contains(&line),
        "the refused run's log holds its line"
    );
}

#[test]
fn a_light_gate_held_back_after_a_wait_counts_its_remote_take_from_its_queue_line() {
    let case = reroute_case("a_light_gate_held_back_after_a_wait_counts_its_remote_take_from_its_queue_line", 2);
    let (lights, light_go) = light_holders(&case, 3);
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);
    let light = light_with_client(&case, "light.log", &steps, &with_heartbeat(), &ROUTED_COMMAND);
    let pid = light.id();
    // Twenty still-waiting lines, one each 50 ms, make the wait before the barrier longer than the assertion's slack.
    light.wait_for_stderr_where("twenty still-waiting lines", |text| {
        text.matches("gate: still waiting for a light slot (").count() >= 20
    });

    // A critical request ahead of it, naming this live process, leaves it no place beside it.
    let planted = (process::id(), own_start());
    write_ticket(&case, TicketKind::Critical, planted, 0, false);
    let outcome = light.finish();

    assert_passed(&outcome);
    let events = own(&case, pid);
    assert_rerouted(&events, planted.0);
    let time = |event: &Value| -> Timestamp {
        event["time"]
            .as_str()
            .expect("a line has a time")
            .parse()
            .expect("the time is a timestamp")
    };
    let gap = time(&events[1]).duration_since(time(&events[0])).as_secs_f64();
    let waited = events[1]["waitedSeconds"].as_f64().expect("the take says its wait");
    assert!(
        waited >= gap - 0.5,
        "the remote take counts from the queue line, {gap} s before it: {events:#?}"
    );
    let ticket = v1::ticket_path(&case.root(), planted.0, planted.1.as_millisecond().unsigned_abs());
    fs::remove_file(&ticket).expect("the planted ticket can be deleted");
    let_go(&light_go);
    for light in lights {
        assert_passed(&light.finish());
    }
}

#[test]
fn a_light_gate_with_a_place_beside_the_critical_request_stays_here() {
    let case = reroute_case("a_light_gate_with_a_place_beside_the_critical_request_stays_here", 2);
    let held = HeldBack::start(&case, false, 1);
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, &POLL, &ROUTED_COMMAND);
    let pid = light.id();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("sending this light run"), "{outcome:?}");
    assert!(!call.exists(), "the builder ran nothing: {outcome:?}");
    assert_stayed_here(&own(&case, pid));
    held.finish();
}

#[test]
fn a_light_gate_behind_a_request_to_hold_the_machine_alone_routes() {
    let case = reroute_case("a_light_gate_behind_a_request_to_hold_the_machine_alone_routes", 2);
    let held = HeldBack::start(&case, true, 0);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, &POLL, &ROUTED_COMMAND);
    let pid = light.id();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert!(outcome.stderr.contains(&sent_to_builder(critical_pid)), "{outcome:?}");
    assert!(call.exists(), "the builder ran the command: {outcome:?}");
    assert_rerouted(&own(&case, pid), critical_pid);
    held.finish();
}

/// A light run a critical request sends to a builder is not scored: its first try goes to the first builder listed,
/// though that builder's one remote slot is held, and that pool's full refusal hands the run to the next.
#[test]
fn a_light_run_rerouted_by_a_critical_request_keeps_file_order() {
    const FIRST: &str = "builder-a";
    let case = balance_case("a_light_run_rerouted_by_a_critical_request_keeps_file_order");
    let routing = json!({
        "host": HOST,
        "slots": 2,
        "repos": { repo(&case): ["*tool*"] },
        "builders": [{ "host": FIRST, "os": "windows", "repos": { repo(&case): ["*tool*"] } }],
    });
    write_remote_json(&case, &routing.to_string());
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let remote_go = case.folder().join("remote-go");
    let holder_steps = shown(&stub(&case, &[&format!("wait {}", shown(&remote_go))]));
    let remote = case.start(&routed_words("remote.log", &holder_steps, &["--builder", FIRST], &ROUTED_COMMAND));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, remote.id()));
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, &POLL, &ROUTED_COMMAND);
    let pid = light.id();
    let outcome = light.finish();
    held.finish();
    let_go(&remote_go);
    assert_passed(&remote.finish());

    assert_passed(&outcome);
    let sent = format!("gate: held back by the critical request of gate {critical_pid}; sending this light run to {FIRST}");
    assert!(outcome.stderr.contains(&sent), "{outcome:?}");
    let events = own(&case, pid);
    let tried: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| Some((event["event"].as_str()?, event["host"].as_str()?)))
        .collect();
    assert_eq!(tried.first(), Some(&("fallback", FIRST)), "the first builder is tried first: {events:#?}");
    assert!(tried.contains(&("take", HOST)), "the next builder takes the run: {events:#?}");
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&call).expect("the builder was called")).expect("the call is JSON");
    assert_eq!(recorded["host"], HOST, "{recorded}");
}

#[test]
fn a_light_gate_finds_no_spare_remote_slot_and_waits_here_in_its_place() {
    let case = reroute_case("a_light_gate_finds_no_spare_remote_slot_and_waits_here_in_its_place", 1);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, &with_heartbeat(), &ROUTED_COMMAND);
    let pid = light.id();
    wait_for_journal_line(&case.root(), &format!(r#""event":"fallback","pid":{pid},"#));
    light.wait_for_stderr(&still_beside(critical_pid));

    let waiting = own(&case, pid);
    assert_eq!(names(&waiting), ["queue", "fallback"], "{waiting:#?}");
    assert!(has_no_remote_fields(&waiting[0]), "{waiting:#?}");
    assert_eq!(waiting[1]["reason"], "full", "{waiting:#?}");
    assert_eq!(waiting[1]["criticalPid"], critical_pid, "{waiting:#?}");
    assert!(!call.exists(), "the builder ran nothing");
    held.let_lights_go();
    let outcome = light.finish();

    assert_passed(&outcome);
    let events = own(&case, pid);
    assert_eq!(names(&events), ["queue", "fallback", "take", "release"], "{events:#?}");
    assert_eq!(events[3]["fellBack"], json!(["full"]), "{events:#?}");
    assert!(events[2..].iter().all(has_no_remote_fields), "{events:#?}");
    assert!(!call.exists(), "the builder ran nothing: {outcome:?}");
    held.finish();
}

#[test]
fn a_light_gate_the_builder_refuses_waits_here_and_never_routes_again() {
    let case = reroute_case("a_light_gate_the_builder_refuses_waits_here_and_never_routes_again", 2);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = stub(&case, &[&format!("record {}", shown(&call)), "refuse busy"]);

    let light = light_with_client(&case, "light.log", &steps, &with_heartbeat(), &ROUTED_COMMAND);
    let pid = light.id();
    let still = still_beside(critical_pid);
    light.wait_for_stderr_where("two still-waiting lines after the fallback line", |text| {
        text.split_once(BUSY_LINE)
            .is_some_and(|(_, after)| after.matches(still.as_str()).count() >= 2)
    });
    held.let_lights_go();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert_eq!(outcome.stderr.matches(&sent_to_builder(critical_pid)).count(), 1, "{outcome:?}");
    let events = own(&case, pid);
    assert_eq!(names(&events), ["queue", "take", "fallback", "take", "release"], "{events:#?}");
    assert_eq!(events[4]["fellBack"], json!(["busy"]), "{events:#?}");
    assert!(has_no_remote_fields(&events[0]), "{events:#?}");
    for event in &events[1..3] {
        assert!(has_remote_tag(event), "{events:#?}");
        assert_eq!(event["criticalPid"], critical_pid, "{event}");
    }
    assert_eq!(events[2]["reason"], "busy", "{events:#?}");
    assert!(events[3..].iter().all(has_no_remote_fields), "{events:#?}");
    let log = fs::read_to_string(case.folder().join("light.log")).expect("the gate's log was written");
    assert_in_order(&log, &[&sent_to_builder(critical_pid), BUSY_LINE]);
    held.finish();
}

/// Starts the light run with `extra` and `command`, which never routes, behind a critical request with no place
/// beside it, and asserts it waits there as any light gate does, then runs here once the light gates beside it go.
fn assert_waits_behind_the_request(name: &str, extra: &[&str], command: &[&str]) {
    let case = reroute_case(name, 2);
    let held = HeldBack::start(&case, false, 2);
    let critical_pid = held.critical_pid();
    let call = case.folder().join("call.json");
    let steps = recording_stub(&case, &call);

    let light = light_with_client(&case, "light.log", &steps, extra, command);
    let pid = light.id();
    light.wait_for_stderr(&beside(critical_pid));
    assert_eq!(events_of(&case, pid), ["queue"], "two light gates run beside the critical gate already");
    held.let_lights_go();
    let outcome = light.finish();

    assert_passed(&outcome);
    assert!(!outcome.stderr.contains("sending this light run"), "{outcome:?}");
    assert!(!call.exists(), "the builder ran nothing: {outcome:?}");
    assert_stayed_here(&own(&case, pid));
    held.finish();
}

#[test]
fn a_light_gate_whose_command_would_not_route_waits_behind_the_critical_request() {
    assert_waits_behind_the_request(
        "a_light_gate_whose_command_would_not_route_waits_behind_the_critical_request",
        &POLL,
        &["cmd", "/c", "exit", "0"],
    );
}

#[test]
fn a_light_gate_run_with_local_waits_behind_the_critical_request() {
    let mut extra = POLL.to_vec();
    extra.push("--local");
    extra.extend(LOCAL_REASON);
    assert_waits_behind_the_request("a_light_gate_run_with_local_waits_behind_the_critical_request", &extra, &ROUTED_COMMAND);
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

/// The seams of a light gate that waits between taking its slot and writing its claim until the file `go` exists.
fn slow_claim(go: &str) -> [&str; 4] {
    ["--test-poll-ms", "20", "--test-beside-claim-wait", go]
}

/// The arrival file the seam writes before it waits, for the go file `go`: once it exists, the gate is held at the seam
/// between its slot and its claim.
fn reached(go: &Path) -> PathBuf {
    let mut path = go.as_os_str().to_owned();
    path.push(".reached");
    PathBuf::from(path)
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

    let slow = case.start(&run_words(
        "slow.log",
        &slow_claim(&claim_go_word),
        &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
    ));
    let slow_claim = v1::claim_path(&case.root(), KernelObject::LightSlot(1)).expect("a light slot has a claim");
    wait_for_file(&reached(&claim_go));
    assert!(!slow_claim.exists(), "the slow gate has taken its slot and not yet claimed it");
    assert!(!took(&case, &slow), "no take line before the gate's claim");
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
    let early = case.start(&run_words(
        "early.log",
        &slow_claim(&claim_go_word),
        &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"],
    ));
    let early_claim = v1::claim_path(&case.root(), KernelObject::LightSlot(0)).expect("a light slot has a claim");
    wait_for_file(&reached(&claim_go));
    assert!(!early_claim.exists(), "the early gate has taken its slot and not yet claimed it");

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

/// The ticket file of the gate `pid` in the case's root, a `.json` file and not a rewrite's temporary one beside it;
/// fails when there is none.
fn ticket_file(case: &Case, pid: u32) -> PathBuf {
    fs::read_dir(v1::tickets_dir(&case.root()))
        .expect("the tickets folder can be listed")
        .map(|entry| entry.expect("a ticket entry can be read").path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "json")
                && path
                    .file_name()
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

#[test]
fn a_heavy_gate_nested_in_a_lent_gate_runs_covered_after_the_critical_gate_closes_its_lends() {
    let case = Case::new("a_heavy_gate_nested_in_a_lent_gate_runs_covered_after_the_critical_gate_closes_its_lends");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let (nested_go, release) = (case.folder().join("nested-go"), case.folder().join("release"));
    let (nested_go_word, release_word) = (shown(&nested_go), shown(&release));
    let pid_file = case.folder().join("nested.pid");
    let pid_word = shown(&pid_file);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical = case.start(&critical_words("critical.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr(WAITING);
    write_claim(&case, KernelObject::LightSlot(3), ClaimKind::Light, (process::id(), own_start()));
    let mut command = vec![FAKE_TOOL, "wait", &nested_go_word, "spawn-pid", &pid_word, &release_word, GATE];
    command.extend(heavy_words("nested.log", &POLL, &[FAKE_TOOL, "env", "GATE_SLOT_HELD"]));
    let lent_gate = start(marked(&case, &heavy_words("lent.log", &POLL, &command), &test_marker(&case, "light")));
    let lent_pid = lent_gate.id();
    lent_gate.wait_for_stderr(&lent(process::id()));

    let_go(&go);
    assert_passed(&held.finish());
    wait_until("the critical gate closing its lends", || lends_closed(&case, critical_pid));
    let_go(&nested_go);
    wait_for_file(&pid_file);
    let nested_pid: u32 = fs::read_to_string(&pid_file)
        .expect("the pid file can be read")
        .trim()
        .parse()
        .expect("the pid file holds a pid");
    // The nested gate's first journal line says how it runs: a covered gate journals `covered`, a queued one `queue`.
    wait_until("the nested gate journalling", || !events_of(&case, nested_pid).is_empty());
    assert_eq!(events_of(&case, nested_pid), ["covered"], "the nested gate is neither lent nor queued");
    let nested_log = || log_texts(&case.folder().join("nested.log"));
    wait_until("the nested gate passing", || nested_log().contains("gate: passed in "));
    let log = nested_log();
    assert!(
        log.contains(&format!("gate: running under the heavy slot of gate {lent_pid}")),
        "the nested gate runs under the lent gate's slot: {log}"
    );
    assert!(
        log.contains(&format!("heavy:{lent_pid}:")),
        "its command has the lent gate's marker: {log}"
    );
    assert_eq!(events_of(&case, nested_pid), ["covered"], "the nested gate is neither lent nor queued");
    // The nested gate has ended and the lent gate still runs, so the critical gate holding nothing yet pins that it
    // holds the machine only after both end.
    assert_eq!(events_of(&case, critical_pid), ["queue"], "the critical gate waits for the lent gate");

    let_go(&release);
    assert_passed(&lent_gate.finish());
    assert_passed(&critical.finish());
    assert_eq!(events_of(&case, lent_pid), ["parent"]);
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
        reason: None,
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
    let log = log_texts(&case.folder().join("critical.log"));
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
        reason: None,
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
    let inner_log = log_texts(&case.folder().join("inner.log"));
    assert!(
        inner_log.contains(&format!("gate: running under the critical slot of gate {outer_pid}")),
        "{inner_log}"
    );
    assert!(!inner_log.contains("waiting for"), "{inner_log}");
}

/// The prefix of the waiting line of a critical gate asked for `--alone`.
const ALONE_WAITING: &str = "gate: waiting for the machine alone (critical)";
/// The prefix of the holds line of a critical gate asked for `--alone`.
const ALONE_HOLDS: &str = "gate: holds the machine alone (critical) at ";

/// The words of a critical run asked for `--alone`, as [`critical_words`] gives those of a plain one.
fn alone_words<'a>(log: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut words = critical_words(log, extra, command);
    let at = words.iter().position(|word| *word == "--").expect("the words have a command");
    words.insert(at, "--alone");
    words
}

/// The first line of a light gate held back by the request of gate `pid` to hold the machine alone.
fn behind_alone(pid: u32) -> String {
    format!("gate: waiting for a light slot behind the request of gate {pid} that holds the machine alone\n")
}

/// Every slot of a count of one heavy and `light` light slots, by name, sorted.
fn every_slot(case: &Case, light: u32) -> Vec<String> {
    let mut names = vec![name(case, KernelObject::HeavySlot(0))];
    names.extend((0..light).map(|index| name(case, KernelObject::LightSlot(index))));
    names.sort();
    names
}

#[test]
fn an_alone_critical_gate_waits_for_running_light_gates() {
    let case = Case::new("an_alone_critical_gate_waits_for_running_light_gates");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let light_pid = light.id();

    let alone = case.start(&alone_words("alone.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let alone_pid = alone.id();
    alone.wait_for_stderr(&format!(
        "{ALONE_WAITING}: 0 heavy and 1 light slots held (0 light slots beside it), 0 earlier waiters\n"
    ));
    assert_eq!(events_of(&case, alone_pid), ["queue"], "no take while a light gate runs");

    let_go(&go);
    assert_passed(&alone.finish());
    assert_passed(&light.finish());
    assert!(at(&case, "release", light_pid) < at(&case, "take", alone_pid));
    assert_eq!(sorted(&take_of(&case, alone_pid)["slots"]), every_slot(&case, 3));
}

#[test]
fn a_light_gate_queued_after_an_alone_request_waits_until_it_ends() {
    let case = Case::new("a_light_gate_queued_after_an_alone_request_waits_until_it_ends");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let first_pid = first.id();
    let alone = case.start(&alone_words("alone.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let alone_pid = alone.id();
    alone.wait_for_stderr(ALONE_WAITING);

    let later = case.start(&run_words("later.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let later_pid = later.id();
    later.wait_for_stderr(&behind_alone(alone_pid));
    assert_eq!(
        events_of(&case, later_pid),
        ["queue"],
        "no light gate starts beside a request to hold the machine alone, two light slots free or not"
    );

    let_go(&go);
    assert_passed(&alone.finish());
    assert_passed(&later.finish());
    assert_passed(&first.finish());
    assert!(at(&case, "release", first_pid) < at(&case, "take", alone_pid));
    assert!(
        at(&case, "release", alone_pid) < at(&case, "take", later_pid),
        "the later light gate starts only once the alone gate let the machine go"
    );
}

#[test]
fn an_alone_critical_gate_holds_every_light_slot() {
    let case = Case::new("an_alone_critical_gate_holds_every_light_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let alone = holder(&case, &alone_words("alone.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let alone_pid = alone.id();
    assert_eq!(sorted(&take_of(&case, alone_pid)["slots"]), every_slot(&case, 3));

    let light = case.start(&run_words("light.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let light_pid = light.id();
    light.wait_for_stderr(&behind_alone(alone_pid));
    light.wait_for_stderr(&format!(
        "gate: still waiting for a light slot behind the request of gate {alone_pid} that holds the machine alone ("
    ));
    assert_eq!(events_of(&case, light_pid), ["queue"], "no light slot is left beside the alone gate");

    let_go(&go);
    assert_passed(&alone.finish());
    assert_passed(&light.finish());
    assert!(at(&case, "release", alone_pid) < at(&case, "take", light_pid));
}

#[test]
fn a_plain_critical_gate_still_lets_its_beside_light_gates_run() {
    let case = Case::new("a_plain_critical_gate_still_lets_its_beside_light_gates_run");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = holder(&case, &critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let critical_pid = critical.id();
    critical.wait_for_stderr("light slots (2 light slots beside it); other live claims: ");
    let left = [name(&case, KernelObject::HeavySlot(0)), name(&case, KernelObject::LightSlot(0))];
    assert_eq!(sorted(&take_of(&case, critical_pid)["slots"]), sorted(&json!(left)));

    let light_go = case.folder().join("light-go");
    let light_go_word = shown(&light_go);
    let first = holder(&case, &run_words("first.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    let second = holder(&case, &run_words("second.log", &POLL, &[FAKE_TOOL, "wait", &light_go_word, "exit", "0"]));
    assert_eq!(
        events_of(&case, first.id()),
        ["queue", "take"],
        "a light gate runs beside the critical gate"
    );
    assert_eq!(events_of(&case, second.id()), ["queue", "take"], "and a second one");

    let_go(&light_go);
    assert_passed(&first.finish());
    assert_passed(&second.finish());
    assert_eq!(events_of(&case, critical_pid), ["queue", "take"], "the critical gate holds on");
    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn alone_without_critical_is_a_usage_error() {
    let case = Case::new("alone_without_critical_is_a_usage_error");
    for slot in ["heavy", "light", "none"] {
        let mut words = alone_words("refused.log", &[], &[FAKE_TOOL, "exit", "0"]);
        words[2] = slot;
        let outcome = finish(case.gate(&words));
        assert_eq!(outcome.code, Some(2), "{slot}: {outcome:?}");
        assert_eq!(outcome.stderr, format!("gate: --alone needs --slot critical\n{USAGE}"), "{slot}");
    }
    let words = [
        "-Log",
        "refused.log",
        "-TimeoutSeconds",
        "60",
        "-Slot",
        "light",
        "-Alone",
        "--",
        FAKE_TOOL,
        "exit",
        "0",
    ];
    let mut compat = case.gate_without_state_dir(&words);
    compat.env("GATE_STATE_DIR", case.root());
    let outcome = finish(compat);
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr, format!("gate: -Alone needs -Slot critical\n{USAGE}"));
    assert!(!case.root().join("events.jsonl").exists(), "a refused run journals nothing");
}

#[test]
fn alone_inside_a_gates_command_is_a_usage_error() {
    for held in ["heavy", "light", "critical"] {
        let case = Case::new(&format!("alone_inside_a_gates_command_is_a_usage_error_{held}"));
        write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
        let words = alone_words("refused.log", &[], &[FAKE_TOOL, "exit", "0"]);

        let outcome = finish(marked(&case, &words, &test_marker(&case, held)));

        assert_eq!(outcome.code, Some(2), "under a {held} marker: {outcome:?}");
        assert_eq!(
            outcome.stderr, "gate: --alone cannot hold the machine alone from inside a gate's command\n",
            "under a {held} marker"
        );
        assert!(!case.root().join("events.jsonl").exists(), "the refused gate journals nothing");
        assert!(!v1::tickets_dir(&case.root()).exists(), "the refused gate queued nothing");
    }
}

#[test]
fn an_alone_critical_gate_says_alone_in_its_lines() {
    let case = Case::new("an_alone_critical_gate_says_alone_in_its_lines");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let light = holder(&case, &run_words("light.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));

    let alone = case.start(&alone_words("alone.log", &with_heartbeat(), &[FAKE_TOOL, "exit", "0"]));
    let status = "0 heavy and 1 light slots held (0 light slots beside it), 0 earlier waiters";
    alone.wait_for_stderr(&format!("{ALONE_WAITING}: {status}\n"));
    alone.wait_for_stderr(&format!("gate: still waiting for the machine alone (critical): {status} ("));

    let_go(&go);
    let outcome = alone.finish();
    assert_passed(&outcome);
    assert_passed(&light.finish());
    let holds = outcome
        .stderr
        .lines()
        .find(|line| line.starts_with(ALONE_HOLDS))
        .expect("the alone gate says its holds line");
    assert!(
        holds.contains("counts heavy 1, light 3; other gates hold 0 heavy and 0 light slots (0 light slots beside it); other live claims: none"),
        "{holds}"
    );
    assert!(
        !outcome.stderr.contains(WAITING) && !outcome.stderr.contains(HOLDS),
        "no plain critical line: {outcome:?}"
    );
}

#[test]
fn an_alone_request_lends_a_running_light_gate_s_slot_to_its_heavy_child_while_it_drains() {
    let case = Case::new("an_alone_request_lends_a_running_light_gate_s_slot_to_its_heavy_child_while_it_drains");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":4}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let held = holder(&case, &heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let held_pid = held.id();
    let mut child = vec![GATE];
    child.extend(heavy_words("child.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let parent = holder(&case, &run_words("parent.log", &POLL, &child));
    let parent_pid = parent.id();
    wait_until("the child gate queueing", || {
        sequence(&case)
            .iter()
            .any(|(event, pid)| event == "queue" && ![held_pid, parent_pid].contains(pid))
    });

    let alone = case.start(&alone_words("alone.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    let alone_pid = alone.id();
    alone.wait_for_stderr(ALONE_WAITING);
    wait_until("the light parent lending its slot to its heavy child", || {
        sequence(&case).iter().any(|(event, _)| event == "parent")
    });
    assert_passed(&parent.finish());

    let child_pid = lent_to(&case, parent_pid);
    assert_eq!(
        events_of(&case, child_pid),
        ["queue", "parent"],
        "the queued child left the queue for its parent's slot"
    );
    assert_eq!(events_of(&case, alone_pid), ["queue"], "the child ran while the alone request drains");

    let_go(&go);
    assert_passed(&alone.finish());
    assert_passed(&held.finish());
    assert!(
        at(&case, "release", parent_pid) < at(&case, "take", alone_pid),
        "the alone gate takes the machine only once the light gate ended"
    );
    assert_eq!(sorted(&take_of(&case, alone_pid)["slots"]), every_slot(&case, 4));
}

/// The seams of an alone run whose load windows last a tenth of a second.
fn short_windows() -> Vec<&'static str> {
    let mut seams = POLL.to_vec();
    seams.extend(["--test-load-window-ms", "100"]);
    seams
}

/// The load block at `when` (`start` or `end`) of `log`, its lines in order, asserted to have the shape the gate writes:
/// the headline, at most 8 process lines, the unattributed line, one GPU line or more, and one other-gates line or more.
fn load_block<'a>(log: &'a str, when: &str) -> Vec<&'a str> {
    let at = format!("gate: load at {when}");
    let block: Vec<&str> = log.lines().filter(|line| line.starts_with(&at)).collect();
    let shapes = [
        format!(r"^{at} over \d+\.\d s: \d+\.\d\d of \d+ cores busy outside this gate$"),
        format!(r"^{at}:   .+ \d+ \d+\.\d\d cores$"),
        format!(r"^{at}:   unattributed \d+\.\d\d cores$"),
        format!(r"^{at}: GPU 3D( \d+% on adapter 0x[0-9a-f]+_0x[0-9a-f]+|: counter unavailable \(.+\))$"),
        format!(r"^{at}: other gate(s: none| \d+ holds \w+ \d+: .+)$"),
    ]
    .map(|shape| Regex::new(&shape).expect("the shape compiles"));
    let unattributed = block.iter().position(|line| shapes[2].is_match(line));
    assert!(
        unattributed.is_some_and(|at| (1..=9).contains(&at)),
        "the unattributed line ends the CPU lines: {block:#?}"
    );
    let unattributed = unattributed.expect("checked above");
    assert!(shapes[0].is_match(block[0]), "the headline comes first: {block:#?}");
    assert!(
        block[1..unattributed].iter().all(|line| shapes[1].is_match(line)),
        "process lines: {block:#?}"
    );
    let rest = &block[unattributed + 1..];
    let gpu = rest.iter().take_while(|line| shapes[3].is_match(line)).count();
    assert!(gpu >= 1, "a GPU line follows: {block:#?}");
    assert!(
        rest.len() > gpu && rest[gpu..].iter().all(|line| shapes[4].is_match(line)),
        "the other gates' lines end the block: {block:#?}"
    );
    block
}

/// Asserts no line of `outcome`'s console is a load line.
fn assert_no_load_on_the_console(outcome: &crate::support::Outcome) {
    assert!(
        !outcome.stdout.contains("gate: load at") && !outcome.stderr.contains("gate: load at"),
        "no load line on the console: {outcome:?}"
    );
}

/// Asserts no journal line has a key about the load.
fn assert_no_load_in_the_journal(case: &Case) {
    for event in events(case) {
        let keys = event.as_object().expect("a line is an object").keys();
        assert!(keys.into_iter().all(|key| !key.to_ascii_lowercase().contains("load")), "{event}");
    }
}

#[test]
fn an_alone_run_logs_the_load_at_its_start_and_end() {
    let case = Case::new("an_alone_run_logs_the_load_at_its_start_and_end");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);

    let outcome = case.run(&alone_words("alone.log", &short_windows(), &[FAKE_TOOL, "out", "hi", "exit", "0"]));

    assert_passed(&outcome);
    assert_no_load_on_the_console(&outcome);
    assert_no_load_in_the_journal(&case);
    let log = log_texts(&case.folder().join("alone.log"));
    let start = load_block(&log, "start");
    let end = load_block(&log, "end");
    assert_eq!(
        start.last(),
        Some(&"gate: load at start: other gates: none"),
        "its own claims are left out"
    );
    assert_in_order(
        &log,
        &[
            ALONE_HOLDS,
            start[0],
            start[start.len() - 1],
            end[0],
            end[end.len() - 1],
            "gate: passed in ",
        ],
    );
}

#[test]
fn a_plain_critical_run_logs_no_load() {
    let case = Case::new("a_plain_critical_run_logs_no_load");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);

    let outcome = case.run(&critical_words("critical.log", &short_windows(), &[FAKE_TOOL, "out", "hi", "exit", "0"]));

    assert_passed(&outcome);
    let log = log_texts(&case.folder().join("critical.log"));
    assert!(log.contains(HOLDS), "{log}");
    assert!(!log.contains("gate: load at"), "{log}");
}

#[test]
fn an_alone_run_killed_at_its_ceiling_still_logs_the_load_at_its_end() {
    let case = Case::new("an_alone_run_killed_at_its_ceiling_still_logs_the_load_at_its_end");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let mut seams = short_windows();
    seams.extend(["--test-sample-ms", "100"]);
    let mut words = alone_words("alone.log", &seams, &[FAKE_TOOL, "sleep", UNTIL_KILLED]);
    assert_eq!(words[3], "--timeout", "the ceiling's value follows it");
    words[4] = "1";

    let outcome = case.run(&words);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_no_load_on_the_console(&outcome);
    let log = log_texts(&case.folder().join("alone.log"));
    let start = load_block(&log, "start");
    let end = load_block(&log, "end");
    assert_in_order(
        &log,
        &[
            ALONE_HOLDS,
            start[0],
            "gate: TIMED OUT after 1 s",
            end[0],
            end[end.len() - 1],
            "gate: FAILED (status 124)",
        ],
    );
}

#[test]
fn an_alone_run_names_another_gate_s_claim_in_its_load() {
    let case = Case::new("an_alone_run_names_another_gate_s_claim_in_its_load");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    write_claim(&case, KernelObject::RemoteSlot(2), ClaimKind::Remote, (process::id(), own_start()));

    let outcome = case.run(&alone_words("alone.log", &short_windows(), &[FAKE_TOOL, "exit", "0"]));

    assert_passed(&outcome);
    assert_no_load_on_the_console(&outcome);
    let log = log_texts(&case.folder().join("alone.log"));
    let other = format!("other gate {} holds remote 2: cargo build", process::id());
    for when in ["start", "end"] {
        let block = load_block(&log, when);
        let last = format!("gate: load at {when}: {other}");
        assert_eq!(block.last().copied(), Some(last.as_str()), "{block:#?}");
    }
}

#[test]
fn an_alone_run_interrupted_still_logs_the_load_at_its_end() {
    let case = Case::new("an_alone_run_interrupted_still_logs_the_load_at_its_end");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    let mut seams = short_windows();
    seams.extend(["--test-sample-ms", "100", "--test-interrupt-after-samples", "1"]);

    let outcome = case.run(&alone_words("alone.log", &seams, &[FAKE_TOOL, "sleep", UNTIL_KILLED]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_no_load_on_the_console(&outcome);
    let log = log_texts(&case.folder().join("alone.log"));
    let start = load_block(&log, "start");
    // The interrupt that ended the command ends the end window at once, so its CPU times may not have moved: the block
    // is asserted by its ends only.
    let end: Vec<&str> = log.lines().filter(|line| line.starts_with("gate: load at end")).collect();
    assert!(
        end.first()
            .is_some_and(|line| line.starts_with("gate: load at end over ") || line.starts_with("gate: load at end: ")),
        "{end:#?}"
    );
    assert_eq!(end.last(), Some(&"gate: load at end: other gates: none"), "{end:#?}");
    assert!(end.iter().any(|line| line.starts_with("gate: load at end: GPU 3D")), "{end:#?}");
    assert_in_order(
        &log,
        &[
            ALONE_HOLDS,
            start[0],
            "gate: interrupted; stopping the command",
            end[0],
            end[end.len() - 1],
            "gate: FAILED",
        ],
    );
}

#[test]
fn an_alone_run_keeps_its_load_off_the_console_however_long_another_gate_s_command() {
    let case = Case::new("an_alone_run_keeps_its_load_off_the_console_however_long_another_gate_s_command");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    write_claim(&case, KernelObject::RemoteSlot(2), ClaimKind::Remote, (process::id(), own_start()));
    let path = v1::claim_path(&case.root(), KernelObject::RemoteSlot(2)).expect("a slot has a claim");
    let mut claim = v1::read_claim(&path).expect("the claim reads back");
    claim.command = format!("pwsh -EncodedCommand {}", "A".repeat(5000));
    v1::write_claim(&path, &claim).expect("the claim can be written");

    let outcome = case.run(&alone_words("alone.log", &short_windows(), &[FAKE_TOOL, "exit", "0"]));

    assert_passed(&outcome);
    assert_no_load_on_the_console(&outcome);
    let log = log_texts(&case.folder().join("alone.log"));
    let end = load_block(&log, "end");
    let other = format!(
        "gate: load at end: other gate {} holds remote 2: pwsh -EncodedCommand AAAA",
        process::id()
    );
    let last = end.last().copied().unwrap_or("");
    assert!(last.starts_with(&other) && last.ends_with('…'), "{end:#?}");
}

#[test]
fn an_alone_run_whose_command_cannot_start_still_logs_the_load_at_its_end() {
    let case = Case::new("an_alone_run_whose_command_cannot_start_still_logs_the_load_at_its_end");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);

    let outcome = case.run(&alone_words("alone.log", &short_windows(), &["gate-no-such-program", "x"]));

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert_no_load_on_the_console(&outcome);
    let log = log_texts(&case.folder().join("alone.log"));
    let start = load_block(&log, "start");
    let end = load_block(&log, "end");
    assert_in_order(
        &log,
        &[
            ALONE_HOLDS,
            start[0],
            "gate: could not start gate-no-such-program",
            end[0],
            end[end.len() - 1],
            "gate: FAILED (status 127)",
        ],
    );
}

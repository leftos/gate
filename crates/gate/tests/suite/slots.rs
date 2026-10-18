//! The slot a run takes: waiters of one kind in ticket order, a killed waiter's ticket cleared, the claim and the
//! hashed names of a test root, a slot taken over from a killed holder (GATE-30), and the counts and wait lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_state::v1::{Ticket, TicketKind, read_claim, ticket_path, write_ticket};
use gate_state::{KernelObject, StateRoot};
use gate_win32::{NamedMutex, mutex_exists};
use serde_json::Value;

use crate::support::{
    Case, FAKE_TOOL, Outcome, Running, assert_in_order, events_of, heavy_words, none_words, run_words, shown, wait_for_file, wait_for_journal_line,
    wait_until, wait_until_closed, write_counts,
};

/// A light pool of one slot.
const ONE_LIGHT: &str = r#"{"v":1,"light":1}"#;
/// What a held run's command writes to its log once it runs.
const HOLDING: &str = "holding";

/// Starts a light run logging to `log`, `extra` before `--`, whose command says it is holding, waits for `go` when
/// there is one, and exits 0.
fn start(case: &Case, log: &str, go: Option<&Path>, extra: &[&str]) -> Running {
    let go = go.map(shown);
    let mut command = vec![FAKE_TOOL, "out", HOLDING];
    if let Some(go) = &go {
        command.extend(["wait", go.as_str()]);
    }
    case.start(&run_words(log, extra, &command))
}

/// Runs a light run logging to `out.log` whose command exits 0 at once.
fn run_once(case: &Case) -> Outcome {
    case.run(&run_words("out.log", &[], &[FAKE_TOOL, "exit", "0"]))
}

/// Starts a gate logging to `log` that takes the slot and holds it until `go` exists, and returns once it has taken it
/// and its command runs.
fn holder(case: &Case, log: &str, go: &Path) -> Running {
    let running = start(case, log, Some(go), &[]);
    wait_for_journal_line(&case.root(), &take_needle(running.id()));
    let path = case.folder().join(log);
    let started = format!("{HOLDING}\n");
    wait_until("the holder's command writing its log", || {
        fs::read(&path).is_ok_and(|bytes| bytes.starts_with(started.as_bytes()))
    });
    running
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

/// Lets go the command a killed holder left running, which logs to `log`, and returns once every handle on that log is
/// closed.
fn end_orphan(case: &Case, go: &Path, log: &str) {
    let_go(go);
    wait_until_closed(&case.folder().join(log));
}

/// What a journal line of the take of the gate `pid` contains.
fn take_needle(pid: u32) -> String {
    format!(r#""event":"take","pid":{pid},"#)
}

fn journal(case: &Case) -> String {
    fs::read_to_string(case.root().join("events.jsonl")).expect("the journal was written")
}

/// The journal's take lines, in order.
fn takes(case: &Case) -> Vec<Value> {
    journal(case)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a journal line is JSON"))
        .filter(|line| line["event"] == "take")
        .collect()
}

/// The case root's name of its light slot `index`.
fn light_slot(case: &Case, index: u32) -> String {
    StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::LightSlot(index))
}

fn claim_path(case: &Case) -> PathBuf {
    case.root().join("slots").join("gate-light-slot-0.json")
}

/// Whether the gate `pid` has a ticket in the case root.
fn has_ticket(case: &Case, pid: u32) -> bool {
    let prefix = format!("{pid}-");
    match fs::read_dir(case.root().join("waiting")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        listed => listed.expect("the tickets folder can be listed").any(|entry| {
            entry
                .expect("a ticket entry can be read")
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)
        }),
    }
}

fn assert_passed(running: Running) {
    let outcome = running.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The names in the case root's `folder`; none when there is no such folder.
fn names_in(case: &Case, folder: &str) -> Vec<String> {
    match fs::read_dir(case.root().join(folder)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        listed => listed
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
            .collect(),
    }
}

#[test]
fn a_no_slot_run_starts_while_every_heavy_and_light_slot_is_held() {
    let case = Case::new("a_no_slot_run_starts_while_every_heavy_and_light_slot_is_held");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let heavy = case.start(&heavy_words("heavy.log", &[], &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    wait_for_journal_line(&case.root(), &take_needle(heavy.id()));
    let light = holder(&case, "light.log", &go);

    let none = case.start(&none_words("none.log", &[], &[FAKE_TOOL, "exit", "0"]));
    let none_pid = none.id();
    wait_for_journal_line(&case.root(), &format!(r#""event":"release","pid":{none_pid},"#));
    let outcome = none.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.contains("waiting for"), "{outcome:?}");
    assert_eq!(events_of(&case, none_pid), ["queue", "take", "release"]);
    assert!(!has_ticket(&case, none_pid), "a no-slot run leaves no ticket");
    let_go(&go);
    assert_passed(heavy);
    assert_passed(light);
}

#[test]
fn a_no_slot_run_writes_no_claim_or_ticket() {
    let case = Case::new("a_no_slot_run_writes_no_claim_or_ticket");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let running = case.start(&none_words("none.log", &[], &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    wait_for_journal_line(&case.root(), &take_needle(running.id()));

    for folder in ["slots", "waiting", "covered"] {
        assert_eq!(
            names_in(&case, folder),
            Vec::<String>::new(),
            "the running no-slot gate wrote in {folder}"
        );
    }
    let_go(&go);
    assert_passed(running);
}

#[test]
fn a_gate_that_cannot_read_its_own_start_refuses_and_writes_nothing() {
    let case = Case::new("a_gate_that_cannot_read_its_own_start_refuses_and_writes_nothing");
    let ran = shown(&case.folder().join("ran"));
    let outcome = case.run(&heavy_words("out.log", &["--test-own-start-fails"], &[FAKE_TOOL, "mark", ran.as_str()]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.contains("gate: cannot read this gate's own start: "), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("--test-own-start-fails makes the start unreadable (test seam only); not running "),
        "{outcome:?}"
    );
    assert!(case.root_entries().is_empty(), "the state root holds {:?}", case.root_entries());
    assert!(case.folder_entries().is_empty(), "the case folder holds {:?}", case.folder_entries());
}

#[test]
fn waiters_of_one_kind_take_a_freed_slot_in_ticket_order() {
    let case = Case::new("waiters_of_one_kind_take_a_freed_slot_in_ticket_order");
    write_counts(&case.root(), ONE_LIGHT);
    let (go_h, go_1, go_2) = (case.folder().join("go-h"), case.folder().join("go-1"), case.folder().join("go-2"));
    let holding = holder(&case, "holder.log", &go_h);
    let first = start(&case, "first.log", Some(&go_1), &[]);
    first.wait_for_stderr("gate: waiting for a light slot (1 busy)");
    let mut second = start(&case, "second.log", Some(&go_2), &[]);
    second.wait_for_stderr("gate: waiting for a light slot (");

    let_go(&go_h);
    wait_for_journal_line(&case.root(), &take_needle(first.id()));

    assert!(second.is_running(), "the second waiter still waits");
    assert!(
        !journal(&case).contains(&take_needle(second.id())),
        "the second waiter has not taken the slot the first holds"
    );
    let_go(&go_1);
    wait_for_journal_line(&case.root(), &take_needle(second.id()));
    let_go(&go_2);
    let order = [take_needle(first.id()), take_needle(second.id())];
    for running in [holding, first, second] {
        assert_passed(running);
    }
    assert_in_order(&journal(&case), &[&order[0], &order[1]]);
}

#[test]
fn a_gate_killed_while_waiting_leaves_no_ticket_after_the_poll() {
    let case = Case::new("a_gate_killed_while_waiting_leaves_no_ticket_after_the_poll");
    write_counts(&case.root(), ONE_LIGHT);
    let go_h = case.folder().join("go-h");
    let holding = holder(&case, "holder.log", &go_h);
    let killed = start(&case, "killed.log", None, &[]);
    killed.wait_for_stderr("gate: waiting for a light slot (");
    let pid = killed.id();
    assert!(has_ticket(&case, pid), "the waiter queued with a ticket");
    killed.kill();

    let waiter = start(&case, "waiter.log", None, &["--test-poll-ms", "50"]);
    wait_until("the killed waiter's ticket going", || !has_ticket(&case, pid));
    let_go(&go_h);

    let waiter_pid = waiter.id();
    assert_passed(waiter);
    assert_passed(holding);
    assert!(journal(&case).contains(&take_needle(waiter_pid)), "the waiter took the slot");
}

#[test]
fn simultaneous_arrivals_with_free_slots_do_not_wait() {
    let case = Case::new("simultaneous_arrivals_with_free_slots_do_not_wait");
    write_counts(&case.root(), r#"{"v":1,"light":3}"#);
    let gos: Vec<PathBuf> = (1..=3).map(|number| case.folder().join(format!("go-{number}"))).collect();
    let logs: Vec<String> = (1..=3).map(|number| format!("out-{number}.log")).collect();
    let runs: Vec<Running> = gos.iter().zip(&logs).map(|(go, log)| start(&case, log, Some(go), &[])).collect();

    for running in &runs {
        wait_for_journal_line(&case.root(), &take_needle(running.id()));
    }

    for go in &gos {
        let_go(go);
    }
    for running in runs {
        let outcome = running.finish();
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert!(
            !outcome.stderr.contains("waiting for"),
            "a gate with a free slot for it did not wait: {outcome:?}"
        );
    }
}

#[test]
fn a_holder_whose_ticket_was_not_deleted_does_not_block_the_queue() {
    let case = Case::new("a_holder_whose_ticket_was_not_deleted_does_not_block_the_queue");
    write_counts(&case.root(), r#"{"v":1,"light":2}"#);
    let go_h = case.folder().join("go-h");
    let holding = holder(&case, "holder.log", &go_h);
    wait_for_file(&claim_path(&case));
    let claim = read_claim(&claim_path(&case)).expect("the holder's claim reads");
    let start_ms = u64::try_from(claim.process_start.as_millisecond()).expect("a gate starts after the epoch");
    let ticket = Ticket {
        kind: TicketKind::Light,
        arrived: 0,
        pid: claim.pid,
        start_ms,
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        expected: None,
    };
    write_ticket(&ticket_path(&case.root(), claim.pid, start_ms), &ticket).expect("the holder's ticket can be written back");

    let outcome = run_once(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: waiting for"),
        "the holder's ticket is not ahead: {outcome:?}"
    );
    let take = takes(&case).pop().expect("the gate journalled its take");
    let slot = take["slots"][0].as_str().expect("the take names its slot");
    assert!(slot.ends_with("light-slot-1"), "{take}");
    let_go(&go_h);
    assert_passed(holding);
}

#[test]
fn a_waiter_deferring_to_a_gate_that_took_its_slot_does_not_say_it_waits() {
    let case = Case::new("a_waiter_deferring_to_a_gate_that_took_its_slot");
    write_counts(&case.root(), r#"{"v":1,"light":2}"#);
    // A gate between its take of slot 0 and its ticket's deletion, held still; this test process stands in for it.
    let mutex = NamedMutex::open(&light_slot(&case, 0)).expect("slot 0's mutex opens");
    let held = mutex.acquire(Duration::from_secs(5)).expect("slot 0 is free");
    let pid = std::process::id();
    let began = gate_win32::process_start_of(pid)
        .expect("this process can be queried")
        .expect("this process runs");
    let start_ms = u64::try_from(began.as_millisecond()).expect("this process started after the epoch");
    let ticket = Ticket {
        kind: TicketKind::Light,
        arrived: 0,
        pid,
        start_ms,
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        expected: None,
    };
    let taker = ticket_path(&case.root(), pid, start_ms);
    write_ticket(&taker, &ticket).expect("the taker's ticket can be written");
    let waiter = start(&case, "out.log", None, &["--test-heartbeat-ms", "50", "--test-poll-ms", "50"]);
    waiter.wait_for_stderr("gate: still waiting for a light slot");

    fs::remove_file(&taker).expect("the taker's ticket can be deleted, as its take does next");

    let outcome = waiter.finish();
    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("gate: waiting for a light slot ("),
        "slot 1 was free for the waiter, and the only gate ahead of it held slot 0: {outcome:?}"
    );
}

#[test]
fn a_waiter_with_every_slot_held_says_it_waits_with_every_slot_busy() {
    let case = Case::new("a_waiter_with_every_slot_held_says_it_waits");
    write_counts(&case.root(), r#"{"v":1,"light":2}"#);
    let mutexes: Vec<NamedMutex> = (0..2)
        .map(|index| NamedMutex::open(&light_slot(&case, index)).expect("the slot's mutex opens"))
        .collect();
    let held: Vec<_> = mutexes
        .iter()
        .map(|mutex| mutex.acquire(Duration::from_secs(5)).expect("the slot is free"))
        .collect();
    let waiter = start(&case, "out.log", None, &["--test-heartbeat-ms", "50", "--test-poll-ms", "50"]);
    waiter.wait_for_stderr("gate: still waiting for a light slot");

    drop(held);

    let outcome = waiter.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &["gate: waiting for a light slot (2 busy)", "gate: still waiting for a light slot ("],
    );
}

#[test]
fn a_test_root_names_its_slots_with_its_hash() {
    let case = Case::new("a_test_root_names_its_slots_with_its_hash");
    write_counts(&case.root(), ONE_LIGHT);
    let root = StateRoot::new(&shown(&case.root()), None);
    let hash = root.hash().expect("a case root is not the default root");
    let name = light_slot(&case, 0);
    assert!(name.starts_with(&format!(r"Local\gate-{hash}-")), "{name}");
    let go = case.folder().join("go");
    let running = start(&case, "out.log", Some(&go), &[]);
    let claim = claim_path(&case);

    wait_for_file(&claim);

    let read = read_claim(&claim).expect("the claim reads as a v1 claim");
    assert_eq!(read.pid, running.id());
    assert!(mutex_exists(&name).expect("the slot can be probed"), "{name} is held");
    let take = takes(&case).pop().expect("the gate journalled its take");
    assert_eq!(take["slots"], Value::from(vec![name.as_str()]));
    let_go(&go);
    assert_passed(running);
    assert!(!claim.exists(), "the claim goes with the release");
}

#[test]
fn a_slot_left_by_a_killed_holder_is_journalled_abandoned() {
    let case = Case::new("a_slot_left_by_a_killed_holder_is_journalled_abandoned");
    write_counts(&case.root(), ONE_LIGHT);
    let name = light_slot(&case, 0);
    let go = case.folder().join("go");
    let holding = holder(&case, "holder.log", &go);
    wait_for_file(&claim_path(&case));
    holding.kill();
    // Read before the next run takes the slot over; asserted once the killed gate's command is gone.
    let claim_left = claim_path(&case).exists();

    let outcome = run_once(&case);
    // The killed gate's command is let go, and every handle on its log seen closed, before any assertion, so none is
    // left waiting.
    end_orphan(&case, &go, "holder.log");

    assert!(claim_left, "a killed holder leaves its claim");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let take = takes(&case).pop().expect("the gate journalled its take");
    assert_eq!(take["abandoned"], Value::from(vec![name.as_str()]), "{take}");
    let line = format!("gate: took {name}, which a killed gate had left held");
    assert_eq!(outcome.stderr.matches(&line).count(), 1, "{outcome:?}");
}

#[test]
fn an_abandoned_mutex_is_journalled_too() {
    let case = Case::new("an_abandoned_mutex_is_journalled_too");
    write_counts(&case.root(), ONE_LIGHT);
    let name = light_slot(&case, 0);
    let go = case.folder().join("go");
    let holding = holder(&case, "holder.log", &go);
    wait_for_file(&claim_path(&case));
    let kept = NamedMutex::open(&name).expect("the slot's mutex opens beside its holder");
    holding.kill();

    let outcome = run_once(&case);
    drop(kept);
    end_orphan(&case, &go, "holder.log");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let take = takes(&case).pop().expect("the gate journalled its take");
    assert_eq!(take["abandoned"], Value::from(vec![name.as_str()]), "the name once: {take}");
    let line = format!("gate: took {name}, which a killed gate had left held");
    assert_eq!(outcome.stderr.matches(&line).count(), 1, "{outcome:?}");
}

#[test]
fn a_count_raised_while_waiting_applies_on_the_next_poll() {
    let case = Case::new("a_count_raised_while_waiting_applies_on_the_next_poll");
    write_counts(&case.root(), ONE_LIGHT);
    let go_h = case.folder().join("go-h");
    let holding = holder(&case, "holder.log", &go_h);
    let waiter = start(&case, "waiter.log", None, &["--test-poll-ms", "50"]);
    waiter.wait_for_stderr("gate: waiting for a light slot (");

    write_counts(&case.root(), r#"{"v":1,"light":2}"#);

    let pid = waiter.id();
    assert_passed(waiter);
    let take = takes(&case)
        .into_iter()
        .find(|take| take["pid"] == pid)
        .expect("the waiter journalled its take");
    let slot = take["slots"][0].as_str().expect("the take names its slot");
    assert!(slot.ends_with("light-slot-1"), "{take}");
    let_go(&go_h);
    assert_passed(holding);
}

#[test]
fn a_waiter_says_it_still_waits() {
    let case = Case::new("a_waiter_says_it_still_waits");
    write_counts(&case.root(), ONE_LIGHT);
    let go_h = case.folder().join("go-h");
    let holding = holder(&case, "holder.log", &go_h);
    let waiter = start(&case, "waiter.log", None, &["--test-heartbeat-ms", "100"]);

    waiter.wait_for_stderr("gate: still waiting for a light slot (");

    let_go(&go_h);
    assert_passed(waiter);
    assert_passed(holding);
}

#[test]
fn a_counts_file_without_v_is_said_once() {
    let case = Case::new("a_counts_file_without_v_is_said_once");
    write_counts(&case.root(), r#"{"light":1}"#);

    let outcome = run_once(&case);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!(
        "gate: ignored {}: missing key \"v\"; using ",
        shown(&case.root().join("slot-counts.json"))
    );
    assert_eq!(outcome.stderr.matches(&line).count(), 1, "{outcome:?}");
    assert!(!outcome.stderr.contains("heavy"), "only the gate's own pool is said: {outcome:?}");
}

//! The slot a seam run takes: waiters of one kind in ticket order, a killed waiter's ticket cleared, the claim and the
//! hashed names of a test root, a slot taken over from a killed holder (GATE-30), and the counts and wait lines.

use std::fs;
use std::path::{Path, PathBuf};

use gate_state::v1::{Ticket, TicketKind, read_claim, ticket_path, write_ticket};
use gate_state::{KernelObject, StateRoot};
use gate_win32::{NamedMutex, mutex_exists};
use serde_json::Value;

use crate::support::{Case, Running, assert_in_order, shown, wait_for_file, wait_for_journal_line, wait_until, write_counts};

/// A light pool of one slot.
const ONE_LIGHT: &str = r#"{"v":1,"light":1}"#;

/// The words of a light seam run that exits 0, holding until `go` exists when there is one, `extra` before `--`.
fn seam_words(go: Option<&Path>, extra: &[&str]) -> Vec<String> {
    let mut words: Vec<String> = ["run", "--slot", "light", "--timeout", "5", "--log", "out.log"].map(str::to_owned).into();
    words.extend(extra.iter().map(|word| (*word).to_owned()));
    words.extend(["--test-journal-run".to_owned(), "0".to_owned()]);
    if let Some(go) = go {
        words.extend(["--test-hold-until".to_owned(), shown(go)]);
    }
    words.extend(["--", "cmd", "/c", "exit", "0"].map(str::to_owned));
    words
}

fn start(case: &Case, go: Option<&Path>, extra: &[&str]) -> Running {
    let words = seam_words(go, extra);
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    case.start(&refs)
}

/// Starts a gate that takes the slot and holds it until `go` exists, and returns once it has taken it.
fn holder(case: &Case, go: &Path) -> Running {
    let running = start(case, Some(go), &[]);
    wait_for_journal_line(&case.root(), &take_needle(running.id()));
    running
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
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

#[test]
fn waiters_of_one_kind_take_a_freed_slot_in_ticket_order() {
    let case = Case::new("waiters_of_one_kind_take_a_freed_slot_in_ticket_order");
    write_counts(&case.root(), ONE_LIGHT);
    let (go_h, go_1, go_2) = (case.root().join("go-h"), case.root().join("go-1"), case.root().join("go-2"));
    let holding = holder(&case, &go_h);
    let first = start(&case, Some(&go_1), &[]);
    first.wait_for_stderr("gate: waiting for a light slot (1 busy)");
    let mut second = start(&case, Some(&go_2), &[]);
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
    let go_h = case.root().join("go-h");
    let holding = holder(&case, &go_h);
    let killed = start(&case, None, &[]);
    killed.wait_for_stderr("gate: waiting for a light slot (");
    let pid = killed.id();
    assert!(has_ticket(&case, pid), "the waiter queued with a ticket");
    killed.kill();

    let waiter = start(&case, None, &["--test-poll-ms", "50"]);
    wait_until("the killed waiter's ticket going", || !has_ticket(&case, pid));
    let_go(&go_h);

    let waiter_pid = waiter.id();
    assert_passed(waiter);
    assert_passed(holding);
    assert!(journal(&case).contains(&take_needle(waiter_pid)), "the waiter took the slot");
}

#[test]
fn a_holder_whose_ticket_was_not_deleted_does_not_block_the_queue() {
    let case = Case::new("a_holder_whose_ticket_was_not_deleted_does_not_block_the_queue");
    write_counts(&case.root(), r#"{"v":1,"light":2}"#);
    let go_h = case.root().join("go-h");
    let holding = holder(&case, &go_h);
    wait_for_file(&claim_path(&case));
    let claim = read_claim(&claim_path(&case)).expect("the holder's claim reads");
    let start_ms = u64::try_from(claim.process_start.as_millisecond()).expect("a gate starts after the epoch");
    let ticket = Ticket {
        kind: TicketKind::Light,
        arrived: 0,
        pid: claim.pid,
        start_ms,
        holds: false,
        remote: None,
        expected: None,
    };
    write_ticket(&ticket_path(&case.root(), claim.pid, start_ms), &ticket).expect("the holder's ticket can be written back");

    let outcome = case.run(&seam_words(None, &[]).iter().map(String::as_str).collect::<Vec<_>>());

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
fn a_test_root_names_its_slots_with_its_hash() {
    let case = Case::new("a_test_root_names_its_slots_with_its_hash");
    write_counts(&case.root(), ONE_LIGHT);
    let root = StateRoot::new(&shown(&case.root()), None);
    let hash = root.hash().expect("a case root is not the default root");
    let name = light_slot(&case, 0);
    assert!(name.starts_with(&format!(r"Local\gate-{hash}-")), "{name}");
    let go = case.root().join("go");
    let running = start(&case, Some(&go), &[]);
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
    let holding = holder(&case, &case.root().join("go"));
    wait_for_file(&claim_path(&case));
    holding.kill();
    assert!(claim_path(&case).exists(), "a killed holder leaves its claim");

    let outcome = case.run(&seam_words(None, &[]).iter().map(String::as_str).collect::<Vec<_>>());

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
    let holding = holder(&case, &case.root().join("go"));
    wait_for_file(&claim_path(&case));
    let kept = NamedMutex::open(&name).expect("the slot's mutex opens beside its holder");
    holding.kill();

    let outcome = case.run(&seam_words(None, &[]).iter().map(String::as_str).collect::<Vec<_>>());
    drop(kept);

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
    let go_h = case.root().join("go-h");
    let holding = holder(&case, &go_h);
    let waiter = start(&case, None, &["--test-poll-ms", "50"]);
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
    let go_h = case.root().join("go-h");
    let holding = holder(&case, &go_h);
    let waiter = start(&case, None, &["--test-heartbeat-ms", "100"]);

    waiter.wait_for_stderr("gate: still waiting for a light slot (");

    let_go(&go_h);
    assert_passed(waiter);
    assert_passed(holding);
}

#[test]
fn a_counts_file_without_v_is_said_once() {
    let case = Case::new("a_counts_file_without_v_is_said_once");
    write_counts(&case.root(), r#"{"light":1}"#);

    let outcome = case.run(&seam_words(None, &[]).iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let line = format!(
        "gate: ignored {}: missing key \"v\"; using ",
        shown(&case.root().join("slot-counts.json"))
    );
    assert_eq!(outcome.stderr.matches(&line).count(), 1, "{outcome:?}");
    assert!(!outcome.stderr.contains("heavy"), "only the gate's own pool is said: {outcome:?}");
}

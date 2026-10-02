//! Slot rows from claim files, the process table and the slot mutexes.

use std::fs;

use gate_state::{SlotKind, SlotRow, SlotState};
use serde_json::{Value, json};

use crate::support::{Machine, START, START_CLOSE, START_FAR, claim, pool};

fn heavy_slot(machine: &Machine, index: u32) -> SlotRow {
    let state = machine.state();
    pool(&state, SlotKind::Heavy)
        .slots
        .iter()
        .find(|slot| slot.index == index)
        .expect("the heavy pool has the slot")
        .clone()
}

#[test]
fn live_claim_is_busy_with_descendants_and_last_line() {
    let mut machine = Machine::new("live_claim_is_busy_with_descendants_and_last_line");
    let log = machine.dir.join("build.log");
    fs::write(&log, "first\r\nsecond line\r\n\r\n   \n").expect("the log can be written");
    let claim = claim(100, Some(log.to_str().expect("the log path is text")), "C:\\elsewhere");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START_CLOSE));
    machine.add_process(101, 100, "cargo.exe", None, None);
    machine.add_process(102, 101, "rustc.exe", None, None);
    machine.add_process(103, 100, "conhost.exe", None, None);
    machine.add_process(200, 1, "unrelated.exe", None, None);
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(row.state, SlotState::Busy);
    assert_eq!(row.claim, Some(claim));
    assert_eq!(row.stale, None);
    assert_eq!(row.processes, 3);
    assert_eq!(row.last_line.as_deref(), Some("second line"));
    assert_eq!(row.extra, Some(false));
}

#[test]
fn claim_with_mismatched_process_start_is_stale() {
    let mut machine = Machine::new("claim_with_mismatched_process_start_is_stale");
    let held = claim(100, None, "C:\\work");
    let free = claim(101, None, "C:\\work");
    machine.write_claim("gate-slot-0.json", &held.to_string());
    machine.write_claim("gate-slot-1.json", &free.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START_FAR));
    machine.add_process(101, 1, "pwsh.exe", None, Some(START_FAR));
    machine.open.insert("gate-slot-0".to_owned());

    for (index, stale, state) in [(0, held, SlotState::Held), (1, free, SlotState::Free)] {
        let row = heavy_slot(&machine, index);
        assert_eq!(row.state, state, "slot {index}");
        assert_eq!(row.stale, Some(stale), "slot {index}");
        assert_eq!((row.claim, row.processes, row.last_line), (None, 0, None), "slot {index}");
    }
}

#[test]
fn open_mutex_without_claim_is_held() {
    let mut machine = Machine::new("open_mutex_without_claim_is_held");
    machine.open.insert("gate-light-slot-2".to_owned());
    let state = machine.state();

    for slot in &pool(&state, SlotKind::Light).slots {
        let expected = if slot.index == 2 { SlotState::Held } else { SlotState::Free };
        assert_eq!(slot.state, expected, "{}", slot.name);
        assert_eq!((&slot.claim, &slot.stale), (&None, &None), "{}", slot.name);
    }
    assert!(pool(&state, SlotKind::Heavy).slots.iter().all(|slot| slot.state == SlotState::Free));
}

#[test]
fn claim_index_past_count_is_extra() {
    let machine = Machine::new("claim_index_past_count_is_extra");
    machine.write_claim("gate-slot-5.json", &claim(100, None, "C:\\work").to_string());
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!(heavy.count, 3);
    let rows: Vec<(u32, Option<bool>)> = heavy.slots.iter().map(|slot| (slot.index, slot.extra)).collect();
    assert_eq!(rows, [(0, Some(false)), (1, Some(false)), (2, Some(false)), (5, Some(true))]);
    let extra = heavy.slots.iter().find(|slot| slot.index == 5).expect("the extra row exists");
    assert_eq!(extra.name, "gate-slot-5");
    assert!(extra.stale.is_some());
}

#[test]
fn self_test_prefixed_claim_file_is_ignored() {
    let mut machine = Machine::new("self_test_prefixed_claim_file_is_ignored");
    machine.write_claim("xyz-gate-slot-0.json", &claim(100, None, "C:\\work").to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!(heavy.slots.len(), 3);
    for slot in &heavy.slots {
        assert_eq!(slot.state, SlotState::Free, "{}", slot.name);
        assert_eq!((&slot.claim, &slot.stale), (&None, &None), "{}", slot.name);
    }
}

#[test]
fn unreadable_claim_is_stale_unreadable() {
    let machine = Machine::new("unreadable_claim_is_stale_unreadable");
    machine.write_claim("gate-slot-0.json", "{\"pid\": 1");

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Free);
    assert_eq!(row.claim, None);
    let stale = row.stale.expect("an unreadable claim is stale");
    let object = stale.as_object().expect("the stale claim is an object");
    assert_eq!(object.keys().collect::<Vec<_>>(), ["unreadable"]);
    let message = object.get("unreadable").and_then(Value::as_str).expect("the reason is text");
    assert_ne!(message, "");
}

#[test]
fn claim_missing_pid_is_stale_unreadable() {
    let mut machine = Machine::new("claim_missing_pid_is_stale_unreadable");
    let mut claim = claim(100, None, "C:\\work");
    claim.as_object_mut().expect("a claim is an object").remove("pid");
    machine.write_claim("gate-slot-0.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    machine.open.insert("gate-slot-0".to_owned());

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Held);
    assert_eq!(row.claim, None);
    assert_eq!(row.stale, Some(json!({"unreadable": "the claim has no pid that is a process id"})));
}

#[test]
fn relative_claim_log_resolves_against_claim_cwd() {
    let mut machine = Machine::new("relative_claim_log_resolves_against_claim_cwd");
    fs::create_dir_all(machine.dir.join("logs")).expect("the log folder can be made");
    fs::write(machine.dir.join("logs").join("run.log"), "building\ndone\n").expect("the log can be written");
    let cwd = machine.dir.to_str().expect("the test folder is text").to_owned();
    machine.write_claim("gate-slot-0.json", &claim(100, Some("logs/run.log"), &cwd).to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Busy);
    assert_eq!(row.last_line.as_deref(), Some("done"));
}

#[test]
fn last_line_reads_only_the_final_4096_bytes() {
    let mut machine = Machine::new("last_line_reads_only_the_final_4096_bytes");
    let log = machine.dir.join("long.log");
    let line = format!("start-{}-end", "x".repeat(5000));
    fs::write(&log, format!("earlier\n{line}\n")).expect("the log can be written");
    machine.write_claim(
        "gate-slot-0.json",
        &claim(100, Some(log.to_str().expect("the log path is text")), "C:\\work").to_string(),
    );
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));

    let row = heavy_slot(&machine, 0);

    let tail = line.get(line.len() - 4095..).expect("the line is longer than the tail");
    assert_eq!(
        row.last_line.as_deref(),
        Some(tail),
        "the final 4096 bytes are the line's last 4095 and its newline"
    );
}

#[test]
fn claim_exactly_two_seconds_off_is_not_live() {
    let mut machine = Machine::new("claim_exactly_two_seconds_off_is_not_live");
    let stale = claim(100, None, "C:\\work");
    machine.write_claim("gate-slot-0.json", &stale.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some("2026-10-01T11:00:02.5Z"));

    let row = heavy_slot(&machine, 0);

    assert_eq!(row.state, SlotState::Free);
    assert_eq!((row.claim, row.stale), (None, Some(stale)));
}

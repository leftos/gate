//! Slot rows from claim files, the process table and the slot mutexes.

use std::fs;

use gate_state::{RemotePool, SlotKind, SlotRow, SlotState};
use serde_json::{Value, json};

use crate::support::{Machine, START, START_CLOSE, START_FAR, claim, pool, remote_claim};

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

fn remote_pool(machine: &Machine) -> RemotePool {
    machine.state().remote.expect("the state has a remote pool")
}

/// Each remote slot's index, state and `extra`.
fn remote_rows(pool: &RemotePool) -> Vec<(u32, SlotState, Option<bool>)> {
    pool.slots.iter().map(|slot| (slot.index, slot.state, slot.extra)).collect()
}

#[test]
fn remote_claim_is_busy_in_the_remote_pool_and_in_no_other() {
    let mut machine = Machine::new("remote_claim_is_busy_in_the_remote_pool_and_in_no_other");
    machine.write_remote(r#"{"slots": 2}"#);
    let claim = remote_claim(100, 1);
    machine.write_claim("gate-remote-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    machine.add_process(101, 100, "ssh.exe", None, None);
    machine.open.insert("gate-remote-slot-0".to_owned());
    let state = machine.state();

    let remote = state.remote.as_ref().expect("remote.json sizes a remote pool");
    assert_eq!((remote.count, remote.problem.as_deref()), (2, None));
    assert_eq!(
        remote_rows(remote),
        [(0, SlotState::Held, Some(false)), (1, SlotState::Busy, Some(false))]
    );
    let busy = remote.slots.get(1).expect("remote slot 1 is listed");
    assert_eq!((busy.name.as_str(), busy.processes), ("gate-remote-slot-1", 1));
    assert_eq!(busy.claim, Some(claim));
    for kind in [SlotKind::Heavy, SlotKind::Light] {
        assert!(
            pool(&state, kind)
                .slots
                .iter()
                .all(|slot| slot.state == SlotState::Free && !slot.name.contains("remote")),
            "{kind:?} holds no remote slot"
        );
    }
}

#[test]
fn remote_claim_past_the_count_is_extra() {
    let machine = Machine::new("remote_claim_past_the_count_is_extra");
    machine.write_remote(r#"{"slots": 1}"#);
    machine.write_claim("gate-remote-slot-3.json", &remote_claim(100, 3).to_string());

    let remote = remote_pool(&machine);

    assert_eq!(
        remote_rows(&remote),
        [(0, SlotState::Free, Some(false)), (3, SlotState::Free, Some(true))]
    );
    assert!(
        remote.slots.get(1).is_some_and(|slot| slot.stale.is_some()),
        "the dead gate's claim is stale"
    );
}

#[test]
fn no_remote_json_and_no_remote_claim_is_no_remote_pool() {
    let machine = Machine::new("no_remote_json_and_no_remote_claim_is_no_remote_pool");
    let state = machine.state();
    assert_eq!(state.remote, None);
    let value = serde_json::to_value(&state).expect("the state serialises");
    assert_eq!(value.get("remote"), Some(&Value::Null));
}

#[test]
fn remote_claim_without_remote_json_is_an_extra_slot_with_a_problem() {
    let machine = Machine::new("remote_claim_without_remote_json_is_an_extra_slot_with_a_problem");
    machine.write_claim("gate-remote-slot-0.json", &remote_claim(100, 0).to_string());

    let remote = remote_pool(&machine);

    assert_eq!(remote.count, 0);
    let path = machine.dir.join("gate").join("remote.json");
    assert_eq!(remote.problem, Some(format!("no remote.json at {}", path.display())));
    assert_eq!(remote_rows(&remote), [(0, SlotState::Free, Some(true))]);
}

#[test]
fn remote_json_slots_is_read_in_any_letter_case_up_to_16() {
    let machine = Machine::new("remote_json_slots_is_read_in_any_letter_case_up_to_16");
    machine.write_remote(r#"{"SLOTS": 16.0, "repos": {"x": ["dotnet test *"]}}"#);
    let remote = remote_pool(&machine);
    assert_eq!((remote.count, remote.problem), (16, None));
    assert_eq!(remote.slots.len(), 16);
}

#[test]
fn remote_json_that_cannot_be_used_is_a_problem() {
    let machine = Machine::new("remote_json_that_cannot_be_used_is_a_problem");
    let path = machine.dir.join("gate").join("remote.json");
    let ignored = |reason: &str| format!("gate: ignored {}: {reason}; using no remote slots", path.display());
    for (text, reason) in [
        (r#"{"slots": 0}"#, ignored("slots must be a whole number 1 to 16, got '0'")),
        (r#"{"slots": 17}"#, ignored("slots must be a whole number 1 to 16, got '17'")),
        (r#"{"slots": "2"}"#, ignored("slots must be a whole number 1 to 16, got '2'")),
        (r#"{"slots": 2.5}"#, ignored("slots must be a whole number 1 to 16, got '2.5'")),
        (r#"{"slots": null}"#, ignored("slots must be a whole number 1 to 16, got null")),
        (r#"{"repos": {}}"#, ignored("it has no slots")),
        ("[2]", ignored("it is not a JSON object")),
    ] {
        machine.write_remote(text);
        let remote = remote_pool(&machine);
        assert_eq!((remote.count, remote.problem.as_deref()), (0, Some(reason.as_str())), "{text}");
        assert!(remote.slots.is_empty(), "{text}");
    }
    machine.write_remote("{slots: 2}");
    let problem = remote_pool(&machine).problem.expect("malformed JSON is a problem");
    assert!(
        problem.starts_with(&format!("gate: ignored {}: it is not valid JSON: ", path.display())),
        "{problem}"
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

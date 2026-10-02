//! The files `gate.ps1` writes, captured from a real run (`fixtures/README.md`), read the way the Gates tab reads them.

use std::fs;
use std::path::Path;

use gate_state::{SlotCount, SlotKind, SlotState, dashboard_state};
use serde_json::Value;

use crate::support::{Machine, pool, ts};

/// A heavy slot's claim file, captured while its gate ran `-Log .tmp/o28-capture/first.log`.
const CLAIM: &str = include_str!("fixtures/claim.json");
/// The arrival ticket of a heavy gate waiting for the only heavy slot.
const TICKET: &str = include_str!("fixtures/ticket.json");
/// The counts file the capture ran under.
const SLOT_COUNTS: &str = include_str!("fixtures/slot-counts.json");
/// The self-test prefix the capture ran under, in front of the claim's name and the waiting folder's.
const PREFIX: &str = "ops28-";
/// The neutral working directory the fixtures name in place of the capture's, JSON-escaped as the files have it.
const NEUTRAL_CWD: &str = r"C:\\work\\repo";
/// The waiting gate's command line.
const WAITING_GATE: &str =
    r"pwsh -NoProfile -File C:\Users\me\.claude\tools\gate\gate.ps1 -Log .tmp/o28-capture/second.log -TimeoutSeconds 120 -Slot heavy -- pwsh";

fn count(value: u32) -> SlotCount {
    SlotCount::new(value).expect("a test count is 1 to 64")
}

/// A string field of a JSON object.
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).expect("the field is a string")
}

#[test]
fn captured_claim_is_read_as_a_busy_slot_with_its_log() {
    let mut machine = Machine::new("captured_claim_is_read_as_a_busy_slot_with_its_log");
    machine.prefix = PREFIX.to_owned();
    let sandbox = machine.dir.to_str().expect("the test folder is text").to_owned();
    let escaped = serde_json::to_string(&sandbox).expect("a path serialises");
    let claim_text = CLAIM.replace(NEUTRAL_CWD, escaped.trim_matches('"'));
    let fixture: Value = serde_json::from_str(&claim_text).expect("the fixture is JSON");
    let log = text(&fixture, "log").to_owned();
    assert!(
        log.starts_with(text(&fixture, "cwd")),
        "the gate writes a relative -Log resolved against its cwd: {log}"
    );
    let log_dir = Path::new(&log).parent().expect("the log has a folder");
    fs::create_dir_all(log_dir).expect("the log folder can be made");
    fs::write(&log, "building\nfinished\n").expect("the log can be written");
    machine.write_claim("ops28-gate-slot-0.json", &claim_text);
    machine.add_process(51000, 1, "pwsh.exe", None, Some("2026-10-02T21:16:00.5711853Z"));

    let state = machine.state();
    let heavy = pool(&state, SlotKind::Heavy);
    let row = heavy.slots.iter().find(|slot| slot.index == 0).expect("the heavy pool has slot 0");

    assert_eq!(row.state, SlotState::Busy, "the claim's pid and processStart match the live gate");
    assert_eq!(row.stale, None);
    let claim = row.claim.as_ref().expect("a busy slot shows its claim");
    assert_eq!(claim.get("pid").and_then(Value::as_u64), Some(51000));
    assert_eq!(text(claim, "kind"), "heavy");
    assert_eq!(claim.get("index").and_then(Value::as_u64), Some(0));
    assert_eq!(text(claim, "processStart"), "2026-10-02T21:16:00.5711853Z");
    assert_eq!(text(claim, "started"), "2026-10-02T21:16:02.0157648Z");
    assert_eq!(text(claim, "command"), "pwsh -NoProfile -Command Start-Sleep 20");
    assert_eq!(text(claim, "log"), log);
    assert_eq!(claim.get("timeoutSeconds").and_then(Value::as_f64), Some(120.0));
    assert_eq!(row.last_line.as_deref(), Some("finished"), "the slot reads the log the claim names");
}

#[test]
fn captured_ticket_is_read_into_the_queue() {
    let mut machine = Machine::new("captured_ticket_is_read_into_the_queue");
    machine.prefix = PREFIX.to_owned();
    machine.write_ticket("ops28-waiting", "60080-639265725620427420.json", TICKET);
    machine.add_process(60080, 1, "pwsh.exe", Some(WAITING_GATE), Some("2026-10-02T21:16:02.0427420Z"));
    let mut inputs = machine.inputs();
    inputs.now = ts("2026-10-02T21:16:07.4458960Z");

    let state = dashboard_state(&inputs, &machine, &machine, &machine);

    assert_eq!(state.queue.len(), 1, "the ticket's gate is live");
    let ticket = state.queue.first().expect("one ticket");
    assert_eq!(ticket.kind, "heavy");
    assert_eq!(ticket.pid, 60080);
    assert!(!ticket.holds);
    assert_eq!(ticket.arrived, "2026-10-02T21:16:02.4458960Z");
    assert!((ticket.age_seconds - 5.0).abs() < f64::EPSILON, "age {}", ticket.age_seconds);
    assert_eq!(ticket.command_line.as_deref(), Some(WAITING_GATE));
}

#[test]
fn captured_slot_counts_set_both_pools() {
    let machine = Machine::new("captured_slot_counts_set_both_pools");
    machine.write_counts(SLOT_COUNTS);

    let state = machine.state();

    let heavy = pool(&state, SlotKind::Heavy);
    assert_eq!((heavy.count, heavy.default_count, heavy.set), (1, 3, Some(count(1))));
    assert_eq!(heavy.problem, None);
    assert_eq!(heavy.slots.len(), 1);
    let light = pool(&state, SlotKind::Light);
    assert_eq!((light.count, light.default_count, light.set), (2, 7, Some(count(2))));
    assert_eq!(light.problem, None);
    assert_eq!(light.slots.len(), 2);
}

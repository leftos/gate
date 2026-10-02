//! The critical request: its holder row, the slots it holds and the gates waiting for it.

use gate_state::{DashboardState, SlotKind, SlotState};

use crate::support::{CRITICAL_GATE, LIGHT_GATE, Machine, START, claim, pool};

fn light_critical(state: &DashboardState) -> Vec<(u32, bool)> {
    pool(state, SlotKind::Light)
        .slots
        .iter()
        .map(|slot| (slot.index, slot.critical))
        .collect()
}

fn critical_waiting(state: &DashboardState) -> Vec<u32> {
    state.critical.waiting.iter().map(|waiting| waiting.pid).collect()
}

#[test]
fn idle_slot_rows_are_not_critical() {
    let machine = Machine::new("idle_slot_rows_are_not_critical");
    let state = machine.state();

    for pool in &state.pools {
        for slot in &pool.slots {
            assert!(!slot.critical, "{}", slot.name);
        }
    }
    let holder = &state.critical.holder;
    assert_eq!((holder.index, holder.name.as_str(), holder.state), (0, "gate-critical", SlotState::Free));
    assert!(!holder.critical);
    assert_eq!(holder.extra, None, "the critical row has no extra key");
    assert!(state.critical.waiting.is_empty());
}

#[test]
fn critical_claim_marks_its_slot() {
    let mut machine = Machine::new("critical_claim_marks_its_slot");
    let critical = claim(700, None, "C:\\work");
    machine.write_claim("gate-critical.json", &critical.to_string());
    machine.add_process(700, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.write_claim("gate-light-slot-0.json", &claim(600, None, "C:\\work").to_string());
    machine.add_process(600, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.hold_light_slots(1);
    let state = machine.state();

    assert_eq!(state.critical.holder.state, SlotState::Busy);
    assert_eq!(state.critical.holder.claim, Some(critical));
    assert!(!state.critical.holder.critical);
    let expected: Vec<(u32, bool)> = (0..7).map(|index| (index, index > 0)).collect();
    assert_eq!(
        light_critical(&state),
        expected,
        "the held slots are the critical gate's; the busy one is not"
    );
    assert!(
        pool(&state, SlotKind::Heavy).slots.iter().all(|slot| !slot.critical),
        "free slots are not critical"
    );
}

#[test]
fn open_critical_mutex_marks_held_slots() {
    let mut machine = Machine::new("open_critical_mutex_marks_held_slots");
    machine.open.insert("gate-critical".to_owned());
    machine.hold_light_slots(5);
    let state = machine.state();

    assert_eq!(state.critical.holder.state, SlotState::Held);
    let expected: Vec<(u32, bool)> = (0..7).map(|index| (index, index >= 5)).collect();
    assert_eq!(light_critical(&state), expected);
}

#[test]
fn held_slots_are_not_critical_without_critical_holder() {
    let mut machine = Machine::new("held_slots_are_not_critical_without_critical_holder");
    machine.hold_light_slots(0);
    let state = machine.state();

    assert_eq!(state.critical.holder.state, SlotState::Free);
    assert!(light_critical(&state).iter().all(|(_, critical)| !critical));
}

#[test]
fn critical_waiting_lists_critical_gates_when_pools_free() {
    let mut machine = Machine::new("critical_waiting_lists_critical_gates_when_pools_free");
    machine.add_process(701, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.cwds.insert(701, "C:\\work".to_owned());
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    let state = machine.state();

    assert_eq!(critical_waiting(&state), [701]);
    let waiting = state.critical.waiting.first().expect("one critical gate waits");
    assert_eq!(waiting.cwd.as_deref(), Some("C:\\work"));
    assert_eq!(waiting.command_line, CRITICAL_GATE);
    assert!(pool(&state, SlotKind::Light).waiting.is_empty(), "nothing bars the light pool");
}

#[test]
fn critical_holder_is_not_waiting() {
    let mut machine = Machine::new("critical_holder_is_not_waiting");
    machine.write_claim("gate-critical.json", &claim(700, None, "C:\\work").to_string());
    machine.add_process(700, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.add_process(701, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    let state = machine.state();

    assert_eq!(state.critical.holder.state, SlotState::Busy);
    assert_eq!(critical_waiting(&state), [701]);
}

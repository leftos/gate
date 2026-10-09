//! The gate's test prefix (`GATE_TEST_SLOT_PREFIX`) in front of the slot names, the claims read, the critical mutex and
//! the ticket folder.

use gate_state::{SlotKind, SlotState};

use crate::support::{LIGHT_GATE, Machine, NOW, START, claim, pool, ticket, ticks};

fn prefixed(test: &str) -> Machine {
    with_prefix(test, "t1-")
}

fn with_prefix(test: &str, prefix: &str) -> Machine {
    Machine {
        prefix: prefix.to_owned(),
        ..Machine::new(test)
    }
}

#[test]
fn slot_prefix_names_slots() {
    let mut machine = prefixed("slot_prefix_names_slots");
    machine.open.insert("t1-gate-slot-1".to_owned());
    machine.open.insert("gate-slot-2".to_owned());
    let state = machine.state();

    for kind in SlotKind::ALL {
        for slot in &pool(&state, kind).slots {
            assert_eq!(slot.name, format!("t1-{}{}", kind.prefix(), slot.index));
        }
    }
    let heavy: Vec<SlotState> = pool(&state, SlotKind::Heavy).slots.iter().map(|slot| slot.state).collect();
    assert_eq!(
        heavy,
        [SlotState::Free, SlotState::Held, SlotState::Free],
        "only the prefixed mutex is asked"
    );
}

#[test]
fn slot_prefix_reads_only_prefixed_claims() {
    let machine = prefixed("slot_prefix_reads_only_prefixed_claims");
    machine.write_claim("gate-slot-0.json", &claim(100, None, "C:\\work").to_string());
    machine.write_claim("gate-slot-12.json", &claim(100, None, "C:\\work").to_string());
    machine.write_claim("t1-gate-slot-1.json", &claim(101, None, "C:\\work").to_string());
    machine.write_claim("t1-gate-slot-9.json", &claim(102, None, "C:\\work").to_string());
    let state = machine.state();

    let heavy: Vec<(u32, bool)> = pool(&state, SlotKind::Heavy)
        .slots
        .iter()
        .map(|slot| (slot.index, slot.stale.is_some()))
        .collect();
    assert_eq!(heavy, [(0, false), (1, true), (2, false), (9, true)]);
}

#[test]
fn slot_prefix_matches_claims_in_any_letter_case() {
    let machine = with_prefix("slot_prefix_matches_claims_in_any_letter_case", "T1-");
    machine.write_claim("t1-GATE-slot-1.json", &claim(101, None, "C:\\work").to_string());
    let state = machine.state();

    let slot = pool(&state, SlotKind::Heavy).slots.get(1).expect("heavy slot 1 is listed");
    assert_eq!(slot.name, "T1-gate-slot-1");
    assert!(slot.stale.is_some());
}

#[test]
fn slot_prefix_names_critical_mutex() {
    let mut machine = prefixed("slot_prefix_names_critical_mutex");
    machine.open.insert("gate-critical".to_owned());
    assert_eq!(
        machine.state().critical.holder.state,
        SlotState::Free,
        "the unprefixed mutex is not asked"
    );

    machine.open.insert("t1-gate-critical".to_owned());
    let state = machine.state();
    assert_eq!(state.critical.holder.name, "t1-gate-critical");
    assert_eq!(state.critical.holder.state, SlotState::Held);
}

#[test]
fn slot_prefix_names_critical_claim() {
    let mut machine = prefixed("slot_prefix_names_critical_claim");
    machine.add_process(700, 1, "pwsh.exe", None, Some(START));
    machine.write_claim("gate-critical.json", &claim(700, None, "C:\\work").to_string());
    assert_eq!(machine.state().critical.holder.state, SlotState::Free, "the unprefixed claim is not read");

    machine.write_claim("t1-gate-critical.json", &claim(700, None, "C:\\work").to_string());
    assert_eq!(machine.state().critical.holder.state, SlotState::Busy);
}

#[test]
fn slot_prefix_names_ticket_folder() {
    let mut machine = prefixed("slot_prefix_names_ticket_folder");
    for pid in [301, 302] {
        machine.add_process(pid, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    }
    machine.write_ticket("waiting", "301.json", &ticket("light", 301, ticks(NOW), false).to_string());
    machine.write_ticket("t1-waiting", "302.json", &ticket("light", 302, ticks(NOW), false).to_string());
    let pids: Vec<u32> = machine.state().queue.iter().map(|ticket| ticket.pid).collect();
    assert_eq!(pids, [302]);
}

//! The gates listed as waiting for a full pool.

use gate_state::{SlotKind, SlotState, Waiting};

use crate::support::{LIGHT_GATE, Machine, START, claim, pool};

fn light_waiting(machine: &Machine) -> Vec<u32> {
    let state = machine.state();
    pool(&state, SlotKind::Light).waiting.iter().map(|waiting| waiting.pid).collect()
}

#[test]
fn waiting_listed_only_when_pool_full() {
    let mut machine = Machine::new("waiting_listed_only_when_pool_full");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.cwds.insert(300, "C:\\work".to_owned());

    assert!(light_waiting(&machine).is_empty(), "every light slot is free");
    machine.hold_light_slots(1);
    assert!(light_waiting(&machine).is_empty(), "light slot 0 is still free");

    machine.hold_light_slots(0);
    let state = machine.state();
    let light = pool(&state, SlotKind::Light);
    assert_eq!(light.waiting.len(), 1);
    let waiting = light.waiting.first().expect("one gate waits");
    assert_eq!(waiting.pid, 300);
    assert_eq!(waiting.since.as_deref(), Some("2026-10-01T11:00:00.5000000Z"));
    assert_eq!(waiting.cwd.as_deref(), Some("C:\\work"));
    assert_eq!(waiting.command_line, LIGHT_GATE);
    assert_eq!(pool(&state, SlotKind::Heavy).waiting, Vec::<Waiting>::new());
}

#[test]
fn waiting_excludes_holders_and_processes_with_real_children() {
    let mut machine = Machine::new("waiting_excludes_holders_and_processes_with_real_children");
    machine.write_claim("gate-light-slot-0.json", &claim(400, None, "C:\\work").to_string());
    machine.add_process(400, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.hold_light_slots(1);
    machine.add_process(401, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(402, 401, "conhost.exe", None, None);
    machine.add_process(403, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(404, 403, "cargo.exe", None, None);

    assert_eq!(light_waiting(&machine), [401]);
}

#[test]
fn waiting_ignores_extra_slots_when_deciding_full() {
    let mut machine = Machine::new("waiting_ignores_extra_slots_when_deciding_full");
    machine.hold_light_slots(0);
    machine.write_claim("gate-light-slot-9.json", &claim(900, None, "C:\\work").to_string());
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    let state = machine.state();

    let light = pool(&state, SlotKind::Light);
    let extra = light.slots.iter().find(|slot| slot.index == 9).expect("the extra slot is listed");
    assert_eq!(extra.extra, Some(true));
    assert_eq!(extra.state, SlotState::Free);
    let waiting: Vec<u32> = light.waiting.iter().map(|waiting| waiting.pid).collect();
    assert_eq!(waiting, [300], "a free extra slot does not stop the pool being full");
}

#[test]
fn waiting_kind_match_is_case_insensitive_and_word_bounded() {
    let mut machine = Machine::new("waiting_kind_match_is_case_insensitive_and_word_bounded");
    machine.hold_light_slots(0);
    machine.add_process(
        500,
        1,
        "pwsh.exe",
        Some(r"pwsh C:\tools\GATE.ps1 -Log y -slot LIGHT -- cargo build"),
        Some(START),
    );
    machine.add_process(
        501,
        1,
        "pwsh.exe",
        Some(r"pwsh C:\tools\gate.ps1 -Log y -Slot lightweight -- cargo build"),
        Some(START),
    );
    machine.add_process(
        502,
        1,
        "pwsh.exe",
        Some(r"pwsh C:\tools\gate.ps1 -Log y -Slot heavy -- cargo build"),
        Some(START),
    );

    assert_eq!(light_waiting(&machine), [500]);
}

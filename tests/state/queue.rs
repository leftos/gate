//! The arrival tickets of the gates waiting for a slot or for the machine, and the pools a critical ticket bars.

use gate_state::{DashboardState, QueueTicket, SlotKind};
use serde_json::{Value, json};

use crate::support::{CRITICAL_GATE, LIGHT_GATE, Machine, NOW, START, START_CLOSE, START_FAR, ticket, ticks};

const SECOND: i64 = 10_000_000;

fn queue_pids(state: &DashboardState) -> Vec<u32> {
    state.queue.iter().map(|ticket| ticket.pid).collect()
}

fn light_waiting(state: &DashboardState) -> Vec<u32> {
    crate::support::pool(state, SlotKind::Light)
        .waiting
        .iter()
        .map(|waiting| waiting.pid)
        .collect()
}

#[test]
fn queue_is_empty_when_no_tickets() {
    let machine = Machine::new("queue_is_empty_when_no_tickets");
    assert!(machine.state().queue.is_empty(), "no ticket folder");
    std::fs::create_dir_all(machine.dir.join("gate").join("waiting")).expect("the ticket folder can be made");
    assert!(machine.state().queue.is_empty(), "an empty ticket folder");
}

#[test]
fn queue_lists_waiting_tickets() {
    let mut machine = Machine::new("queue_lists_waiting_tickets");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.cwds.insert(300, "C:\\work".to_owned());
    machine.write_ticket("waiting", "300.json", &ticket("light", 300, ticks(NOW) - 5 * SECOND, false).to_string());
    let state = machine.state();

    let queue = serde_json::to_value(&state.queue).expect("the queue serialises");
    let expected = json!([{
        "kind": "light",
        "holds": false,
        "pid": 300,
        "arrived": "2026-10-01T11:59:55.1234567Z",
        "ageSeconds": 5.0,
        "cwd": "C:\\work",
        "commandLine": LIGHT_GATE,
    }]);
    assert_eq!(queue, expected);
}

#[test]
fn queue_is_sorted_oldest_first() {
    let mut machine = Machine::new("queue_is_sorted_oldest_first");
    for (pid, age) in [(301, 1), (302, 30), (303, 7)] {
        machine.add_process(pid, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
        machine.write_ticket(
            "waiting",
            &format!("{pid}.json"),
            &ticket("light", pid, ticks(NOW) - age * SECOND, false).to_string(),
        );
    }
    assert_eq!(queue_pids(&machine.state()), [302, 303, 301]);
}

#[test]
fn queue_skips_tickets_of_ended_gates() {
    let mut machine = Machine::new("queue_skips_tickets_of_ended_gates");
    machine.add_process(301, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START_CLOSE));
    machine.add_process(302, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START_FAR));
    machine.add_process(303, 1, "pwsh.exe", Some(LIGHT_GATE), None);
    for pid in [301, 302, 303, 304] {
        machine.write_ticket("waiting", &format!("{pid}.json"), &ticket("light", pid, ticks(NOW), false).to_string());
    }
    assert_eq!(
        queue_pids(&machine.state()),
        [301],
        "only the gate whose start is within 2 s of the ticket's is live"
    );
}

#[test]
fn queue_skips_unreadable_tickets() {
    let mut machine = Machine::new("queue_skips_unreadable_tickets");
    machine.add_process(301, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    let good = ticket("light", 301, ticks(NOW), false);
    machine.write_ticket("waiting", "good.json", &good.to_string());
    machine.write_ticket("waiting", "text.txt", &good.to_string());
    machine.write_ticket("waiting", "broken.json", "{not json");
    let mut missing = good.clone();
    missing.as_object_mut().expect("a ticket is an object").remove("holds");
    machine.write_ticket("waiting", "missing.json", &missing.to_string());
    let mut out_of_range = good;
    out_of_range["arrived"] = json!(-1);
    machine.write_ticket("waiting", "range.json", &out_of_range.to_string());

    assert_eq!(queue_pids(&machine.state()), [301]);
}

#[test]
fn queue_age_rounds_half_to_even() {
    let mut machine = Machine::new("queue_age_rounds_half_to_even");
    for (pid, tenths) in [(301, 35), (302, 25)] {
        machine.add_process(pid, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
        let arrived = ticks(NOW) - tenths * SECOND / 10;
        machine.write_ticket("waiting", &format!("{pid}.json"), &ticket("light", pid, arrived, false).to_string());
    }
    let queue = serde_json::to_value(machine.state().queue).expect("the queue serialises");
    let ages: Vec<Value> = queue
        .as_array()
        .expect("the queue is an array")
        .iter()
        .filter_map(|ticket| ticket.get("ageSeconds").cloned())
        .collect();
    assert_eq!(ages, [json!(4.0), json!(2.0)]);
}

#[test]
fn barred_pool_lists_waiting_when_not_full() {
    let mut machine = Machine::new("barred_pool_lists_waiting_when_not_full");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(700, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.write_ticket("waiting", "700.json", &ticket("critical", 700, ticks(NOW), false).to_string());
    let state = machine.state();

    assert!(
        crate::support::pool(&state, SlotKind::Light)
            .slots
            .iter()
            .all(|slot| slot.state == gate_state::SlotState::Free)
    );
    assert_eq!(light_waiting(&state), [300]);
}

#[test]
fn unbarred_pool_with_free_slot_lists_no_waiting() {
    let mut machine = Machine::new("unbarred_pool_with_free_slot_lists_no_waiting");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.write_ticket("waiting", "300.json", &ticket("light", 300, ticks(NOW), false).to_string());
    let state = machine.state();

    assert_eq!(queue_pids(&state), [300]);
    assert_eq!(light_waiting(&state), Vec::<u32>::new());
}

#[test]
fn critical_ticket_bars_in_any_letter_case() {
    let mut machine = Machine::new("critical_ticket_bars_in_any_letter_case");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(700, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.write_ticket("waiting", "700.json", &ticket("Critical", 700, ticks(NOW), false).to_string());
    assert_eq!(light_waiting(&machine.state()), [300]);
}

#[test]
fn ended_critical_ticket_does_not_bar() {
    let mut machine = Machine::new("ended_critical_ticket_does_not_bar");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.write_ticket("waiting", "700.json", &ticket("critical", 700, ticks(NOW), false).to_string());
    let state = machine.state();

    assert_eq!(state.queue, Vec::<QueueTicket>::new());
    assert_eq!(light_waiting(&state), Vec::<u32>::new());
}

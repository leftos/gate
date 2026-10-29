//! The arrival tickets of the gates waiting for a slot or for the machine, and the pools a critical ticket bars.

use gate_state::{DashboardState, QueueTicket, SlotKind, v1};
use serde_json::{Value, json};

use crate::support::{CRITICAL_GATE, HEAVY_GATE, LIGHT_GATE, Machine, NOW, START, START_CLOSE, START_FAR, ms, remote_ticket, ticket, ticks};

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
fn queue_row_carries_the_ticket_remote_flag() {
    let mut machine = Machine::new("queue_row_carries_the_ticket_remote_flag");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(301, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.write_ticket("waiting", "300.json", &ticket("light", 300, ticks(NOW) - 2 * SECOND, false).to_string());
    machine.write_ticket("waiting", "301.json", &remote_ticket(301, ticks(NOW) - SECOND).to_string());
    let state = machine.state();

    let flags: Vec<(u32, bool)> = state.queue.iter().map(|ticket| (ticket.pid, ticket.remote)).collect();
    assert_eq!(flags, [(300, false), (301, true)], "a ticket without remote is local");
    let queue = serde_json::to_value(&state.queue).expect("the queue serialises");
    let remote: Vec<Option<&Value>> = queue
        .as_array()
        .expect("the queue is an array")
        .iter()
        .map(|ticket| ticket.get("remote"))
        .collect();
    assert_eq!(remote, [None, Some(&json!(true))], "remote is written only when true");
}

#[test]
fn a_remote_ticket_shows_its_pool_and_pinned_in_the_queue() {
    let mut machine = Machine::new("a_remote_ticket_shows_its_pool_and_pinned_in_the_queue");
    machine.add_process(300, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.add_process(301, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.write_ticket("waiting", "300.json", &ticket("light", 300, ticks(NOW), false).to_string());
    let mut pooled = remote_ticket(301, ticks(NOW));
    pooled["pool"] = json!("1f0efc62");
    pooled["pinned"] = json!(true);
    machine.write_ticket("waiting", "301.json", &pooled.to_string());
    let state = machine.state();

    let remote = state.queue.iter().find(|ticket| ticket.pid == 301).expect("the routed gate's ticket");
    assert_eq!(
        (remote.pool.as_deref(), remote.pinned),
        (Some("1f0efc62"), true),
        "a ticket of a builders pool's queue says which pool and that it is pinned"
    );
    let local = state.queue.iter().find(|ticket| ticket.pid == 300).expect("the local gate's ticket");
    assert_eq!((local.pool.as_deref(), local.pinned), (None, false), "a local ticket has neither");

    let queue = serde_json::to_value(&state.queue).expect("the queue serialises");
    let keys: Vec<Vec<&str>> = queue
        .as_array()
        .expect("the queue is an array")
        .iter()
        .map(crate::support::keys)
        .collect();
    assert_eq!(keys[0], ["kind", "holds", "pid", "arrived", "ageSeconds", "cwd", "commandLine"]);
    assert_eq!(
        keys[1],
        [
            "kind",
            "holds",
            "remote",
            "pool",
            "pinned",
            "pid",
            "arrived",
            "ageSeconds",
            "cwd",
            "commandLine"
        ],
        "pool and pinned follow remote, each written only when it has something to say"
    );
}

#[test]
fn a_v1_remote_ticket_shows_its_pool_and_pinned_in_the_queue() {
    let mut machine = Machine::v1("a_v1_remote_ticket_shows_its_pool_and_pinned_in_the_queue");
    machine.add_process(400, 1, "gate.exe", Some(HEAVY_GATE), Some(START));
    let ticket = v1::Ticket {
        kind: v1::TicketKind::Heavy,
        arrived: ms(NOW),
        pid: 400,
        start_ms: ms(START),
        holds: false,
        lends_closed: false,
        alone: false,
        remote: Some(true),
        pool: Some("1f0efc62".to_owned()),
        also_pools: Vec::new(),
        pinned: Some(true),
        reason: None,
        expected: None,
    };
    v1::write_ticket(&v1::ticket_path(&machine.v1_root(), 400, ms(START)), &ticket).expect("the ticket can be written");

    let state = machine.state();

    let shown = state.queue.first().expect("the ticket's gate is live");
    assert_eq!(
        (shown.pool.as_deref(), shown.pinned),
        (Some("1f0efc62"), true),
        "a gate.exe ticket of a builders pool says which pool and that it is pinned"
    );
}

#[test]
fn queue_row_reads_a_remote_that_is_not_a_boolean_as_local() {
    let mut machine = Machine::new("queue_row_reads_a_remote_that_is_not_a_boolean_as_local");
    machine.add_process(301, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    let mut text = remote_ticket(301, ticks(NOW));
    text["remote"] = json!("true");
    machine.write_ticket("waiting", "301.json", &text.to_string());
    let state = machine.state();

    let flags: Vec<(u32, bool)> = state.queue.iter().map(|ticket| (ticket.pid, ticket.remote)).collect();
    assert_eq!(flags, [(301, false)]);
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

//! The serialised state's shape: the keys `GET /api/gate/slots` serves, in their order, at every level.

use gate_state::SlotKind;
use serde_json::Value;

use crate::support::{CRITICAL_GATE, HEAVY_GATE, LIGHT_GATE, Machine, NOW, START, claim, keys, remote_claim, remote_ticket, ticket, ticks};

const TOP: [&str; 7] = ["now", "machine", "processors", "critical", "queue", "pools", "remote"];
const POOL: [&str; 7] = ["kind", "count", "default", "set", "problem", "slots", "waiting"];
const REMOTE: [&str; 3] = ["count", "problem", "slots"];
/// A remote ticket's keys: a ticket's, with `remote` after `holds`.
const QUEUE_REMOTE: [&str; 8] = ["kind", "holds", "remote", "pid", "arrived", "ageSeconds", "cwd", "commandLine"];
const SLOT: [&str; 9] = ["index", "name", "state", "claim", "stale", "processes", "lastLine", "critical", "extra"];
const CRITICAL: [&str; 2] = ["holder", "waiting"];
const HOLDER: [&str; 8] = ["index", "name", "state", "claim", "stale", "processes", "lastLine", "critical"];
const QUEUE: [&str; 7] = ["kind", "holds", "pid", "arrived", "ageSeconds", "cwd", "commandLine"];
const WAITING: [&str; 4] = ["pid", "since", "cwd", "commandLine"];
const CLAIM: [&str; 9] = [
    "kind",
    "index",
    "pid",
    "processStart",
    "started",
    "cwd",
    "command",
    "log",
    "timeoutSeconds",
];

fn array(value: &Value, key: &str) -> Vec<Value> {
    value.get(key).and_then(Value::as_array).expect("the key holds an array").clone()
}

fn assert_shape(value: &Value) {
    assert_eq!(keys(value), TOP);
    let critical = value.get("critical").expect("the state has a critical row");
    assert_eq!(keys(critical), CRITICAL);
    assert_eq!(keys(critical.get("holder").expect("the critical row has a holder")), HOLDER);
    for waiting in array(critical, "waiting") {
        assert_eq!(keys(&waiting), WAITING);
    }
    for ticket in array(value, "queue") {
        let expected: &[&str] = if ticket.get("remote").is_some() { &QUEUE_REMOTE } else { &QUEUE };
        assert_eq!(keys(&ticket), expected);
    }
    for pool in array(value, "pools") {
        assert_eq!(keys(&pool), POOL);
        assert_rows(&pool);
    }
    let remote = value.get("remote").expect("the state has a remote key");
    if !remote.is_null() {
        assert_eq!(keys(remote), REMOTE);
        for slot in array(remote, "slots") {
            assert_eq!(keys(&slot), SLOT);
        }
    }
}

/// The keys of a pool's slots and waiting gates.
fn assert_rows(pool: &Value) {
    for slot in array(pool, "slots") {
        assert_eq!(keys(&slot), SLOT);
    }
    for waiting in array(pool, "waiting") {
        assert_eq!(keys(&waiting), WAITING);
    }
}

#[test]
fn remote_pool_and_ticket_keys_follow_dashboard_order() {
    let mut machine = Machine::new("remote_pool_and_ticket_keys_follow_dashboard_order");
    machine.write_remote(r#"{"slots": 2}"#);
    machine.write_claim("gate-remote-slot-0.json", &remote_claim(800, 0).to_string());
    machine.add_process(800, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.add_process(801, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.add_process(802, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.write_ticket("waiting", "801.json", &remote_ticket(801, ticks(NOW)).to_string());
    machine.write_ticket("waiting", "802.json", &ticket("light", 802, ticks(NOW), false).to_string());
    let state = serde_json::to_value(machine.state()).expect("the state serialises");

    assert_shape(&state);
    let remote = state.get("remote").expect("the state has a remote pool");

    let slots = array(remote, "slots");
    let busy = slots.first().expect("remote slot 0 is listed");
    let mut claim_keys = CLAIM.to_vec();
    claim_keys.extend(["remote", "host", "key"]);
    assert_eq!(keys(busy.get("claim").expect("a busy slot has its claim")), claim_keys);
    assert_eq!(array(&state, "queue").len(), 2);
}

#[test]
fn serialised_key_order_matches_script() {
    let mut machine = Machine::new("serialised_key_order_matches_script");
    let idle = serde_json::to_value(machine.state()).expect("the idle state serialises");
    assert_shape(&idle);
    let kinds: Vec<Value> = array(&idle, "pools").iter().filter_map(|pool| pool.get("kind").cloned()).collect();
    assert_eq!(kinds, ["heavy", "light"]);

    machine.write_claim("gate-light-slot-0.json", &claim(600, None, "C:\\work").to_string());
    machine.add_process(600, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.hold_light_slots(1);
    machine.add_process(601, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    let busy = serde_json::to_value(machine.state()).expect("the busy state serialises");
    assert_shape(&busy);

    let light = array(&busy, "pools")
        .into_iter()
        .find(|pool| pool.get("kind").and_then(Value::as_str) == Some(SlotKind::Light.as_str()));
    let light = light.expect("the light pool is listed");
    assert_eq!(array(&light, "waiting").len(), 1);
    let slots = array(&light, "slots");
    let first = slots.first().expect("light slot 0 is listed");
    assert_eq!(first.get("state").and_then(Value::as_str), Some("busy"));
    assert_eq!(keys(first.get("claim").expect("a busy slot has its claim")), CLAIM);
    let second = slots.get(1).expect("light slot 1 is listed");
    assert_eq!(second.get("state").and_then(Value::as_str), Some("held"));
}

#[test]
fn state_keys_follow_dashboard_order() {
    let machine = Machine::new("state_keys_follow_dashboard_order");
    let idle = serde_json::to_value(machine.state()).expect("the idle state serialises");
    assert_eq!(keys(&idle), TOP);
}

#[test]
fn critical_and_queue_keys_follow_dashboard_order() {
    let mut machine = Machine::new("critical_and_queue_keys_follow_dashboard_order");
    machine.write_claim("gate-critical.json", &claim(700, None, "C:\\work").to_string());
    machine.add_process(700, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.add_process(701, 1, "pwsh.exe", Some(CRITICAL_GATE), Some(START));
    machine.write_ticket("waiting", "701.json", &ticket("critical", 701, ticks(NOW), false).to_string());
    let state = serde_json::to_value(machine.state()).expect("the state serialises");

    assert_shape(&state);
    let critical = state.get("critical").expect("the state has a critical row");
    assert_eq!(array(critical, "waiting").len(), 1);
    assert_eq!(array(&state, "queue").len(), 1);
}

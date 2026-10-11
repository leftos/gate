//! The PowerShell gates still on the machine, as the cutover's drain reads them from the PowerShell gate's claims and
//! tickets under `gate\slots` and `gate\waiting`.

use std::fs;

use gate_state::{PowerShellGate, PowerShellGates, PowerShellHold, PowerShellSlot, powershell_gates};
use jiff::Timestamp;
use serde_json::json;

use crate::support::{Machine, START, START_CLOSE, START_FAR, claim, ticket, ticks, ts};

const CLAIM: &str = include_str!("fixtures/claim.json");
const REMOTE_CLAIM: &str = include_str!("fixtures/remote-claim.json");
const TICKET: &str = include_str!("fixtures/ticket.json");
/// The fixture claims' `processStart` and `started`.
const FIXTURE_START: &str = "2026-10-02T21:16:00.5711853Z";
const FIXTURE_STARTED: &str = "2026-10-02T21:16:02.0157648Z";
/// The fixture ticket's `startTicks` and `arrived`.
const TICKET_START_TICKS: i64 = 639_265_725_620_427_420;
const TICKET_ARRIVED_TICKS: i64 = 639_265_725_624_458_960;
/// .NET ticks (100 ns since 0001-01-01) at the Unix epoch.
const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;
/// One tick past the last a .NET `DateTime` holds, 9999-12-31T23:59:59.9999999.
const PAST_MAX_TICKS: i64 = 3_155_378_976_000_000_000;

fn at_ticks(ticks: i64) -> Timestamp {
    Timestamp::from_nanosecond((i128::from(ticks) - UNIX_EPOCH_TICKS) * 100).expect("the test's ticks are a moment")
}

fn gates(machine: &Machine) -> PowerShellGates {
    powershell_gates(&machine.dir, machine)
}

fn pids(found: &PowerShellGates) -> Vec<u32> {
    found.gates.iter().map(|gate| gate.pid).collect()
}

/// A claim of `kind` as the gate writes it, held by `pid` since `started` with `command`.
fn claim_of(kind: &str, pid: u32, started: &str, command: &str) -> String {
    let mut claim = claim(pid, None, r"C:\work");
    claim["kind"] = json!(kind);
    claim["started"] = json!(started);
    claim["command"] = json!(command);
    claim.to_string()
}

#[test]
fn live_claims_and_tickets_are_listed_with_what_they_hold() {
    let mut machine = Machine::new("live_claims_and_tickets_are_listed_with_what_they_hold");
    machine.write_claim("gate-slot-0.json", CLAIM);
    machine.write_claim("gate-remote-slot-1.json", REMOTE_CLAIM);
    machine.write_claim("gate-light-slot-2.json", &claim_of("light", 301, START, "cargo clippy"));
    machine.write_claim("gate-critical.json", &claim_of("critical", 300, START, "cargo test --release"));
    let holding = ticket("critical", 300, ticks("2026-10-01T11:00:01Z"), true);
    machine.write_ticket("waiting", "300-1.json", &holding.to_string());
    machine.write_ticket("waiting", "60080-1.json", TICKET);
    for pid in [51000, 52000] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(FIXTURE_START));
    }
    for pid in [300, 301] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(START));
    }
    machine.add_process(60080, 1, "pwsh.exe", None, Some(&at_ticks(TICKET_START_TICKS).to_string()));

    let found = gates(&machine);

    let gate = |pid: u32, what: PowerShellHold, command: Option<&str>, since: Timestamp| PowerShellGate {
        pid,
        what,
        command: command.map(str::to_owned),
        since,
    };
    let ticket = |kind: &str, holds: bool| PowerShellHold::Ticket {
        kind: kind.to_owned(),
        holds,
    };
    assert_eq!(
        found,
        PowerShellGates {
            gates: vec![
                gate(
                    300,
                    PowerShellHold::Claim(PowerShellSlot::Critical),
                    Some("cargo test --release"),
                    ts(START)
                ),
                gate(301, PowerShellHold::Claim(PowerShellSlot::Light(2)), Some("cargo clippy"), ts(START)),
                gate(300, ticket("critical", true), None, ts("2026-10-01T11:00:01Z")),
                gate(
                    51000,
                    PowerShellHold::Claim(PowerShellSlot::Heavy(0)),
                    Some("pwsh -NoProfile -Command Start-Sleep 20"),
                    ts(FIXTURE_STARTED)
                ),
                gate(
                    52000,
                    PowerShellHold::Claim(PowerShellSlot::Remote(1)),
                    Some("dotnet test Delve.slnx -c Release"),
                    ts(FIXTURE_STARTED)
                ),
                gate(60080, ticket("heavy", false), None, at_ticks(TICKET_ARRIVED_TICKS)),
            ],
            unreadable: 0,
        },
        "claims by their file's slot with their command and start, tickets by kind and holds with their arrival"
    );
}

#[test]
fn stale_and_unreadable_files_are_left_out() {
    let mut machine = Machine::new("stale_and_unreadable_files_are_left_out");
    machine.write_claim("gate-slot-0.json", &claim(100, None, r"C:\work").to_string());
    machine.write_claim("gate-slot-1.json", &claim(101, None, r"C:\work").to_string());
    machine.write_claim("gate-slot-2.json", &claim(102, None, r"C:\work").to_string());
    machine.write_claim("gate-slot-3.json", "not json");
    machine.write_ticket("waiting", "200-1.json", &ticket("heavy", 200, ticks(START), false).to_string());
    machine.write_ticket("waiting", "201-1.json", &ticket("light", 201, ticks(START), false).to_string());
    machine.write_ticket("waiting", "202-1.json", r#"{"kind":"#);
    machine.write_ticket("waiting", "203-1.json", &ticket("light", 203, ticks(START_CLOSE), false).to_string());
    machine.add_process(101, 1, "pwsh.exe", None, Some(START_FAR));
    machine.add_process(102, 1, "pwsh.exe", None, Some(START_CLOSE));
    machine.add_process(201, 1, "pwsh.exe", None, Some(START_FAR));
    machine.add_process(203, 1, "pwsh.exe", None, Some(START_CLOSE));

    let found = gates(&machine);

    assert_eq!(
        pids(&found),
        [102, 203],
        "not a gate whose pid is gone (100, 200) or was reused (101, 201, 3 s off); a start 1.5 s off is the same process"
    );
    assert_eq!(found.unreadable, 2, "the claim and the ticket that are not JSON are counted");
}

#[test]
fn a_ticket_whose_start_is_past_what_ticks_hold_is_unreadable() {
    let mut machine = Machine::new("a_ticket_whose_start_is_past_what_ticks_hold_is_unreadable");
    let mut past = ticket("heavy", 400, ticks(START), false);
    past["startTicks"] = json!(PAST_MAX_TICKS);
    machine.write_ticket("waiting", "400-1.json", &past.to_string());
    machine.add_process(400, 1, "pwsh.exe", None, Some(START));

    assert_eq!(
        gates(&machine),
        PowerShellGates {
            gates: Vec::new(),
            unreadable: 1,
        },
        "a ticket whose startTicks no DateTime holds is counted, not listed"
    );
}

#[test]
fn prefixed_test_files_are_ignored() {
    let mut machine = Machine::new("prefixed_test_files_are_ignored");
    machine.write_claim("priocheck-7-gate-slot-0.json", &claim(100, None, r"C:\work").to_string());
    machine.write_ticket(
        "priocheck-7-waiting",
        "101-1.json",
        &ticket("heavy", 101, ticks(START), false).to_string(),
    );
    machine.add_process(100, 1, "pwsh.exe", None, Some(START));
    machine.add_process(101, 1, "pwsh.exe", None, Some(START));

    assert_eq!(gates(&machine), PowerShellGates::default());
}

#[test]
fn gates_are_sorted_by_since() {
    let mut machine = Machine::new("gates_are_sorted_by_since");
    machine.write_ticket(
        "waiting",
        "500-1.json",
        &ticket("heavy", 500, ticks("2026-10-01T11:00:03Z"), false).to_string(),
    );
    machine.write_ticket(
        "waiting",
        "501-1.json",
        &ticket("light", 501, ticks("2026-10-01T11:00:01Z"), false).to_string(),
    );
    // Taken 2.1 s after its process started: a liveness check against `started` instead of `processStart` drops it.
    machine.write_claim("gate-slot-0.json", &claim_of("heavy", 502, "2026-10-01T11:00:02.6Z", "cargo test"));
    for pid in [500, 501, 502] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(START));
    }

    assert_eq!(pids(&gates(&machine)), [501, 502, 500]);
}

#[test]
fn an_empty_or_missing_folder_has_no_gates() {
    let machine = Machine::new("an_empty_or_missing_folder_has_no_gates");
    fs::create_dir_all(machine.dir.join("gate").join("waiting")).expect("the ticket folder can be made");

    assert_eq!(gates(&machine), PowerShellGates::default(), "empty folders");

    fs::remove_dir_all(machine.dir.join("gate")).expect("the gate folder can be removed");

    assert_eq!(gates(&machine), PowerShellGates::default(), "no gate folder");
}

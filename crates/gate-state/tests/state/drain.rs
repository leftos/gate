//! The PowerShell gates still on the machine, as the cutover's drain reads them from the PowerShell gate's claims and
//! tickets under `gate\slots` and `gate\waiting`, and the routed gates the remote cutover's drain waits on.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use gate_state::{
    KernelObject, MutexProbe, PowerShellGate, PowerShellGates, PowerShellHold, PowerShellSlot, RemoteGate, RemoteGates, StateRoot, StateRootAt,
    powershell_gates, remote_gates,
};
use jiff::Timestamp;
use serde_json::json;

use crate::support::{Machine, START, START_CLOSE, START_FAR, claim, ms, remote_ticket, ticket, ticks, ts};

const CLAIM: &str = include_str!("fixtures/claim.json");
const REMOTE_CLAIM: &str = include_str!("fixtures/remote-claim.json");
const TICKET: &str = include_str!("fixtures/ticket.json");
const V1_REMOTE_CLAIM: &str = include_str!("fixtures/v1-remote-claim.json");
const V1_REMOTE_TICKET: &str = include_str!("fixtures/v1-remote-ticket.json");
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

/// The remote pool's size the routed tests probe.
const REMOTE_COUNT: u32 = 3;

/// The held remote slot `slot` with no live claim: a gate whose pid is not known.
fn unknown(slot: u32) -> RemoteGate {
    RemoteGate {
        pid: None,
        command: None,
        since: None,
        slot: Some(slot),
    }
}

/// The machine's default state root, `gate\v1`, judged against itself, so its kernel objects carry the PowerShell
/// gate's names.
fn default_root(machine: &Machine) -> StateRoot {
    let path = machine.v1_root().to_string_lossy().into_owned();
    StateRoot::new(&path, Some(&path))
}

/// The routed gates [`remote_gates`] finds on this machine: its default v1 root's and its PowerShell root's.
fn routed(machine: &Machine) -> RemoteGates {
    let root = StateRootAt {
        path: &machine.v1_root(),
        root: &default_root(machine),
    };
    remote_gates(root, Some(&machine.dir.join("gate")), REMOTE_COUNT, machine, machine)
}

/// A routed gate whose pid is known.
fn known(pid: u32, command: Option<&str>, since: Timestamp) -> RemoteGate {
    RemoteGate {
        pid: Some(pid),
        command: command.map(str::to_owned),
        since: Some(since),
        slot: None,
    }
}

/// The routed gates of `gates` with the unreadable entries `unreadable`, in the order [`remote_gates`] sorts them.
fn found(gates: Vec<RemoteGate>, mut unreadable: Vec<PathBuf>) -> RemoteGates {
    unreadable.sort();
    RemoteGates { gates, unreadable }
}

#[test]
fn a_live_v1_remote_claim_is_listed() {
    let mut machine = Machine::new("a_live_v1_remote_claim_is_listed");
    machine.write_v1_claim("gate-remote-slot-1.json", V1_REMOTE_CLAIM);
    machine.write_v1_claim("gate-slot-0.json", &claim(51000, None, r"C:\work").to_string());
    for pid in [51000, 53000] {
        machine.add_process(pid, 1, "gate.exe", None, Some(START));
    }

    assert_eq!(
        routed(&machine),
        found(
            vec![known(53000, Some("cargo build --release"), ts("2026-10-01T11:00:03.0000000Z"))],
            Vec::new()
        ),
        "the routed claim with its command and when it took the remote slot; the local heavy claim is not routed work"
    );
}

#[test]
fn a_waiting_remote_ticket_is_listed() {
    let mut machine = Machine::new("a_waiting_remote_ticket_is_listed");
    machine.write_v1_ticket("53100-1790852400500.json", V1_REMOTE_TICKET);
    let local = json!({
        "v": 1,
        "kind": "light",
        "arrived": ms("2026-10-01T11:00:02Z"),
        "pid": 60080,
        "startTicks": ms(START),
        "holds": false,
    });
    machine.write_v1_ticket("60080-1790852400500.json", &local.to_string());
    for pid in [53100, 60080] {
        machine.add_process(pid, 1, "gate.exe", None, Some(START));
    }

    assert_eq!(
        routed(&machine),
        found(vec![known(53100, None, ts("2026-10-01T11:00:02Z"))], Vec::new()),
        "a ticket waiting for the remote pool, which names no command; a live local ticket is not routed work"
    );
}

#[test]
fn a_dead_holders_remote_claim_is_not() {
    let mut machine = Machine::new("a_dead_holders_remote_claim_is_not");
    machine.write_v1_claim("gate-remote-slot-1.json", V1_REMOTE_CLAIM);
    machine.write_v1_claim("gate-remote-slot-2.json", &V1_REMOTE_CLAIM.replacen("53000", "54000", 1));
    machine.add_process(54000, 1, "gate.exe", None, Some(START_FAR));

    assert_eq!(
        routed(&machine),
        RemoteGates::default(),
        "no process runs the claim's pid (53000), and 54000 runs since another start (3 s off)"
    );
}

#[test]
fn a_powershell_remote_claim_is_listed_with_the_v1_ones() {
    let mut machine = Machine::new("a_powershell_remote_claim_is_listed_with_the_v1_ones");
    machine.write_claim("gate-remote-slot-1.json", REMOTE_CLAIM);
    machine.write_claim("gate-slot-0.json", CLAIM);
    machine.write_v1_claim("gate-remote-slot-0.json", V1_REMOTE_CLAIM);
    for pid in [51000, 52000] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(FIXTURE_START));
    }
    machine.add_process(53000, 1, "gate.exe", None, Some(START));

    assert_eq!(
        routed(&machine),
        found(
            vec![
                known(53000, Some("cargo build --release"), ts("2026-10-01T11:00:03.0000000Z")),
                known(52000, Some("dotnet test Delve.slnx -c Release"), ts(FIXTURE_STARTED)),
            ],
            Vec::new()
        ),
        "the v1 root's routed claim and the PowerShell root's, by since; the PowerShell heavy claim is not routed work"
    );
}

#[test]
fn an_open_remote_slot_with_no_claim_is_a_live_gate() {
    let mut machine = Machine::new("an_open_remote_slot_with_no_claim_is_a_live_gate");
    machine.write_v1_claim("gate-remote-slot-2.json", &V1_REMOTE_CLAIM.replacen("53000", "54000", 1));
    machine.add_process(999, 1, "explorer.exe", None, Some(START_FAR));
    for index in [0, 2] {
        machine.open.insert(format!("gate-remote-slot-{index}"));
    }

    assert_eq!(
        routed(&machine),
        found(vec![unknown(0), unknown(2)], Vec::new()),
        "slot 0's gate has deleted its ticket and not written its claim yet; slot 2's claim names a gone pid that started \
         before the snapshot's latest process, so its holder is another gate"
    );
}

#[test]
fn a_claim_written_after_the_snapshot_is_live_while_its_mutex_is_open() {
    let mut machine = Machine::new("a_claim_written_after_the_snapshot_is_live_while_its_mutex_is_open");
    machine.write_v1_claim("gate-remote-slot-1.json", V1_REMOTE_CLAIM);
    machine.write_v1_claim("gate-remote-slot-2.json", &V1_REMOTE_CLAIM.replacen("53000", "54000", 1));
    machine.add_process(999, 1, "explorer.exe", None, Some("2026-10-01T10:00:00Z"));
    machine.open.insert("gate-remote-slot-1".to_owned());

    assert_eq!(
        routed(&machine),
        found(
            vec![known(53000, Some("cargo build --release"), ts("2026-10-01T11:00:03.0000000Z"))],
            Vec::new()
        ),
        "both claims' gates started after every process in the snapshot; slot 1's mutex is open, slot 2's is not"
    );
}

#[test]
fn a_closed_remote_slot_with_a_dead_claim_is_not() {
    let mut machine = Machine::new("a_closed_remote_slot_with_a_dead_claim_is_not");
    machine.write_v1_claim("gate-remote-slot-1.json", V1_REMOTE_CLAIM);
    machine.add_process(999, 1, "explorer.exe", None, Some(START_FAR));

    assert_eq!(
        routed(&machine),
        RemoteGates::default(),
        "no process runs the claim's pid, it started before the snapshot's latest process, and no mutex is open"
    );
}

/// A mutex probe that records the names it is asked about and finds only `open` open.
struct RecordingProbe {
    asked: RefCell<Vec<String>>,
    open: Vec<String>,
}

impl RecordingProbe {
    fn new(open: Vec<String>) -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
            open,
        }
    }
}

impl MutexProbe for RecordingProbe {
    fn is_open(&self, name: &str) -> bool {
        self.asked.borrow_mut().push(name.to_owned());
        self.open.iter().any(|open| open == name)
    }
}

/// The machine's `gate\v1` judged against another default, so its kernel objects carry its hash.
fn non_default_root(machine: &Machine) -> StateRoot {
    StateRoot::new(&machine.v1_root().to_string_lossy(), Some(r"C:\Users\nobody\AppData\Local\gate\v1"))
}

/// The name [`remote_gates`] probes remote slot `index`'s mutex by under `root`, without `Local\`.
fn probed_name(root: &StateRoot, index: u32) -> String {
    root.object_name(KernelObject::RemoteSlot(index))
        .strip_prefix(r"Local\")
        .expect("a gate's kernel object is session-local")
        .to_owned()
}

#[test]
fn a_non_default_roots_held_remote_slot_is_probed_by_its_hashed_name() {
    let machine = Machine::new("a_non_default_roots_held_remote_slot_is_probed_by_its_hashed_name");
    let root = non_default_root(&machine);
    let name = |index| probed_name(&root, index);
    assert_ne!(name(1), "gate-remote-slot-1", "a non-default root's names carry its hash");
    let probe = RecordingProbe::new(vec![name(1)]);

    let at = StateRootAt {
        path: &machine.v1_root(),
        root: &root,
    };
    let routed = remote_gates(at, None, REMOTE_COUNT, &machine, &probe);

    assert_eq!(routed, found(vec![unknown(1)], Vec::new()), "the held slot under the root's own name");
    assert_eq!(
        probe.asked.into_inner(),
        [name(0), name(1), name(2)],
        "each remote slot's mutex by the root's hashed name, never the machine's own"
    );
}

#[test]
fn a_live_powershell_remote_claim_and_its_open_mutex_are_one_gate_under_the_default_root() {
    let mut machine = Machine::new("a_live_powershell_remote_claim_and_its_open_mutex_are_one_gate_under_the_default_root");
    machine.write_claim("gate-remote-slot-1.json", REMOTE_CLAIM);
    machine.add_process(52000, 1, "pwsh.exe", None, Some(FIXTURE_START));
    machine.open.insert("gate-remote-slot-1".to_owned());

    assert_eq!(
        routed(&machine),
        found(
            vec![known(52000, Some("dotnet test Delve.slnx -c Release"), ts(FIXTURE_STARTED))],
            Vec::new()
        ),
        "the PowerShell gate holds the default root's own gate-remote-slot-1, so the open mutex is that gate"
    );
}

#[test]
fn a_non_default_root_lists_a_live_powershell_remote_claim_once_and_probes_only_its_own_names() {
    let mut machine = Machine::new("a_non_default_root_lists_a_live_powershell_remote_claim_once_and_probes_only_its_own_names");
    machine.write_claim("gate-remote-slot-1.json", REMOTE_CLAIM);
    machine.add_process(52000, 1, "pwsh.exe", None, Some(FIXTURE_START));
    let root = non_default_root(&machine);
    let probe = RecordingProbe::new(vec!["gate-remote-slot-1".to_owned()]);

    let at = StateRootAt {
        path: &machine.v1_root(),
        root: &root,
    };
    let routed = remote_gates(at, Some(&machine.dir.join("gate")), REMOTE_COUNT, &machine, &probe);

    assert_eq!(
        routed,
        found(
            vec![known(52000, Some("dotnet test Delve.slnx -c Release"), ts(FIXTURE_STARTED))],
            Vec::new()
        ),
        "the PowerShell gate by its claim; the machine's own gate-remote-slot-1 is not this root's slot"
    );
    assert_eq!(
        probe.asked.into_inner(),
        [probed_name(&root, 0), probed_name(&root, 1), probed_name(&root, 2)],
        "only the root's hashed names are probed"
    );
}

#[test]
fn a_dead_claim_past_the_count_with_its_mutex_open_is_one_unknown_gate() {
    let mut machine = Machine::new("a_dead_claim_past_the_count_with_its_mutex_open_is_one_unknown_gate");
    machine.write_v1_claim("gate-remote-slot-5.json", &V1_REMOTE_CLAIM.replacen("53000", "54000", 1));
    machine.add_process(999, 1, "explorer.exe", None, Some(START_FAR));
    machine.open.insert("gate-remote-slot-5".to_owned());

    assert_eq!(
        routed(&machine),
        found(vec![unknown(5)], Vec::new()),
        "slot 5 is past the count of 3 but a claim names it, so its mutex is probed; the claim's gate is gone"
    );
}

#[test]
fn a_live_powershell_remote_ticket_is_listed() {
    let mut machine = Machine::new("a_live_powershell_remote_ticket_is_listed");
    let arrived = "2026-10-01T11:00:02Z";
    machine.write_ticket("waiting", "60090-1.json", &remote_ticket(60090, ticks(arrived)).to_string());
    machine.write_ticket("waiting", "60091-1.json", &ticket("heavy", 60091, ticks(arrived), false).to_string());
    for pid in [60090, 60091] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(START_CLOSE));
    }

    assert_eq!(
        routed(&machine),
        found(vec![known(60090, None, ts(arrived))], Vec::new()),
        "the PowerShell gate waiting for the remote pool, by its arrival; the live local ticket is not routed work"
    );
}

#[test]
fn an_unreadable_v1_remote_claim_is_listed_and_a_heavy_one_is_not() {
    let machine = Machine::new("an_unreadable_v1_remote_claim_is_listed_and_a_heavy_one_is_not");
    machine.write_v1_claim("gate-remote-slot-1.json", "not json");
    machine.write_v1_claim("gate-slot-0.json", "not json");

    assert_eq!(
        routed(&machine),
        found(Vec::new(), vec![machine.v1_root().join("slots").join("gate-remote-slot-1.json")]),
        "an unreadable heavy claim is no routed work"
    );
}

#[test]
fn an_unreadable_v1_ticket_is_listed() {
    let machine = Machine::new("an_unreadable_v1_ticket_is_listed");
    machine.write_v1_ticket("60080-1790852400500.json", r#"{"v":1,"kind":"light""#);

    assert_eq!(
        routed(&machine),
        found(Vec::new(), vec![machine.v1_root().join("waiting").join("60080-1790852400500.json")]),
        "a ticket that cannot be read may be a routed gate's, whatever kind it was"
    );
}

#[test]
fn an_unreadable_powershell_remote_claim_is_listed_and_a_heavy_one_is_not() {
    let machine = Machine::new("an_unreadable_powershell_remote_claim_is_listed_and_a_heavy_one_is_not");
    for name in [
        "gate-remote-slot-1.json",
        "gate-slot-0.json",
        "gate-light-slot-0.json",
        "gate-critical.json",
    ] {
        machine.write_claim(name, "not json");
    }
    machine.write_ticket("waiting", "60080-1.json", r#"{"kind":"#);
    let powershell = machine.dir.join("gate");

    assert_eq!(
        routed(&machine),
        found(
            Vec::new(),
            vec![
                powershell.join("slots").join("gate-remote-slot-1.json"),
                powershell.join("waiting").join("60080-1.json"),
            ]
        ),
        "the remote claim and the ticket, either of which may be routed work; not the heavy, light or critical claim"
    );
}

#[test]
fn an_out_of_range_remote_slot_index_is_listed() {
    let mut machine = Machine::new("an_out_of_range_remote_slot_index_is_listed");
    machine.write_v1_claim("gate-remote-slot-4294967296.json", V1_REMOTE_CLAIM);
    machine.write_claim("gate-remote-slot-4294967296.json", REMOTE_CLAIM);
    machine.write_claim("gate-slot-4294967296.json", CLAIM);
    machine.add_process(53000, 1, "gate.exe", None, Some(START));
    for pid in [51000, 52000] {
        machine.add_process(pid, 1, "pwsh.exe", None, Some(FIXTURE_START));
    }

    assert_eq!(
        routed(&machine),
        found(
            Vec::new(),
            vec![
                machine.v1_root().join("slots").join("gate-remote-slot-4294967296.json"),
                machine.dir.join("gate").join("slots").join("gate-remote-slot-4294967296.json"),
            ]
        ),
        "a live remote claim whose index is past a u32 names no slot; the heavy one is not routed work"
    );
}

#[test]
fn a_v1_ticket_whose_arrival_is_out_of_range_is_listed() {
    let mut machine = Machine::new("a_v1_ticket_whose_arrival_is_out_of_range_is_listed");
    let ticket = V1_REMOTE_TICKET.replacen("1790852402000", &u64::MAX.to_string(), 1);
    machine.write_v1_ticket("53100-1790852400500.json", &ticket);
    machine.add_process(53100, 1, "gate.exe", None, Some(START));

    assert_eq!(
        routed(&machine),
        found(Vec::new(), vec![machine.v1_root().join("waiting").join("53100-1790852400500.json")]),
        "a live remote ticket whose arrived no Timestamp holds"
    );
}

#[test]
fn a_live_remote_v1_ticket_whose_process_started_1_ms_off_is_not_listed() {
    let mut machine = Machine::new("a_live_remote_v1_ticket_whose_process_started_1_ms_off_is_not_listed");
    machine.write_v1_ticket("53100-1790852400500.json", V1_REMOTE_TICKET);
    machine.add_process(53100, 1, "gate.exe", None, Some("2026-10-01T11:00:00.501Z"));

    assert_eq!(
        routed(&machine),
        RemoteGates::default(),
        "a gate.exe ticket's process starts at its startTicks to the millisecond"
    );
}

#[test]
fn a_folder_that_cannot_be_listed_is_listed() {
    let machine = Machine::new("a_folder_that_cannot_be_listed_is_listed");
    fs::create_dir_all(machine.v1_root()).expect("the v1 root can be made");
    fs::remove_dir(machine.dir.join("gate").join("slots")).expect("the empty PowerShell slots folder can be removed");
    let folders = [
        machine.v1_root().join("slots"),
        machine.v1_root().join("waiting"),
        machine.dir.join("gate").join("slots"),
        machine.dir.join("gate").join("waiting"),
    ];
    for folder in &folders {
        fs::write(folder, "").expect("a file can stand where the folder would be");
    }

    assert_eq!(
        routed(&machine),
        found(Vec::new(), folders.to_vec()),
        "a folder that exists but cannot be listed may hold routed work"
    );
}

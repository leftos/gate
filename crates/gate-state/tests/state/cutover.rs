//! The state read from gate.exe's v1 root once `%LOCALAPPDATA%\gate\cutover.flag` exists, and from the PowerShell root
//! until then.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use gate_state::v1::{self, CoveredMarker, Ticket, TicketKind};
use gate_state::{
    CountsCopy, CoveredRun, Generation, SlotCount, SlotCounts, SlotKind, SlotState, Waiting, copy_slot_counts_to_v1, link_no_replace,
    write_cutover_flag,
};
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::support::{HEAVY_GATE, LIGHT_GATE, Machine, START, START_CLOSE, START_FAR, claim, heavy_slot, ms, pool, ticket, ticks};

const V1_CRITICAL_CLAIM: &str = include_str!("fixtures/v1-critical-claim.json");
const V1_CRITICAL_TICKET: &str = include_str!("fixtures/v1-critical-ticket.json");
const V1_REMOTE_CLAIM: &str = include_str!("fixtures/v1-remote-claim.json");
const V1_REMOTE_TICKET: &str = include_str!("fixtures/v1-remote-ticket.json");
const V1_SLOT_COUNTS: &str = include_str!("fixtures/v1-slot-counts.json");
const V1_COVERED: &str = include_str!("fixtures/v1-covered.json");
const V1_CLAIM_EXPECTED: &str = include_str!("fixtures/v1-claim-expected.json");
const V1_TICKET_EXPECTED: &str = include_str!("fixtures/v1-ticket-expected.json");
/// The file names gate.exe gives the fixture tickets: `<pid>-<start ms>.json`.
const V1_CRITICAL_TICKET_NAME: &str = "52000-1790852400500.json";
const V1_REMOTE_TICKET_NAME: &str = "53100-1790852400500.json";
const V1_TICKET_EXPECTED_NAME: &str = "57100-1790852400500.json";
/// A gate.exe gate's command line, which a waiting gate's row shows as it is.
const GATE_EXE: &str = r"C:\Users\me\AppData\Local\gate\bin\gate.exe --log .tmp\b.log --slot heavy -- cargo build";

/// Writes the v1 ticket of gate `pid`, started at [`START`] and arrived at `arrived`, local and not holding the machine.
fn v1_ticket(machine: &Machine, kind: TicketKind, pid: u32, arrived: &str) {
    let ticket = Ticket {
        kind,
        arrived: ms(arrived),
        pid,
        start_ms: ms(START),
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        pool: None,
        also_pools: Vec::new(),
        pinned: None,
        reason: None,
        expected: None,
    };
    v1::write_ticket(&v1::ticket_path(&machine.v1_root(), pid, ms(START)), &ticket).expect("a v1 ticket can be written");
}

/// A live heavy gate's claim in the PowerShell root, and gate.exe's live critical and remote claims in the v1 root.
fn claims_in_both_roots(machine: &mut Machine) {
    machine.write_claim("gate-slot-0.json", &claim(100, None, r"C:\work").to_string());
    machine.add_process(100, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.write_v1_claim("gate-critical.json", V1_CRITICAL_CLAIM);
    machine.write_v1_claim("gate-remote-slot-1.json", V1_REMOTE_CLAIM);
    machine.add_process(52000, 1, "gate.exe", Some(GATE_EXE), Some(START));
    machine.add_process(53000, 1, "gate.exe", Some(GATE_EXE), Some(START));
}

#[test]
fn the_flag_switches_the_state_to_the_v1_root() {
    let mut machine = Machine::v1("the_flag_switches_the_state_to_the_v1_root");
    claims_in_both_roots(&mut machine);

    let state = machine.state();

    assert_eq!(machine.inputs().generation, Generation::V1);
    let holder = &state.critical.holder;
    assert_eq!(holder.state, SlotState::Busy, "{holder:?}");
    let critical = holder.claim.as_ref().expect("a busy row has its claim");
    assert_eq!(
        (&critical["pid"], &critical["kind"], &critical["v"]),
        (&json!(52000), &json!("critical"), &json!(1)),
        "gate.exe's critical claim, as the file has it"
    );
    let heavy = &pool(&state, SlotKind::Heavy).slots[0];
    assert_eq!(
        (heavy.state, &heavy.stale),
        (SlotState::Free, &None),
        "the PowerShell root's claim is not read"
    );
    let remote = state.remote.as_ref().expect("gate.exe's remote claim shows the remote pool");
    let slots: Vec<(u32, SlotState)> = remote.slots.iter().map(|slot| (slot.index, slot.state)).collect();
    assert_eq!(slots, [(1, SlotState::Busy)]);
}

#[test]
fn a_powershell_gate_holding_a_slot_after_the_cutover_is_busy_with_its_claim() {
    let mut machine = Machine::v1("a_powershell_gate_holding_a_slot_after_the_cutover_is_busy_with_its_claim");
    let claim = claim(100, None, r"C:\work");
    machine.write_claim("gate-slot-1.json", &claim.to_string());
    machine.add_process(100, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Busy, Some(&claim), None),
        "a gate started through a stale launcher holds gate.exe's mutex and writes its claim under the PowerShell root"
    );
}

#[test]
fn a_powershell_claim_does_not_beat_a_live_v1_claim() {
    let mut machine = Machine::v1("a_powershell_claim_does_not_beat_a_live_v1_claim");
    let v1_claim = claim(100, None, r"C:\v1");
    let powershell = claim(200, None, r"C:\powershell");
    machine.write_v1_claim("gate-slot-1.json", &v1_claim.to_string());
    machine.write_claim("gate-slot-1.json", &powershell.to_string());
    machine.add_process(100, 1, "gate.exe", Some(GATE_EXE), Some(START));
    machine.add_process(200, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Busy, Some(&v1_claim), None),
        "the generation's own live claim is what the slot holds"
    );
}

#[test]
fn a_dead_v1_claim_gives_way_to_a_live_powershell_claim() {
    let mut machine = Machine::v1("a_dead_v1_claim_gives_way_to_a_live_powershell_claim");
    machine.write_v1_claim("gate-slot-1.json", &claim(100, None, r"C:\v1").to_string());
    let powershell = claim(200, None, r"C:\powershell");
    machine.write_claim("gate-slot-1.json", &powershell.to_string());
    machine.add_process(200, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Busy, Some(&powershell), None),
        "the v1 claim's gate is gone, so the live PowerShell claim is what the slot holds"
    );
}

#[test]
fn a_powershell_claim_started_after_the_snapshot_is_busy() {
    let mut machine = Machine::v1("a_powershell_claim_started_after_the_snapshot_is_busy");
    let mut powershell = claim(100, None, r"C:\work");
    powershell["processStart"] = json!("2026-10-01T11:00:02Z");
    machine.write_claim("gate-slot-1.json", &powershell.to_string());
    machine.add_process(5, 1, "pwsh.exe", Some(HEAVY_GATE), Some("2026-10-01T11:00:01Z"));
    machine.open.insert("gate-slot-1".to_owned());

    let row = heavy_slot(&machine, 1);

    assert_eq!(
        (row.state, row.claim.as_ref(), row.stale.as_ref()),
        (SlotState::Busy, Some(&powershell), None),
        "the snapshot was taken before the PowerShell gate started, so its claim is newer than the table"
    );
}

#[test]
fn no_flag_reads_the_powershell_root() {
    let mut machine = Machine::new("no_flag_reads_the_powershell_root");
    claims_in_both_roots(&mut machine);

    let state = machine.state();

    assert_eq!(machine.inputs().generation, Generation::PowerShell);
    assert_eq!(pool(&state, SlotKind::Heavy).slots[0].state, SlotState::Busy);
    assert_eq!(state.critical.holder.state, SlotState::Free, "gate.exe's critical claim is not read");
    assert!(state.remote.is_none(), "gate.exe's remote claim is not read: {:?}", state.remote);
}

#[test]
fn a_flag_without_a_v1_folder_is_an_empty_state() {
    let mut machine = Machine::new("a_flag_without_a_v1_folder_is_an_empty_state");
    machine.set_flag();
    machine.write_claim("gate-slot-0.json", &claim(100, None, r"C:\work").to_string());
    machine.add_process(100, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.write_ticket("waiting", "200-1.json", &ticket("light", 200, ticks(START), false).to_string());
    machine.add_process(200, 1, "pwsh.exe", Some(LIGHT_GATE), Some(START));
    machine.write_counts(r#"{"heavy":5}"#);

    let state = machine.state();

    assert!(!machine.v1_root().exists(), "the test has no v1 folder");
    assert_eq!(machine.inputs().generation, Generation::V1);
    assert!(state.queue.is_empty(), "no tickets: {:?}", state.queue);
    assert_eq!(state.critical.holder.state, SlotState::Free);
    assert!(state.critical.waiting.is_empty(), "{:?}", state.critical.waiting);
    for kind in SlotKind::ALL {
        let row = pool(&state, kind);
        assert_eq!(
            (row.count, row.set, row.problem.as_deref()),
            (row.default_count, None, None),
            "{kind:?}: the default counts"
        );
        assert!(
            row.slots.iter().all(|slot| slot.state == SlotState::Free && slot.stale.is_none()),
            "{kind:?}: no claims: {:?}",
            row.slots
        );
        assert!(row.waiting.is_empty(), "{kind:?}");
    }
}

#[test]
fn v1_waiters_come_from_live_tickets() {
    let mut machine = Machine::v1("v1_waiters_come_from_live_tickets");
    machine.hold_heavy_slots();
    for (pid, start) in [
        (300, START),
        (302, START_FAR),
        (303, START_CLOSE),
        (400, START),
        (52000, START),
        (53100, START),
    ] {
        machine.add_process(pid, 1, "gate.exe", Some(GATE_EXE), Some(start));
    }
    machine.add_process(310, 1, "pwsh.exe", Some(HEAVY_GATE), Some(START));
    machine.cwds.insert(300, r"C:\work\repo".to_owned());
    v1_ticket(&machine, TicketKind::Heavy, 300, "2026-10-01T11:00:02Z");
    v1_ticket(&machine, TicketKind::Heavy, 301, "2026-10-01T11:00:02Z");
    v1_ticket(&machine, TicketKind::Heavy, 302, "2026-10-01T11:00:02Z");
    v1_ticket(&machine, TicketKind::Heavy, 303, "2026-10-01T11:00:02Z");
    v1_ticket(&machine, TicketKind::Critical, 400, "2026-10-01T11:00:02Z");
    machine.write_v1_ticket(V1_CRITICAL_TICKET_NAME, V1_CRITICAL_TICKET);
    machine.write_v1_ticket(V1_REMOTE_TICKET_NAME, V1_REMOTE_TICKET);

    let state = machine.state();

    assert_eq!(
        pool(&state, SlotKind::Heavy).waiting,
        [Waiting {
            pid: 300,
            since: Some("2026-10-01T11:00:00.5000000Z".to_owned()),
            cwd: Some(r"C:\work\repo".to_owned()),
            command_line: GATE_EXE.to_owned(),
        }],
        "the live local heavy ticket; not a ticket whose gate is gone (301), whose pid was reused (302), or whose start \
         is not its process's to the millisecond (303, 1.5 s off), the routed gate (53100), or a gate.ps1 process without \
         a ticket (310)"
    );
    let critical: Vec<u32> = state.critical.waiting.iter().map(|waiting| waiting.pid).collect();
    assert_eq!(critical, [400], "the critical gate holding the machine (52000) does not wait");
    let light = &pool(&state, SlotKind::Light).waiting;
    assert!(light.is_empty(), "no light tickets: {light:?}");
}

#[test]
fn a_v1_ticket_queue_is_ordered_by_epoch_ms() {
    let mut machine = Machine::v1("a_v1_ticket_queue_is_ordered_by_epoch_ms");
    for pid in [400, 700, 999, 1000, 52000, 53100] {
        machine.add_process(pid, 1, "gate.exe", Some(GATE_EXE), Some(START));
    }
    v1_ticket(&machine, TicketKind::Heavy, 400, "2026-10-01T11:00:03Z");
    v1_ticket(&machine, TicketKind::Light, 1000, "2026-10-01T11:00:02Z");
    v1_ticket(&machine, TicketKind::Heavy, 999, "2026-10-01T11:00:02Z");
    machine.write_v1_ticket(V1_CRITICAL_TICKET_NAME, V1_CRITICAL_TICKET);
    machine.write_v1_ticket(V1_REMOTE_TICKET_NAME, V1_REMOTE_TICKET);
    machine.write_v1_ticket("700-1.json", &ticket("heavy", 700, ticks("2026-10-01T11:00:00Z"), false).to_string());

    let state = machine.state();

    let rows: Vec<(u32, &str, bool, bool, &str, f64)> = state
        .queue
        .iter()
        .map(|row| (row.pid, row.kind.as_str(), row.holds, row.remote, row.arrived.as_str(), row.age_seconds))
        .collect();
    assert_eq!(
        rows,
        [
            (52000, "critical", true, false, "2026-10-01T11:00:01.0000000Z", 3599.0),
            (999, "heavy", false, false, "2026-10-01T11:00:02.0000000Z", 3598.0),
            (1000, "light", false, false, "2026-10-01T11:00:02.0000000Z", 3598.0),
            (53100, "heavy", false, true, "2026-10-01T11:00:02.0000000Z", 3598.0),
            (400, "heavy", false, false, "2026-10-01T11:00:03.0000000Z", 3597.0),
        ],
        "by arrival in epoch ms, then by pid (999 before 1000, though its file name sorts after); the PowerShell ticket \
         (700) is not a v1 ticket"
    );
    assert_eq!(state.queue[0].command_line.as_deref(), Some(GATE_EXE));
}

#[test]
fn expected_passes_from_v1_claims_and_tickets_into_the_state_and_is_absent_without_it() {
    let mut machine = Machine::v1("expected_passes_from_v1_claims_and_tickets_into_the_state_and_is_absent_without_it");
    for pid in [52000, 57000, 57100, 53100] {
        machine.add_process(pid, 1, "gate.exe", Some(GATE_EXE), Some(START));
    }
    machine.write_v1_claim("gate-slot-0.json", V1_CLAIM_EXPECTED);
    machine.open.insert("gate-slot-0".to_owned());
    machine.write_v1_claim("gate-critical.json", V1_CRITICAL_CLAIM);
    machine.write_v1_ticket(V1_TICKET_EXPECTED_NAME, V1_TICKET_EXPECTED);
    machine.write_v1_ticket(V1_REMOTE_TICKET_NAME, V1_REMOTE_TICKET);
    let expected = json!({"p50": 240.5, "p90": 380.25, "n": 30, "key": "local::repo | cargo build"});

    let state = machine.state();

    let heavy = &pool(&state, SlotKind::Heavy).slots[0];
    let claim = heavy.claim.as_ref().expect("the busy slot has its claim");
    assert_eq!(
        (heavy.state, claim.get("expected")),
        (SlotState::Busy, Some(&expected)),
        "the claim as the file has it"
    );
    let critical = state.critical.holder.claim.as_ref().expect("the critical holder has its claim");
    assert_eq!(critical.get("expected"), None, "a claim without it has none");
    let rows: Vec<(u32, Option<Value>)> = state.queue.iter().map(|row| (row.pid, row.expected.clone().map(Value::Object))).collect();
    assert_eq!(rows, [(53100, None), (57100, Some(expected.clone()))], "arrived together, ordered by pid");
    let serialised = serde_json::to_value(&state).expect("the state serialises");
    let queue = serialised["queue"].as_array().expect("the queue is an array");
    assert_eq!(
        (queue[0].get("expected"), queue[1].get("expected")),
        (None, Some(&expected)),
        "a ticket without it serialises no key"
    );
}

#[test]
fn a_v1_tickets_reason_reaches_the_queue_row_before_expected() {
    let mut machine = Machine::v1("a_v1_tickets_reason_reaches_the_queue_row_before_expected");
    machine.add_process(400, 1, "gate.exe", Some(GATE_EXE), Some(START));
    let expected = json!({"p50": 240.5}).as_object().cloned().expect("expected is an object");
    let ticket = Ticket {
        kind: TicketKind::Heavy,
        arrived: ms("2026-10-01T11:00:02Z"),
        pid: 400,
        start_ms: ms(START),
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        pool: None,
        also_pools: Vec::new(),
        pinned: None,
        reason: Some("checking the fallback".to_owned()),
        expected: Some(expected),
    };
    v1::write_ticket(&v1::ticket_path(&machine.v1_root(), 400, ms(START)), &ticket).expect("a v1 ticket can be written");

    let state = machine.state();

    assert_eq!(state.queue.len(), 1, "one waiting gate");
    assert_eq!(state.queue[0].reason.as_deref(), Some("checking the fallback"));
    let serialised = serde_json::to_value(&state).expect("the state serialises");
    let row = serialised["queue"][0].as_object().expect("a queue row is an object");
    let keys: Vec<&str> = row.keys().map(String::as_str).collect();
    let after = keys
        .iter()
        .position(|key| *key == "commandLine")
        .map(|at| &keys[at..])
        .expect("the row has a command line");
    assert_eq!(after, ["commandLine", "reason", "expected"]);
}

/// A covered marker of gate `pid` started at `start`, under the slot of gate 52000, written at `since`.
fn covered_marker(pid: u32, start: &str, since: &str) -> CoveredMarker {
    CoveredMarker {
        pid,
        process_start: ms(start),
        parent_pid: 52000,
        command: "cargo fmt --check".to_owned(),
        cwd: r"C:\work\repo".to_owned(),
        since: ms(since),
        kind: None,
    }
}

/// Writes `marker` where gate.exe keeps it, in `covered` under the v1 root and its prefix.
fn write_covered(machine: &Machine, marker: &CoveredMarker) {
    let path = machine
        .v1_root()
        .join(format!("{}covered", machine.prefix))
        .join(format!("{}-{}.json", marker.pid, marker.process_start));
    v1::write_covered(&path, marker).expect("a covered marker can be written");
}

#[test]
fn v1_covered_gates_come_from_live_markers() {
    let mut machine = Machine::v1("v1_covered_gates_come_from_live_markers");
    let own = process::id();
    machine.add_process(own, 1, "gate.exe", Some(GATE_EXE), Some(START));
    machine.add_process(54100, 1, "gate.exe", Some(GATE_EXE), Some(START_CLOSE));
    write_covered(&machine, &covered_marker(own, START, "2026-10-01T11:00:02Z"));
    write_covered(&machine, &covered_marker(54100, START, "2026-10-01T11:00:01Z"));
    write_covered(&machine, &covered_marker(54200, START, "2026-10-01T11:00:01Z"));

    let state = machine.state();

    assert_eq!(
        state.covered,
        [CoveredRun {
            pid: own,
            start: "2026-10-01T11:00:00.5000000Z".to_owned(),
            parent_pid: 52000,
            command: "cargo fmt --check".to_owned(),
            cwd: r"C:\work\repo".to_owned(),
            since: "2026-10-01T11:00:02.0000000Z".to_owned(),
        }],
        "the live marker; not one whose pid runs since another start (54100, 1.5 s off) or runs no process (54200)"
    );
    let folder = v1::covered_dir(&machine.v1_root());
    assert_eq!(
        fs::read_dir(&folder).expect("the covered folder lists").count(),
        3,
        "the reader deletes nothing"
    );
}

#[test]
fn v1_covered_markers_are_read_under_the_prefix_oldest_first() {
    let mut machine = Machine::v1("v1_covered_markers_are_read_under_the_prefix_oldest_first");
    machine.prefix = "T1-".to_owned();
    for pid in [9000, 54000, 54001] {
        machine.add_process(pid, 1, "gate.exe", Some(GATE_EXE), Some(START));
    }
    // 54001's file name sorts before 9000's, so only the pid tiebreak puts 9000 first among the two of one since.
    write_covered(&machine, &covered_marker(54000, START, "2026-10-01T11:00:03Z"));
    write_covered(&machine, &covered_marker(54001, START, "2026-10-01T11:00:02Z"));
    write_covered(&machine, &covered_marker(9000, START, "2026-10-01T11:00:02Z"));
    let unprefixed = covered_marker(54000, START, "2026-10-01T11:00:01Z");
    v1::write_covered(&v1::covered_path(&machine.v1_root(), 54000, ms(START)), &unprefixed).expect("a marker can be written");

    let state = machine.state();

    let rows: Vec<(u32, &str)> = state.covered.iter().map(|row| (row.pid, row.since.as_str())).collect();
    assert_eq!(
        rows,
        [
            (9000, "2026-10-01T11:00:02.0000000Z"),
            (54001, "2026-10-01T11:00:02.0000000Z"),
            (54000, "2026-10-01T11:00:03.0000000Z"),
        ],
        "by since, then by pid in numeric order, not the file names'; the marker outside the prefixed folder is not read"
    );
}

#[test]
fn the_powershell_generation_has_no_covered_gates() {
    let mut machine = Machine::new("the_powershell_generation_has_no_covered_gates");
    machine.add_process(54000, 1, "gate.exe", Some(GATE_EXE), Some(START));
    write_covered(&machine, &covered_marker(54000, START, "2026-10-01T11:00:02Z"));
    let powershell_covered = machine.dir.join("gate").join("covered");
    fs::create_dir_all(&powershell_covered).expect("the PowerShell root's covered folder can be made");
    fs::write(powershell_covered.join("54000-1790852400500.json"), V1_COVERED).expect("a marker can be written");

    let state = machine.state();

    assert_eq!(machine.inputs().generation, Generation::PowerShell);
    assert_eq!(state.covered, [], "neither root's markers are read before the cutover");
}

#[test]
fn a_v1_counts_file_without_v_is_not_used() {
    let machine = Machine::v1("a_v1_counts_file_without_v_is_not_used");
    machine.write_counts(r#"{"heavy":5}"#);
    let path = machine.v1_counts_path();
    fs::write(&path, r#"{"heavy":4}"#).expect("the v1 counts file can be written");

    let state = machine.state();

    for (kind, default) in [(SlotKind::Heavy, 3), (SlotKind::Light, 7)] {
        let row = pool(&state, kind);
        let line = format!(
            "gate: ignored {}: missing key \"v\"; using {default} {} slots",
            path.display(),
            kind.as_str()
        );
        assert_eq!(
            (row.count, row.set, row.problem.as_deref()),
            (default, None, Some(line.as_str())),
            "{kind:?}"
        );
    }

    fs::write(&path, V1_SLOT_COUNTS).expect("the v1 counts file can be written");
    let state = machine.state();

    for (kind, set) in [(SlotKind::Heavy, 3), (SlotKind::Light, 6)] {
        let row = pool(&state, kind);
        assert_eq!(
            (row.count, row.set, row.problem.as_deref()),
            (set, SlotCount::new(set), None),
            "{kind:?}: a value the file sets is set, even the default"
        );
    }
}

/// An empty folder named after the test, standing for `%LOCALAPPDATA%` before any gate ran.
fn empty_local_app_data(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gate-state").join(test);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    fs::create_dir_all(&dir).expect("the test's folder can be made");
    dir
}

/// The names of the files in `dir`, sorted.
fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the folder can be listed")
        .map(|entry| entry.expect("the folder's entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_flag_writer_makes_the_gate_folder_and_hands_the_state_to_v1() {
    let dir = empty_local_app_data("the_flag_writer_makes_the_gate_folder_and_hands_the_state_to_v1");

    let path = write_cutover_flag(&dir).expect("the flag can be written");

    assert_eq!(path, Generation::cutover_flag(&dir));
    assert_eq!(Generation::detect(&dir).expect("the flag can be looked for"), Generation::V1);
    let text = fs::read_to_string(&path).expect("the flag can be read");
    assert!(text.ends_with('Z'), "the write time is in UTC: {text}");
    text.parse::<Timestamp>().expect("the flag holds an ISO 8601 timestamp");
    assert_eq!(file_names(&dir.join("gate")), ["cutover.flag"], "no temp file is left");
}

#[test]
fn a_second_flag_write_replaces_the_first() {
    let dir = empty_local_app_data("a_second_flag_write_replaces_the_first");
    let path = Generation::cutover_flag(&dir);
    fs::create_dir_all(dir.join("gate")).expect("the gate folder can be made");
    fs::write(&path, "an earlier flag").expect("an earlier flag can be written");

    write_cutover_flag(&dir).expect("the flag can be rewritten");

    let text = fs::read_to_string(&path).expect("the flag can be read");
    text.parse::<Timestamp>().expect("the rewritten flag holds the new write time");
    assert_eq!(file_names(&dir.join("gate")), ["cutover.flag"], "no temp file is left");
}

#[test]
fn the_flag_writer_fails_when_the_gate_folder_cannot_be_made() {
    let dir = empty_local_app_data("the_flag_writer_fails_when_the_gate_folder_cannot_be_made");
    fs::write(dir.join("gate"), "a file where the gate folder goes").expect("the blocking file can be written");

    let error = write_cutover_flag(&dir).expect_err("no flag can be written under a file");

    assert_eq!(error.path, Generation::cutover_flag(&dir), "{error}");
    assert!(dir.join("gate").is_file(), "the blocking file is left as it was");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("the file can be read")
}

/// Writes gate.exe's counts file, making the v1 folder.
fn write_v1_counts(machine: &Machine, text: &str) {
    fs::create_dir_all(machine.v1_root()).expect("the v1 folder can be made");
    fs::write(machine.v1_counts_path(), text).expect("the v1 counts file can be written");
}

/// The race with a writer that puts the v1 file in place between the look and the link is covered by
/// `linking_into_place_never_replaces_a_file_there`.
#[test]
fn the_counts_copy_writes_the_powershell_counts_as_v1_text() {
    let machine = Machine::new("the_counts_copy_writes_the_powershell_counts_as_v1_text");
    machine.write_counts(r#"{"heavy":3,"light":5}"#);

    let copy = copy_slot_counts_to_v1(&machine.dir).expect("the counts can be copied");

    let counts = SlotCounts {
        heavy: SlotCount::new(3),
        light: SlotCount::new(5),
    };
    assert_eq!(copy, CountsCopy::Copied(counts));
    assert_eq!(read(&machine.v1_counts_path()), r#"{"v":1,"heavy":3,"light":5}"#);
    assert_eq!(
        read(&machine.counts_path()),
        r#"{"heavy":3,"light":5}"#,
        "the PowerShell file is untouched"
    );
    assert_eq!(file_names(&machine.v1_root()), ["slot-counts.json"], "no temp file is left");
}

#[test]
fn the_counts_copy_keeps_a_v1_counts_file_already_there() {
    let machine = Machine::new("the_counts_copy_keeps_a_v1_counts_file_already_there");
    machine.write_counts(r#"{"heavy":3,"light":5}"#);
    for v1_text in [r#"{"v":1,"heavy":7}"#, "not json"] {
        write_v1_counts(&machine, v1_text);

        let copy = copy_slot_counts_to_v1(&machine.dir).expect("the copy can look for the v1 file");

        assert_eq!(copy, CountsCopy::Kept, "{v1_text}");
        assert_eq!(read(&machine.v1_counts_path()), v1_text, "the v1 file is kept, readable or not");
        assert_eq!(file_names(&machine.v1_root()), ["slot-counts.json"], "no temp file is left");
    }
}

#[test]
fn the_counts_copy_writes_nothing_without_powershell_counts() {
    let machine = Machine::new("the_counts_copy_writes_nothing_without_powershell_counts");

    let copy = copy_slot_counts_to_v1(&machine.dir).expect("the copy can look for the files");

    assert_eq!(copy, CountsCopy::NoPowerShellCounts);
    assert!(!machine.v1_counts_path().exists(), "no v1 counts file is written");
}

#[test]
fn the_counts_copy_writes_nothing_when_no_value_survives() {
    let machine = Machine::new("the_counts_copy_writes_nothing_when_no_value_survives");
    for text in ["{}", r#"{"heavy":0}"#] {
        machine.write_counts(text);

        let copy = copy_slot_counts_to_v1(&machine.dir).expect("the copy can look for the files");

        assert_eq!(copy, CountsCopy::Copied(SlotCounts::default()), "{text}");
        assert!(
            !machine.v1_counts_path().exists(),
            "{text}: empty counts write no v1 file, as a machine at its defaults holds none"
        );
    }
}

#[test]
fn the_counts_copy_ignores_a_powershell_file_gate_state_rejects() {
    let machine = Machine::new("the_counts_copy_ignores_a_powershell_file_gate_state_rejects");
    for text in ["{heavy: 4}", "not json"] {
        machine.write_counts(text);

        let copy = copy_slot_counts_to_v1(&machine.dir).expect("the copy can look for the files");

        let reason = match &copy {
            CountsCopy::Ignored { path, reason } if *path == machine.counts_path() => reason.as_str(),
            _ => "",
        };
        assert!(
            reason.starts_with("it is not valid JSON: "),
            "{text}: the PowerShell file is ignored with the reason, got {copy:?}"
        );
        assert!(!machine.v1_counts_path().exists(), "{text}: no v1 counts file is written");
        assert_eq!(read(&machine.counts_path()), text, "the PowerShell file is untouched");
    }
}

#[test]
fn linking_into_place_never_replaces_a_file_there() {
    let machine = Machine::new("linking_into_place_never_replaces_a_file_there");
    let temp = machine.dir.join("new.tmp");
    let target = machine.dir.join("target.json");
    fs::write(&temp, "the new text").expect("the temp file can be written");
    fs::write(&target, "a file another writer put there first").expect("the target can be written");

    let error = link_no_replace(&temp, &target).expect_err("an existing target is not replaced");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(read(&temp), "the new text", "the temp file is left as it was");
    assert_eq!(read(&target), "a file another writer put there first", "the target is left as it was");
}

//! The state read from gate.exe's v1 root once `%LOCALAPPDATA%\gate\cutover.flag` exists, and from the PowerShell root
//! until then.

use std::fs;

use gate_state::v1::{self, Ticket, TicketKind};
use gate_state::{Generation, SlotCount, SlotKind, SlotState, Waiting};
use serde_json::json;

use crate::support::{HEAVY_GATE, LIGHT_GATE, Machine, START, START_CLOSE, START_FAR, claim, ms, pool, ticket, ticks};

const V1_CRITICAL_CLAIM: &str = include_str!("fixtures/v1-critical-claim.json");
const V1_CRITICAL_TICKET: &str = include_str!("fixtures/v1-critical-ticket.json");
const V1_REMOTE_CLAIM: &str = include_str!("fixtures/v1-remote-claim.json");
const V1_REMOTE_TICKET: &str = include_str!("fixtures/v1-remote-ticket.json");
const V1_SLOT_COUNTS: &str = include_str!("fixtures/v1-slot-counts.json");
/// The file names gate.exe gives the two fixture tickets: `<pid>-<start ms>.json`.
const V1_CRITICAL_TICKET_NAME: &str = "52000-1790852400500.json";
const V1_REMOTE_TICKET_NAME: &str = "53100-1790852400500.json";
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
        remote: None,
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

//! The gates `gate cutover`'s drain sees: the PowerShell gates still on the machine, the live claims and arrival tickets
//! in the PowerShell gate's `gate\slots` and `gate\waiting` folders. Nothing here deletes a file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use proc_table::{ProcessTable, ProcessTree};
use serde_json::Value;

use crate::claims::{
    Claim, ReadableClaim, Ticket, claim_started_by, json_base_name, read_claims, read_ticket, ticket_started_by, ticks_in_range, timestamp_of_ticks,
};
use crate::model::Generation;

/// The start of a remote slot's claim name, its file's name lowered and without `.json`, before the slot's index.
const REMOTE_SLOT_PREFIX: &str = "gate-remote-slot-";

/// The slot a PowerShell claim holds, from its file's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerShellSlot {
    /// A heavy slot, `gate-slot-<index>.json`.
    Heavy(u32),
    /// A light slot, `gate-light-slot-<index>.json`.
    Light(u32),
    /// A remote pool slot, `gate-remote-slot-<index>.json`: a routed gate, which holds nothing on this machine.
    Remote(u32),
    /// The machine, `gate-critical.json`.
    Critical,
}

/// What a PowerShell gate has on the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PowerShellHold {
    /// A claim on a slot.
    Claim(PowerShellSlot),
    /// An arrival ticket.
    Ticket {
        /// The slot the gate asked for, as the ticket has it: `heavy`, `light` or `critical`.
        kind: String,
        /// True for a critical gate that holds the machine, whose ticket stays until it ends.
        holds: bool,
    },
}

/// A live PowerShell gate's claim or ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerShellGate {
    /// The gate's process id.
    pub pid: u32,
    /// The claim's slot, or the ticket's kind.
    pub what: PowerShellHold,
    /// The claim's `command`; `None` for a ticket, which names no command, or a claim without one.
    pub command: Option<String>,
    /// When the gate took its slot, the claim's `started` (its `processStart` when it has none), or when it arrived,
    /// the ticket's `arrived`.
    pub since: Timestamp,
}

/// The PowerShell gates [`powershell_gates`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PowerShellGates {
    /// The live claims and tickets, by `since`, then by pid.
    pub gates: Vec<PowerShellGate>,
    /// How many claim and ticket files were skipped because they could not be read or used. A file deleted between the
    /// listing and the read is a gate finishing, not an unreadable file, and is not counted.
    pub unreadable: usize,
}

/// The PowerShell gates still on the machine: every live claim in `<local app data>\gate\slots` (heavy, light, remote
/// and critical) and every live ticket in `<local app data>\gate\waiting`, a critical gate's holding one included.
/// Only the files without a test prefix are read, so a self-test's `priocheck-*` claims and folders are not gates.
///
/// A claim is live while its pid is a process that started within 2 s of its `processStart`, a ticket while its pid is
/// a process that started within 2 s of its `startTicks`, as the Gates tab tells them, and either while its pid is a
/// process whose start cannot be read, as `gate.exe` counts it; the start times come from `processes`. A missing folder
/// holds none, and nothing is deleted.
#[must_use]
pub fn powershell_gates(local_app_data: &Path, processes: &dyn ProcessTable) -> PowerShellGates {
    let folder = Generation::PowerShell.state_dir(local_app_data);
    let table = ProcessTree::new(processes.snapshot());
    let mut found = PowerShellGates::default();
    let slots = folder.join("slots");
    for (name, claim) in read_claims(&slots, "") {
        found.add_claim(&slots, &name, &claim, &table);
    }
    for (_, ticket) in read_ticket_files(&folder.join("waiting")).unwrap_or_default() {
        found.add_ticket(ticket.as_ref(), &table);
    }
    found.gates.sort_by_key(|gate| (gate.since, gate.pid));
    found
}

impl PowerShellGates {
    /// Adds the claim in the file named `name` (lowered, without `.json`) in `slots` when its gate is live, or counts it
    /// when it cannot be read and is still there: a claim deleted between the listing and the read is a gate ending.
    fn add_claim(&mut self, slots: &Path, name: &str, claim: &Claim, table: &ProcessTree) {
        let Claim::Readable(claim) = claim else {
            if still_there(&slots.join(format!("{name}.json"))) {
                self.unreadable += 1;
            }
            return;
        };
        let Some(slot) = claim_slot(name) else {
            self.unreadable += 1;
            return;
        };
        if !claim_live(claim, table) {
            return;
        }
        self.gates.push(PowerShellGate {
            pid: claim.pid,
            what: PowerShellHold::Claim(slot),
            command: claim_command(claim),
            since: claim_since(claim),
        });
    }

    /// Adds the ticket when its gate is live, or counts it when it could not be read or its start or arrival is outside
    /// the ticks a `DateTime` holds.
    fn add_ticket(&mut self, ticket: Option<&Ticket>, table: &ProcessTree) {
        let Some((ticket, arrived)) = ticket.and_then(|ticket| Some((ticket, ticket_arrival(ticket)?))) else {
            self.unreadable += 1;
            return;
        };
        if !ticket_live(ticket, table) {
            return;
        }
        self.gates.push(PowerShellGate {
            pid: ticket.pid,
            what: PowerShellHold::Ticket {
                kind: ticket.kind.clone(),
                holds: ticket.holds,
            },
            command: None,
            since: arrived,
        });
    }
}

/// The claim's `command`, `None` when it has none.
fn claim_command(claim: &ReadableClaim) -> Option<String> {
    claim.value.get("command").and_then(Value::as_str).map(str::to_owned)
}

/// When the claim's gate took its slot: its `started`, or its `processStart` when it has none.
fn claim_since(claim: &ReadableClaim) -> Timestamp {
    claim
        .value
        .get("started")
        .and_then(Value::as_str)
        .and_then(|started| started.parse().ok())
        .unwrap_or(claim.process_start)
}

/// The slot a claim file named `name` (lowered, without `.json`) holds; `None` for an index past a `u32`.
fn claim_slot(name: &str) -> Option<PowerShellSlot> {
    if name == "gate-critical" {
        return Some(PowerShellSlot::Critical);
    }
    if let Some(index) = name.strip_prefix("gate-light-slot-") {
        return index.parse().ok().map(PowerShellSlot::Light);
    }
    if let Some(index) = name.strip_prefix(REMOTE_SLOT_PREFIX) {
        return index.parse().ok().map(PowerShellSlot::Remote);
    }
    name.strip_prefix("gate-slot-")?.parse().ok().map(PowerShellSlot::Heavy)
}

/// Each `.json` file's path and ticket in `dir`, the ticket `None` for one that could not be read and is still there; a
/// ticket deleted between the listing and the read is a gate taking its slot or ending, and is left out. A missing
/// folder holds none.
///
/// # Errors
///
/// The io error when the folder exists but cannot be listed.
fn read_ticket_files(dir: &Path) -> io::Result<Vec<(PathBuf, Option<Ticket>)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut tickets = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if json_base_name(&path).is_none() || !path.is_file() {
            continue;
        }
        match read_ticket(&path) {
            Some(ticket) => tickets.push((path, Some(ticket))),
            None if still_there(&path) => tickets.push((path, None)),
            None => {}
        }
    }
    Ok(tickets)
}

/// True unless the file at `path` is known to be gone; a file whose presence cannot be told counts as there.
fn still_there(path: &Path) -> bool {
    path.try_exists().unwrap_or(true)
}

/// True when the claim's pid is a process that started within 2 s of the claim's `processStart`, or a process whose
/// start cannot be read.
fn claim_live(claim: &ReadableClaim, table: &ProcessTree) -> bool {
    table
        .get(claim.pid)
        .is_some_and(|process| process.created.is_none_or(|created| claim_started_by(claim.process_start, created)))
}

/// When the ticket's gate arrived, or `None` when its `startTicks` or `arrived` is outside the ticks a `DateTime` holds.
fn ticket_arrival(ticket: &Ticket) -> Option<Timestamp> {
    if !ticks_in_range(ticket.start_ticks) {
        return None;
    }
    timestamp_of_ticks(ticket.arrived_ticks)
}

/// True when the ticket's pid is a process that started within 2 s of the ticket's `startTicks`, or a process whose
/// start cannot be read.
fn ticket_live(ticket: &Ticket, table: &ProcessTree) -> bool {
    table
        .get(ticket.pid)
        .is_some_and(|process| process.created.is_none_or(|created| ticket_started_by(ticket.start_ticks, created)))
}

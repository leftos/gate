//! The gates the drains see: the PowerShell gates still on the machine, the live claims and arrival tickets in the
//! PowerShell gate's `gate\slots` and `gate\waiting` folders; the routed work the remote cutover's drain waits on, the
//! live remote-pool claims, held remote slots and remote tickets of a v1 state root and of the PowerShell root; and the
//! remote cutover's drain marker judged against the process table. Nothing here deletes a file.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTable, ProcessTree};
use serde_json::Value;

use crate::claims::{
    Claim, ReadableClaim, Ticket, claim_started_by, json_base_name, read_claims, read_ticket, started_at_ms, ticket_started_by, ticks_in_range,
    timestamp_of_ticks, try_read_claims,
};
use crate::model::{Generation, MutexProbe};
use crate::root::{KernelObject, StateRoot};
use crate::v1;

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

/// A v1 state root as [`remote_gates`] reads it: its path as the gate uses it, which every file is read under and every
/// unreadable entry is named by, and its identity, which names its kernel objects.
#[derive(Clone, Copy, Debug)]
pub struct StateRootAt<'a> {
    /// The root's absolute path, as `gate.exe` resolved it.
    pub path: &'a Path,
    /// The root's identity, built as `gate.exe` builds its own (`StateRoot::new(path, Some(default))`).
    pub root: &'a StateRoot,
}

/// A gate the remote cutover's drain waits on: a live remote-pool claim, a remote slot whose mutex is held with no live
/// claim, or a live ticket waiting for the remote pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteGate {
    /// The gate's process id; `None` for a held remote slot with no live claim, whose holder is not known.
    pub pid: Option<u32>,
    /// The claim's `command`; `None` for a ticket, which names no command, a claim without one, or a held remote slot
    /// with no live claim.
    pub command: Option<String>,
    /// When the gate took its remote slot, the claim's `started` (its `processStart` when it has none), or when it
    /// arrived, the ticket's `arrived`; `None` for a held remote slot with no live claim.
    pub since: Option<Timestamp>,
    /// The index of a held remote slot with no live claim, by which it is told from another; `None` for a claim or a
    /// ticket, whose gate's pid is known.
    pub slot: Option<u32>,
}

/// The routed gates [`remote_gates`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteGates {
    /// The live routed gates, by `since` (the held remote slots with no live claim first), then by pid.
    pub gates: Vec<RemoteGate>,
    /// The entries that could not be read or used and might be routed work, by path: the remote-slot claims and the
    /// tickets of every kind of either root, a remote-slot claim whose index is past a `u32`, a live remote ticket whose
    /// arrival no `Timestamp` holds, and a `slots` or `waiting` folder of either root that exists but cannot be listed.
    /// A file deleted between the listing and the read is a gate finishing, not an unreadable file, and is not listed;
    /// nor is a v1 ticket whose open is refused, one whose delete is pending.
    pub unreadable: Vec<PathBuf>,
}

/// The routed work the remote cutover's drain waits for, under the state root `root` and the PowerShell root
/// `powershell_root` (`<local app data>\gate`; `None` reads none).
///
/// Every live remote ticket of either root first: a v1 ticket is live while its pid is a process created at its
/// `startTicks` to the millisecond, a PowerShell one while its pid is a process that started within 2 s of its
/// `startTicks`. Then every live `gate-remote-slot-<n>` claim of the PowerShell root, live while its pid is a process
/// that started within 2 s of its `processStart`. Then `root`'s claims, and last its mutexes: each remote slot
/// `0..remote_count`, and each that a claim names, whose claim is live or whose mutex is open, the mutex named as
/// `gate.exe` names it under `root`. As the Gates tab tells a slot, a claim is live while its pid is a process that
/// started within 2 s of its `processStart`, or, while the mutex is open, when its pid is not in the snapshot and its
/// process started after every process the snapshot holds; an open mutex with no live claim is a gate whose pid is not
/// known, unless under the default root a live PowerShell-root claim of that slot, which holds the same mutex, is listed.
/// A gate live across the scan is seen by its ticket, its claim or its mutex. A pid whose process start cannot be read
/// counts as live, as `gate.exe` counts it.
///
/// The start times come from `processes`, the mutexes from `mutexes`. A missing folder holds none, and nothing is
/// deleted.
#[must_use]
pub fn remote_gates(
    root: StateRootAt<'_>,
    powershell_root: Option<&Path>,
    remote_count: u32,
    processes: &dyn ProcessTable,
    mutexes: &dyn MutexProbe,
) -> RemoteGates {
    let snapshot = Snapshot::new(processes.snapshot());
    let mut found = RemoteGates::default();
    found.add_v1_tickets(&v1::tickets_dir(root.path), &snapshot);
    if let Some(folder) = powershell_root {
        found.add_powershell_tickets(&folder.join("waiting"), &snapshot);
    }
    let held_by_powershell = powershell_root
        .map(|folder| found.add_powershell_claims(&folder.join("slots"), &snapshot))
        .unwrap_or_default();
    let slots = v1::slots_dir(root.path);
    let claims = try_read_claims(&slots, "").unwrap_or_else(|_| {
        found.unreadable.push(slots.clone());
        HashMap::new()
    });
    let probe = RemoteSlots {
        root: root.root,
        remote_count,
        mutexes,
        held_by_powershell: if root.root.is_default() { held_by_powershell } else { BTreeSet::new() },
    };
    found.add_v1_slots(&probe, &slots, &claims, &snapshot);
    found.gates.sort_by_key(|gate| (gate.since, gate.pid));
    found.unreadable.sort();
    found
}

/// A v1 root's remote slots, as [`remote_gates`] probes them.
struct RemoteSlots<'a> {
    root: &'a StateRoot,
    remote_count: u32,
    mutexes: &'a dyn MutexProbe,
    /// The remote slots a live PowerShell-root claim holds through this root's own mutex: under the default root, whose
    /// names are the PowerShell gate's.
    held_by_powershell: BTreeSet<u32>,
}

impl RemoteSlots<'_> {
    /// True when the mutex of remote slot `index`, named as `gate.exe` names it under the root, is open.
    fn is_open(&self, index: u32) -> bool {
        let name = self.root.object_name(KernelObject::RemoteSlot(index));
        self.mutexes.is_open(name.strip_prefix(r"Local\").unwrap_or(&name))
    }
}

/// The process snapshot the routed gates and the drainer are judged against.
struct Snapshot {
    tree: ProcessTree,
    /// The latest `created` the snapshot holds: a claim whose process started later is newer than the snapshot.
    latest_created: Option<Timestamp>,
}

impl Snapshot {
    fn new(processes: Vec<ProcessInfo>) -> Self {
        let latest_created = processes.iter().filter_map(|process| process.created).max();
        Self {
            tree: ProcessTree::new(processes),
            latest_created,
        }
    }

    /// True when the claim is what its remote slot holds: its gate runs as the process the claim names, or, while the
    /// slot's mutex is `open`, the claim's pid is not in the snapshot and its process started after every process the
    /// snapshot holds, a gate newer than the snapshot. A snapshot with no process start at all holds no such gate.
    fn claim_holds(&self, claim: &ReadableClaim, open: bool) -> bool {
        let after_snapshot = self.tree.get(claim.pid).is_none() && self.latest_created.is_some_and(|created| claim.process_start > created);
        claim_live(claim, &self.tree) || (open && after_snapshot)
    }

    /// True when `pid` runs as a process created at `start_ms` Unix epoch milliseconds, to the millisecond, or as a
    /// process whose start cannot be read.
    fn started_at(&self, pid: u32, start_ms: u64) -> bool {
        self.tree
            .get(pid)
            .is_some_and(|process| process.created.is_none_or(|created| started_at_ms(created, start_ms)))
    }
}

impl RemoteGates {
    /// Adds the live tickets waiting for the remote pool in the v1 root's `waiting` folder `dir`, and lists the tickets
    /// of every kind that could not be read and are still there, or the folder when it exists but cannot be listed.
    fn add_v1_tickets(&mut self, dir: &Path, snapshot: &Snapshot) {
        let Ok(tickets) = v1::read_tickets(dir) else {
            self.unreadable.push(dir.to_owned());
            return;
        };
        for (path, ticket) in tickets {
            match ticket {
                Ok(ticket) => self.add_v1_ticket(path, &ticket, snapshot),
                // Windows refuses to open a file whose delete is pending: a ticket its gate is deleting is going away, as
                // `gate.exe` passes it over, not unreadable.
                Err(v1::FormatError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(_) => self.add_unreadable(path),
            }
        }
    }

    /// Adds the v1 ticket at `path` when it waits for the remote pool and its gate is live, or lists it when its
    /// `arrived` is no moment a `Timestamp` holds.
    fn add_v1_ticket(&mut self, path: PathBuf, ticket: &v1::Ticket, snapshot: &Snapshot) {
        if ticket.remote != Some(true) || !snapshot.started_at(ticket.pid, ticket.start_ms) {
            return;
        }
        match timestamp_of_ms(ticket.arrived) {
            Some(since) => self.gates.push(ticket_gate(ticket.pid, since)),
            None => self.unreadable.push(path),
        }
    }

    /// Adds the live tickets waiting for the remote pool in the PowerShell root's `waiting`, and lists the tickets that
    /// could not be read and are still there, any one of which may be a routed gate's, a live remote one whose start or
    /// arrival is outside the ticks a `DateTime` holds, or the folder when it exists but cannot be listed.
    fn add_powershell_tickets(&mut self, waiting: &Path, snapshot: &Snapshot) {
        let Ok(tickets) = read_ticket_files(waiting) else {
            self.unreadable.push(waiting.to_owned());
            return;
        };
        for (path, ticket) in tickets {
            let Some(ticket) = ticket else {
                self.unreadable.push(path);
                continue;
            };
            if !ticket.remote || !ticket_live(&ticket, &snapshot.tree) {
                continue;
            }
            match ticket_arrival(&ticket) {
                Some(since) => self.gates.push(ticket_gate(ticket.pid, since)),
                None => self.unreadable.push(path),
            }
        }
    }

    /// Adds the live remote-slot claims in the PowerShell root's `slots`, lists its unreadable remote-slot claims or the
    /// folder when it exists but cannot be listed, and gives the indexes of the remote slots its live claims hold.
    fn add_powershell_claims(&mut self, slots: &Path, snapshot: &Snapshot) -> BTreeSet<u32> {
        let mut held = BTreeSet::new();
        let Ok(claims) = try_read_claims(slots, "") else {
            self.unreadable.push(slots.to_owned());
            return held;
        };
        for index in self.remote_claim_indexes(slots, &claims) {
            if let Some(Claim::Readable(claim)) = claims.get(&format!("{REMOTE_SLOT_PREFIX}{index}"))
                && claim_live(claim, &snapshot.tree)
            {
                held.insert(index);
                self.gates.push(claimed(claim));
            }
        }
        held
    }

    /// Adds the v1 root's remote slots from its `claims`, read from `folder`, then its mutexes: each slot
    /// `0..remote_count` and each a claim names is held by its claim while that holds the slot
    /// ([`Snapshot::claim_holds`]), or, with its mutex open and no such claim, by a gate whose pid is not known, unless
    /// the slot is one a listed PowerShell-root claim holds through the same mutex.
    fn add_v1_slots(&mut self, slots: &RemoteSlots, folder: &Path, claims: &HashMap<String, Claim>, snapshot: &Snapshot) {
        let mut indexes: BTreeSet<u32> = (0..slots.remote_count).collect();
        indexes.extend(self.remote_claim_indexes(folder, claims));
        for index in indexes {
            let open = slots.is_open(index);
            match claims.get(&format!("{REMOTE_SLOT_PREFIX}{index}")) {
                Some(Claim::Readable(claim)) if snapshot.claim_holds(claim, open) => self.gates.push(claimed(claim)),
                _ if open && !slots.held_by_powershell.contains(&index) => self.gates.push(RemoteGate {
                    pid: None,
                    command: None,
                    since: None,
                    slot: Some(index),
                }),
                _ => {}
            }
        }
    }

    /// The slot indexes of the remote-slot claims in `claims`, read from `folder`. A claim that could not be read and is
    /// still there, or whose index is past a `u32`, is listed as unreadable; a claim of any other slot is not routed
    /// work and is left alone.
    fn remote_claim_indexes(&mut self, folder: &Path, claims: &HashMap<String, Claim>) -> BTreeSet<u32> {
        let mut indexes = BTreeSet::new();
        for (name, claim) in claims {
            let Some(digits) = name.strip_prefix(REMOTE_SLOT_PREFIX) else {
                continue;
            };
            let index = digits.parse::<u32>().ok();
            if index.is_none() || matches!(claim, Claim::Unreadable(_)) {
                self.add_unreadable(folder.join(format!("{name}.json")));
            }
            indexes.extend(index);
        }
        indexes
    }

    /// Lists the file at `path` as unreadable unless it is known to be gone: a file deleted between the listing and the
    /// read is a gate finishing.
    fn add_unreadable(&mut self, path: PathBuf) {
        if still_there(&path) {
            self.unreadable.push(path);
        }
    }
}

/// The routed gate a live claim names.
fn claimed(claim: &ReadableClaim) -> RemoteGate {
    RemoteGate {
        pid: Some(claim.pid),
        command: claim_command(claim),
        since: Some(claim_since(claim)),
        slot: None,
    }
}

/// The routed gate a live ticket names: its pid and its arrival, and no command.
fn ticket_gate(pid: u32, since: Timestamp) -> RemoteGate {
    RemoteGate {
        pid: Some(pid),
        command: None,
        since: Some(since),
        slot: None,
    }
}

/// What reading the remote cutover's drain marker gave ([`v1::remote_cutover_draining_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainMarker {
    /// The marker names a live drainer.
    Alive {
        /// The drainer's pid.
        pid: u32,
        /// The moment the drain began.
        since: Timestamp,
    },
    /// The marker names a pid that runs no process, or one whose start differs from the marker's `start`: the drain is
    /// not running.
    Dead,
    /// The marker could not be read, or its `since` is no moment a `Timestamp` holds.
    Unreadable,
}

/// The drain marker under the state root `root` ([`v1::remote_cutover_draining_path`]) judged against `processes`:
/// `None` when there is no marker or opening it is refused ([`v1::read_drain_record`]), else whether its drainer is
/// [`DrainMarker::Alive`] or [`DrainMarker::Dead`], or [`DrainMarker::Unreadable`]. A drainer is alive while the
/// marker's pid runs as a process created at the marker's `start` to the millisecond, as a `gate.exe` ticket's gate is
/// judged, or as a process whose start cannot be read.
#[must_use]
pub fn read_drain_marker(root: &Path, processes: &dyn ProcessTable) -> Option<DrainMarker> {
    let record = match v1::read_drain_record(&v1::remote_cutover_draining_path(root)) {
        Ok(record) => record?,
        Err(_) => return Some(DrainMarker::Unreadable),
    };
    let Some(since) = timestamp_of_ms(record.since) else {
        return Some(DrainMarker::Unreadable);
    };
    Some(if Snapshot::new(processes.snapshot()).started_at(record.pid, record.start) {
        DrainMarker::Alive { pid: record.pid, since }
    } else {
        DrainMarker::Dead
    })
}

/// The moment `ms` Unix epoch milliseconds name, or `None` past what a [`Timestamp`] holds.
fn timestamp_of_ms(ms: u64) -> Option<Timestamp> {
    Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok()
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

//! The dashboard state assembled from the claim files, the counts file, the process table and the slot mutexes.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use jiff::Timestamp;
use regex::Regex;

use crate::claims::{Claim, ReadableClaim, last_log_line, read_claims};
use crate::counts::{CountsFile, pool_count, read_counts_file, slot_count_path};
use crate::model::{CwdReader, DashboardState, MutexProbe, PoolState, ProcessInfo, ProcessTable, SlotKind, SlotRow, SlotState, StateInputs, Waiting};

/// How far a live process's start time may be from the one its claim records, in nanoseconds.
const CLAIM_START_TOLERANCE_NANOS: i128 = 2_000_000_000;

/// The state the gate dashboard page shows: each pool's counts, its slots and, when it is full, the gates waiting for
/// it.
#[must_use]
pub fn dashboard_state(inputs: &StateInputs, processes: &dyn ProcessTable, mutexes: &dyn MutexProbe, cwds: &dyn CwdReader) -> DashboardState {
    let table = Table::new(processes.snapshot());
    let claims = read_claims(&inputs.local_app_data.join("gate").join("slots"));
    let counts_path = slot_count_path(inputs);
    let counts_file = read_counts_file(&counts_path);
    let machine = Machine {
        table: &table,
        claims: &claims,
        mutexes,
    };
    let mut pools = Vec::new();
    let mut holders = HashSet::new();
    for kind in SlotKind::ALL {
        let pool = machine.pool_state(kind, &counts_file, &counts_path, inputs.processors);
        for slot in &pool.slots {
            if slot.state == SlotState::Busy
                && let Some(Claim::Readable(claim)) = claims.get(&slot.name)
            {
                holders.insert(claim.pid);
            }
        }
        pools.push(pool);
    }
    for pool in &mut pools {
        // A gate takes a free slot within 2 s, so with one free no gate of the kind is waiting.
        let full = !pool.slots.iter().any(|slot| slot.state == SlotState::Free && !slot.extra);
        if full {
            pool.waiting = table.waiting(pool.kind, &holders, cwds);
        }
    }
    DashboardState {
        now: iso_utc(inputs.now),
        machine: inputs.machine.clone(),
        processors: inputs.processors,
        pools,
    }
}

/// A timestamp as ISO 8601 UTC with seven fractional digits and `Z`, the gate dashboard's round-trip format.
fn iso_utc(timestamp: Timestamp) -> String {
    jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", timestamp).unwrap_or_else(|_| timestamp.to_string())
}

/// The process snapshot, indexed by pid and by parent.
struct Table {
    processes: Vec<ProcessInfo>,
    by_pid: HashMap<u32, usize>,
    children: HashMap<u32, Vec<u32>>,
}

impl Table {
    fn new(processes: Vec<ProcessInfo>) -> Self {
        let mut by_pid = HashMap::new();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (position, process) in processes.iter().enumerate() {
            by_pid.insert(process.pid, position);
            children.entry(process.parent_pid).or_default().push(process.pid);
        }
        Self { processes, by_pid, children }
    }

    fn get(&self, pid: u32) -> Option<&ProcessInfo> {
        self.by_pid.get(&pid).and_then(|position| self.processes.get(*position))
    }

    /// The number of processes under `root`, the root excluded, guarding against pid reuse making a cycle.
    fn descendant_count(&self, root: u32) -> usize {
        let mut found = HashSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(current) = queue.pop_front() {
            let Some(children) = self.children.get(&current) else {
                continue;
            };
            for child in children {
                if *child != root && found.insert(*child) {
                    queue.push_back(*child);
                }
            }
        }
        found.len()
    }

    /// True when the claim's pid is alive and is the process that wrote it: its start time is within 2 s of the claim's.
    fn claim_live(&self, claim: &ReadableClaim) -> bool {
        let Some(created) = self.get(claim.pid).and_then(|process| process.created) else {
            return false;
        };
        (created.as_nanosecond() - claim.process_start.as_nanosecond()).abs() < CLAIM_START_TOLERANCE_NANOS
    }

    /// True when the process has a child other than a console host, which a waiting gate does not have yet.
    fn has_real_child(&self, pid: u32) -> bool {
        let Some(children) = self.children.get(&pid) else {
            return false;
        };
        children
            .iter()
            .any(|child| self.get(*child).is_none_or(|process| !process.name.eq_ignore_ascii_case("conhost.exe")))
    }

    /// The gates waiting for a slot of the kind: gate processes with that `-Slot`, holding no live claim and with no
    /// child yet. A gate running under its parent's slot has a child, so it does not count.
    fn waiting(&self, kind: SlotKind, holders: &HashSet<u32>, cwds: &dyn CwdReader) -> Vec<Waiting> {
        let pattern = waiting_pattern(kind);
        let mut waiting = Vec::new();
        for (position, process) in self.processes.iter().enumerate() {
            if self.by_pid.get(&process.pid) != Some(&position) {
                continue;
            }
            let Some(command_line) = &process.command_line else {
                continue;
            };
            if !pattern.is_match(command_line) || holders.contains(&process.pid) || self.has_real_child(process.pid) {
                continue;
            }
            waiting.push(Waiting {
                pid: process.pid,
                since: process.created.map(iso_utc),
                cwd: cwds.current_dir(process.pid),
                command_line: command_line.clone(),
            });
        }
        waiting
    }
}

#[expect(
    clippy::expect_used,
    reason = "the pattern is built from a fixed kind name; the waiting tests compile both"
)]
fn waiting_pattern(kind: SlotKind) -> Regex {
    Regex::new(&format!(r"(?i)gate\.ps1\S*\s.*-Slot\s+{}\b", kind.as_str())).expect("the waiting pattern is a valid regex")
}

/// What one state reads from the machine.
struct Machine<'a> {
    table: &'a Table,
    claims: &'a HashMap<String, Claim>,
    mutexes: &'a dyn MutexProbe,
}

impl Machine<'_> {
    fn pool_state(&self, kind: SlotKind, counts_file: &CountsFile, counts_path: &std::path::Path, processors: usize) -> PoolState {
        let counts = pool_count(counts_file, counts_path, kind, processors);
        let count = counts.set.map_or(counts.default, crate::model::SlotCount::get);
        let mut indexes: BTreeSet<u32> = (0..count).collect();
        for name in self.claims.keys() {
            if let Some(index) = name.strip_prefix(kind.prefix()).and_then(|digits| digits.parse::<u32>().ok()) {
                indexes.insert(index);
            }
        }
        let mut slots = Vec::new();
        for index in indexes {
            let name = format!("{}{index}", kind.prefix());
            let mut row = self.slot_row(name, index);
            row.extra = index >= count;
            slots.push(row);
        }
        PoolState {
            kind,
            count,
            default_count: counts.default,
            set: counts.set,
            problem: counts.problem,
            slots,
            waiting: Vec::new(),
        }
    }

    fn slot_row(&self, name: String, index: u32) -> SlotRow {
        let claim = self.claims.get(&name);
        let mut row = SlotRow {
            index,
            name,
            state: SlotState::Free,
            claim: None,
            stale: None,
            processes: 0,
            last_line: None,
            extra: false,
        };
        if let Some(Claim::Readable(readable)) = claim
            && self.table.claim_live(readable)
        {
            row.state = SlotState::Busy;
            row.claim = Some(readable.value.clone());
            row.processes = self.table.descendant_count(readable.pid);
            row.last_line = last_log_line(readable.log.as_deref());
            return row;
        }
        row.stale = claim.map(|claim| claim.as_value().clone());
        if self.mutexes.is_open(&row.name) {
            row.state = SlotState::Held;
        }
        row
    }
}

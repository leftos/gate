//! The dashboard state assembled from the claim files, the arrival tickets, the counts file, the process table and the
//! slot mutexes.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use jiff::{SignedDuration, Timestamp};
use regex::Regex;

use crate::claims::{Claim, ReadableClaim, Ticket, last_log_line, read_claims, read_tickets};
use crate::counts::{CountsFile, pool_count, read_counts_file, slot_count_path};
use crate::model::{
    CriticalState, CwdReader, DashboardState, MutexProbe, PoolState, ProcessInfo, ProcessTable, QueueTicket, SlotKind, SlotRow, SlotState,
    StateInputs, Waiting,
};

/// How far a live process's start time may be from the one its claim records, in nanoseconds.
const CLAIM_START_TOLERANCE_NANOS: i128 = 2_000_000_000;
/// .NET ticks (100 ns) in a second.
const TICKS_PER_SECOND: i64 = 10_000_000;
/// .NET ticks from 0001-01-01 to the Unix epoch.
const UNIX_EPOCH_TICKS: i128 = 621_355_968_000_000_000;
/// The last .NET tick a `DateTime` can hold, 9999-12-31T23:59:59.9999999.
const MAX_TICKS: i64 = 3_155_378_975_999_999_999;
/// The `-Slot` value of a gate that asks for the machine.
const CRITICAL: &str = "critical";

/// The state the gate dashboard page shows: the critical request, the queue of arrival tickets, and each pool's counts,
/// its slots and, when it is full or a critical request bars it, the gates waiting for it.
#[must_use]
pub fn dashboard_state(inputs: &StateInputs, processes: &dyn ProcessTable, mutexes: &dyn MutexProbe, cwds: &dyn CwdReader) -> DashboardState {
    let table = Table::new(processes.snapshot());
    let gate_dir = inputs.local_app_data.join("gate");
    let claims = read_claims(&gate_dir.join("slots"), &inputs.slot_prefix);
    let counts_path = slot_count_path(inputs);
    let counts_file = read_counts_file(&counts_path);
    let machine = Machine {
        table: &table,
        claims: &claims,
        mutexes,
    };
    let tickets = read_tickets(&gate_dir.join(format!("{}waiting", inputs.slot_prefix)));
    let queue = table.queue(tickets, inputs.now, cwds);
    let mut holder = machine.slot_row(format!("{}gate-critical", inputs.slot_prefix), 0);
    holder.extra = None;
    let mut holders = HashSet::new();
    holders.extend(busy_pid(&holder));
    let mut pools = Vec::new();
    for kind in SlotKind::ALL {
        let pool = machine.pool_state(kind, inputs, &counts_file, &counts_path);
        holders.extend(pool.slots.iter().filter_map(busy_pid));
        pools.push(pool);
    }
    // A live critical ticket holds back the gates that arrive after it, so they wait with slots free.
    let barred = queue.iter().any(|ticket| ticket.kind.eq_ignore_ascii_case(CRITICAL));
    let critical_held = holder.state != SlotState::Free;
    for pool in &mut pools {
        // A gate takes a free slot within 2 s, so with one free no gate of the kind is waiting unless it is barred.
        let full = !pool.slots.iter().any(|slot| slot.state == SlotState::Free && slot.extra != Some(true));
        if full || barred {
            pool.waiting = table.waiting(pool.kind.as_str(), &holders, cwds);
        }
        if critical_held {
            for slot in &mut pool.slots {
                slot.critical = slot.state == SlotState::Held;
            }
        }
    }
    let waiting = table.waiting(CRITICAL, &holders, cwds);
    DashboardState {
        now: iso_utc(inputs.now),
        machine: inputs.machine.clone(),
        processors: inputs.processors,
        critical: CriticalState { holder, waiting },
        queue,
        pools,
    }
}

/// The pid of the gate holding a busy row, from its claim.
fn busy_pid(row: &SlotRow) -> Option<u32> {
    if row.state != SlotState::Busy {
        return None;
    }
    let pid = row.claim.as_ref()?.get("pid")?.as_u64()?;
    u32::try_from(pid).ok()
}

/// A timestamp as ISO 8601 UTC with seven fractional digits and `Z`, the gate dashboard's round-trip format.
fn iso_utc(timestamp: Timestamp) -> String {
    jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", timestamp).unwrap_or_else(|_| timestamp.to_string())
}

/// A timestamp as .NET ticks, truncated to the tick.
fn ticks_of(timestamp: Timestamp) -> i128 {
    timestamp.as_nanosecond().div_euclid(100) + UNIX_EPOCH_TICKS
}

/// .NET ticks as the gate dashboard's round-trip format, or `None` outside the range a `DateTime` holds.
fn format_ticks(ticks: i64) -> Option<String> {
    if !(0..=MAX_TICKS).contains(&ticks) {
        return None;
    }
    let nanos = i32::try_from((ticks % TICKS_PER_SECOND) * 100).ok()?;
    let since_year_one = SignedDuration::new(ticks / TICKS_PER_SECOND, nanos);
    let moment = jiff::civil::date(1, 1, 1).at(0, 0, 0, 0).checked_add(since_year_one).ok()?;
    jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", moment).ok()
}

/// The seconds from `arrived` to `now`, both in ticks, rounded to a whole number with halves to even, as `Math.Round`
/// rounds them.
#[expect(
    clippy::cast_precision_loss,
    reason = "TimeSpan.TotalSeconds divides the ticks as a double, so the same loss keeps the age equal"
)]
fn age_seconds(now: i128, arrived: i64) -> f64 {
    ((now - i128::from(arrived)) as f64 / 1e7).round_ties_even()
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

    /// The gates waiting for a slot of the kind (`heavy`, `light` or `critical`): gate processes with that `-Slot`,
    /// holding no live claim and with no child yet. A gate running under its parent's slot has a child, so it does not
    /// count.
    fn waiting(&self, kind: &str, holders: &HashSet<u32>, cwds: &dyn CwdReader) -> Vec<Waiting> {
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

    /// The live gates' tickets, oldest first; tickets that arrived together keep the folder's order.
    fn queue(&self, tickets: Vec<Ticket>, now: Timestamp, cwds: &dyn CwdReader) -> Vec<QueueTicket> {
        let now = ticks_of(now);
        let mut live: Vec<(i64, QueueTicket)> = tickets
            .into_iter()
            .filter_map(|ticket| Some((ticket.arrived_ticks, self.queue_row(ticket, now, cwds)?)))
            .collect();
        live.sort_by_key(|(arrived, _)| *arrived);
        live.into_iter().map(|(_, row)| row).collect()
    }

    /// The ticket's row, or `None` when its gate has ended (no process with its pid started within 2 s of its start
    /// ticks) or its arrival is no moment a `DateTime` holds.
    fn queue_row(&self, ticket: Ticket, now: i128, cwds: &dyn CwdReader) -> Option<QueueTicket> {
        let process = self.get(ticket.pid)?;
        let created = ticks_of(process.created?);
        if (created - i128::from(ticket.start_ticks)).abs() >= i128::from(2 * TICKS_PER_SECOND) {
            return None;
        }
        Some(QueueTicket {
            arrived: format_ticks(ticket.arrived_ticks)?,
            age_seconds: age_seconds(now, ticket.arrived_ticks),
            cwd: cwds.current_dir(ticket.pid),
            command_line: process.command_line.clone(),
            kind: ticket.kind,
            holds: ticket.holds,
            pid: ticket.pid,
        })
    }
}

#[expect(
    clippy::expect_used,
    reason = "the pattern is built from a fixed kind name; the waiting and critical tests compile each"
)]
fn waiting_pattern(kind: &str) -> Regex {
    Regex::new(&format!(r"(?i)gate\.ps1\S*\s.*-Slot\s+{kind}\b")).expect("the waiting pattern is a valid regex")
}

/// What one state reads from the machine.
struct Machine<'a> {
    table: &'a Table,
    claims: &'a HashMap<String, Claim>,
    mutexes: &'a dyn MutexProbe,
}

impl Machine<'_> {
    fn pool_state(&self, kind: SlotKind, inputs: &StateInputs, counts_file: &CountsFile, counts_path: &std::path::Path) -> PoolState {
        let counts = pool_count(counts_file, counts_path, kind, inputs.processors);
        let count = counts.set.map_or(counts.default, crate::model::SlotCount::get);
        let prefix = format!("{}{}", inputs.slot_prefix, kind.prefix());
        let lowered = prefix.to_ascii_lowercase();
        let mut indexes: BTreeSet<u32> = (0..count).collect();
        for name in self.claims.keys() {
            if let Some(index) = name.strip_prefix(&lowered).and_then(|digits| digits.parse::<u32>().ok()) {
                indexes.insert(index);
            }
        }
        let mut slots = Vec::new();
        for index in indexes {
            let mut row = self.slot_row(format!("{prefix}{index}"), index);
            row.extra = Some(index >= count);
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

    /// The row of the mutex `name`; its claim is looked up in any letter case, as the gate dashboard's table does.
    fn slot_row(&self, name: String, index: u32) -> SlotRow {
        let claim = self.claims.get(&name.to_ascii_lowercase());
        let mut row = SlotRow {
            index,
            name,
            state: SlotState::Free,
            claim: None,
            stale: None,
            processes: 0,
            last_line: None,
            critical: false,
            extra: Some(false),
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

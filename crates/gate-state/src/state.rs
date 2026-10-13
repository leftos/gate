//! The dashboard state assembled from the claim files, the arrival tickets, the counts file, the process table and the
//! slot mutexes, of the PowerShell gate or of `gate.exe` as the inputs' [`Generation`] says.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use proc_table::{ProcessInfo, ProcessTable, ProcessTree};
use regex::Regex;

use crate::claims::{
    Claim, ReadableClaim, TICKS_PER_SECOND, Ticket, UNIX_EPOCH_TICKS, claim_started_by, last_log_line, read_claims, read_tickets, started_at_ms,
    ticket_started_by, ticks_in_range, ticks_of,
};
use crate::counts::{PoolCount, pool_count, read_counts_file, state_counts_path};
use crate::model::{
    CoveredRun, CriticalState, CwdReader, DashboardState, Generation, MutexProbe, PoolState, QueueTicket, RemotePool, SlotKind, SlotRow, SlotState,
    StateInputs, Waiting,
};
use crate::remote::{RemoteCount, read_remote_count, remote_settings_path};
use crate::v1;

/// .NET ticks in a millisecond, the unit of `gate.exe`'s ticket times.
const TICKS_PER_MILLISECOND: i128 = 10_000;
/// The `-Slot` value of a gate that asks for the machine.
pub(crate) const CRITICAL: &str = "critical";

/// The state the Gates tab shows: the critical request, the queue of arrival tickets, each pool's counts, its slots
/// and, when it is full or a critical request bars it, the gates waiting for it; then the remote pool, when the gate's
/// `remote.json` or a remote claim file exists. A gate routed to the remote pool shows only as its queue ticket.
///
/// The claims, tickets and counts file are the PowerShell gate's under `gate\` or, for [`Generation::V1`], `gate.exe`'s
/// under `gate\v1\` and the slots' claims under the PowerShell root, where a missing folder is an empty state.
/// `gate.exe`'s waiting gates are its live tickets' gates; the PowerShell gate's are found by their command lines.
/// `gate.exe`'s gates running under a parent gate's slot are its live covered markers' gates; the PowerShell gate
/// writes no marker.
///
/// Past the cutover a slot's row also reads the claims under the PowerShell root, where a gate started through a stale
/// per-repo launcher writes its claim while holding a mutex `gate.exe` names: with the mutex open and no live claim of
/// this generation, such a claim is what the slot holds.
#[must_use]
pub fn dashboard_state(inputs: &StateInputs, processes: &dyn ProcessTable, mutexes: &dyn MutexProbe, cwds: &dyn CwdReader) -> DashboardState {
    let table = Table::new(processes.snapshot());
    let state_dir = inputs.generation.state_dir(&inputs.local_app_data);
    let claims = read_claims(&state_dir.join("slots"), &inputs.slot_prefix);
    let powershell_claims = match inputs.generation {
        Generation::PowerShell => HashMap::new(),
        Generation::V1 => read_claims(
            &Generation::PowerShell.state_dir(&inputs.local_app_data).join("slots"),
            &inputs.slot_prefix,
        ),
    };
    let machine = Machine {
        table: &table,
        claims: &claims,
        powershell_claims: &powershell_claims,
        mutexes,
    };
    let tickets = read_queue(inputs.generation, &state_dir.join(format!("{}waiting", inputs.slot_prefix)));
    let queue = table.queue(&tickets, inputs.now, cwds);
    let covered = table.covered(inputs.generation, &state_dir.join(format!("{}covered", inputs.slot_prefix)));
    let waiters = Waiters {
        generation: inputs.generation,
        table: &table,
        tickets: &tickets,
        cwds,
    };
    let mut holder = machine.slot_row(format!("{}gate-critical", inputs.slot_prefix), 0);
    holder.extra = None;
    let mut holders = HashSet::new();
    holders.extend(busy_pid(&holder));
    let mut pools = Vec::new();
    for (kind, counts) in SlotKind::ALL.into_iter().zip(pool_counts(inputs)) {
        let pool = machine.pool_state(kind, inputs, counts);
        holders.extend(pool.slots.iter().filter_map(busy_pid));
        pools.push(pool);
    }
    let remote = machine.remote_pool(inputs);
    holders.extend(remote.iter().flat_map(|pool| pool.slots.iter().filter_map(busy_pid)));
    // A gate routed to the remote pool keeps `-Slot heavy`: it shows only as its ticket in the queue, so it is held out
    // of the pools' waiting gates.
    let routed: HashSet<u32> = queue.iter().filter(|ticket| ticket.remote).map(|ticket| ticket.pid).collect();
    let held_out: HashSet<u32> = holders.union(&routed).copied().collect();
    // A live critical ticket holds back the gates that arrive after it, so they wait with slots free.
    let barred = queue.iter().any(|ticket| ticket.kind.eq_ignore_ascii_case(CRITICAL));
    let critical_held = holder.state != SlotState::Free;
    for pool in &mut pools {
        // A gate takes a free slot within 2 s, so with one free no gate of the kind is waiting unless it is barred.
        let full = !pool.slots.iter().any(|slot| slot.state == SlotState::Free && slot.extra != Some(true));
        if full || barred {
            pool.waiting = waiters.of(pool.kind.as_str(), &held_out);
        }
        if critical_held {
            for slot in &mut pool.slots {
                slot.critical = slot.state == SlotState::Held;
            }
        }
    }
    let waiting = waiters.of(CRITICAL, &holders);
    DashboardState {
        now: iso_utc(inputs.now),
        machine: inputs.machine.clone(),
        processors: inputs.processors,
        critical: CriticalState { holder, waiting },
        queue,
        pools,
        remote,
        covered,
    }
}

/// Each pool's counts, heavy then light, from the generation's counts file: the PowerShell gate's read leniently, or
/// `gate.exe`'s read strictly, so a file without `"v":1` is ignored as `gate.exe` ignores it.
fn pool_counts(inputs: &StateInputs) -> [PoolCount; 2] {
    let path = state_counts_path(inputs);
    match inputs.generation {
        Generation::PowerShell => {
            let file = read_counts_file(&path);
            SlotKind::ALL.map(|kind| pool_count(&file, &path, kind, inputs.processors))
        }
        Generation::V1 => v1::read_pool_counts(&path, inputs.processors),
    }
}

/// The arrival tickets in `dir`, in queue order: the PowerShell gate's by arrival, those that arrived together in the
/// folder's order; `gate.exe`'s by arrival in epoch milliseconds, then by pid ([`v1::Ticket::queue_order`]), their times
/// turned into ticks. A ticket that cannot be read, or is not of the generation's format, is left out, and a folder that
/// is missing or cannot be listed holds none.
fn read_queue(generation: Generation, dir: &Path) -> Vec<Ticket> {
    match generation {
        Generation::PowerShell => {
            let mut tickets = read_tickets(dir);
            tickets.sort_by_key(|ticket| ticket.arrived_ticks);
            tickets
        }
        Generation::V1 => {
            let Ok(read) = v1::read_tickets(dir) else {
                return Vec::new();
            };
            let mut tickets: Vec<v1::Ticket> = read.into_iter().filter_map(|(_, ticket)| ticket.ok()).collect();
            tickets.sort_by_key(v1::Ticket::queue_order);
            tickets.iter().filter_map(ticket_from_v1).collect()
        }
    }
}

/// A `gate.exe` ticket with its times in ticks, or `None` when one is past what ticks hold.
fn ticket_from_v1(ticket: &v1::Ticket) -> Option<Ticket> {
    Some(Ticket {
        kind: ticket.kind.as_str().to_owned(),
        holds: ticket.holds,
        pid: ticket.pid,
        start_ticks: ticks_of_ms(ticket.start_ms)?,
        arrived_ticks: ticks_of_ms(ticket.arrived)?,
        remote: ticket.remote == Some(true),
        exact_start_ms: Some(ticket.start_ms),
        expected: ticket.expected.clone(),
    })
}

/// A covered marker's row, its times as ISO text, or `None` when one is no moment a `DateTime` holds.
fn covered_row(marker: &v1::CoveredMarker) -> Option<CoveredRun> {
    Some(CoveredRun {
        pid: marker.pid,
        start: format_ticks(ticks_of_ms(marker.process_start)?)?,
        parent_pid: marker.parent_pid,
        command: marker.command.clone(),
        cwd: marker.cwd.clone(),
        since: format_ticks(ticks_of_ms(marker.since)?)?,
    })
}

/// Unix epoch milliseconds as .NET ticks, or `None` past what an `i64` holds.
fn ticks_of_ms(ms: u64) -> Option<i64> {
    i64::try_from(i128::from(ms) * TICKS_PER_MILLISECOND + UNIX_EPOCH_TICKS).ok()
}

/// Where the gates waiting for a slot come from: the PowerShell gate's command lines, or `gate.exe`'s tickets.
struct Waiters<'a> {
    generation: Generation,
    table: &'a Table,
    tickets: &'a [Ticket],
    cwds: &'a dyn CwdReader,
}

impl Waiters<'_> {
    /// The gates waiting for a slot of the kind (`heavy`, `light` or `critical`), leaving out the pids `held_out`.
    fn of(&self, kind: &str, held_out: &HashSet<u32>) -> Vec<Waiting> {
        match self.generation {
            Generation::PowerShell => self.table.waiting(kind, held_out, self.cwds),
            Generation::V1 => self.table.ticket_waiting(self.tickets, kind, held_out, self.cwds),
        }
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

/// A timestamp as ISO 8601 UTC with seven fractional digits and `Z`, .NET's round-trip `DateTime` format (`o`).
pub(crate) fn iso_utc(timestamp: Timestamp) -> String {
    jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", timestamp).unwrap_or_else(|_| timestamp.to_string())
}

/// .NET ticks as text in .NET's round-trip `DateTime` format (`o`), or `None` outside the range a `DateTime` holds.
fn format_ticks(ticks: i64) -> Option<String> {
    if !ticks_in_range(ticks) {
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
    tree: ProcessTree,
    children: HashMap<u32, Vec<u32>>,
    /// The latest `created` the snapshot holds, the moment a claim must start after to be newer than the table.
    latest_created: Option<Timestamp>,
}

impl Table {
    fn new(processes: Vec<ProcessInfo>) -> Self {
        let latest_created = processes.iter().filter_map(|process| process.created).max();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for process in &processes {
            children.entry(process.parent_pid).or_default().push(process.pid);
        }
        Self {
            tree: ProcessTree::new(processes),
            children,
            latest_created,
        }
    }

    fn get(&self, pid: u32) -> Option<&ProcessInfo> {
        self.tree.get(pid)
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
        self.get(claim.pid)
            .and_then(|process| process.created)
            .is_some_and(|created| claim_started_by(claim.process_start, created))
    }

    /// True when the claim's pid is absent from the snapshot and its start is later than every process the snapshot
    /// holds: a gate that started after the snapshot was taken, whose claim is newer than the table. A snapshot with no
    /// process start at all holds no such gate.
    fn claim_after_snapshot(&self, claim: &ReadableClaim) -> bool {
        self.get(claim.pid).is_none() && self.latest_created.is_some_and(|created| claim.process_start > created)
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
        for process in self.tree.iter() {
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

    /// The gates waiting for a slot of the kind, from `gate.exe`'s tickets: each live ticket of the kind that is local,
    /// does not hold the machine and is not `held_out`, its gate's start as `since` and its command line as the process
    /// table has it (empty when it could not be read).
    fn ticket_waiting(&self, tickets: &[Ticket], kind: &str, held_out: &HashSet<u32>, cwds: &dyn CwdReader) -> Vec<Waiting> {
        tickets
            .iter()
            .filter(|ticket| ticket.kind == kind && !ticket.holds && !ticket.remote && !held_out.contains(&ticket.pid))
            .filter_map(|ticket| {
                let process = self.ticket_process(ticket)?;
                Some(Waiting {
                    pid: ticket.pid,
                    since: format_ticks(ticket.start_ticks),
                    cwd: cwds.current_dir(ticket.pid),
                    command_line: process.command_line.clone().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The live gates' tickets, in the order given.
    fn queue(&self, tickets: &[Ticket], now: Timestamp, cwds: &dyn CwdReader) -> Vec<QueueTicket> {
        let now = ticks_of(now);
        tickets.iter().filter_map(|ticket| self.queue_row(ticket, now, cwds)).collect()
    }

    /// The process of the ticket's gate, or `None` when it has ended: no process with its pid started at the ticket's
    /// start, to the millisecond for a `gate.exe` ticket, within 2 s for a PowerShell one.
    fn ticket_process(&self, ticket: &Ticket) -> Option<&ProcessInfo> {
        let process = self.get(ticket.pid)?;
        let created = process.created?;
        let live = match ticket.exact_start_ms {
            Some(start_ms) => started_at_ms(created, start_ms),
            None => ticket_started_by(ticket.start_ticks, created),
        };
        live.then_some(process)
    }

    /// The live covered gates of `gate.exe`'s markers in `dir`, oldest marker first, then by pid: each marker whose pid
    /// runs as a process created at the marker's start, to the millisecond, as a `gate.exe` ticket's is judged. None for
    /// [`Generation::PowerShell`], and none when the folder is missing or cannot be listed. The reader deletes nothing.
    fn covered(&self, generation: Generation, dir: &Path) -> Vec<CoveredRun> {
        match generation {
            Generation::PowerShell => Vec::new(),
            Generation::V1 => {
                let Ok(read) = v1::read_covered(dir) else {
                    return Vec::new();
                };
                let mut live: Vec<v1::CoveredMarker> = read
                    .into_iter()
                    .map(|(_, marker)| marker)
                    .filter(|marker| {
                        let created = self.get(marker.pid).and_then(|process| process.created);
                        created.is_some_and(|created| started_at_ms(created, marker.process_start))
                    })
                    .collect();
                live.sort_by_key(|marker| (marker.since, marker.pid));
                live.iter().filter_map(covered_row).collect()
            }
        }
    }

    /// The ticket's row, or `None` when its gate has ended or its arrival is no moment a `DateTime` holds.
    fn queue_row(&self, ticket: &Ticket, now: i128, cwds: &dyn CwdReader) -> Option<QueueTicket> {
        let process = self.ticket_process(ticket)?;
        Some(QueueTicket {
            arrived: format_ticks(ticket.arrived_ticks)?,
            age_seconds: age_seconds(now, ticket.arrived_ticks),
            cwd: cwds.current_dir(ticket.pid),
            command_line: process.command_line.clone(),
            kind: ticket.kind.clone(),
            holds: ticket.holds,
            remote: ticket.remote,
            pid: ticket.pid,
            expected: ticket.expected.clone(),
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
    /// The PowerShell root's claims, empty for the PowerShell generation: past the cutover a gate started through a
    /// stale launcher writes there while holding one of this generation's mutexes.
    powershell_claims: &'a HashMap<String, Claim>,
    mutexes: &'a dyn MutexProbe,
}

impl Machine<'_> {
    fn pool_state(&self, kind: SlotKind, inputs: &StateInputs, counts: PoolCount) -> PoolState {
        let count = counts.set.map_or(counts.default, crate::model::SlotCount::get);
        let prefix = format!("{}{}", inputs.slot_prefix, kind.prefix());
        PoolState {
            kind,
            count,
            default_count: counts.default,
            set: counts.set,
            problem: counts.problem,
            slots: self.slot_rows(&prefix, count, self.claimed_indexes(&prefix)),
            waiting: Vec::new(),
        }
    }

    /// The remote pool, sized by the gate's `remote.json`; `None` while that file is absent and no remote claim file
    /// exists.
    fn remote_pool(&self, inputs: &StateInputs) -> Option<RemotePool> {
        let path = remote_settings_path(inputs);
        let prefix = format!("{}gate-remote-slot-", inputs.slot_prefix);
        let claimed = self.claimed_indexes(&prefix);
        let setting = read_remote_count(&path);
        if setting.is_none() && claimed.is_empty() {
            return None;
        }
        let RemoteCount { count, problem } = setting.unwrap_or_else(|| RemoteCount {
            count: 0,
            problem: Some(format!("no remote.json at {}", path.display())),
        });
        Some(RemotePool {
            count,
            problem,
            slots: self.slot_rows(&prefix, count, claimed),
        })
    }

    /// The indexes of the claim files whose name is `prefix` and a number, the prefix matched in any letter case.
    fn claimed_indexes(&self, prefix: &str) -> BTreeSet<u32> {
        let lowered = prefix.to_ascii_lowercase();
        self.claims
            .keys()
            .filter_map(|name| name.strip_prefix(&lowered).and_then(|digits| digits.parse::<u32>().ok()))
            .collect()
    }

    /// The rows of the slots `0..count` and of the `claimed` slots past it, which are `extra`.
    fn slot_rows(&self, prefix: &str, count: u32, claimed: BTreeSet<u32>) -> Vec<SlotRow> {
        let mut indexes: BTreeSet<u32> = (0..count).collect();
        indexes.extend(claimed);
        let mut slots = Vec::new();
        for index in indexes {
            let mut row = self.slot_row(format!("{prefix}{index}"), index);
            row.extra = Some(index >= count);
            slots.push(row);
        }
        slots
    }

    /// The row of the mutex `name`; its claim is looked up by the name in any letter case. A claim whose gate started
    /// after the snapshot was taken holds the slot while the mutex is open, and past the cutover so does a live claim
    /// under the PowerShell root when this generation has no live claim of its own.
    fn slot_row(&self, name: String, index: u32) -> SlotRow {
        let key = name.to_ascii_lowercase();
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
        let open = self.mutexes.is_open(&row.name);
        let claim = self.claims.get(&key);
        if let Some(Claim::Readable(readable)) = claim
            && self.claim_holds(readable, open)
        {
            return self.held_by(row, readable);
        }
        if open
            && let Some(Claim::Readable(readable)) = self.powershell_claims.get(&key)
            && self.claim_holds(readable, open)
        {
            return self.held_by(row, readable);
        }
        row.stale = claim.map(|claim| claim.as_value().clone());
        if open {
            row.state = SlotState::Held;
        }
        row
    }

    /// True when the claim is what the slot holds: the gate runs as the process the claim names, or, while the mutex is
    /// open, the gate started after the snapshot was taken and so is newer than the table.
    fn claim_holds(&self, claim: &ReadableClaim, open: bool) -> bool {
        self.table.claim_live(claim) || (open && self.table.claim_after_snapshot(claim))
    }

    /// The row of a slot a live claim holds: busy, with the claim, its gate's descendants and its log's last line.
    fn held_by(&self, mut row: SlotRow, claim: &ReadableClaim) -> SlotRow {
        row.state = SlotState::Busy;
        row.claim = Some(claim.value.clone());
        row.processes = self.table.descendant_count(claim.pid);
        row.last_line = last_log_line(claim.log.as_deref());
        row
    }
}

//! `gate cutover`: copies the PowerShell gate's slot counts into `gate.exe`'s default root, writes the cutover flag that
//! sends every new gate to `gate.exe`, then waits until no PowerShell gate is left, naming on stderr the gates it waits
//! on, those that finish and those that arrive. The drain is a pure poll of the PowerShell gate's claim and ticket
//! files; it takes and probes no mutex, and deletes nothing. It ends only once every scan over a confirmation window
//! has found no gate, since a PowerShell gate that takes its slot deletes its ticket before it writes its claim.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::{CutoverRequest, CutoverSeams};
use gate_state::{
    CountsCopy, Generation, PowerShellGate, PowerShellHold, PowerShellSlot, copy_slot_counts_to_v1, powershell_gates, write_cutover_flag,
};
use gate_win32::Interrupt;
use jiff::Timestamp;
use proc_table::{Win32ProcessTable, command_line_of};

use crate::answer::USAGE_STATUS;
use crate::console::Console;
use crate::heartbeat::Heartbeat;
use crate::request_json;
use crate::resolve::Environment;
use crate::run::CONTROL_C_EXIT;

/// How often the drain reads the claims and tickets again.
const POLL: Duration = Duration::from_secs(2);
/// How often the drain says it still waits.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How long every scan must find no PowerShell gate before the drain ends: wider than the gap between a PowerShell
/// gate's deleting its ticket and writing its claim, which waits up to 2 s for the journal's mutex.
const CONFIRM: Duration = Duration::from_secs(5);
/// The status of a cutover that could not copy the counts or write the flag.
const FAILED: u32 = 1;
/// What a gate's line says for a command its claim does not name and its process does not give.
const UNREADABLE_COMMAND: &str = "(command line unreadable)";

/// Runs `gate cutover` and returns its exit status.
pub(crate) fn cutover(request: &CutoverRequest, environment: &Environment, console: &mut Console) -> u32 {
    let Some(local_app_data) = environment.local_app_data.as_deref().filter(|value| !value.is_empty()) else {
        console.err("gate: cutover needs LOCALAPPDATA");
        return u32::from(USAGE_STATUS);
    };
    let local_app_data = Path::new(local_app_data);
    if request.print_request {
        console.out(&request_json::cutover(request, local_app_data));
        return 0;
    }
    let (flag, flag_time) = match hand_over(local_app_data, console) {
        Ok(written) => written,
        Err(status) => return status,
    };
    let interrupt = Interrupt::install()
        .inspect_err(|error| {
            console.err(&format!(
                "gate: cannot install the Ctrl+C handler, so Ctrl+C ends the gate at once: {error}"
            ));
        })
        .ok();
    if drain(local_app_data, flag_time, &request.seams, interrupt, console) == Drained::Interrupted {
        console.err("gate: cutover: interrupted; the flag stays set, and running gate cutover again resumes the wait");
        return CONTROL_C_EXIT;
    }
    console.err("gate: cutover: done; every gate now runs gate.exe");
    console.err(&format!("gate: cutover: to go back to the PowerShell gate, delete {}", flag.display()));
    0
}

/// Copies the slot counts, then writes the flag; the flag's path and the time taken just before it was written, or the
/// status of the step that failed, which was said.
fn hand_over(local_app_data: &Path, console: &mut Console) -> Result<(PathBuf, Timestamp), u32> {
    match copy_slot_counts_to_v1(local_app_data) {
        Err(error) => {
            console.err(&format!("gate: cutover: cannot copy the slot counts: {error}"));
            return Err(FAILED);
        }
        Ok(CountsCopy::Ignored { path, reason }) => console.err(&format!(
            "gate: cutover: ignored {}: {reason}; gates use the default counts",
            path.display()
        )),
        Ok(CountsCopy::Kept | CountsCopy::NoPowerShellCounts | CountsCopy::Copied(_)) => {}
    }
    let flag_time = Timestamp::now();
    match write_cutover_flag(local_app_data) {
        Ok(flag) => {
            console.err("gate: cutover: flag written; new gates now start gate.exe");
            Ok((flag, flag_time))
        }
        Err(error) => {
            console.err(&format!("gate: cutover: cannot write the cutover flag: {error}"));
            Err(FAILED)
        }
    }
}

/// How a drain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drained {
    /// Nothing is left to wait for: every scan over the confirmation window found no gate.
    Empty,
    /// The console was interrupted first.
    Interrupted,
}

/// Reads the PowerShell gates every poll and says what changed, until every scan over the confirmation window finds
/// none or the console is interrupted.
fn drain(local_app_data: &Path, flag_time: Timestamp, seams: &CutoverSeams, interrupt: Option<Interrupt>, console: &mut Console) -> Drained {
    let poll = seams.poll_ms.map_or(POLL, Duration::from_millis);
    let mut heartbeat = Heartbeat::new(seams.heartbeat_ms.map_or(HEARTBEAT, Duration::from_millis));
    let mut confirm = Confirm::new(seams.confirm_ms.map_or(CONFIRM, Duration::from_millis));
    let mut drain = Drain::new(flag_time, Generation::PowerShell.state_dir(local_app_data));
    let start = Instant::now();
    loop {
        let found = powershell_gates(local_app_data, &Win32ProcessTable);
        let gates = group(&found.gates);
        let listed = drain.listed;
        for line in drain.scan(&gates, found.unreadable, &mut command_line_of) {
            console.err(&line);
        }
        let elapsed = start.elapsed();
        if confirm.over(elapsed, gates.is_empty()) {
            return Drained::Empty;
        }
        if !listed
            && drain.listed
            && seams.interrupt_after_list
            && let Some(interrupt) = interrupt
        {
            interrupt.raise();
        }
        // A still-waiting line due while no gate is left is skipped, not held back for one that comes.
        if let Some(waited) = heartbeat.due(elapsed).filter(|_| !gates.is_empty()) {
            console.err(&still_waiting(gates.len(), waited));
        }
        if interrupt.is_some_and(Interrupt::is_raised) {
            return Drained::Interrupted;
        }
        thread::sleep(poll.min(heartbeat.until_due(start.elapsed())));
    }
}

/// Whether the drain is over: every scan over a window, from the first empty scan on, found no gate.
#[derive(Debug)]
struct Confirm {
    window: Duration,
    /// When the empty scans began, counted from the start of the drain; `None` while the last scan found a gate.
    empty_since: Option<Duration>,
}

impl Confirm {
    fn new(window: Duration) -> Confirm {
        Confirm { window, empty_since: None }
    }

    /// Whether the drain is over after a scan made `elapsed` into it that found no gate (`empty`) or some; a scan that
    /// finds one starts the window again.
    fn over(&mut self, elapsed: Duration, empty: bool) -> bool {
        if !empty {
            self.empty_since = None;
            return false;
        }
        let since = *self.empty_since.get_or_insert(elapsed);
        elapsed.saturating_sub(since) >= self.window
    }
}

/// One PowerShell gate: every claim and ticket of one pid, described together.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Gate {
    pid: u32,
    /// Its claims, then its tickets, each part as [`part`] says it, joined with `, `.
    what: String,
    /// Its claims' command; `None` when no claim names one, and its line reads its process's command line.
    command: Option<String>,
    /// The earliest `since` of its claims and tickets.
    since: Timestamp,
}

/// The gates `entries` hold, one per pid, by their earliest `since`, then by pid.
fn group(entries: &[PowerShellGate]) -> Vec<Gate> {
    let mut by_pid: BTreeMap<u32, Vec<&PowerShellGate>> = BTreeMap::new();
    for entry in entries {
        by_pid.entry(entry.pid).or_default().push(entry);
    }
    let mut gates = Vec::with_capacity(by_pid.len());
    for (pid, mine) in by_pid {
        let Some(since) = mine.iter().map(|entry| entry.since).min() else {
            continue;
        };
        let claims = mine.iter().filter(|entry| matches!(entry.what, PowerShellHold::Claim(_)));
        let tickets = mine.iter().filter(|entry| matches!(entry.what, PowerShellHold::Ticket { .. }));
        let what = claims.chain(tickets).map(|entry| part(&entry.what)).collect::<Vec<_>>().join(", ");
        let command = mine.iter().find_map(|entry| entry.command.clone());
        gates.push(Gate { pid, what, command, since });
    }
    gates.sort_by_key(|gate| (gate.since, gate.pid));
    gates
}

/// One claim or ticket, as a gate's line describes it.
fn part(hold: &PowerShellHold) -> String {
    match hold {
        PowerShellHold::Claim(PowerShellSlot::Heavy(index)) => format!("heavy slot {index}"),
        PowerShellHold::Claim(PowerShellSlot::Light(index)) => format!("light slot {index}"),
        PowerShellHold::Claim(PowerShellSlot::Remote(index)) => format!("remote slot {index}"),
        PowerShellHold::Claim(PowerShellSlot::Critical) => "critical claim".to_owned(),
        PowerShellHold::Ticket { holds: true, .. } => "holds the machine (critical)".to_owned(),
        PowerShellHold::Ticket { kind, holds: false } => format!("waiting ({kind})"),
    }
}

/// The command a line names for `gate`: its claims' command, else the command line `read_command` gives for its pid,
/// else [`UNREADABLE_COMMAND`].
fn command_text(gate: &Gate, read_command: &mut dyn FnMut(u32) -> Option<String>) -> String {
    gate.command
        .clone()
        .or_else(|| read_command(gate.pid))
        .unwrap_or_else(|| UNREADABLE_COMMAND.to_owned())
}

/// `count` and `noun`, the noun given an `s` unless the count is 1.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
}

/// The still-waiting line for `count` gates, `waited` into the drain.
fn still_waiting(count: usize, waited: Duration) -> String {
    format!(
        "gate: cutover: still waiting for {} ({} s)",
        counted(count, "PowerShell gate"),
        waited.as_secs()
    )
}

/// What the drain has said so far: whether it has listed the gates, the pids it waits on, every pid it has named, and
/// whether it has said that some files could not be read.
struct Drain {
    /// When the flag was written: a gate whose earliest `since` is later arrived after it.
    flag_time: Timestamp,
    /// The PowerShell gate's folder, `%LOCALAPPDATA%\gate`.
    folder: PathBuf,
    listed: bool,
    waiting: BTreeSet<u32>,
    named: BTreeSet<u32>,
    said_unreadable: bool,
}

impl Drain {
    fn new(flag_time: Timestamp, folder: PathBuf) -> Drain {
        Drain {
            flag_time,
            folder,
            listed: false,
            waiting: BTreeSet::new(),
            named: BTreeSet::new(),
            said_unreadable: false,
        }
    }

    /// The lines one scan says: the list of the gates, the first time any is found; afterwards each gate gone since the
    /// last scan and each new one; then, the first time some are, the files that could not be read. `read_command` is
    /// asked only for the gates named, whose claims name no command.
    fn scan(&mut self, gates: &[Gate], unreadable: usize, read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        let mut lines = if self.listed {
            self.changes(gates, read_command)
        } else {
            self.first_list(gates, read_command)
        };
        if unreadable > 0 && !self.said_unreadable {
            self.said_unreadable = true;
            lines.push(format!(
                "gate: cutover: cannot read {} under {}; they are not waited on",
                counted(unreadable, "claim or ticket file"),
                self.folder.display()
            ));
        }
        lines
    }

    fn first_list(&mut self, gates: &[Gate], read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        if gates.is_empty() {
            return Vec::new();
        }
        self.listed = true;
        let mut lines = vec![format!(
            "gate: cutover: waiting for {} to finish:",
            counted(gates.len(), "PowerShell gate")
        )];
        for gate in gates {
            let command = command_text(gate, read_command);
            lines.push(format!("gate:   pid {}  {}  {command}", gate.pid, gate.what));
            self.waiting.insert(gate.pid);
            self.named.insert(gate.pid);
        }
        lines
    }

    /// Each gate waited on that is gone, by pid, with how many are left once it is, then each gate not waited on yet, in
    /// scan order, named only the first time its pid is seen.
    fn changes(&mut self, gates: &[Gate], read_command: &mut dyn FnMut(u32) -> Option<String>) -> Vec<String> {
        let now: BTreeSet<u32> = gates.iter().map(|gate| gate.pid).collect();
        let gone: Vec<u32> = self.waiting.difference(&now).copied().collect();
        let mut lines = Vec::new();
        for pid in gone {
            self.waiting.remove(&pid);
            lines.push(format!("gate: cutover: pid {pid} finished ({} left)", self.waiting.len()));
        }
        for gate in gates {
            if self.waiting.insert(gate.pid) && self.named.insert(gate.pid) {
                lines.push(self.arrival(gate, &command_text(gate, read_command)));
            }
        }
        lines
    }

    fn arrival(&self, gate: &Gate, command: &str) -> String {
        if gate.since > self.flag_time {
            format!("gate: cutover: late PowerShell gate pid {} started after the flag: {command}", gate.pid)
        } else {
            format!("gate: cutover: also waiting for pid {}  {}  {command}", gate.pid, gate.what)
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that cannot be made must fail loudly")]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gate_state::{PowerShellGate, PowerShellHold, PowerShellSlot};
    use jiff::Timestamp;

    use super::{Confirm, Drain, Gate, group, still_waiting};

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).expect("a test second is a timestamp")
    }

    fn claim(pid: u32, slot: PowerShellSlot, command: Option<&str>, since: i64) -> PowerShellGate {
        PowerShellGate {
            pid,
            what: PowerShellHold::Claim(slot),
            command: command.map(str::to_owned),
            since: at(since),
        }
    }

    fn ticket(pid: u32, kind: &str, holds: bool, since: i64) -> PowerShellGate {
        PowerShellGate {
            pid,
            what: PowerShellHold::Ticket {
                kind: kind.to_owned(),
                holds,
            },
            command: None,
            since: at(since),
        }
    }

    fn gate(pid: u32, what: &str, command: &str, since: i64) -> Gate {
        Gate {
            pid,
            what: what.to_owned(),
            command: Some(command.to_owned()),
            since: at(since),
        }
    }

    /// A drain whose flag was written at second 100.
    fn drain() -> Drain {
        Drain::new(at(100), PathBuf::from(r"C:\appdata\gate"))
    }

    /// The lines `drain` says for a scan of `gates`, no process command line readable.
    fn said(drain: &mut Drain, gates: &[Gate], unreadable: usize) -> Vec<String> {
        drain.scan(gates, unreadable, &mut |_| None)
    }

    fn secs(seconds: f64) -> Duration {
        Duration::from_secs_f64(seconds)
    }

    #[test]
    fn a_critical_gates_claim_and_holding_ticket_are_one_gate() {
        let entries = [
            ticket(7, "critical", true, 10),
            claim(3, PowerShellSlot::Heavy(2), Some("cargo build"), 5),
            claim(7, PowerShellSlot::Critical, Some("cargo test"), 12),
        ];
        assert_eq!(
            group(&entries),
            [
                gate(3, "heavy slot 2", "cargo build", 5),
                gate(7, "critical claim, holds the machine (critical)", "cargo test", 10),
            ]
        );
    }

    #[test]
    fn gates_are_in_since_order_not_pid_order() {
        let entries = [
            claim(4, PowerShellSlot::Heavy(0), Some("later"), 2),
            claim(9, PowerShellSlot::Heavy(1), Some("earlier"), 1),
        ];
        let pids: Vec<u32> = group(&entries).into_iter().map(|gate| gate.pid).collect();
        assert_eq!(pids, [9, 4]);
    }

    #[test]
    fn each_part_is_described() {
        let entries = [
            claim(1, PowerShellSlot::Light(0), Some("a"), 1),
            claim(2, PowerShellSlot::Remote(3), Some("b"), 2),
            ticket(3, "heavy", false, 3),
        ];
        let described: Vec<String> = group(&entries).into_iter().map(|gate| gate.what).collect();
        assert_eq!(described, ["light slot 0", "remote slot 3", "waiting (heavy)"]);
    }

    #[test]
    fn a_gate_without_a_claimed_command_reads_its_process_once_named_or_says_it_cannot() {
        let mut asked = Vec::new();
        let mut read = |pid: u32| {
            asked.push(pid);
            (pid == 4).then(|| "pwsh -File gate.ps1 -Slot light -- dotnet test".to_owned())
        };
        let entries = [
            ticket(4, "light", false, 1),
            ticket(5, "heavy", false, 2),
            claim(6, PowerShellSlot::Heavy(0), Some("cargo build"), 3),
        ];
        let gates = group(&entries);
        let mut drain = drain();
        assert_eq!(
            drain.scan(&gates, 0, &mut read),
            [
                "gate: cutover: waiting for 3 PowerShell gates to finish:",
                "gate:   pid 4  waiting (light)  pwsh -File gate.ps1 -Slot light -- dotnet test",
                "gate:   pid 5  waiting (heavy)  (command line unreadable)",
                "gate:   pid 6  heavy slot 0  cargo build",
            ]
        );
        assert_eq!(drain.scan(&gates, 0, &mut read), Vec::<String>::new());
        assert_eq!(
            asked,
            [4, 5],
            "only the gates named are read, and a claim's own command is not read from its process"
        );
    }

    #[test]
    fn the_first_list_names_every_gate_in_the_order_given() {
        let mut drain = drain();
        let gates = [gate(9, "heavy slot 0", "cargo build", 1), gate(4, "waiting (light)", "dotnet test", 2)];
        assert_eq!(
            said(&mut drain, &gates, 0),
            [
                "gate: cutover: waiting for 2 PowerShell gates to finish:",
                "gate:   pid 9  heavy slot 0  cargo build",
                "gate:   pid 4  waiting (light)  dotnet test",
            ]
        );
        assert_eq!(said(&mut drain, &gates, 0), Vec::<String>::new(), "an unchanged scan says nothing");
    }

    #[test]
    fn one_gate_is_counted_in_the_singular() {
        let mut drain = drain();
        assert_eq!(
            said(&mut drain, &[gate(9, "heavy slot 0", "cargo build", 1)], 0)[0],
            "gate: cutover: waiting for 1 PowerShell gate to finish:"
        );
        assert_eq!(
            still_waiting(1, Duration::from_millis(30_500)),
            "gate: cutover: still waiting for 1 PowerShell gate (30 s)"
        );
        assert_eq!(
            still_waiting(3, Duration::from_secs(60)),
            "gate: cutover: still waiting for 3 PowerShell gates (60 s)"
        );
    }

    #[test]
    fn no_gate_at_the_first_scan_says_nothing() {
        let mut drain = drain();
        assert_eq!(said(&mut drain, &[], 0), Vec::<String>::new());
    }

    #[test]
    fn each_finished_gate_says_how_many_are_left() {
        let mut drain = drain();
        let a = gate(1, "heavy slot 0", "a", 1);
        let b = gate(2, "heavy slot 1", "b", 2);
        let c = gate(3, "light slot 0", "c", 3);
        said(&mut drain, &[a, b, c.clone()], 0);
        assert_eq!(
            said(&mut drain, std::slice::from_ref(&c), 0),
            ["gate: cutover: pid 1 finished (2 left)", "gate: cutover: pid 2 finished (1 left)"]
        );
        assert_eq!(said(&mut drain, &[], 0), ["gate: cutover: pid 3 finished (0 left)"]);
    }

    #[test]
    fn a_scan_says_the_finished_before_the_new_and_counts_only_the_gates_already_waited_on() {
        let mut drain = drain();
        let two = gate(2, "heavy slot 1", "b", 2);
        said(&mut drain, &[gate(1, "heavy slot 0", "a", 1), two.clone()], 0);
        assert_eq!(
            said(&mut drain, &[two, gate(3, "light slot 0", "c", 3)], 0),
            [
                "gate: cutover: pid 1 finished (1 left)",
                "gate: cutover: also waiting for pid 3  light slot 0  c",
            ]
        );
    }

    #[test]
    fn a_new_gate_is_late_only_when_it_came_after_the_flag() {
        let mut drain = drain();
        said(&mut drain, &[gate(1, "heavy slot 0", "a", 50)], 0);
        let before = gate(2, "waiting (heavy)", "pwsh before", 99);
        let at_the_flag = gate(3, "waiting (light)", "pwsh at", 100);
        let after = gate(4, "light slot 1", "pwsh after", 101);
        assert_eq!(
            said(&mut drain, &[gate(1, "heavy slot 0", "a", 50), before, at_the_flag, after], 0),
            [
                "gate: cutover: also waiting for pid 2  waiting (heavy)  pwsh before",
                "gate: cutover: also waiting for pid 3  waiting (light)  pwsh at",
                "gate: cutover: late PowerShell gate pid 4 started after the flag: pwsh after",
            ]
        );
    }

    #[test]
    fn each_pid_is_named_once() {
        let mut drain = drain();
        let first = gate(1, "heavy slot 0", "a", 50);
        let late = gate(2, "heavy slot 1", "b", 150);
        said(&mut drain, std::slice::from_ref(&first), 0);
        assert_eq!(
            said(&mut drain, &[first.clone(), late.clone()], 0),
            ["gate: cutover: late PowerShell gate pid 2 started after the flag: b"]
        );
        assert_eq!(
            said(&mut drain, std::slice::from_ref(&first), 0),
            ["gate: cutover: pid 2 finished (1 left)"]
        );
        assert_eq!(
            said(&mut drain, &[first.clone(), late], 0),
            Vec::<String>::new(),
            "a pid already named joins the wait again unnamed"
        );
        assert_eq!(
            said(&mut drain, std::slice::from_ref(&first), 0),
            ["gate: cutover: pid 2 finished (1 left)"]
        );
    }

    #[test]
    fn unreadable_files_are_said_once() {
        let mut drain = drain();
        let a = gate(1, "heavy slot 0", "a", 1);
        assert_eq!(said(&mut drain, std::slice::from_ref(&a), 0).len(), 2, "no file was unreadable yet");
        assert_eq!(
            said(&mut drain, std::slice::from_ref(&a), 2),
            [r"gate: cutover: cannot read 2 claim or ticket files under C:\appdata\gate; they are not waited on"]
        );
        assert_eq!(said(&mut drain, std::slice::from_ref(&a), 3), Vec::<String>::new());
        let mut other = Drain::new(at(100), PathBuf::from(r"C:\appdata\gate"));
        assert_eq!(
            said(&mut other, &[], 1),
            [r"gate: cutover: cannot read 1 claim or ticket file under C:\appdata\gate; they are not waited on"]
        );
    }

    #[test]
    fn the_unreadable_line_follows_the_first_list() {
        let mut drain = drain();
        assert_eq!(
            said(&mut drain, &[gate(1, "heavy slot 0", "a", 1)], 1),
            [
                "gate: cutover: waiting for 1 PowerShell gate to finish:",
                "gate:   pid 1  heavy slot 0  a",
                r"gate: cutover: cannot read 1 claim or ticket file under C:\appdata\gate; they are not waited on",
            ]
        );
    }

    #[test]
    fn the_drain_ends_after_a_whole_window_of_empty_scans() {
        let mut confirm = Confirm::new(Duration::from_secs(5));
        assert!(!confirm.over(secs(10.0), false));
        assert!(!confirm.over(secs(10.0), true), "the window starts at the first empty scan");
        assert!(!confirm.over(secs(14.9), true));
        assert!(confirm.over(secs(15.0), true), "exactly the window ends the drain");
    }

    #[test]
    fn an_empty_first_scan_waits_out_the_window() {
        let mut confirm = Confirm::new(Duration::from_secs(5));
        assert!(!confirm.over(secs(0.0), true));
        assert!(!confirm.over(secs(2.0), true));
        assert!(confirm.over(secs(5.0), true));
    }

    #[test]
    fn a_gate_found_inside_the_window_starts_it_again() {
        let mut confirm = Confirm::new(Duration::from_secs(5));
        assert!(!confirm.over(secs(0.0), false));
        assert!(!confirm.over(secs(2.0), true));
        assert!(!confirm.over(secs(4.0), false), "a gap shorter than the window");
        assert!(!confirm.over(secs(6.0), true));
        assert!(!confirm.over(secs(8.0), true), "counted from 6 s, not 2 s");
        assert!(!confirm.over(secs(10.9), true));
        assert!(confirm.over(secs(11.0), true));
    }
}

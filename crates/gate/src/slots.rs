//! A gate's slot: the gate's own identity, the pool its kind takes from, whether the gates the state root's tickets
//! and claims name still run, the take with its claim and its takeover from a gate that died holding the slot, and
//! the waking of the waiters.

use std::collections::HashSet;
use std::fmt::Display;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use gate_args::{RunRequest, Slot};
use gate_state::v1::{self, Claim, ClaimKind, Counts, FormatError, PoolSetting, Ticket, TicketKind};
use gate_state::{JournalIdentity, KernelObject, RemoteTag, SlotKind, reserved_light};
use gate_win32::{NamedEvent, OwnedMutexGuard, mutex_exists};
use jiff::Timestamp;

use crate::acquire;
use crate::console::Console;
use crate::resolve::ResolvedRoot;

/// The pool of slots a gate takes from: this machine's pool of one kind, or the builder's remote pool.
#[derive(Clone, Copy)]
pub(crate) enum Pool {
    Local(SlotKind),
    /// The remote pool, of this many slots, whose waiters' tickets are heavy and marked remote.
    Remote(u32),
}

impl Pool {
    /// The pool a gate of `slot` takes from; `None` for a critical gate, which takes the machine ([`crate::machine`]).
    pub(crate) fn of(slot: Slot) -> Option<Pool> {
        match slot {
            Slot::Heavy => Some(Pool::Local(SlotKind::Heavy)),
            Slot::Light => Some(Pool::Local(SlotKind::Light)),
            Slot::Critical => None,
        }
    }

    /// The kernel object of the pool's slot `index`.
    pub(crate) fn object(self, index: u32) -> KernelObject {
        match self {
            Pool::Local(SlotKind::Heavy) => KernelObject::HeavySlot(index),
            Pool::Local(SlotKind::Light) => KernelObject::LightSlot(index),
            Pool::Remote(_) => KernelObject::RemoteSlot(index),
        }
    }

    /// The kind the tickets of the pool's waiters carry.
    pub(crate) fn ticket_kind(self) -> TicketKind {
        match self {
            Pool::Local(SlotKind::Heavy) | Pool::Remote(_) => TicketKind::Heavy,
            Pool::Local(SlotKind::Light) => TicketKind::Light,
        }
    }

    /// Whether the pool is the remote one.
    pub(crate) fn is_remote(self) -> bool {
        matches!(self, Pool::Remote(_))
    }

    /// The kind of the claim a gate holding a slot of the pool writes.
    pub(crate) fn claim_kind(self) -> ClaimKind {
        match self {
            Pool::Local(SlotKind::Heavy) => ClaimKind::Heavy,
            Pool::Local(SlotKind::Light) => ClaimKind::Light,
            Pool::Remote(_) => ClaimKind::Remote,
        }
    }

    /// The pool's kind as the wait lines name it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Pool::Local(kind) => kind.as_str(),
            Pool::Remote(_) => "remote",
        }
    }

    /// The threads one gate of the pool is taken to keep busy here; read of a local pool, where a light command is
    /// measured against them.
    pub(crate) fn threads(self) -> usize {
        match self {
            Pool::Local(kind) => kind.threads(),
            Pool::Remote(_) => unreachable!("nothing reads the threads of the remote pool"),
        }
    }
}

/// The setting of the local pool of `kind` in `counts`.
pub(crate) fn setting(kind: SlotKind, counts: &Counts) -> &PoolSetting {
    match kind {
        SlotKind::Heavy => &counts.heavy,
        SlotKind::Light => &counts.light,
    }
}

/// This gate as its journal lines, its ticket, its claim and its wake event name it, read once a run.
pub(crate) struct ThisGate {
    pub pid: u32,
    /// The gate's start, or the time it was read at when the start could not be.
    pub start: Timestamp,
    /// `start` in Unix epoch milliseconds.
    pub start_ms: u64,
    /// Why the start could not be read, which keeps the journal from being written.
    start_error: Option<String>,
    kind: &'static str,
    cwd: Result<String, String>,
    /// The command's words as given, each quoted for the C runtime and joined by spaces ([`gate_args::command_text`]).
    command: String,
}

impl ThisGate {
    /// This gate, running `request`.
    pub(crate) fn read(request: &RunRequest) -> ThisGate {
        let (start, start_error) = match gate_win32::process_start() {
            Ok(start) => (start, None),
            Err(error) => (Timestamp::now(), Some(format!("cannot read this gate's start time: {error}"))),
        };
        let cwd = std::env::current_dir()
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .map_err(|error| format!("cannot read the working directory: {error}"));
        ThisGate {
            pid: std::process::id(),
            start,
            start_ms: epoch_ms(start),
            start_error,
            kind: request.slot.as_str(),
            cwd,
            command: gate_args::command_text(&request.command),
        }
    }

    /// What every journal line of this gate carries, or why the journal cannot be written.
    pub(crate) fn journal_identity(&self) -> Result<JournalIdentity, String> {
        if let Some(error) = &self.start_error {
            return Err(error.clone());
        }
        Ok(JournalIdentity {
            pid: self.pid,
            process_start: self.start,
            kind: self.kind.to_owned(),
            cwd: self.cwd.clone()?,
            command: self.command.clone(),
        })
    }
}

/// `time` in whole Unix epoch milliseconds. A gate's times are all after the epoch, where truncation is the floor.
pub(crate) fn epoch_ms(time: Timestamp) -> u64 {
    time.as_millisecond().unsigned_abs()
}

/// A slot this gate holds: its mutex, released when this is dropped, its index and where its claim goes.
pub(crate) struct Held {
    guard: OwnedMutexGuard,
    index: u32,
    claim: Option<PathBuf>,
}

impl Held {
    /// The slot's full mutex name.
    pub(crate) fn name(&self) -> &str {
        self.guard.name()
    }
}

/// A slot just taken, and whether a gate that died holding it had left it held.
pub(crate) struct Taken {
    pub held: Held,
    pub abandoned: bool,
}

/// Whether a gate a ticket, a claim or a slot marker names still runs, as its pid's start tells.
#[derive(Debug)]
pub(crate) enum Liveness {
    /// A process of the pid runs, started when the gate did.
    Live,
    /// No process of the pid runs since the gate's start, or the pid is another user's process.
    Gone,
    /// The pid's start cannot be read, for this reason.
    Unknown(io::Error),
}

/// The liveness of the gate `pid` started at `start_ms` in Unix epoch milliseconds, its pid's start read on this machine.
pub(crate) fn gate_liveness(pid: u32, start_ms: u64) -> Liveness {
    liveness(pid, start_ms, gate_win32::process_start_of)
}

/// The liveness of the gate `pid` started at `start_ms`, its pid's start read through `start_of`, which gives `None` for
/// a pid no process runs as.
fn liveness(pid: u32, start_ms: u64, start_of: impl FnOnce(u32) -> io::Result<Option<Timestamp>>) -> Liveness {
    match start_of(pid) {
        Ok(Some(start)) if epoch_ms(start) == start_ms => Liveness::Live,
        Ok(_) => Liveness::Gone,
        // A gate of this user, elevated or not, opens with limited query rights. A pid that does not is another
        // user's process, such as a service that reused a dead gate's pid, so the gate it names is gone.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Liveness::Gone,
        Err(error) => Liveness::Unknown(error),
    }
}

/// What this gate reads of the other gates under its state root: their tickets and claims, judged live or dead, with
/// each problem in reading them said once a wait for each name or path.
pub(crate) struct Survey {
    /// This gate's own pid and start, which it never judges.
    own: (u32, u64),
    /// The problem lines said already, each by what it says and of which name or path.
    said: HashSet<String>,
}

impl Survey {
    pub(crate) fn new(gate: &ThisGate) -> Survey {
        Survey {
            own: (gate.pid, gate.start_ms),
            said: HashSet::new(),
        }
    }

    /// Says `gate: <what> <subject>: <detail>`, unless this wait said `what` of `subject` already.
    pub(crate) fn say_once(&mut self, what: &str, subject: &str, detail: impl Display, console: &mut Console) {
        if self.said.insert(format!("{what} {subject}")) {
            console.err(&format!("gate: {what} {subject}: {detail}"));
        }
    }

    /// Whether the gate `pid` started at `start_ms`, named by the file at `path`, still runs. This gate's own ticket
    /// counts as live unjudged. A gate that cannot be checked counts as live, said once for `path`: a gate never
    /// deletes what it cannot judge.
    fn is_live(&mut self, path: &Path, pid: u32, start_ms: u64, console: &mut Console) -> bool {
        if (pid, start_ms) == self.own {
            return true;
        }
        match gate_liveness(pid, start_ms) {
            Liveness::Live => true,
            Liveness::Gone => false,
            Liveness::Unknown(error) => {
                self.say_once("cannot check the gate of", &path.display().to_string(), error, console);
                true
            }
        }
    }

    /// The live tickets under `root`, and whether a dead one was deleted on the way. An unreadable ticket is said once
    /// and left in place. A tickets folder that cannot be read holds none, said once.
    pub(crate) fn live_tickets(&mut self, root: &Path, console: &mut Console) -> (Vec<Ticket>, bool) {
        let dir = v1::tickets_dir(root);
        let listed = match v1::read_tickets(&dir) {
            Ok(listed) => listed,
            Err(error) => {
                self.say_once("cannot read the tickets in", &dir.display().to_string(), error, console);
                return (Vec::new(), false);
            }
        };
        let mut live = Vec::new();
        let mut deleted = false;
        for (path, read) in listed {
            let shown = path.display().to_string();
            match read {
                // Windows refuses to open a file whose delete is pending, so a ticket its gate is deleting as this gate
                // reads it comes back access denied: it is going away, not unreadable, and is passed over unsaid. A
                // skipped ticket never counts as ahead, so one that stays denied cannot block the queue either.
                Err(FormatError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(reason) => self.say_once("ignored the ticket", &shown, reason, console),
                Ok(ticket) if self.is_live(&path, ticket.pid, ticket.start_ms, console) => live.push(ticket),
                Ok(_) => match delete(&path) {
                    Ok(()) => deleted = true,
                    Err(error) => self.say_once("cannot delete the dead gate's ticket", &shown, error, console),
                },
            }
        }
        (live, deleted)
    }

    /// The live tickets under `root` of the other gates, this gate's own left out, and whether a dead one was deleted
    /// on the way, as [`Survey::live_tickets`] reads them.
    pub(crate) fn others(&mut self, root: &Path, console: &mut Console) -> (Vec<Ticket>, bool) {
        let (live, deleted) = self.live_tickets(root, console);
        (others(live, self.own), deleted)
    }

    /// The live claims under `root` of the other gates, each with its file name, sorted by it: only the files a claim
    /// is named as ([`is_claim_file`]), this gate's own claims and a claim that cannot be read left out. An entry of
    /// the folder that cannot be read is said once and passed over. Else why the claims folder cannot be listed; a
    /// missing one holds none.
    pub(crate) fn other_claims(&mut self, root: &Path, console: &mut Console) -> io::Result<Vec<(String, Claim)>> {
        let dir = v1::slots_dir(root);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut claims = Vec::new();
        for entry in entries {
            let entry = match entry.and_then(|entry| entry.file_type().map(|kind| (entry, kind))) {
                Ok((entry, kind)) if kind.is_file() && is_claim_file(&entry.file_name().to_string_lossy()) => entry,
                Ok(_) => continue,
                Err(error) => {
                    self.say_once("cannot read the claims in", &dir.display().to_string(), error, console);
                    continue;
                }
            };
            let path = entry.path();
            let Ok(claim) = v1::read_claim(&path) else { continue };
            let start_ms = epoch_ms(claim.process_start);
            if (claim.pid, start_ms) != self.own && self.is_live(&path, claim.pid, start_ms, console) {
                claims.push((entry.file_name().to_string_lossy().into_owned(), claim));
            }
        }
        claims.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(claims)
    }

    /// The pid and slot index of each other gate's live light claim under `root`, as [`Survey::other_claims`] reads
    /// them. A claims folder that cannot be listed holds none, said once.
    pub(crate) fn light_claims(&mut self, root: &Path, console: &mut Console) -> Vec<(u32, u32)> {
        match self.other_claims(root, console) {
            Ok(claims) => claims
                .into_iter()
                .filter(|(_, claim)| claim.kind == ClaimKind::Light)
                .map(|(_, claim)| (claim.pid, claim.index))
                .collect(),
            Err(error) => {
                self.say_once("cannot read the claims in", &v1::slots_dir(root).display().to_string(), error, console);
                Vec::new()
            }
        }
    }

    /// The pid and start, in epoch milliseconds, of every gate a live claim under `root` names: the gates that hold a
    /// slot. An unreadable claim names none. A claims folder that cannot be read holds none, said once.
    pub(crate) fn holders(&mut self, root: &Path, console: &mut Console) -> HashSet<(u32, u64)> {
        let dir = v1::slots_dir(root);
        let mut holders = HashSet::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return holders,
            Err(error) => {
                self.say_once("cannot read the claims in", &dir.display().to_string(), error, console);
                return holders;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    self.say_once("cannot read the claims in", &dir.display().to_string(), error, console);
                    continue;
                }
            };
            let Ok(claim) = v1::read_claim(&path) else { continue };
            let start_ms = epoch_ms(claim.process_start);
            if self.is_live(&path, claim.pid, start_ms, console) {
                holders.insert((claim.pid, start_ms));
            }
        }
        holders
    }

    /// Whether the claim at `path` names a gate that is no longer alive.
    fn left_by_a_dead_gate(&mut self, path: &Path, console: &mut Console) -> bool {
        // An unreadable or missing claim is no takeover; the gate overwrites it with its own claim.
        let Ok(claim) = v1::read_claim(path) else {
            return false;
        };
        !self.is_live(path, claim.pid, epoch_ms(claim.process_start), console)
    }
}

/// Tries the pool's slots `indices` in order and takes the first free one. The take is a takeover when the mutex
/// comes back abandoned, or when the slot's claim names a gate no longer alive. A slot that cannot be tried is said
/// once and passed over.
pub(crate) fn try_pool(root: &ResolvedRoot, pool: Pool, indices: Range<u32>, survey: &mut Survey, console: &mut Console) -> Option<Taken> {
    indices
        .into_iter()
        .find_map(|index| try_object(root, pool.object(index), index, survey, console))
}

/// Whether a critical request is pending among the other gates' live tickets `others`: a critical ticket of theirs, or
/// else the critical mutex open, which `critical_open` reads only when no such ticket settles it.
pub(crate) fn critical_pending(others: &[Ticket], critical_open: impl FnOnce() -> bool) -> bool {
    others.iter().any(|ticket| ticket.kind == TicketKind::Critical) || critical_open()
}

/// Whether a critical request among the other gates' live tickets `others` holds the machine, as its ticket says.
pub(crate) fn critical_holding(others: &[Ticket]) -> bool {
    others.iter().any(|ticket| ticket.kind == TicketKind::Critical && ticket.holds)
}

/// Whether some gate has the critical mutex under `root` open. A mutex that cannot be probed is said once and counts as
/// not open.
pub(crate) fn critical_open(root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) -> bool {
    let name = root.root.object_name(KernelObject::Critical);
    mutex_exists(&name)
        .inspect_err(|error| survey.say_once("cannot probe", &name, error, console))
        .unwrap_or(false)
}

/// Whether the light slot `index` of a count of `light` is one no critical gate takes: one of the reserved slots, the
/// highest `min(2, light)`, or one past the count.
pub(crate) fn on_reserved_light(index: u32, light: u32) -> bool {
    index >= light - reserved_light(light)
}

/// How many light gates run beside a critical request, out of `count` light slots. While it waits for the machine, the
/// `reserved_open` reserved slots some gate has open: the light gates on the other slots are ones its drain waits for.
/// Once it is `holding` the machine, those and every other gate's live light claim of `claims`, by slot index, below
/// the reserved slots or past the count, where a count changed since they took it left them.
pub(crate) fn light_beside(count: u32, holding: bool, reserved_open: usize, claims: &[u32]) -> usize {
    if !holding {
        return reserved_open;
    }
    let first = count - reserved_light(count);
    reserved_open + claims.iter().filter(|index| **index < first || **index >= count).count()
}

/// Tries the mutex of `object`, the slot `index` of its pool or the critical mutex at index 0, once, and takes it when
/// it is free. The take is a takeover when the mutex comes back abandoned, or when its claim names a gate no longer
/// alive. A mutex that cannot be tried is said once and counts as not free.
pub(crate) fn try_object(root: &ResolvedRoot, object: KernelObject, index: u32, survey: &mut Survey, console: &mut Console) -> Option<Taken> {
    let name = root.root.object_name(object);
    match acquire::try_slot(&name) {
        Ok(Some(guard)) => {
            let claim = v1::claim_path(&root.path, object);
            let abandoned = guard.abandoned() || claim.as_deref().is_some_and(|path| survey.left_by_a_dead_gate(path, console));
            Some(Taken {
                held: Held { guard, index, claim },
                abandoned,
            })
        }
        Ok(None) => None,
        Err(error) => {
            survey.say_once("could not take", &name, error, console);
            None
        }
    }
}

/// Whether a file of the claims folder named `name` is a claim of this root: `gate-<slot>.json`, as
/// [`KernelObject::claim_file_name`] names them, so a claim's `*.tmp` file and another prefix's claim are not.
fn is_claim_file(name: &str) -> bool {
    name.starts_with("gate-") && Path::new(name).extension().is_some_and(|extension| extension == "json")
}

/// `tickets` less the one of the gate `own`, its pid and start in epoch milliseconds.
pub(crate) fn others(tickets: Vec<Ticket>, own: (u32, u64)) -> Vec<Ticket> {
    tickets.into_iter().filter(|ticket| (ticket.pid, ticket.start_ms) != own).collect()
}

/// Whose claim a slot holds: the gate, its request and log, and the remote tag of a routed run's remote slot.
pub(crate) struct ClaimOf<'a> {
    pub gate: &'a ThisGate,
    pub request: &'a RunRequest,
    pub log: &'a Path,
    pub remote: Option<&'a RemoteTag>,
}

/// Writes this gate's claim of `kind` on `held`, saying so when it cannot be written; the run goes on either way.
pub(crate) fn write_claim(held: &Held, kind: ClaimKind, of: &ClaimOf<'_>, console: &mut Console) {
    let Some(path) = &held.claim else {
        return;
    };
    let written = claim(held, kind, of).and_then(|claim| v1::write_claim(path, &claim));
    if let Err(error) = written {
        console.err(&format!("gate: cannot write the claim {}: {error}", path.display()));
    }
}

fn claim(held: &Held, kind: ClaimKind, of: &ClaimOf<'_>) -> io::Result<Claim> {
    let ClaimOf { gate, request, log, remote } = *of;
    Ok(Claim {
        kind,
        index: held.index,
        pid: gate.pid,
        process_start: gate.start,
        started: Timestamp::now(),
        cwd: gate.cwd.clone().map_err(io::Error::other)?,
        command: gate.command.clone(),
        log: log.to_string_lossy().into_owned(),
        timeout_seconds: seconds(request.timeout),
        remote: remote.map(|_| true),
        host: remote.map(|tag| tag.host.clone()),
        key: remote.map(|tag| tag.key.clone()),
        expected: None,
    })
}

#[expect(clippy::cast_precision_loss, reason = "the CLI bounds a timeout to 2^31 - 1, which an f64 holds exactly")]
fn seconds(timeout: u64) -> f64 {
    timeout as f64
}

/// Deletes this gate's claim on `held`, before its mutex is let go so it never deletes the next holder's.
pub(crate) fn delete_claim(held: &Held, console: &mut Console) {
    if let Some(path) = &held.claim
        && let Err(error) = delete(path)
    {
        console.err(&format!("gate: cannot delete the claim {}: {error}", path.display()));
    }
}

/// Signals the wake event of every live ticket's waiter under `root`.
pub(crate) fn wake_waiters(root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) {
    let (live, _) = survey.live_tickets(&root.path, console);
    signal(root, &live);
}

/// Signals the wake event of the waiter of each of `tickets`.
pub(crate) fn signal<'t>(root: &ResolvedRoot, tickets: impl IntoIterator<Item = &'t Ticket>) {
    for ticket in tickets {
        let name = root.root.object_name(KernelObject::WakeEvent {
            pid: ticket.pid,
            start_ms: ticket.start_ms,
        });
        if let Ok(Some(event)) = NamedEvent::open_existing(&name) {
            // A wake that cannot be set is deliberately not said: the waiter's fallback poll re-reads the slots in
            // a moment, so the lost wake costs it one poll at most. An event that cannot be opened is skipped the same.
            let _ = event.set();
        }
    }
}

/// Deletes the file at `path`. A file already gone is no failure.
pub(crate) fn delete(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fixed times cannot be built must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::io;

    use gate_args::Invocation;
    use gate_state::normalize_command;
    use jiff::Timestamp;

    use super::{Liveness, ThisGate, is_claim_file, light_beside, liveness, on_reserved_light};

    #[test]
    fn light_gates_beside_a_critical_request_are_its_open_reserved_slots_and_once_it_holds_every_light_claim_off_its_slots() {
        let claims = [0, 1, 3, 7];
        assert_eq!(light_beside(5, false, 1, &claims), 1, "while it waits, the open reserved slots alone");
        assert_eq!(
            light_beside(5, true, 1, &claims),
            4,
            "once it holds, the claims below the reserved slots and past the count too"
        );
        assert_eq!(light_beside(5, true, 0, &[]), 0);
        assert_eq!(light_beside(1, true, 1, &[0]), 1, "a count of 1 reserves its one slot");
    }

    #[test]
    fn a_light_slot_is_one_no_critical_gate_takes_when_it_is_reserved_or_past_the_count() {
        assert!(!on_reserved_light(1, 4));
        assert!(on_reserved_light(2, 4));
        assert!(on_reserved_light(3, 4));
        assert!(on_reserved_light(5, 4), "past the count");
        assert!(on_reserved_light(0, 1), "a count of 1 reserves its one slot");
        assert!(on_reserved_light(0, 2));
    }

    #[test]
    fn only_this_roots_claim_files_are_claims() {
        assert!(is_claim_file("gate-slot-0.json"));
        assert!(is_claim_file("gate-light-slot-1.json"));
        assert!(is_claim_file("gate-critical.json"));
        assert!(!is_claim_file("gate-light-slot-0.json.4242.1.tmp"), "a claim being written");
        assert!(!is_claim_file("t1-gate-slot-0.json"), "another prefix's claim");
        assert!(!is_claim_file("notes.json"));
    }

    #[test]
    fn a_gate_is_live_only_while_its_pid_runs_since_its_start_and_unknown_when_its_start_cannot_be_read() {
        let start = Timestamp::from_millisecond(1_759_439_760_571).expect("a time after the epoch");
        let start_ms = 1_759_439_760_571;
        let of = |answer: io::Result<Option<Timestamp>>| liveness(7, start_ms, move |_| answer);

        assert!(matches!(of(Ok(Some(start))), Liveness::Live), "running since its start");
        let later = Timestamp::from_millisecond(1_759_439_760_572).expect("a time after the epoch");
        assert!(matches!(of(Ok(Some(later))), Liveness::Gone), "its pid reused by a later process");
        assert!(matches!(of(Ok(None)), Liveness::Gone), "no process of its pid");
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(of(Err(denied)), Liveness::Gone), "another user's process on its pid");
        let failed = of(Err(io::Error::other("the system failed")));
        assert!(
            matches!(&failed, Liveness::Unknown(error) if error.to_string() == "the system failed"),
            "{failed:?}"
        );
    }

    /// What the PowerShell gate's `Format-CommandText` writes for the command below.
    const POWERSHELL: &str = "cargo test --manifest-path X:/dev/ops.wt/ops-61/Cargo.toml -- --exact \"a b\" \"a\u{a0}b\" .tmp/out.txt";

    /// The journal history keys one command's runs together whichever gate wrote them.
    #[test]
    fn this_gate_carries_the_command_as_the_powershell_gate_writes_it() {
        let words = [
            "run",
            "--slot",
            "light",
            "--timeout",
            "5",
            "--log",
            "out.log",
            "--",
            "cargo",
            "test",
            "--manifest-path",
            "X:/dev/ops.wt/ops-61/Cargo.toml",
            "--",
            "--exact",
            "a b",
            "a\u{a0}b",
            ".tmp/out.txt",
        ];
        let request = match gate_args::parse(words.into_iter().map(OsString::from).collect()) {
            Ok(Invocation::Run(request)) => request,
            other => unreachable!("the words are a run: {other:?}"),
        };

        let gate = ThisGate::read(&request);

        assert_eq!(gate.command, POWERSHELL);
        assert_eq!(normalize_command(&gate.command), normalize_command(POWERSHELL));
        assert_eq!(normalize_command(&gate.command), "cargo test -- --exact \"a b\" \"a\u{a0}b\" \u{2026}");
    }
}

//! A gate's slot: the gate's own identity, the pool its kind takes from, whether the gates the state root's tickets
//! and claims name still run, the take with its claim and its takeover from a gate that died holding the slot, and
//! the waking of the waiters.

use std::collections::HashSet;
use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_args::{RunRequest, Slot};
use gate_state::v1::{self, Claim, ClaimKind, Counts, PoolSetting, Ticket, TicketKind};
use gate_state::{JournalIdentity, KernelObject, SlotKind};
use gate_win32::{NamedEvent, OwnedMutexGuard};
use jiff::Timestamp;

use crate::acquire;
use crate::console::Console;
use crate::resolve::ResolvedRoot;

/// The pool of slots a gate of one kind takes from.
#[derive(Clone, Copy)]
pub(crate) struct Pool(SlotKind);

impl Pool {
    /// The pool a gate of `slot` takes from; `None` for a critical gate, which has no pool of its own.
    pub(crate) fn of(slot: Slot) -> Option<Pool> {
        match slot {
            Slot::Heavy => Some(Pool(SlotKind::Heavy)),
            Slot::Light => Some(Pool(SlotKind::Light)),
            Slot::Critical => None,
        }
    }

    /// The kernel object of the pool's slot `index`.
    pub(crate) fn object(self, index: u32) -> KernelObject {
        match self.0 {
            SlotKind::Heavy => KernelObject::HeavySlot(index),
            SlotKind::Light => KernelObject::LightSlot(index),
        }
    }

    /// The kind the tickets of the pool's waiters carry.
    pub(crate) fn ticket_kind(self) -> TicketKind {
        match self.0 {
            SlotKind::Heavy => TicketKind::Heavy,
            SlotKind::Light => TicketKind::Light,
        }
    }

    fn claim_kind(self) -> ClaimKind {
        match self.0 {
            SlotKind::Heavy => ClaimKind::Heavy,
            SlotKind::Light => ClaimKind::Light,
        }
    }

    /// The pool's own setting in `counts`.
    pub(crate) fn setting(self, counts: &Counts) -> &PoolSetting {
        match self.0 {
            SlotKind::Heavy => &counts.heavy,
            SlotKind::Light => &counts.light,
        }
    }

    /// The pool's kind as the wait lines name it.
    pub(crate) fn name(self) -> &'static str {
        self.0.as_str()
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
    /// The command's words joined by spaces.
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
        let command: Vec<String> = request.command.iter().map(|word| word.to_string_lossy().into_owned()).collect();
        ThisGate {
            pid: std::process::id(),
            start,
            start_ms: epoch_ms(start),
            start_error,
            kind: request.slot.as_str(),
            cwd,
            command: command.join(" "),
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
        match gate_win32::process_start_of(pid) {
            Ok(Some(start)) => epoch_ms(start) == start_ms,
            Ok(None) => false,
            // A gate of this user, elevated or not, opens with limited query rights. A pid that does not is another
            // user's process, such as a service that reused a dead gate's pid, so the gate it names is gone.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
            Err(error) => {
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

/// Tries the pool's slots `0..count` in order and takes the first free one. The take is a takeover when the mutex
/// comes back abandoned, or when the slot's claim names a gate no longer alive. A slot that cannot be tried is said
/// once and passed over.
pub(crate) fn try_pool(root: &ResolvedRoot, pool: Pool, count: u32, survey: &mut Survey, console: &mut Console) -> Option<Taken> {
    for index in 0..count {
        let name = root.root.object_name(pool.object(index));
        match acquire::try_slot(&name) {
            Ok(Some(guard)) => {
                let claim = v1::claim_path(&root.path, pool.object(index));
                let abandoned = guard.abandoned() || claim.as_deref().is_some_and(|path| survey.left_by_a_dead_gate(path, console));
                return Some(Taken {
                    held: Held { guard, index, claim },
                    abandoned,
                });
            }
            Ok(None) => {}
            Err(error) => survey.say_once("could not take", &name, error, console),
        }
    }
    None
}

/// Writes this gate's claim on `held`, saying so when it cannot be written; the run goes on either way.
pub(crate) fn write_claim(held: &Held, pool: Pool, gate: &ThisGate, request: &RunRequest, log: &Path, console: &mut Console) {
    let Some(path) = &held.claim else {
        return;
    };
    let written = claim(held, pool, gate, request, log).and_then(|claim| v1::write_claim(path, &claim));
    if let Err(error) = written {
        console.err(&format!("gate: cannot write the claim {}: {error}", path.display()));
    }
}

fn claim(held: &Held, pool: Pool, gate: &ThisGate, request: &RunRequest, log: &Path) -> io::Result<Claim> {
    Ok(Claim {
        kind: pool.claim_kind(),
        index: held.index,
        pid: gate.pid,
        process_start: gate.start,
        started: Timestamp::now(),
        cwd: gate.cwd.clone().map_err(io::Error::other)?,
        command: gate.command.clone(),
        log: log.to_string_lossy().into_owned(),
        timeout_seconds: seconds(request.timeout),
        remote: None,
        host: None,
        key: None,
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

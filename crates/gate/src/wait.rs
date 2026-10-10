//! The wait for a slot: the gate's wake event and arrival ticket, the passes that re-read the counts and the tickets
//! and try the pool when fewer live tickets of the gate's kind are ahead of it than slots are free, and the lines a
//! waiting gate says. The waiter's core, which a critical gate's wait for the machine shares, is [`Waiter`].

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::KernelObject;
use gate_state::v1::{self, Ticket};
use gate_win32::{Interrupt, NamedEvent, mutex_exists};

use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, Pool, Survey, Taken, ThisGate};

/// The fallback poll: the longest a waiter sleeps before it reads the counts and the tickets again.
const POLL: Duration = Duration::from_secs(2);
/// How often a waiter says it still waits.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How long a routed run waits for a remote slot before it falls back here.
const REMOTE_LIMIT: Duration = Duration::from_secs(60);

/// A probe of a pool's slots: for each, whether some gate has it open, `None` for a slot that could not be probed.
type Probe = Vec<Option<bool>>;

/// What every waiting gate has, a slot's waiter and a critical gate alike: its wake event and its ticket, when it
/// queued, the poll and the still-waiting period (`--test-poll-ms`, `--test-heartbeat-ms`), the console interrupt, and
/// the interrupt it raises itself once it has said it waits.
pub(crate) struct Waiter {
    /// When the gate queued, which its waited time and its still-waiting lines count from.
    queued: Instant,
    poll: Duration,
    heartbeat: Duration,
    /// When the next still-waiting line is due, counted from `queued`.
    next_heartbeat: Duration,
    /// The gate's own wake event, or `None` when it could not be created and the poll alone wakes the gate.
    event: Option<NamedEvent>,
    /// The console interrupt that ends the wait, or `None` when it could not be installed.
    interrupt: Option<Interrupt>,
    /// The interrupt the gate raises itself once it has said it waits (`--test-interrupt-queued`), else `None`.
    interrupt_when_waiting: Option<Interrupt>,
    /// Where the gate's ticket is.
    ticket: PathBuf,
    said_wait_failed: bool,
}

impl Waiter {
    /// Queues the arriving gate under `root` with `ticket`: creates its wake event before it writes the ticket, so no
    /// wake is lost. Each that cannot be made is said, and the gate waits on without it.
    pub(crate) fn queue(root: &ResolvedRoot, arrival: Arrival<'_>, ticket: &Ticket, host: Host, console: &mut Console) -> Waiter {
        let name = root.root.object_name(KernelObject::WakeEvent {
            pid: ticket.pid,
            start_ms: ticket.start_ms,
        });
        let event = NamedEvent::create(&name)
            .inspect_err(|error| console.err(&format!("gate: cannot create the wake event {name}: {error}")))
            .ok();
        let path = v1::ticket_path(&root.path, ticket.pid, ticket.start_ms);
        write_ticket(&path, ticket, console);
        let seams = &arrival.request.test_seams;
        let heartbeat = seams.heartbeat_ms.map_or(HEARTBEAT, Duration::from_millis);
        Waiter {
            queued: arrival.queued,
            poll: seams.poll_ms.map_or(POLL, Duration::from_millis),
            heartbeat,
            next_heartbeat: heartbeat,
            event,
            interrupt: host.interrupt,
            interrupt_when_waiting: host.interrupt.filter(|_| seams.interrupt_queued),
            ticket: path,
            said_wait_failed: false,
        }
    }

    /// Where the gate's ticket is.
    pub(crate) fn ticket(&self) -> &Path {
        &self.ticket
    }

    /// Writes `ticket` over the gate's ticket, saying so when it cannot be written.
    pub(crate) fn rewrite(&self, ticket: &Ticket, console: &mut Console) {
        write_ticket(&self.ticket, ticket, console);
    }

    /// Whether the console was interrupted.
    pub(crate) fn interrupted(&self) -> bool {
        self.interrupt.is_some_and(Interrupt::is_raised)
    }

    /// Raises the interrupt the gate raises itself once it has said it waits, when it was asked to.
    pub(crate) fn said_waiting(&self) {
        if let Some(interrupt) = self.interrupt_when_waiting {
            interrupt.raise();
        }
    }

    /// How long the gate has waited when a still-waiting line is due, which moves the next one on; else `None`.
    pub(crate) fn heartbeat_due(&mut self) -> Option<Duration> {
        let waited = self.queued.elapsed();
        if waited < self.next_heartbeat {
            return None;
        }
        while self.next_heartbeat <= waited {
            self.next_heartbeat += self.heartbeat;
        }
        Some(waited)
    }

    /// Sleeps on the wake event until it is set, the console is interrupted, the poll runs out, the next still-waiting
    /// line is due or `limit`, counted from the queueing, passes. A wait that fails is said once and slept out instead.
    pub(crate) fn sleep(&mut self, limit: Option<Duration>, console: &mut Console) {
        let elapsed = self.queued.elapsed();
        let mut timeout = self.poll.min(self.next_heartbeat.saturating_sub(elapsed));
        if let Some(limit) = limit {
            timeout = timeout.min(limit.saturating_sub(elapsed));
        }
        let Some(event) = &self.event else {
            thread::sleep(timeout);
            return;
        };
        let waited = match self.interrupt {
            Some(interrupt) => event.wait_or_interrupt(timeout, interrupt),
            None => event.wait(timeout),
        };
        if let Err(error) = waited {
            if !self.said_wait_failed {
                self.said_wait_failed = true;
                console.err(&format!("gate: cannot wait on the wake event {}: {error}", event.name()));
            }
            thread::sleep(timeout);
        }
    }

    /// Deletes the gate's ticket.
    pub(crate) fn end(self, console: &mut Console) {
        delete_ticket(&self.ticket, console);
    }
}

/// Writes `ticket` at `path`, saying so when it cannot be written; the gate waits on either way.
fn write_ticket(path: &Path, ticket: &Ticket, console: &mut Console) {
    if let Err(error) = v1::write_ticket(path, ticket) {
        console.err(&format!("gate: cannot write the ticket {}: {error}", path.display()));
    }
}

/// Deletes the ticket at `path`, saying so when it cannot be deleted.
pub(crate) fn delete_ticket(path: &Path, console: &mut Console) {
    if let Err(error) = slots::delete(path) {
        console.err(&format!("gate: cannot delete the ticket {}: {error}", path.display()));
    }
}

/// A gate queued for a slot of its pool.
pub(crate) struct Wait<'a> {
    root: &'a ResolvedRoot,
    pool: Pool,
    /// The gate's place among the waiters of its kind.
    order: (u64, u32),
    processors: usize,
    /// How long the gate waits before it gives up, `None` for as long as it takes: a remote slot's wait has a limit.
    limit: Option<Duration>,
    /// What the wait lines say the gate waits for: `heavy slot`, or `remote slot on <host>`.
    place: String,
    waiter: Waiter,
    said_waiting: bool,
    said_ignored: bool,
}

/// What a run reads of its host once: the machine's logical processors and the console interrupt, `None` when its
/// handler could not be installed.
#[derive(Clone, Copy)]
pub(crate) struct Host {
    pub processors: usize,
    pub interrupt: Option<Interrupt>,
}

/// The gate that queues and what it asked: which gate it is, its request, when it queued for this wait, its first
/// arrival in Unix epoch milliseconds, which its ticket keeps across a fallback, and the builder of a remote wait.
#[derive(Clone, Copy)]
pub(crate) struct Arrival<'g> {
    pub gate: &'g ThisGate,
    pub request: &'g RunRequest,
    pub queued: Instant,
    pub arrived: u64,
    pub builder: Option<&'g str>,
}

/// How a wait ended.
pub(crate) enum Waited {
    Taken(Taken),
    /// The console was interrupted.
    Interrupted,
    /// The wait's limit passed with no slot taken.
    TimedOut,
}

impl<'a> Wait<'a> {
    /// Queues the arriving gate for a slot of `pool` ([`Waiter::queue`]). A raised interrupt of `host` ends the wait;
    /// so does the limit of a remote pool's wait, `--test-remote-wait-ms` else 60 s.
    pub(crate) fn queue(root: &'a ResolvedRoot, pool: Pool, arrival: Arrival<'_>, host: Host, console: &mut Console) -> Wait<'a> {
        let Arrival {
            gate,
            request,
            arrived,
            builder,
            ..
        } = arrival;
        let ticket = Ticket {
            kind: pool.ticket_kind(),
            arrived,
            pid: gate.pid,
            start_ms: gate.start_ms,
            holds: false,
            remote: pool.is_remote().then_some(true),
            expected: None,
        };
        let waiter = Waiter::queue(root, arrival, &ticket, host, console);
        Wait {
            root,
            pool,
            order: ticket.queue_order(),
            processors: host.processors,
            limit: pool
                .is_remote()
                .then(|| request.test_seams.remote_wait_ms.map_or(REMOTE_LIMIT, Duration::from_millis)),
            place: match builder.filter(|_| pool.is_remote()) {
                Some(builder) => format!("remote slot on {builder}"),
                None => format!("{} slot", pool.name()),
            },
            waiter,
            said_waiting: false,
            said_ignored: false,
        }
    }

    /// Passes until the gate takes a slot: each pass reads the counts and the tickets, tries the pool when fewer live
    /// tickets of the gate's kind are ahead of it than slots are free, and otherwise sleeps on the wake event until
    /// the poll or the next still-waiting line. Interrupted once the console is, read again before the limit, which
    /// ends the wait at once; timed out when a pass fails at or after the wait's limit.
    pub(crate) fn until_taken(&mut self, survey: &mut Survey, console: &mut Console) -> Waited {
        loop {
            if self.waiter.interrupted() {
                return Waited::Interrupted;
            }
            let count = self.count(console);
            let (may_try, probe) = self.may_try(count, survey, console);
            if may_try && let Some(taken) = slots::try_pool(self.root, self.pool, count, survey, console) {
                return Waited::Taken(taken);
            }
            if !self.said_waiting {
                let probe = probe.unwrap_or_else(|| self.probe(count, survey, console));
                self.say_waiting(&probe, console);
            }
            if let Some(ended) = self.pass_ended() {
                return ended;
            }
            self.waiter.sleep(self.limit, console);
            if self.waiter.interrupted() {
                return Waited::Interrupted;
            }
            self.say_heartbeat(console);
        }
    }

    /// Whether this pass ended the wait, and how. The console is read first, so a Ctrl+C raised by this pass's own
    /// still-waiting line is the interrupt, and never a remote pool read as full at the wait's limit.
    fn pass_ended(&self) -> Option<Waited> {
        if self.waiter.interrupted() {
            return Some(Waited::Interrupted);
        }
        self.limit
            .is_some_and(|limit| self.waiter.queued.elapsed() >= limit)
            .then_some(Waited::TimedOut)
    }

    /// Deletes the gate's ticket, the first step of its take.
    pub(crate) fn end(self, console: &mut Console) {
        self.waiter.end(console);
    }

    /// The pool's count: the remote pool's own size, or a local pool's from the counts file as it stands, its ignored
    /// line said once a wait.
    fn count(&mut self, console: &mut Console) -> u32 {
        let kind = match self.pool {
            Pool::Remote(slots) => return slots,
            Pool::Local(kind) => kind,
        };
        let counts = v1::read_counts(&v1::counts_path(&self.root.path), self.processors);
        let setting = slots::setting(kind, &counts);
        if let Some(line) = &setting.ignored
            && !self.said_ignored
        {
            self.said_ignored = true;
            console.err(line);
        }
        setting.count
    }

    /// Whether the gate may try the pool's slots `0..count`: when fewer live tickets of its kind are ahead of it than
    /// the pool has free slots, so a try never takes the slot an earlier waiter would get. The probe that decided comes
    /// with it, `None` when no ticket is ahead and none was needed.
    fn may_try(&self, count: u32, survey: &mut Survey, console: &mut Console) -> (bool, Option<Probe>) {
        let ahead = self.ahead(survey, console);
        if ahead == 0 {
            return (true, None);
        }
        let probe = self.probe(count, survey, console);
        (ahead < slots_where(&probe, false), Some(probe))
    }

    /// Says the gate waits, once a wait, when `probe`, this pass's only probe, found none of the pool's slots free,
    /// with the busy slots of that same probe. A gate that defers to the tickets ahead of it while a slot is free for
    /// it says nothing; its still-waiting lines report a long wait. The probe may predate a try that failed in the same
    /// pass, so a pass whose try lost the race for a slot it saw free says nothing until the next pass.
    fn say_waiting(&mut self, probe: &[Option<bool>], console: &mut Console) {
        if slots_where(probe, false) > 0 {
            return;
        }
        self.said_waiting = true;
        let busy = slots_where(probe, true);
        console.err(&format!("gate: waiting for a {} ({busy} busy)", self.place));
        self.waiter.said_waiting();
    }

    /// How many live tickets of the gate's kind and pool, local or remote, are ahead of it, so a local and a remote
    /// waiter never count each other. A ticket whose gate a live claim names is not counted:
    /// that gate holds a slot already, and only failed to delete its ticket. A pass that deleted a dead gate's ticket
    /// wakes the other waiters, since the queue moved.
    fn ahead(&self, survey: &mut Survey, console: &mut Console) -> usize {
        let (live, deleted) = survey.live_tickets(&self.root.path, console);
        if deleted {
            slots::signal(self.root, live.iter().filter(|ticket| ticket.pid != self.order.1));
        }
        let kind = self.pool.ticket_kind();
        let remote = self.pool.is_remote();
        let ahead: Vec<&Ticket> = live
            .iter()
            .filter(|ticket| ticket.kind == kind && (ticket.remote == Some(true)) == remote && ticket.queue_order() < self.order)
            .collect();
        if ahead.is_empty() {
            return 0;
        }
        let holders = survey.holders(&self.root.path, console);
        ahead.iter().filter(|ticket| !holders.contains(&(ticket.pid, ticket.start_ms))).count()
    }

    /// Whether some gate has each of the pool's slots `0..count` open; `None` for a slot that cannot be probed, said
    /// once, which counts as neither free nor busy.
    fn probe(&self, count: u32, survey: &mut Survey, console: &mut Console) -> Probe {
        (0..count)
            .map(|index| {
                let name = self.root.root.object_name(self.pool.object(index));
                mutex_exists(&name)
                    .inspect_err(|error| survey.say_once("cannot probe", &name, error, console))
                    .ok()
            })
            .collect()
    }

    /// Says the still-waiting line when it is due, with the whole seconds since the gate queued.
    fn say_heartbeat(&mut self, console: &mut Console) {
        if let Some(waited) = self.waiter.heartbeat_due() {
            console.err(&format!("gate: still waiting for a {} ({} s)", self.place, waited.as_secs()));
        }
    }
}

/// How many slots of `probe` some gate has open (`held`) or none has (not `held`); a slot that could not be probed is
/// neither.
fn slots_where(probe: &[Option<bool>], held: bool) -> usize {
    probe.iter().filter(|slot| **slot == Some(held)).count()
}

/// The machine's logical processors, which the default counts and the watch's share are made from. A count that cannot
/// be read is said and taken as 1.
pub(crate) fn processors(console: &mut Console) -> usize {
    std::thread::available_parallelism().map_or_else(
        |error| {
            console.err(&format!("gate: cannot read the processor count: {error}; counting 1"));
            1
        },
        std::num::NonZero::get,
    )
}

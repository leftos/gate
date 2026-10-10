//! The wait for a slot: the gate's wake event and arrival ticket, the passes that re-read the counts and the tickets
//! and try the pool when no live ticket of the gate's kind is ahead of it, and the lines a waiting gate says.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::KernelObject;
use gate_state::v1::{self, Ticket};
use gate_win32::{Interrupt, NamedEvent, mutex_exists};
use jiff::Timestamp;

use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, Pool, Survey, Taken, ThisGate, epoch_ms};

/// The fallback poll: the longest a waiter sleeps before it reads the counts and the tickets again.
const POLL: Duration = Duration::from_secs(2);
/// How often a waiter says it still waits.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// A gate queued for a slot of its pool.
pub(crate) struct Wait<'a> {
    root: &'a ResolvedRoot,
    pool: Pool,
    /// When the gate queued, which its waited time and its still-waiting lines count from.
    queued: Instant,
    /// The gate's place among the waiters of its kind.
    order: (u64, u32),
    poll: Duration,
    heartbeat: Duration,
    /// When the next still-waiting line is due, counted from `queued`.
    next_heartbeat: Duration,
    processors: usize,
    /// The gate's own wake event, or `None` when it could not be created and the poll alone wakes the gate.
    event: Option<NamedEvent>,
    /// The console interrupt that ends the wait, or `None` when it could not be installed.
    interrupt: Option<Interrupt>,
    /// The interrupt the gate raises itself once it has said it waits (`--test-interrupt-queued`), else `None`.
    interrupt_when_waiting: Option<Interrupt>,
    ticket: PathBuf,
    said_waiting: bool,
    said_ignored: bool,
    said_wait_failed: bool,
}

/// What a run reads of its host once: the machine's logical processors and the console interrupt, `None` when its
/// handler could not be installed.
#[derive(Clone, Copy)]
pub(crate) struct Host {
    pub processors: usize,
    pub interrupt: Option<Interrupt>,
}

/// The gate that queues and what it asked: which gate it is, its request, and when it queued.
#[derive(Clone, Copy)]
pub(crate) struct Arrival<'g> {
    pub gate: &'g ThisGate,
    pub request: &'g RunRequest,
    pub queued: Instant,
}

impl<'a> Wait<'a> {
    /// Queues the arriving gate for a slot of `pool`: creates its wake event before it writes its ticket, so no wake
    /// is lost. Each that cannot be made is said, and the gate waits on without it. A raised interrupt of `host` ends
    /// the wait.
    pub(crate) fn queue(root: &'a ResolvedRoot, pool: Pool, arrival: Arrival<'_>, host: Host, console: &mut Console) -> Wait<'a> {
        let Arrival { gate, request, queued } = arrival;
        let name = root.root.object_name(KernelObject::WakeEvent {
            pid: gate.pid,
            start_ms: gate.start_ms,
        });
        let event = NamedEvent::create(&name)
            .inspect_err(|error| console.err(&format!("gate: cannot create the wake event {name}: {error}")))
            .ok();
        let ticket = Ticket {
            kind: pool.ticket_kind(),
            arrived: epoch_ms(Timestamp::now()),
            pid: gate.pid,
            start_ms: gate.start_ms,
            holds: false,
            remote: None,
            expected: None,
        };
        let path = v1::ticket_path(&root.path, gate.pid, gate.start_ms);
        if let Err(error) = v1::write_ticket(&path, &ticket) {
            console.err(&format!("gate: cannot write the ticket {}: {error}", path.display()));
        }
        let heartbeat = request.test_seams.heartbeat_ms.map_or(HEARTBEAT, Duration::from_millis);
        Wait {
            root,
            pool,
            queued,
            order: ticket.queue_order(),
            poll: request.test_seams.poll_ms.map_or(POLL, Duration::from_millis),
            heartbeat,
            next_heartbeat: heartbeat,
            processors: host.processors,
            event,
            interrupt: host.interrupt,
            interrupt_when_waiting: host.interrupt.filter(|_| request.test_seams.interrupt_queued),
            ticket: path,
            said_waiting: false,
            said_ignored: false,
            said_wait_failed: false,
        }
    }

    /// Passes until the gate takes a slot: each pass reads the counts and the tickets, tries the pool when no live
    /// ticket of the gate's kind is ahead of it, and otherwise sleeps on the wake event until the poll or the next
    /// still-waiting line. `None` once the console is interrupted, which ends the wait at once.
    pub(crate) fn until_taken(&mut self, survey: &mut Survey, console: &mut Console) -> Option<Taken> {
        loop {
            if self.interrupted() {
                return None;
            }
            let count = self.count(console);
            if self.may_try(count, survey, console)
                && let Some(taken) = slots::try_pool(self.root, self.pool, count, survey, console)
            {
                return Some(taken);
            }
            if !self.said_waiting {
                self.said_waiting = true;
                let busy = self.busy(count, survey, console);
                console.err(&format!("gate: waiting for a {} slot ({busy} busy)", self.pool.name()));
                if let Some(interrupt) = self.interrupt_when_waiting {
                    interrupt.raise();
                }
            }
            self.sleep(console);
            if self.interrupted() {
                return None;
            }
            self.say_heartbeat(console);
        }
    }

    /// Deletes the gate's ticket, the first step of its take.
    pub(crate) fn end(self, console: &mut Console) {
        if let Err(error) = slots::delete(&self.ticket) {
            console.err(&format!("gate: cannot delete the ticket {}: {error}", self.ticket.display()));
        }
    }

    /// The pool's count from the counts file as it stands, its ignored line said once a wait.
    fn count(&mut self, console: &mut Console) -> u32 {
        let counts = v1::read_counts(&v1::counts_path(&self.root.path), self.processors);
        let setting = self.pool.setting(&counts);
        if let Some(line) = &setting.ignored
            && !self.said_ignored
        {
            self.said_ignored = true;
            console.err(line);
        }
        setting.count
    }

    /// Whether the gate may try the pool's slots `0..count`: when fewer live tickets of its kind are ahead of it than
    /// the pool has free slots, so a try never takes the slot an earlier waiter would get.
    fn may_try(&self, count: u32, survey: &mut Survey, console: &mut Console) -> bool {
        let ahead = self.ahead(survey, console);
        ahead == 0 || ahead < self.probe(count, survey, console).iter().filter(|held| **held == Some(false)).count()
    }

    /// How many live tickets of the gate's kind are ahead of it. A ticket whose gate a live claim names is not counted:
    /// that gate holds a slot already, and only failed to delete its ticket. A pass that deleted a dead gate's ticket
    /// wakes the other waiters, since the queue moved.
    fn ahead(&self, survey: &mut Survey, console: &mut Console) -> usize {
        let (live, deleted) = survey.live_tickets(&self.root.path, console);
        if deleted {
            slots::signal(self.root, live.iter().filter(|ticket| ticket.pid != self.order.1));
        }
        let kind = self.pool.ticket_kind();
        let ahead: Vec<&Ticket> = live
            .iter()
            .filter(|ticket| ticket.kind == kind && ticket.queue_order() < self.order)
            .collect();
        if ahead.is_empty() {
            return 0;
        }
        let holders = survey.holders(&self.root.path, console);
        ahead.iter().filter(|ticket| !holders.contains(&(ticket.pid, ticket.start_ms))).count()
    }

    /// How many of the pool's slots `0..count` some gate has open.
    fn busy(&self, count: u32, survey: &mut Survey, console: &mut Console) -> usize {
        self.probe(count, survey, console).iter().filter(|held| **held == Some(true)).count()
    }

    /// Whether some gate has each of the pool's slots `0..count` open; `None` for a slot that cannot be probed, said
    /// once, which counts as neither free nor busy.
    fn probe(&self, count: u32, survey: &mut Survey, console: &mut Console) -> Vec<Option<bool>> {
        (0..count)
            .map(|index| {
                let name = self.root.root.object_name(self.pool.object(index));
                mutex_exists(&name)
                    .inspect_err(|error| survey.say_once("cannot probe", &name, error, console))
                    .ok()
            })
            .collect()
    }

    /// Whether the console was interrupted.
    fn interrupted(&self) -> bool {
        self.interrupt.is_some_and(Interrupt::is_raised)
    }

    /// Sleeps on the wake event until it is set, the console is interrupted, the poll runs out or the next
    /// still-waiting line is due. A wait that fails is said once and slept out instead.
    fn sleep(&mut self, console: &mut Console) {
        let timeout = self.poll.min(self.next_heartbeat.saturating_sub(self.queued.elapsed()));
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

    /// Says the still-waiting line when it is due, with the whole seconds since the gate queued.
    fn say_heartbeat(&mut self, console: &mut Console) {
        let waited = self.queued.elapsed();
        if waited < self.next_heartbeat {
            return;
        }
        console.err(&format!("gate: still waiting for a {} slot ({} s)", self.pool.name(), waited.as_secs()));
        while self.next_heartbeat <= waited {
            self.next_heartbeat += self.heartbeat;
        }
    }
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

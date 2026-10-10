//! The wait for a slot: the gate's wake event and arrival ticket, the passes that re-read the counts and the tickets
//! and try the pool when no live ticket of the gate's kind is ahead of it, and the lines a waiting gate says.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::KernelObject;
use gate_state::v1::{self, Ticket};
use gate_win32::{NamedEvent, mutex_exists};
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
    ticket: PathBuf,
    said_waiting: bool,
    said_ignored: bool,
    said_wait_failed: bool,
}

impl<'a> Wait<'a> {
    /// Queues `gate` for a slot of `pool`, which it queued for at `queued`: creates its wake event before it writes its
    /// ticket, so no wake is lost. Each that cannot be made is said, and the gate waits on without it.
    pub(crate) fn queue(
        root: &'a ResolvedRoot,
        pool: Pool,
        gate: &ThisGate,
        request: &RunRequest,
        queued: Instant,
        console: &mut Console,
    ) -> Wait<'a> {
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
            processors: processors(console),
            event,
            ticket: path,
            said_waiting: false,
            said_ignored: false,
            said_wait_failed: false,
        }
    }

    /// Passes until the gate takes a slot: each pass reads the counts and the tickets, tries the pool when no live
    /// ticket of the gate's kind is ahead of it, and otherwise sleeps on the wake event until the poll or the next
    /// still-waiting line.
    pub(crate) fn until_taken(&mut self, survey: &mut Survey, console: &mut Console) -> Taken {
        loop {
            let count = self.count(console);
            if !self.is_behind(survey, console)
                && let Some(taken) = slots::try_pool(self.root, self.pool, count, survey, console)
            {
                return taken;
            }
            if !self.said_waiting {
                self.said_waiting = true;
                let busy = self.busy(count, survey, console);
                console.err(&format!("gate: waiting for a {} slot ({busy} busy)", self.pool.name()));
            }
            self.sleep(console);
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

    /// Whether a live ticket of the gate's kind is ahead of it. A ticket whose gate a live claim names is not: that gate
    /// holds a slot already, and only failed to delete its ticket. A pass that deleted a dead gate's ticket wakes the
    /// other waiters, since the queue moved.
    fn is_behind(&self, survey: &mut Survey, console: &mut Console) -> bool {
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
            return false;
        }
        let holders = survey.holders(&self.root.path, console);
        ahead.iter().any(|ticket| !holders.contains(&(ticket.pid, ticket.start_ms)))
    }

    /// How many of the pool's slots `0..count` some gate has open. A slot that cannot be probed is said once and not
    /// counted.
    fn busy(&self, count: u32, survey: &mut Survey, console: &mut Console) -> usize {
        (0..count)
            .filter(|&index| {
                let name = self.root.root.object_name(self.pool.object(index));
                mutex_exists(&name).unwrap_or_else(|error| {
                    survey.say_once("cannot probe", &name, error, console);
                    false
                })
            })
            .count()
    }

    /// Sleeps on the wake event until it is set, the poll runs out or the next still-waiting line is due. A wait that
    /// fails is said once and slept out instead.
    fn sleep(&mut self, console: &mut Console) {
        let timeout = self.poll.min(self.next_heartbeat.saturating_sub(self.queued.elapsed()));
        let Some(event) = &self.event else {
            thread::sleep(timeout);
            return;
        };
        if let Err(error) = event.wait(timeout) {
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

/// The machine's logical processors, which the default counts are made from. A count that cannot be read is said and
/// taken as 1.
fn processors(console: &mut Console) -> usize {
    std::thread::available_parallelism().map_or_else(
        |error| {
            console.err(&format!("gate: cannot read the processor count: {error}; counting 1"));
            1
        },
        std::num::NonZero::get,
    )
}

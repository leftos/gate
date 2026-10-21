//! The wait for a slot: the gate's wake event and arrival ticket, the passes that re-read the counts and the tickets
//! and try the pool when fewer live tickets of the gate's kind are ahead of it than slots are free, the barrier of a
//! critical request that arrived before the gate, the lend of a light parent's slot while a critical request drains,
//! the pause marker a gate under a parent's slot keeps while a critical request holds it back, and the lines a waiting
//! gate says. The waiter's core, which a critical gate's wait for the machine shares, is [`Waiter`].

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::v1::{self, Claim, ClaimKind, Counts, Ticket, TicketKind};
use gate_state::{KernelObject, SlotKind, beside_allowance};
use gate_win32::{Interrupt, NamedEvent, OwnedMutexGuard, mutex_exists};
use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::console::Console;
use crate::durations::span_text;
use crate::pause::Pause;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, BesideTry, Pool, Survey, Taken, ThisGate};

/// The fallback poll: the longest a waiter sleeps before it reads the counts and the tickets again.
const POLL: Duration = Duration::from_secs(2);
/// How often a waiter says it still waits, a routed run's wait for its work folder's key among them.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(30);
/// The longest a waiter sleeps after a pass that found the beside mutex held by another gate: that gate wakes the
/// waiters only once it took or lent something, so a pass that found no room for this reason alone looks again soon.
const BUSY_RETRY: Duration = Duration::from_millis(100);
/// How long a routed run waits for a remote slot before it falls back here.
pub(crate) const REMOTE_LIMIT: Duration = Duration::from_secs(60);

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
    /// The interrupt the gate raises itself once it has said it waits (`--test-interrupt-queued`, and
    /// `--test-interrupt-queued-after-fallback` in a wait that follows a `full` fallback), else `None`.
    interrupt_when_waiting: Option<Interrupt>,
    /// Where the gate's ticket is.
    ticket: PathBuf,
    said_wait_failed: bool,
    /// Whether the last pass found the beside mutex held by another gate, so the next sleep is [`BUSY_RETRY`] at most.
    retry_soon: bool,
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
            next_heartbeat: first_heartbeat(heartbeat, arrival.queued.elapsed()),
            event,
            interrupt: host.interrupt,
            interrupt_when_waiting: host
                .interrupt
                .filter(|_| seams.interrupt_queued || (seams.interrupt_after_fallback && arrival.after_fallback)),
            ticket: path,
            said_wait_failed: false,
            retry_soon: false,
        }
    }

    /// Notes that this pass found the beside mutex held by another gate: the next sleep lasts [`BUSY_RETRY`] at most.
    pub(crate) fn retry_soon(&mut self) {
        self.retry_soon = true;
    }

    /// Where the gate's ticket is.
    pub(crate) fn ticket(&self) -> &Path {
        &self.ticket
    }

    /// Writes `ticket` over the gate's ticket; else why it cannot be written, for the caller to say.
    pub(crate) fn rewrite(&self, ticket: &Ticket) -> io::Result<()> {
        v1::write_ticket(&self.ticket, ticket)
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
    /// line is due, `limit`, counted from the queueing, passes, or after a pass that found the beside mutex held by
    /// another gate, [`BUSY_RETRY`]. A wait that fails is said once and slept out instead.
    pub(crate) fn sleep(&mut self, limit: Option<Duration>, console: &mut Console) {
        let elapsed = self.queued.elapsed();
        let mut timeout = self.poll.min(self.next_heartbeat.saturating_sub(elapsed));
        if let Some(limit) = limit {
            timeout = timeout.min(limit.saturating_sub(elapsed));
        }
        if std::mem::take(&mut self.retry_soon) {
            timeout = timeout.min(BUSY_RETRY);
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

/// When the first still-waiting line of a wait is due, counted from the queueing: the first multiple of `heartbeat`
/// strictly past the time `waited` already at queueing. So a wait entering from a fallback never says a line at once,
/// and an ordinary wait says its first after one period.
fn first_heartbeat(heartbeat: Duration, waited: Duration) -> Duration {
    let periods = waited.as_nanos() / heartbeat.as_nanos() + 1;
    heartbeat.saturating_mul(u32::try_from(periods).unwrap_or(u32::MAX))
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

/// The parent gate whose slot a gate waits under: the gate's pid, and the start it runs since in Unix epoch
/// milliseconds, which between them name the pause marker the waiting gate keeps while a critical request holds it
/// back ([`crate::pause`]), and whether the parent may lend the gate its slot: a light parent whose slot does not cover
/// the gate may, a parent that holds no slot may not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnderParent {
    pub pid: u32,
    pub start_ms: u64,
    pub lends: bool,
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
    /// The parent gate whose slot this gate runs under, which lends it while a critical request drains and whose pause
    /// marker it keeps; `None` for a gate under no such parent, and for a remote wait.
    under: Option<UnderParent>,
    /// The critical request that held the gate back on the last pass; `None` when none did.
    barrier: Option<Barrier>,
    /// The slots the last pass tried, which the still-waiting line probes; none before the first pass.
    range: Range<u32>,
    /// The pause marker this gate keeps while it waits behind a critical request; no marker for a gate under no parent.
    pause: Pause,
    waiter: Waiter,
    said_waiting: bool,
    said_ignored: bool,
}

/// The earliest critical request ahead of a waiting gate, which holds it back: its gate's pid, and whether it holds the
/// machine alone ([`Ticket::holds_alone`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Barrier {
    pid: u32,
    alone: bool,
}

/// What a run reads of its host once: the machine's logical processors and the console interrupt, `None` when its
/// handler could not be installed.
#[derive(Clone, Copy)]
pub(crate) struct Host {
    pub processors: usize,
    pub interrupt: Option<Interrupt>,
}

/// The gate that queues and what it asked: which gate it is, its request, the moment the wait counts from, its first
/// arrival in Unix epoch milliseconds, which its ticket keeps across a fallback, and the builder of a remote wait.
#[derive(Clone, Copy)]
pub(crate) struct Arrival<'g> {
    pub gate: &'g ThisGate,
    pub request: &'g RunRequest,
    /// The moment the wait counts from: the run's own `queue` line for a run the full remote pool sent back, and this
    /// arrival for every other wait. The wait, its still-waiting lines and its journal all count from that one instant.
    pub queued: Instant,
    pub arrived: u64,
    pub builder: Option<&'g str>,
    /// Whether this wait follows a `full` fallback here from the builder, the only reason that keeps `Run::queued_at`,
    /// the instant [`Arrival::queued`] takes; `false` for every other wait, whose `queued` is the arrival itself.
    pub after_fallback: bool,
    /// The parent gate whose slot does not cover this gate, which lends it while a critical request drains, or the parent
    /// that holds no slot, which lends nothing; `None` for any other gate.
    pub under: Option<UnderParent>,
    /// What the durations store expects of the run, which its ticket carries; `None` without history.
    pub expected: Option<&'g Map<String, Value>>,
}

/// How a wait ended.
pub(crate) enum Waited {
    Taken(Taken),
    /// The gate's light parent lends it its slot while a critical request drains, decided holding the beside mutex,
    /// which the gate keeps until it has written its lend marker ([`slots::LendMarker`]).
    Lent(OwnedMutexGuard),
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
            under,
            expected,
            ..
        } = arrival;
        let ticket = Ticket {
            kind: pool.ticket_kind(),
            arrived,
            pid: gate.pid,
            start_ms: gate.start_ms,
            holds: false,
            lends_closed: false,
            alone: false,
            remote: pool.is_remote().then_some(true),
            pool: pool.remote_id().map(|id| id.to_string()),
            reason: request.reason.clone(),
            expected: expected.cloned(),
        };
        let waiter = Waiter::queue(root, arrival, &ticket, host, console);
        let under = under.filter(|_| !pool.is_remote());
        let pause = Pause::of(&root.path, under, gate);
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
            under,
            barrier: None,
            range: 0..0,
            pause,
            waiter,
            said_waiting: false,
            said_ignored: false,
        }
    }

    /// Passes until the gate takes a slot: each pass reads the counts and the tickets, ends the wait when the gate's
    /// light parent lends it its slot, tries the slots a critical request ahead leaves it ([`slot_range`]) when fewer
    /// live tickets of the gate's kind are ahead of it than those slots are free, and otherwise sleeps on the wake
    /// event until the poll or the next still-waiting line. Interrupted once the console is, read again before the
    /// limit, which ends the wait at once; timed out when a pass fails at or after the wait's limit.
    pub(crate) fn until_taken(&mut self, survey: &mut Survey, console: &mut Console) -> Waited {
        loop {
            if let Some(waited) = self.pass(survey, console) {
                return waited;
            }
            self.waiter.sleep(self.limit, console);
            if self.waiter.interrupted() {
                return Waited::Interrupted;
            }
            self.say_heartbeat(survey, console);
        }
    }

    /// One pass of the wait, which never sleeps: reads the counts and the tickets, ends the wait when the gate's light
    /// parent lends it its slot, tries the slots a critical request ahead leaves it when fewer live tickets of the
    /// gate's kind are ahead of it than those slots are free, and says the gate waits the first time it finds none free.
    /// How the wait ended on this pass, or `None` while it goes on.
    pub(crate) fn pass(&mut self, survey: &mut Survey, console: &mut Console) -> Option<Waited> {
        if self.waiter.interrupted() {
            return Some(Waited::Interrupted);
        }
        let count = self.counts(console);
        let others = live_others(self.root, survey, console);
        if let Some(lent) = self.lent(&others, survey, console) {
            return Some(lent);
        }
        self.barrier = self.barrier_in(&others);
        self.pause.refresh(self.barrier.is_some(), console);
        let beside = self.beside(&others, survey, console);
        let (range, room) = self.range(count, &others, &beside, survey, console);
        self.range = range.clone();
        let (may_try, probe) = self.may_try(&others, range.clone(), room, survey, console);
        if may_try && let Some(mut taken) = slots::try_pool(self.root, self.pool, range.clone(), survey, console) {
            taken.beside = beside.into_guard();
            return Some(Waited::Taken(taken));
        }
        drop(beside);
        if !self.said_waiting {
            self.say_waiting(probe, range, survey, console);
        }
        self.pass_ended()
    }

    /// The fallback poll of this wait: the longest a waiter goes between two passes (`--test-poll-ms`, else 2 s).
    pub(crate) fn poll(&self) -> Duration {
        self.waiter.poll
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

    /// Deletes the gate's ticket, the first step of its take, and its pause marker, whichever way the wait ended.
    pub(crate) fn end(mut self, console: &mut Console) {
        self.pause.end(console);
        self.waiter.end(console);
    }

    /// The pool's count: the remote pool's own size, or a local pool's from the counts file as it stands, its ignored
    /// line said once a wait.
    fn counts(&mut self, console: &mut Console) -> u32 {
        let kind = match self.pool {
            Pool::Remote(builder) => return builder.slots,
            Pool::Local(kind) => kind,
        };
        let counts = read_counts(self.root, self.processors);
        let setting = slots::setting(kind, &counts);
        if let Some(line) = &setting.ignored
            && !self.said_ignored
        {
            self.said_ignored = true;
            console.err(line);
        }
        setting.count
    }

    /// How the wait ends when the gate's light parent lends it its slot on a pass that read the other gates' live
    /// tickets `others` ([`lends`]), the lend confirmed holding the beside mutex ([`lend_held`]); `None` for a gate under
    /// no such parent, under a parent that holds no slot or that may not lend now, and when the beside mutex is held by
    /// another gate, which this gate looks again for soon.
    fn lent(&mut self, others: &[Ticket], survey: &mut Survey, console: &mut Console) -> Option<Waited> {
        let root = self.root;
        let unreadable = survey.unreadable_ticket();
        if !self.under.is_some_and(|parent| parent.lends) || !lends(others, unreadable, &mut Probed { root, survey, console }) {
            return None;
        }
        let held = lend_held(root, survey, console);
        if held.is_none() {
            self.waiter.retry_soon();
        }
        held.map(Waited::Lent)
    }

    /// Stops the gate's light parent lending it its slot for the rest of the wait: the gate could not write its lend
    /// marker, without which a critical gate's drain would not wait for it.
    pub(crate) fn refuse_lend(&mut self) {
        if let Some(under) = &mut self.under {
            under.lends = false;
        }
    }

    /// The critical request of the other gates' live tickets `others` that holds the gate back ([`barrier`]); none for
    /// a remote wait, which no critical request holds back.
    fn barrier_in(&self, others: &[Ticket]) -> Option<Barrier> {
        match self.pool {
            Pool::Local(SlotKind::Light) => light_barrier(others, self.order),
            Pool::Local(SlotKind::Heavy) => barrier(others, self.order),
            Pool::Remote(_) => None,
        }
    }

    /// The beside mutex as this pass finds it: a local light gate tries it once ([`slots::try_beside`]) and holds it from
    /// before its count and its try to after its claim, whether or not a critical request holds it back; any other gate
    /// needs none. Held by another gate, this gate finds no room this pass and looks again soon; one that cannot be
    /// taken is as [`beside_of`] reads it, by whether a critical request is queued among the other gates' live tickets
    /// `others`.
    fn beside(&mut self, others: &[Ticket], survey: &mut Survey, console: &mut Console) -> Beside {
        if !matches!(self.pool, Pool::Local(SlotKind::Light)) {
            return Beside::Unneeded;
        }
        let tried = slots::try_beside(self.root, survey, console);
        if matches!(tried, BesideTry::Busy) {
            self.waiter.retry_soon();
        }
        beside_of(tried, others.iter().any(|ticket| ticket.kind == TicketKind::Critical))
    }

    /// The slots of a count of `count` the gate tries on this pass, every one unless a critical request ahead holds it
    /// back ([`slot_range`]), and the room left it, `None` for no cap but the free slots. A light gate held back counts
    /// the light gates beside the request, the other gates' live light claims whatever their slots, holding the beside
    /// mutex `beside`, and has the room the allowance leaves while the other gates' live tickets `others` wait or hold
    /// ([`beside_allowance`]); it has none when it cannot list the claims, said once. A light gate that found the
    /// beside mutex held by another gate has no room this pass.
    fn range(&self, count: u32, others: &[Ticket], beside: &Beside, survey: &mut Survey, console: &mut Console) -> (Range<u32>, Option<usize>) {
        let busy = matches!(beside, Beside::Busy).then_some(0);
        let (Pool::Local(kind), Some(_)) = (self.pool, self.barrier) else {
            return (0..count, busy);
        };
        if kind == SlotKind::Heavy || busy.is_some() {
            return (slot_range(kind, count, 0), busy);
        }
        match survey.light_claims(&self.root.path, console) {
            Ok(claims) => {
                let room = beside_room(beside_allowance(count, others), claims.len());
                (slot_range(kind, count, room), Some(room))
            }
            Err(_) => (slot_range(kind, count, 0), Some(0)),
        }
    }

    /// Whether the gate may try the slots `range` with the room `room` left it: some are left it, and fewer live
    /// tickets of its kind of `others` are ahead of it than those slots are free, or than the room when that is less
    /// ([`may_take`]), so a try never takes the slot or the place beside a critical request an earlier waiter would
    /// get. The slots are probed only when the decision needs it; the pass's probe comes with it, `None` when it made
    /// none.
    fn may_try(
        &self,
        others: &[Ticket],
        range: Range<u32>,
        room: Option<usize>,
        survey: &mut Survey,
        console: &mut Console,
    ) -> (bool, Option<Probe>) {
        if range.is_empty() || room == Some(0) {
            return (false, None);
        }
        let ahead = self.ahead(others, survey, console);
        if ahead == 0 {
            return (true, None);
        }
        let probe = self.probe(range, survey, console);
        (may_take(ahead, slots_where(&probe, false), room), Some(probe))
    }

    /// Says the gate waits, once a wait. Held back by a critical request, it says so whether or not a slot is free.
    /// Else only when `probe`, this pass's only probe of the slots `range`, made now when the pass needed none, found
    /// none of them free, with the busy slots of that same probe. A gate that defers to the tickets ahead of it while a
    /// slot is free for it says nothing; its still-waiting lines report a long wait. The probe may predate a try that
    /// failed in the same pass, so a pass whose try lost the race for a slot it saw free says nothing until the next
    /// pass.
    fn say_waiting(&mut self, probe: Option<Probe>, range: Range<u32>, survey: &mut Survey, console: &mut Console) {
        let Some(line) = self.barrier_line().or_else(|| self.busy_line(probe, range, survey, console)) else {
            return;
        };
        self.said_waiting = true;
        console.err(&line);
        self.waiter.said_waiting();
    }

    /// The first line of a gate that finds none of the slots `range` free by `probe`, made now when the pass made
    /// none, with the busy slots of that same probe and when the first of their holders frees its slot ([`Wait::eta`]);
    /// `None` when one is free.
    fn busy_line(&self, probe: Option<Probe>, range: Range<u32>, survey: &mut Survey, console: &mut Console) -> Option<String> {
        let probe = probe.unwrap_or_else(|| self.probe(range.clone(), survey, console));
        if slots_where(&probe, false) != 0 {
            return None;
        }
        let busy = format!("{} busy", slots_where(&probe, true));
        let figures = with_eta(busy, self.eta(&probe, range, survey, console));
        Some(format!("gate: waiting for a {} ({figures})", self.place))
    }

    /// When the first holder of the busy slots of `range` frees its slot, by `probe`, that range's probe, and the
    /// other gates' live claims as they stand ([`pool_eta`]); `None` when a slot of the probe could not be probed.
    fn eta(&self, probe: &[Option<bool>], range: Range<u32>, survey: &mut Survey, console: &mut Console) -> Option<String> {
        if probe.iter().any(Option::is_none) {
            return None;
        }
        let busy: Vec<u32> = range
            .zip(probe)
            .filter(|(_, slot)| **slot == Some(true))
            .map(|(index, _)| index)
            .collect();
        if busy.is_empty() {
            return None;
        }
        let dir = v1::slots_dir(&self.root.path);
        let claims = survey
            .other_claims(&self.root.path, console)
            .inspect_err(|error| survey.say_once("cannot read the claims in", &dir.display().to_string(), error, console))
            .ok()?;
        pool_eta(&claims, self.pool, &busy, Which::First, Timestamp::now())
    }

    /// The first line of a gate the critical request of the last pass holds back; `None` when none did.
    fn barrier_line(&self) -> Option<String> {
        let barrier = self.barrier?;
        let line = match self.pool {
            Pool::Local(SlotKind::Light) => format!("gate: waiting for a {}", beside_place(barrier)),
            _ => format!("gate: waiting for a {} behind the critical request of gate {}", self.place, barrier.pid),
        };
        Some(line)
    }

    /// How many live tickets of the gate's kind and pool of `others` are ahead of it ([`queued_ahead`]). A ticket whose
    /// gate a live claim names is not counted: that gate holds a slot already, and only failed to delete its ticket.
    fn ahead(&self, others: &[Ticket], survey: &mut Survey, console: &mut Console) -> usize {
        let ahead: Vec<&Ticket> = others.iter().filter(|ticket| queued_ahead(ticket, self.pool, self.order)).collect();
        if ahead.is_empty() {
            return 0;
        }
        let holders = survey.holders(&self.root.path, console);
        ahead.iter().filter(|ticket| !holders.contains(&(ticket.pid, ticket.start_ms))).count()
    }

    /// Whether some gate has each of the pool's slots `indices` open; `None` for a slot that cannot be probed, said
    /// once, which counts as neither free nor busy.
    fn probe(&self, indices: Range<u32>, survey: &mut Survey, console: &mut Console) -> Probe {
        indices
            .map(|index| {
                let name = self.root.root.object_name(self.pool.object(index));
                mutex_exists(&name)
                    .inspect_err(|error| survey.say_once("cannot probe", &name, error, console))
                    .ok()
            })
            .collect()
    }

    /// Says the still-waiting line when it is due, with the whole seconds since the gate queued; a light gate held
    /// back by a critical request names it. A gate no critical request holds back probes the slots of the last pass,
    /// and when every one is busy says how many and when the first of their holders frees its slot ([`Wait::eta`]).
    pub(crate) fn say_heartbeat(&mut self, survey: &mut Survey, console: &mut Console) {
        let Some(waited) = self.waiter.heartbeat_due() else {
            return;
        };
        let seconds = format!("{} s", waited.as_secs());
        let line = match (self.pool, self.barrier) {
            (Pool::Local(SlotKind::Light), Some(barrier)) => format!("gate: still waiting for a {} ({seconds})", beside_place(barrier)),
            (_, Some(_)) => format!("gate: still waiting for a {} ({seconds})", self.place),
            (_, None) => format!(
                "gate: still waiting for a {} ({})",
                self.place,
                self.heartbeat_figures(seconds, survey, console)
            ),
        };
        console.err(&line);
    }

    /// The figures of a still-waiting line, `seconds` waited first: with the busy count and the first holder's ETA when
    /// a probe of the last pass's slots finds every one busy, else `seconds` alone, as for a gate that waits behind
    /// earlier tickets while a slot is free, or one whose probe failed.
    fn heartbeat_figures(&self, seconds: String, survey: &mut Survey, console: &mut Console) -> String {
        let range = self.range.clone();
        let probe = self.probe(range.clone(), survey, console);
        if probe.is_empty() || probe.iter().any(|slot| *slot != Some(true)) {
            return seconds;
        }
        let busy = format!("{seconds}; {} busy", probe.len());
        with_eta(busy, self.eta(&probe, range, survey, console))
    }
}

/// Whether `ticket` waits ahead of a gate at the place `order` in the queue of `pool`: of the pool's ticket kind,
/// remote exactly when the pool is, so a local and a remote waiter never count each other, of the same builder's pool
/// when remote (a ticket without a pool is `host`'s), and earlier.
fn queued_ahead(ticket: &Ticket, pool: Pool, order: (u64, u32)) -> bool {
    let remote = pool.is_remote();
    let id = pool.remote_id().map(|id| id.to_string());
    let same_pool = !remote || ticket.pool == id;
    ticket.kind == pool.ticket_kind() && (ticket.remote == Some(true)) == remote && same_pool && ticket.queue_order() < order
}

/// `figures` of a wait line, followed by `eta` when there is one.
fn with_eta(figures: String, eta: Option<String>) -> String {
    match eta {
        Some(eta) => format!("{figures}, {eta}"),
        None => figures,
    }
}

/// Which holder a wait line names: the first to free its slot, the one a slot's waiter gets, or the last, the one a
/// critical gate waits for once the others have freed theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    First,
    Last,
}

/// One holder as its ETA reads it: the seconds of its usual run time left, negative once past it, its usual run time,
/// the claim's `expected.p50`, and its ceiling in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Usual {
    pub left: f64,
    p50: f64,
    ceiling: f64,
}

/// The holder of `claim` as its ETA reads it at `now` ([`Usual`]); `None` when the claim carries no usual.
pub(crate) fn usual(claim: &Claim, now: Timestamp) -> Option<Usual> {
    let p50 = claim.expected.as_ref()?.get("p50")?.as_f64()?;
    Some(Usual {
        left: p50 - now.duration_since(claim.started).as_secs_f64(),
        p50,
        ceiling: claim.timeout_seconds,
    })
}

/// When the holders of the busy slots `busy`, each a claim kind and slot index, free them, by their live claims
/// `claims` at `now`: the expected end of the holder `which` names, its claim's `started` plus its `expected.p50`, as
/// `the first frees in about 4 min`, or once that end is at or before `now`, how far past it the holder is, with its
/// usual and its ceiling ([`eta_text`]). `None` when no slot is busy, or a busy slot has no claim carrying a usual.
pub(crate) fn holder_eta(claims: &[Claim], busy: &[(ClaimKind, u32)], which: Which, now: Timestamp) -> Option<String> {
    named_eta(holder_usuals(claims, busy, now)?, which)
}

/// When the holders of the busy slots `busy` of `pool` free them, by the other gates' live claims `claims`, each with its
/// file name, at `now`, as [`holder_eta`] says it: a slot's holder is the claim named as that slot's claim file, so a
/// remote pool's estimate never names the holder of another builder's pool, whose claims are of the same kind. `None`
/// when a busy slot has no claim carrying a usual.
fn pool_eta(claims: &[(String, Claim)], pool: Pool, busy: &[u32], which: Which, now: Timestamp) -> Option<String> {
    let files: Vec<String> = busy.iter().filter_map(|&index| pool.object(index).claim_file_name()).collect();
    let own: Vec<Claim> = claims
        .iter()
        .filter(|(name, _)| files.contains(name))
        .map(|(_, claim)| claim.clone())
        .collect();
    let kind = pool.claim_kind();
    let slots: Vec<(ClaimKind, u32)> = busy.iter().map(|&index| (kind, index)).collect();
    holder_eta(&own, &slots, which, now)
}

/// The holders of the busy slots `busy`, each a claim kind and slot index, as their ETAs read them at `now` by their
/// live claims `claims` ([`usual`]); `None` when a busy slot has no claim carrying a usual.
pub(crate) fn holder_usuals(claims: &[Claim], busy: &[(ClaimKind, u32)], now: Timestamp) -> Option<Vec<Usual>> {
    let mut holders = Vec::with_capacity(busy.len());
    for &(kind, index) in busy {
        let claim = claims.iter().find(|claim| claim.kind == kind && claim.index == index)?;
        holders.push(usual(claim, now)?);
    }
    Some(holders)
}

/// What a wait line says of the holder of `holders` that `which` names ([`eta_text`]); `None` when there is none.
pub(crate) fn named_eta(holders: Vec<Usual>, which: Which) -> Option<String> {
    let by_left = |one: &Usual, other: &Usual| one.left.total_cmp(&other.left);
    let named = match which {
        Which::First => holders.into_iter().min_by(by_left),
        Which::Last => holders.into_iter().max_by(by_left),
    }?;
    Some(eta_text(named, which))
}

/// What a wait line says of `holder`, the holder `which` names: `the first frees in about {left}` while its usual run
/// time lasts, else `the first is {over} past its usual {p50}, ceiling {ceiling}`; each figure by the expected-time
/// line's rule ([`span_text`]).
fn eta_text(holder: Usual, which: Which) -> String {
    let name = match which {
        Which::First => "the first",
        Which::Last => "the last",
    };
    if holder.left > 0.0 {
        return format!("{name} frees in about {}", span_text(holder.left));
    }
    format!(
        "{name} is {} past its usual {}, ceiling {}",
        span_text(-holder.left),
        span_text(holder.p50),
        span_text(holder.ceiling)
    )
}

/// What a light gate the critical request `barrier` holds back waits for: a place beside it, or, behind a request that
/// holds the machine alone, its end.
fn beside_place(barrier: Barrier) -> String {
    let Barrier { pid, alone } = barrier;
    if alone {
        format!("light slot behind the request of gate {pid} that holds the machine alone")
    } else {
        format!("light slot beside the critical request of gate {pid}")
    }
}

/// Whether the gate under a light parent gate is lent that parent's slot before it queues, by the same rule and reads
/// as each pass of a wait ([`lends`]): the other gates' live tickets under `root`, then the critical mutex only as the
/// rule needs it, the lend confirmed holding the beside mutex ([`lend_held`]), which comes with it. `None` when the
/// parent does not lend, or the beside mutex is held by another gate: the gate then queues and asks on each pass.
pub(crate) fn lent_before_queue(root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) -> Option<OwnedMutexGuard> {
    let others = live_others(root, survey, console);
    let unreadable = survey.unreadable_ticket();
    if !lends(&others, unreadable, &mut Probed { root, survey, console }) {
        return None;
    }
    lend_held(root, survey, console)
}

/// The beside mutex under `root`, for a lend the tickets allowed a moment ago, once a read of the tickets made holding
/// it still allows it ([`lends`]): a critical gate rewrites its ticket as holding the machine only holding that mutex,
/// after it counted the lend markers, so a lend that holds it until its marker is written is either counted or never
/// made. `None` when another gate holds it, or the lend is no longer allowed.
fn lend_held(root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) -> Option<OwnedMutexGuard> {
    let guard = slots::try_beside(root, survey, console).held()?;
    let others = live_others(root, survey, console);
    let unreadable = survey.unreadable_ticket();
    lends(&others, unreadable, &mut Probed { root, survey, console }).then_some(guard)
}

/// The counts file under `root` as it stands, a missing or unreadable one giving the defaults for `processors`.
fn read_counts(root: &ResolvedRoot, processors: usize) -> Counts {
    v1::read_counts(&v1::counts_path(&root.path), processors)
}

/// The other gates' live tickets under `root`. A read that deleted a dead gate's ticket wakes the waiters, since the
/// queue moved.
fn live_others(root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) -> Vec<Ticket> {
    let (others, deleted) = survey.others(&root.path, console);
    if deleted {
        slots::signal(root, &others);
    }
    others
}

/// What a lend turns on besides the tickets, read only when the tickets leave it open.
trait LendFacts {
    /// Whether some gate has the critical mutex open.
    fn critical_open(&mut self) -> bool;
}

/// The lend facts as the machine has them: the critical mutex under `root`.
struct Probed<'p> {
    root: &'p ResolvedRoot,
    survey: &'p mut Survey,
    console: &'p mut Console,
}

impl LendFacts for Probed<'_> {
    fn critical_open(&mut self) -> bool {
        slots::critical_open(self.root, self.survey, self.console)
    }
}

/// Whether a light parent lends its slot to the gate under it: while a critical request is pending, by a ticket of
/// the other gates' live tickets `others` or else the critical mutex open, and none of them refuses a new lend
/// ([`slots::critical_refuses_lends`]), whatever light slot the parent holds. The tickets are read first: `facts`
/// probes the critical mutex only when no critical ticket is pending, and not at all when the read found a ticket it
/// could not read (`unreadable`), which may be a critical request's that refuses: the gate then queues as any other.
fn lends(others: &[Ticket], unreadable: bool, facts: &mut impl LendFacts) -> bool {
    !slots::critical_refuses_lends(others) && slots::critical_pending(others, || !unreadable && facts.critical_open())
}

/// The barrier of a gate at the place `order`: the earliest of the other gates' live critical tickets `others` ahead
/// of it, by arrival and then pid; `None` when none is ahead.
fn barrier(others: &[Ticket], order: (u64, u32)) -> Option<Barrier> {
    others
        .iter()
        .filter(|ticket| ticket.kind == TicketKind::Critical && ticket.queue_order() < order)
        .min_by_key(|ticket| ticket.queue_order())
        .map(|earliest| Barrier {
            pid: earliest.pid,
            alone: earliest.holds_alone(),
        })
}

/// The barrier of a light gate at the place `order` ([`barrier`]), named by the request that leaves it no room: once a
/// barrier holds it back, the earliest of `others`' requests to hold the machine alone, ahead of it or not, since any
/// of them leaves no light gate room beside a critical request ([`beside_allowance`]); else the barrier itself.
fn light_barrier(others: &[Ticket], order: (u64, u32)) -> Option<Barrier> {
    let ahead = barrier(others, order)?;
    if ahead.alone {
        return Some(ahead);
    }
    let alone = others
        .iter()
        .filter(|ticket| ticket.holds_alone())
        .min_by_key(|ticket| ticket.queue_order());
    Some(alone.map_or(ahead, |ticket| Barrier {
        pid: ticket.pid,
        alone: true,
    }))
}

/// The slots of a local pool of `kind` and `count` a gate behind a critical request tries: none for a heavy gate; for a
/// light gate every one while `room` places are left beside the request ([`beside_room`]), and none once none is.
fn slot_range(kind: SlotKind, count: u32, room: usize) -> Range<u32> {
    match kind {
        SlotKind::Light if room > 0 => 0..count,
        SlotKind::Light | SlotKind::Heavy => count..count,
    }
}

/// How many more light gates may run beside a critical request with `beside` running there, for an `allowance` of
/// light gates beside it ([`beside_allowance`]): the allowance less those.
fn beside_room(allowance: u32, beside: usize) -> usize {
    usize::try_from(allowance).map_or(usize::MAX, |allowance| allowance.saturating_sub(beside))
}

/// Whether a gate with `ahead` live tickets of its kind ahead of it may try a slot, with `free` slots free and `room`
/// places left beside a critical request that holds it back, `None` when none does: fewer are ahead than the free
/// slots, or than the room when that is less, so an earlier gate held back keeps the one place left.
fn may_take(ahead: usize, free: usize, room: Option<usize>) -> bool {
    ahead < room.map_or(free, |room| free.min(room))
}

/// The beside mutex of a local light gate's pass as `tried` found it, with `critical_queued` whether a critical request
/// is queued: held, or busy while another gate holds it. One that cannot be taken is no room this pass while a critical
/// request is queued, whose count could miss a gate that takes without it; with none queued, no count needs it, and the
/// gate takes its slot without it.
fn beside_of(tried: BesideTry, critical_queued: bool) -> Beside {
    match tried {
        BesideTry::Held(guard) => Beside::Held(guard),
        BesideTry::Busy => Beside::Busy,
        BesideTry::Failed if critical_queued => Beside::Busy,
        BesideTry::Failed => Beside::Untaken,
    }
}

/// The beside mutex as a pass of a wait finds it ([`Wait::beside`]).
enum Beside {
    /// The gate takes no local light slot, so it needs none.
    Unneeded,
    /// The gate holds it for this pass's count and try and, once it takes a slot, until its claim is written.
    Held(OwnedMutexGuard),
    /// It cannot be taken and no critical request is queued: the gate takes its slot without it.
    Untaken,
    /// Another gate holds it, or it cannot be taken while a critical request is queued: no room this pass.
    Busy,
}

impl Beside {
    /// The mutex, when the pass holds it.
    fn into_guard(self) -> Option<OwnedMutexGuard> {
        match self {
            Beside::Held(guard) => Some(guard),
            Beside::Unneeded | Beside::Untaken | Beside::Busy => None,
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

/// What the unit tests of a wait over a real state root share: a scratch root, a request of this test process, and a
/// slot held by another thread, as another gate holds one.
#[cfg(test)]
#[expect(clippy::expect_used, reason = "a scratch root or a slot the test cannot set up must fail the test loudly")]
pub(crate) mod scratch {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;

    use gate_args::{Ceiling, RunRequest, Slot, Spelling, TestSeams};
    use gate_state::v1::{Claim, ClaimKind};
    use gate_state::{KernelObject, StateRoot, v1};
    use jiff::Timestamp;
    use serde_json::json;

    use crate::acquire;
    use crate::resolve::ResolvedRoot;

    /// A state root for one test, its folder removed when it ends.
    pub(crate) struct Scratch {
        pub root: ResolvedRoot,
    }

    impl Scratch {
        /// A fresh root named after `test` and this process, so no two tests share one.
        pub(crate) fn new(test: &str) -> Scratch {
            let folder = crate::test_support::temp_dir().join(format!("gate-wait-{}-{test}", std::process::id()));
            // A folder an earlier run of this test left behind is cleared; one that is not there is no failure.
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the scratch folder can be made");
            let root = StateRoot::new(folder.to_str().expect("the scratch folder has a Unicode path"), None);
            Scratch {
                root: ResolvedRoot { path: folder, root },
            }
        }

        /// The root's folder.
        pub(crate) fn path(&self) -> &Path {
            &self.root.path
        }

        /// The full name of `object` under the root.
        pub(crate) fn name(&self, object: KernelObject) -> String {
            self.root.root.object_name(object)
        }

        /// Makes the root's light pool one slot.
        pub(crate) fn one_light_slot(&self) {
            fs::write(v1::counts_path(self.path()), r#"{"v":1,"light":1}"#).expect("the counts file can be written");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // A folder a test still holds a file of is left for the next run to clear.
            let _ = fs::remove_dir_all(&self.root.path);
        }
    }

    /// A run of `slot` of `fake-tool exit 0`, as the gate reads one, logging to `out.log`, whose still-waiting lines come
    /// an hour apart, so no test's lines depend on how long it took.
    pub(crate) fn request(slot: Slot) -> RunRequest {
        let test_seams = TestSeams {
            heartbeat_ms: Some(3_600_000),
            ..TestSeams::default()
        };
        RunRequest {
            slot,
            timeout: Ceiling::Given(60),
            log: PathBuf::from("out.log"),
            stall: 60,
            tail: 0,
            no_markers: false,
            local: false,
            alone: false,
            reason: None,
            bring_tracked: Vec::new(),
            env: Vec::new(),
            state_dir: None,
            cd: None,
            command: ["fake-tool", "exit", "0"].into_iter().map(OsString::from).collect(),
            test_seams,
            print_request: false,
            spelling: Spelling::Cli,
        }
    }

    /// The live claim of another gate holding slot `index` of `kind` since `started` under a 900 s ceiling, with a usual
    /// run time of `p50` seconds when its command has history.
    pub(crate) fn holding(kind: ClaimKind, index: u32, started: Timestamp, p50: Option<f64>) -> Claim {
        let expected = p50.map(|p50| {
            json!({"p50": p50, "p90": p50, "n": 5, "key": "local:: | cargo build"})
                .as_object()
                .cloned()
                .expect("expected is an object")
        });
        Claim {
            kind,
            index,
            pid: 1,
            process_start: started,
            started,
            cwd: r"C:\work".to_owned(),
            command: "cargo build".to_owned(),
            log: r"C:\work\out.log".to_owned(),
            timeout_seconds: 900.0,
            remote: None,
            host: None,
            key: None,
            reason: None,
            expected,
        }
    }

    /// A slot held by a thread of its own, as another gate holds one: the mutex is let go on that thread when this is
    /// dropped.
    pub(crate) struct Holder {
        go: Option<mpsc::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Holder {
        /// Takes the mutex `name` on a thread of its own, and returns once it holds it.
        pub(crate) fn take(name: String) -> Holder {
            let (taken, has_taken) = mpsc::channel();
            let (go, wait_for_go) = mpsc::channel::<()>();
            let thread = thread::spawn(move || {
                let guard = acquire::try_slot(&name).expect("the slot can be tried").expect("the slot is free");
                taken.send(()).expect("the test waits for the take");
                // The sender dropped is the signal: the guard goes on this thread, which owns the mutex.
                let _ = wait_for_go.recv();
                drop(guard);
            });
            has_taken.recv().expect("the holder took the slot");
            Holder {
                go: Some(go),
                thread: Some(thread),
            }
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            drop(self.go.take());
            if let Some(thread) = self.thread.take() {
                thread.join().expect("the holder let the slot go");
            }
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a wait the test cannot set up must fail the test loudly")]
mod tests {
    use gate_args::Slot;
    use gate_state::v1::{self, Ticket, TicketKind};
    use gate_state::{KernelObject, PoolId, SlotKind, beside_allowance};

    use std::time::{Duration, Instant};

    use gate_state::v1::ClaimKind;
    use jiff::{SignedDuration, Timestamp};

    use super::scratch::{Holder, Scratch, holding, request};
    use super::{
        Arrival, Barrier, Beside, Host, LendFacts, Wait, Waited, Which, barrier, beside_of, beside_place, beside_room, first_heartbeat, holder_eta,
        lends, light_barrier, may_take, pool_eta, queued_ahead, slot_range,
    };
    use crate::console::Console;
    use crate::slots::BesideTry;
    use crate::slots::{self, BuilderPool, Pool, Survey, ThisGate};

    /// The fixed moment the ETA cases count from.
    fn now() -> Timestamp {
        "2026-10-13T12:00:00Z".parse().expect("a fixed time")
    }

    /// `seconds` before [`now`].
    fn ago(seconds: i64) -> Timestamp {
        now().checked_sub(SignedDuration::from_secs(seconds)).expect("a moment after the epoch")
    }

    const HEAVY_0: (ClaimKind, u32) = (ClaimKind::Heavy, 0);
    const HEAVY_1: (ClaimKind, u32) = (ClaimKind::Heavy, 1);

    #[test]
    fn the_eta_names_the_holder_that_frees_its_slot_first() {
        let claims = [
            holding(ClaimKind::Heavy, 0, now(), Some(600.0)),
            holding(ClaimKind::Heavy, 1, ago(200), Some(260.0)),
        ];

        assert_eq!(
            holder_eta(&claims, &[HEAVY_0, HEAVY_1], Which::First, now()).as_deref(),
            Some("the first frees in about 60 s"),
            "60 s left of the second holder's usual, 600 s of the first's"
        );
        assert_eq!(
            holder_eta(&claims, &[HEAVY_0], Which::First, now()).as_deref(),
            Some("the first frees in about 10 min"),
            "only the busy slots' holders count"
        );
    }

    #[test]
    fn the_eta_is_in_seconds_below_90_s_and_whole_minutes_from_90_s_never_below_1() {
        let eta = |p50: f64| {
            holder_eta(
                &[holding(ClaimKind::Light, 0, now(), Some(p50))],
                &[(ClaimKind::Light, 0)],
                Which::First,
                now(),
            )
        };

        assert_eq!(eta(89.0).as_deref(), Some("the first frees in about 89 s"));
        assert_eq!(eta(90.0).as_deref(), Some("the first frees in about 2 min"));
        assert_eq!(eta(0.4).as_deref(), Some("the first frees in about 1 s"));
    }

    #[test]
    fn a_busy_slot_whose_holder_has_no_usual_hides_the_eta() {
        let with_history = holding(ClaimKind::Heavy, 0, now(), Some(240.0));
        let without = holding(ClaimKind::Heavy, 1, now(), None);
        let light = holding(ClaimKind::Light, 1, now(), Some(240.0));

        assert_eq!(
            holder_eta(&[with_history.clone(), without], &[HEAVY_0, HEAVY_1], Which::First, now()),
            None,
            "a holder without expected"
        );
        assert_eq!(
            holder_eta(&[with_history.clone(), light], &[HEAVY_0, HEAVY_1], Which::First, now()),
            None,
            "a busy slot with no claim of its kind"
        );
        assert_eq!(holder_eta(&[with_history], &[], Which::First, now()), None, "no busy slot");
    }

    #[test]
    fn a_remote_waiters_eta_names_the_holder_of_its_own_builders_pool() {
        let mac = PoolId::parse("1f0efc62").expect("8 lower-case hex digits are a pool id");
        let claims = [
            ("gate-remote-slot-0.json".to_owned(), holding(ClaimKind::Remote, 0, now(), Some(600.0))),
            (
                "gate-remote-1f0efc62-slot-0.json".to_owned(),
                holding(ClaimKind::Remote, 0, now(), Some(60.0)),
            ),
        ];
        let eta = |id| pool_eta(&claims, Pool::Remote(BuilderPool { slots: 1, id }), &[0], Which::First, now());

        assert_eq!(eta(None).as_deref(), Some("the first frees in about 10 min"), "host's holder");
        assert_eq!(eta(Some(mac)).as_deref(), Some("the first frees in about 60 s"), "mac-1's holder");
    }

    #[test]
    fn a_holder_past_its_usual_is_said_with_its_usual_and_ceiling() {
        let past = |started| holder_eta(&[holding(ClaimKind::Heavy, 0, started, Some(240.0))], &[HEAVY_0], Which::First, now());

        assert_eq!(past(ago(420)).as_deref(), Some("the first is 3 min past its usual 4 min, ceiling 15 min"));
        assert_eq!(
            past(ago(240)).as_deref(),
            Some("the first is 1 s past its usual 4 min, ceiling 15 min"),
            "an end at now is past, never 0 s"
        );
    }

    /// The still-waiting line `wait` says when one is due now, its whole seconds waited shown as `N`.
    fn heartbeat(wait: &mut Wait<'_>, survey: &mut Survey, console: &mut Console) -> Vec<String> {
        wait.waiter.next_heartbeat = Duration::ZERO;
        wait.say_heartbeat(survey, console);
        console
            .take_notes()
            .iter()
            .map(|line| {
                let (head, tail) = line.split_once('(').expect("the line has its figures");
                format!("{head}(N{}", tail.trim_start_matches(|digit: char| digit.is_ascii_digit()))
            })
            .collect()
    }

    #[test]
    fn the_still_waiting_line_says_the_busy_count_only_while_every_slot_is_busy() {
        let scratch = Scratch::new("heartbeat");
        scratch.one_light_slot();
        let request = request(Slot::Light);
        let gate = ThisGate::read(&request).expect("this process's start can be read");
        let mut survey = Survey::new(&gate);
        let mut console = Console::new();
        let holder = Holder::take(scratch.name(KernelObject::LightSlot(0)));
        let arrival = Arrival {
            gate: &gate,
            request: &request,
            queued: Instant::now(),
            arrived: gate.start_ms,
            builder: None,
            after_fallback: false,
            under: None,
            expected: None,
        };
        let host = Host {
            processors: 4,
            interrupt: None,
        };
        let mut wait = Wait::queue(&scratch.root, Pool::Local(SlotKind::Light), arrival, host, &mut console);
        assert!(wait.pass(&mut survey, &mut console).is_none(), "the one light slot is held");
        console.take_notes();

        assert_eq!(
            heartbeat(&mut wait, &mut survey, &mut console),
            ["gate: still waiting for a light slot (N s; 1 busy)"],
            "every slot busy, its holder without history"
        );
        drop(holder);
        assert_eq!(
            heartbeat(&mut wait, &mut survey, &mut console),
            ["gate: still waiting for a light slot (N s)"],
            "a slot free, as for a gate that defers to earlier tickets"
        );
        let held_again = Holder::take(scratch.name(KernelObject::LightSlot(0)));
        wait.barrier = Some(Barrier { pid: 9, alone: false });
        assert_eq!(
            heartbeat(&mut wait, &mut survey, &mut console),
            ["gate: still waiting for a light slot beside the critical request of gate 9 (N s)"],
            "held back by a critical request, the line is as it was"
        );
        drop(held_again);
        wait.end(&mut console);
    }

    #[test]
    fn a_pass_waits_while_the_pool_is_full_and_takes_the_slot_once_it_frees() {
        let scratch = Scratch::new("one-pass");
        scratch.one_light_slot();
        let request = request(Slot::Light);
        let gate = ThisGate::read(&request).expect("this process's start can be read");
        let mut survey = Survey::new(&gate);
        let mut console = Console::new();
        let holder = Holder::take(scratch.name(KernelObject::LightSlot(0)));
        let arrival = Arrival {
            gate: &gate,
            request: &request,
            queued: Instant::now(),
            arrived: gate.start_ms,
            builder: None,
            after_fallback: false,
            under: None,
            expected: None,
        };
        let host = Host {
            processors: 4,
            interrupt: None,
        };
        let mut wait = Wait::queue(&scratch.root, Pool::Local(SlotKind::Light), arrival, host, &mut console);
        let ticket = v1::ticket_path(scratch.path(), gate.pid, gate.start_ms);

        assert!(wait.pass(&mut survey, &mut console).is_none(), "the one light slot is held");
        assert_eq!(console.take_notes(), ["gate: waiting for a light slot (1 busy)"]);
        assert!(wait.pass(&mut survey, &mut console).is_none(), "still held");
        assert!(console.take_notes().is_empty(), "the waiting line is said once");
        assert!(ticket.exists(), "the gate keeps its ticket while it waits");

        drop(holder);
        let taken = match wait.pass(&mut survey, &mut console) {
            Some(Waited::Taken(taken)) => Some(taken),
            _ => None,
        }
        .expect("the freed slot is taken on the next pass");
        assert_eq!(taken.held.name(), scratch.name(KernelObject::LightSlot(0)));
        wait.end(&mut console);
        assert!(!ticket.exists(), "the wait's end deletes the ticket");
    }

    /// A ticket of the gate `pid`, started at `pid * 10` ms, of `kind`, arrived at `arrived`.
    fn ticket(kind: TicketKind, arrived: u64, pid: u32) -> Ticket {
        Ticket {
            kind,
            arrived,
            pid,
            start_ms: u64::from(pid) * 10,
            holds: false,
            lends_closed: false,
            alone: false,
            remote: None,
            pool: None,
            reason: None,
            expected: None,
        }
    }

    #[test]
    fn a_remote_waiter_counts_ahead_only_the_tickets_of_its_own_builders_pool() {
        let mac = PoolId::parse("1f0efc62").expect("8 lower-case hex digits are a pool id");
        let remote = |arrived: u64, pid: u32, pool: Option<&str>| Ticket {
            remote: Some(true),
            pool: pool.map(str::to_owned),
            ..ticket(TicketKind::Heavy, arrived, pid)
        };
        let local_with_pool = Ticket {
            pool: Some("1f0efc62".to_owned()),
            ..ticket(TicketKind::Heavy, 40, 5)
        };
        let others = [
            remote(10, 1, None),
            remote(20, 2, Some("1f0efc62")),
            remote(30, 3, Some("0a1b2c3d")),
            ticket(TicketKind::Heavy, 5, 4),
            local_with_pool,
            remote(200, 6, Some("1f0efc62")),
        ];
        let order = (100, 9);
        let ahead = |pool: Pool| -> Vec<u32> {
            others
                .iter()
                .filter(|ticket| queued_ahead(ticket, pool, order))
                .map(|ticket| ticket.pid)
                .collect()
        };

        assert_eq!(
            ahead(Pool::Remote(BuilderPool { slots: 1, id: None })),
            [1],
            "host's pool: tickets without a pool"
        );
        assert_eq!(
            ahead(Pool::Remote(BuilderPool { slots: 1, id: Some(mac) })),
            [2],
            "the mac's pool: its own tickets"
        );
        assert_eq!(
            ahead(Pool::Local(SlotKind::Heavy)),
            [4, 5],
            "a local waiter counts local tickets, whatever their pool"
        );
    }

    #[test]
    fn the_first_still_waiting_line_after_a_fallback_comes_at_the_next_period() {
        let period = Duration::from_millis(1000);
        assert_eq!(
            first_heartbeat(period, Duration::ZERO),
            period,
            "an ordinary wait says it still waits after one period"
        );
        assert_eq!(
            first_heartbeat(period, Duration::from_millis(2500)),
            Duration::from_millis(3000),
            "a wait 2.5 periods in says its first line at 3 periods, never at once"
        );
        assert_eq!(
            first_heartbeat(period, period),
            Duration::from_millis(2000),
            "a whole period already waited moves the line to the next one"
        );
    }

    #[test]
    fn the_barrier_is_the_earliest_critical_request_strictly_ahead_and_never_the_gates_own() {
        let own = ticket(TicketKind::Heavy, 100, 7);
        let order = own.queue_order();
        let mut holding = ticket(TicketKind::Critical, 60, 12);
        holding.holds = true;
        let tickets = vec![
            own.clone(),
            ticket(TicketKind::Critical, 80, 11),
            holding,
            ticket(TicketKind::Critical, 200, 13),
            ticket(TicketKind::Heavy, 10, 14),
        ];

        let others = slots::others(tickets, (own.pid, own.start_ms));

        assert!(others.iter().all(|ticket| ticket.pid != own.pid), "{others:?}");
        assert_eq!(
            barrier(&others, order),
            Some(Barrier { pid: 12, alone: false }),
            "the earliest critical request ahead, which holds the machine"
        );
        let later = [ticket(TicketKind::Critical, 100, 8), ticket(TicketKind::Critical, 200, 13)];
        assert_eq!(
            barrier(&later, order),
            None,
            "a request arrived with it but of a higher pid, or after it, is no barrier"
        );
        let tie = [ticket(TicketKind::Critical, 100, 6), ticket(TicketKind::Heavy, 50, 5)];
        assert_eq!(
            barrier(&tie, order),
            Some(Barrier { pid: 6, alone: false }),
            "a request arrived with it, of a lower pid, still waiting"
        );
        assert_eq!(barrier(&[], order), None);
    }

    #[test]
    fn a_light_gate_behind_a_plain_critical_request_is_named_behind_a_later_request_to_hold_the_machine_alone() {
        let own = ticket(TicketKind::Light, 100, 7);
        let order = own.queue_order();
        let mut later_alone = ticket(TicketKind::Critical, 150, 21);
        later_alone.alone = true;
        let mut latest_alone = ticket(TicketKind::Critical, 300, 22);
        latest_alone.alone = true;
        let plain = ticket(TicketKind::Critical, 50, 12);

        let behind = light_barrier(&[plain.clone(), latest_alone, later_alone], order);

        assert_eq!(
            behind,
            Some(Barrier { pid: 21, alone: true }),
            "the earliest alone request leaves it no room"
        );
        let line = behind.map(beside_place);
        assert_eq!(
            line.as_deref(),
            Some("light slot behind the request of gate 21 that holds the machine alone")
        );
        assert_eq!(
            light_barrier(std::slice::from_ref(&plain), order),
            Some(Barrier { pid: 12, alone: false }),
            "with no alone request, the plain request ahead"
        );
        let mut alone_only = ticket(TicketKind::Critical, 150, 21);
        alone_only.alone = true;
        assert_eq!(light_barrier(&[alone_only], order), None, "a later request alone is no barrier by itself");
    }

    #[test]
    fn behind_a_barrier_a_heavy_gate_tries_no_slot_and_a_light_one_any_slot_while_fewer_than_the_allowance_run_beside_it() {
        let tried = |kind: SlotKind, count: u32, beside: usize| slot_range(kind, count, beside_room(beside_allowance(count, []), beside));
        assert_eq!(tried(SlotKind::Heavy, 3, 0), 3..3, "a heavy gate tries none");
        assert_eq!(tried(SlotKind::Light, 5, 0), 0..5, "any light slot, the lowest included");
        assert_eq!(tried(SlotKind::Light, 5, 1), 0..5);
        assert_eq!(tried(SlotKind::Light, 5, 2), 5..5, "two light gates beside it already");
        assert_eq!(tried(SlotKind::Light, 1, 0), 0..1, "a count of 1 allows one light gate");
        assert_eq!(tried(SlotKind::Light, 1, 1), 1..1);
        assert_eq!(tried(SlotKind::Light, 5, usize::MAX), 5..5, "no beside count, no slot");
    }

    #[test]
    fn of_two_light_gates_held_back_with_one_place_left_beside_the_request_the_later_one_leaves_it_to_the_earlier() {
        let room = |count: u32, beside: usize| beside_room(beside_allowance(count, []), beside);
        assert_eq!(room(5, 1), 1, "one light gate beside it, one place left");
        assert_eq!(room(5, 3), 0, "past the allowance, no place");
        assert_eq!(room(1, 0), 1);
        assert!(!may_take(1, 4, Some(1)), "four slots free, but the one place goes to the earlier gate");
        assert!(may_take(0, 4, Some(1)), "the earlier gate takes the place");
        assert!(may_take(1, 4, Some(2)), "two places, one for each");
        assert!(may_take(1, 4, None), "no request holds it back: the free slots alone");
        assert!(!may_take(2, 2, None));
    }

    /// One row of the lend table: the other gates' tickets, whether the critical mutex is open, whether the read found
    /// a ticket it could not read, whether the parent lends, and why.
    type Row<'t> = (&'t [Ticket], bool, bool, bool, &'t str);

    #[test]
    fn a_parent_lends_its_slot_whatever_its_index_only_while_a_critical_request_drains() {
        let waiting = [ticket(TicketKind::Critical, 50, 9)];
        let mut holds = ticket(TicketKind::Critical, 50, 9);
        holds.holds = true;
        let holding = [holds];
        let mut closed = ticket(TicketKind::Critical, 50, 9);
        closed.lends_closed = true;
        let closing = [closed];
        let heavy = [ticket(TicketKind::Heavy, 50, 10)];
        let table: [Row<'_>; 8] = [
            (&waiting, false, false, true, "a waiting request"),
            (&waiting, false, true, true, "a waiting request, though another ticket cannot be read"),
            (&heavy, true, false, true, "the critical mutex open with no critical ticket"),
            (&heavy, true, true, false, "the critical mutex open and a ticket that cannot be read"),
            (&heavy, false, false, false, "no critical request"),
            (&[], false, false, false, "no ticket at all"),
            (&holding, true, false, false, "a request that holds the machine"),
            (
                &closing,
                true,
                false,
                false,
                "a request that holds its slots and waits for its lent gates",
            ),
        ];
        for (others, open, unreadable, expected, why) in table {
            assert_eq!(lends(others, unreadable, &mut Known { open }), expected, "{why}");
        }
    }

    #[test]
    fn a_light_gate_that_cannot_take_the_beside_mutex_takes_without_it_only_while_no_critical_request_is_queued() {
        assert!(matches!(beside_of(BesideTry::Failed, false), Beside::Untaken), "no count needs it");
        assert!(
            matches!(beside_of(BesideTry::Failed, true), Beside::Busy),
            "no room while a request is queued"
        );
        assert!(matches!(beside_of(BesideTry::Busy, false), Beside::Busy), "another gate holds it");
        assert!(matches!(beside_of(BesideTry::Busy, true), Beside::Busy));
    }

    /// Lend facts known up front: whether the critical mutex is open.
    struct Known {
        open: bool,
    }

    impl LendFacts for Known {
        fn critical_open(&mut self) -> bool {
            self.open
        }
    }
}

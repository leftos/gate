//! The wait for a slot: the gate's wake event and arrival ticket, the passes that re-read the counts and the tickets
//! and try the pool when fewer live tickets of the gate's kind are ahead of it than slots are free, the barrier of a
//! critical request that arrived before the gate, the lend of a light parent's slot while a critical request drains,
//! the pause marker a gate under a parent's slot keeps while a critical request holds it back, and the lines a waiting
//! gate says. The waiter's core, which a critical gate's wait for the machine shares, is [`Waiter`].

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gate_args::RunRequest;
use gate_state::v1::{self, Counts, Ticket, TicketKind};
use gate_state::{KernelObject, SlotKind, reserved_light};
use gate_win32::{Interrupt, NamedEvent, mutex_exists};

use crate::console::Console;
use crate::pause::Pause;
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
    /// The pause marker this gate keeps while it waits behind a critical request; no marker for a gate under no parent.
    pause: Pause,
    waiter: Waiter,
    said_waiting: bool,
    said_ignored: bool,
}

/// The earliest critical request ahead of a waiting gate, which holds it back: its gate's pid, and whether a critical
/// request ahead of the gate holds the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Barrier {
    pid: u32,
    holding: bool,
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
    /// The parent gate whose slot does not cover this gate, which lends it while a critical request drains, or the parent
    /// that holds no slot, which lends nothing; `None` for any other gate.
    pub under: Option<UnderParent>,
}

/// How a wait ended.
pub(crate) enum Waited {
    Taken(Taken),
    /// The gate's light parent lends it its slot while a critical request drains.
    Lent,
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
            if self.waiter.interrupted() {
                return Waited::Interrupted;
            }
            let (count, light) = self.counts(console);
            let others = live_others(self.root, survey, console);
            if self.lent(&others, light, survey, console) {
                return Waited::Lent;
            }
            self.barrier = self.barrier_in(&others);
            self.pause.refresh(self.barrier.is_some(), console);
            let (range, probe) = self.range(count, survey, console);
            let (may_try, probe) = self.may_try(&others, range.clone(), probe, survey, console);
            if may_try && let Some(taken) = slots::try_pool(self.root, self.pool, range.clone(), survey, console) {
                return Waited::Taken(taken);
            }
            if !self.said_waiting {
                self.say_waiting(probe, range, survey, console);
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

    /// Deletes the gate's ticket, the first step of its take, and its pause marker, whichever way the wait ended.
    pub(crate) fn end(mut self, console: &mut Console) {
        self.pause.end(console);
        self.waiter.end(console);
    }

    /// The pool's count and the light count: the remote pool's own size, or a local pool's from the counts file as it
    /// stands, its ignored line said once a wait; the light count from the same read, or 0 for the remote pool, whose
    /// wait no parent lends.
    fn counts(&mut self, console: &mut Console) -> (u32, u32) {
        let kind = match self.pool {
            Pool::Remote(slots) => return (slots, 0),
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
        (setting.count, counts.light.count)
    }

    /// Whether the gate's light parent lends it its slot on a pass that read the other gates' live tickets `others`
    /// and a light count of `light` ([`lends`]); never for a gate under no such parent, nor under a parent that holds no
    /// slot.
    fn lent(&self, others: &[Ticket], light: u32, survey: &mut Survey, console: &mut Console) -> bool {
        let root = self.root;
        self.under.filter(|parent| parent.lends).is_some_and(|parent| {
            let mut facts = Probed {
                root,
                parent: parent.pid,
                survey,
                console,
            };
            lends(others, light, &mut facts)
        })
    }

    /// The critical request of the other gates' live tickets `others` that holds the gate back ([`barrier`]); none for
    /// a remote wait, which no critical request holds back.
    fn barrier_in(&self, others: &[Ticket]) -> Option<Barrier> {
        match self.pool {
            Pool::Local(_) => barrier(others, self.order),
            Pool::Remote(_) => None,
        }
    }

    /// The slots of a count of `count` the gate tries on this pass: every one, unless a critical request ahead holds
    /// it back ([`slot_range`]). With them, this pass's probe of those slots when it made one: a light gate behind a
    /// critical request probes the reserved slots as it counts the light gates beside it, so the pass need not again.
    fn range(&self, count: u32, survey: &mut Survey, console: &mut Console) -> (Range<u32>, Option<Probe>) {
        let (Pool::Local(kind), Some(barrier)) = (self.pool, self.barrier) else {
            return (0..count, None);
        };
        if kind == SlotKind::Heavy {
            return (slot_range(kind, count, 0), None);
        }
        let reserved = count - reserved_light(count)..count;
        let probe = self.probe(reserved.clone(), survey, console);
        let beside = self.beside(count, barrier.holding, slots_where(&probe, true), survey, console);
        let range = slot_range(kind, count, beside);
        let probe = (range == reserved).then_some(probe);
        (range, probe)
    }

    /// How many light gates run beside the critical request ahead, out of `count` light slots, as it is `holding` the
    /// machine or not, with `reserved_open` reserved slots some gate has open ([`slots::light_beside`]).
    fn beside(&self, count: u32, holding: bool, reserved_open: usize, survey: &mut Survey, console: &mut Console) -> usize {
        let claims: Vec<u32> = if holding {
            survey
                .light_claims(&self.root.path, console)
                .into_iter()
                .map(|(_, index)| index)
                .collect()
        } else {
            Vec::new()
        };
        slots::light_beside(count, holding, reserved_open, &claims)
    }

    /// Whether the gate may try the slots `range`: some are left it, and fewer live tickets of its kind of `others`
    /// are ahead of it than those slots are free, so a try never takes the slot an earlier waiter would get. `probe` is
    /// this pass's probe of `range` when it made one already, else the slots are probed now if the decision needs it.
    /// The pass's probe comes with it, `None` when it made none.
    fn may_try(
        &self,
        others: &[Ticket],
        range: Range<u32>,
        probe: Option<Probe>,
        survey: &mut Survey,
        console: &mut Console,
    ) -> (bool, Option<Probe>) {
        if range.is_empty() {
            return (false, probe);
        }
        let ahead = self.ahead(others, survey, console);
        if ahead == 0 {
            return (true, probe);
        }
        let probe = probe.unwrap_or_else(|| self.probe(range, survey, console));
        (ahead < slots_where(&probe, false), Some(probe))
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
    /// none, with the busy slots of that same probe; `None` when one is free.
    fn busy_line(&self, probe: Option<Probe>, range: Range<u32>, survey: &mut Survey, console: &mut Console) -> Option<String> {
        let probe = probe.unwrap_or_else(|| self.probe(range, survey, console));
        (slots_where(&probe, false) == 0).then(|| format!("gate: waiting for a {} ({} busy)", self.place, slots_where(&probe, true)))
    }

    /// The first line of a gate the critical request of the last pass holds back; `None` when none did.
    fn barrier_line(&self) -> Option<String> {
        let barrier = self.barrier?;
        let line = match self.pool {
            Pool::Local(SlotKind::Light) => format!("gate: waiting for a {}", beside_place(barrier.pid)),
            _ => format!("gate: waiting for a {} behind the critical request of gate {}", self.place, barrier.pid),
        };
        Some(line)
    }

    /// How many live tickets of the gate's kind and pool of `others`, local or remote, are ahead of it, so a local and
    /// a remote waiter never count each other. A ticket whose gate a live claim names is not counted: that gate holds
    /// a slot already, and only failed to delete its ticket.
    fn ahead(&self, others: &[Ticket], survey: &mut Survey, console: &mut Console) -> usize {
        let kind = self.pool.ticket_kind();
        let remote = self.pool.is_remote();
        let ahead: Vec<&Ticket> = others
            .iter()
            .filter(|ticket| ticket.kind == kind && (ticket.remote == Some(true)) == remote && ticket.queue_order() < self.order)
            .collect();
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
    /// back by a critical request names it.
    fn say_heartbeat(&mut self, console: &mut Console) {
        if let Some(waited) = self.waiter.heartbeat_due() {
            let place = match (self.pool, self.barrier) {
                (Pool::Local(SlotKind::Light), Some(barrier)) => beside_place(barrier.pid),
                _ => self.place.clone(),
            };
            console.err(&format!("gate: still waiting for a {place} ({} s)", waited.as_secs()));
        }
    }
}

/// What a light gate kept to the reserved light slots by the critical request of gate `pid` waits for.
fn beside_place(pid: u32) -> String {
    format!("light slot beside the critical request of gate {pid}")
}

/// Whether the gate under the light parent gate `parent` is lent that parent's slot before it queues, by the same rule
/// and reads as each pass of a wait ([`lends`]): the light count of the counts file under `root` and the other gates'
/// live tickets, then the critical mutex and the parent's claim only as the rule needs them.
pub(crate) fn lent_before_queue(root: &ResolvedRoot, parent: u32, processors: usize, survey: &mut Survey, console: &mut Console) -> bool {
    let light = read_counts(root, processors).light.count;
    let others = live_others(root, survey, console);
    let mut facts = Probed {
        root,
        parent,
        survey,
        console,
    };
    lends(&others, light, &mut facts)
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

/// What a lend turns on besides the tickets and the light count, each read only when the tickets leave it open.
trait LendFacts {
    /// Whether some gate has the critical mutex open.
    fn critical_open(&mut self) -> bool;
    /// The slot index of the parent gate's live light claim, `None` when it has none that can be read.
    fn parent_index(&mut self) -> Option<u32>;
}

/// The lend facts as the machine has them: the critical mutex under `root`, and the live light claim of the parent gate
/// `parent` among the other gates' claims.
struct Probed<'p> {
    root: &'p ResolvedRoot,
    parent: u32,
    survey: &'p mut Survey,
    console: &'p mut Console,
}

impl LendFacts for Probed<'_> {
    fn critical_open(&mut self) -> bool {
        slots::critical_open(self.root, self.survey, self.console)
    }

    fn parent_index(&mut self) -> Option<u32> {
        let parent = self.parent;
        self.survey
            .light_claims(&self.root.path, self.console)
            .into_iter()
            .find(|(pid, _)| *pid == parent)
            .map(|(_, index)| index)
    }
}

/// Whether a light parent lends its slot to the gate under it: while a critical request is pending, by a ticket of
/// the other gates' live tickets `others` or else the critical mutex open, and none of them holds the machine, unless
/// the parent's light claim is on a slot of a count of `light` the drain never takes. A parent claim that cannot be
/// read lends. The tickets are read first: `facts` probes the critical mutex only when no critical ticket is pending,
/// and reads the parent's claim only while a critical request is pending and none holds the machine.
fn lends(others: &[Ticket], light: u32, facts: &mut impl LendFacts) -> bool {
    !slots::critical_holding(others)
        && slots::critical_pending(others, || facts.critical_open())
        && !facts.parent_index().is_some_and(|index| slots::on_reserved_light(index, light))
}

/// The barrier of a gate at the place `order`: the earliest of the other gates' live critical tickets `others` ahead
/// of it, by arrival and then pid, and whether any of those holds the machine; `None` when none is ahead.
fn barrier(others: &[Ticket], order: (u64, u32)) -> Option<Barrier> {
    let ahead: Vec<&Ticket> = others
        .iter()
        .filter(|ticket| ticket.kind == TicketKind::Critical && ticket.queue_order() < order)
        .collect();
    let earliest = ahead.iter().min_by_key(|ticket| ticket.queue_order())?;
    Some(Barrier {
        pid: earliest.pid,
        holding: ahead.iter().any(|ticket| ticket.holds),
    })
}

/// The slots of a local pool of `kind` and `count` a gate behind a critical request tries: none for a heavy gate; for a
/// light gate the reserved slots, the highest `min(2, count)`, unless `beside` light gates, as many as are reserved,
/// already run beside it.
fn slot_range(kind: SlotKind, count: u32, beside: usize) -> Range<u32> {
    let reserved = reserved_light(count);
    let room = usize::try_from(reserved).is_ok_and(|reserved| beside < reserved);
    match kind {
        SlotKind::Light if room => count - reserved..count,
        SlotKind::Light | SlotKind::Heavy => count..count,
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

#[cfg(test)]
mod tests {
    use gate_state::SlotKind;
    use gate_state::v1::{Ticket, TicketKind};

    use super::{Barrier, LendFacts, barrier, lends, slot_range};
    use crate::slots;

    /// A ticket of the gate `pid`, started at `pid * 10` ms, of `kind`, arrived at `arrived`.
    fn ticket(kind: TicketKind, arrived: u64, pid: u32) -> Ticket {
        Ticket {
            kind,
            arrived,
            pid,
            start_ms: u64::from(pid) * 10,
            holds: false,
            remote: None,
            expected: None,
        }
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
            Some(Barrier { pid: 12, holding: true }),
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
            Some(Barrier { pid: 6, holding: false }),
            "a request arrived with it, of a lower pid, still waiting"
        );
        assert_eq!(barrier(&[], order), None);
    }

    #[test]
    fn behind_a_barrier_a_heavy_gate_tries_no_slot_and_a_light_one_only_the_reserved_slots_while_any_is_left() {
        assert_eq!(slot_range(SlotKind::Heavy, 3, 0), 3..3, "a heavy gate tries none");
        assert_eq!(slot_range(SlotKind::Light, 5, 0), 3..5, "the two highest light slots are reserved");
        assert_eq!(slot_range(SlotKind::Light, 5, 1), 3..5);
        assert_eq!(slot_range(SlotKind::Light, 5, 2), 5..5, "as many light gates beside it as are reserved");
        assert_eq!(slot_range(SlotKind::Light, 1, 0), 0..1, "a count of 1 reserves its one slot");
        assert_eq!(slot_range(SlotKind::Light, 1, 1), 1..1);
    }

    /// One row of the lend table: the other gates' tickets, whether the critical mutex is open, the parent's light slot,
    /// the light count, whether the parent lends, and why.
    type Row<'t> = (&'t [Ticket], bool, Option<u32>, u32, bool, &'t str);

    #[test]
    fn a_parent_lends_its_slot_only_while_a_critical_request_drains_and_its_slot_is_one_the_drain_takes() {
        let waiting = [ticket(TicketKind::Critical, 50, 9)];
        let mut holds = ticket(TicketKind::Critical, 50, 9);
        holds.holds = true;
        let holding = [holds];
        let heavy = [ticket(TicketKind::Heavy, 50, 10)];
        let table: [Row<'_>; 9] = [
            (&waiting, false, Some(0), 4, true, "a waiting request, the parent on an unreserved slot"),
            (&waiting, false, None, 4, true, "a parent claim that cannot be read"),
            (&heavy, true, Some(1), 4, true, "the critical mutex open with no critical ticket"),
            (&heavy, false, Some(0), 4, false, "no critical request"),
            (&[], false, None, 4, false, "no ticket at all"),
            (&holding, true, Some(0), 4, false, "a request that holds the machine"),
            (&waiting, false, Some(2), 4, false, "the parent on a reserved slot"),
            (&waiting, false, Some(4), 4, false, "the parent past the light count"),
            (&waiting, false, Some(0), 1, false, "a count of 1, whose one slot is reserved"),
        ];
        for (others, open, index, light, expected, why) in table {
            assert_eq!(lends(others, light, &mut Known { open, index }), expected, "{why}");
        }
    }

    /// Lend facts known up front: whether the critical mutex is open, and the parent's light slot.
    struct Known {
        open: bool,
        index: Option<u32>,
    }

    impl LendFacts for Known {
        fn critical_open(&mut self) -> bool {
            self.open
        }

        fn parent_index(&mut self) -> Option<u32> {
            self.index
        }
    }
}

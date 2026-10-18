//! A critical gate's wait for the machine and its hold of it: the passes that take the critical mutex in arrival order,
//! wait for the earlier heavy and light gates to drain, and for the light gates running to number no more than may run
//! beside it, then take every heavy slot and every free light slot but those left for light gates beside it, the lines
//! it says while it waits and once it holds the machine, the snapshot its `take` journals, and the release of all it
//! took. Its wake event, ticket, sleep and heartbeat are a slot waiter's ([`Waiter`]).

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use gate_state::v1::{self, Claim, ClaimKind, Ticket, TicketKind};
use gate_state::{HeavyLight, KernelObject, MachineSnapshot, OtherClaim, beside_allowance};
use gate_win32::{OwnedMutexGuard, mutex_exists};
use jiff::Timestamp;

use crate::acquire;
use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, BesideTry, ClaimOf, ClaimsFolder, Held, Survey, Taken};
use crate::wait::{self, Arrival, Host, Waiter, Which};

/// The kinds of earlier gate the waiting line counts and a drain with a light slot to take waits for.
const LOCAL_KINDS: [TicketKind; 2] = [TicketKind::Heavy, TicketKind::Light];

/// The pid and start, in epoch milliseconds, of the gates that hold a slot by a live claim.
type Holders = HashSet<(u32, u64)>;

/// One slot of a probe: its claim kind and index, and whether some gate has its mutex open, `None` when it could not be
/// probed.
type SlotProbe = (ClaimKind, u32, Option<bool>);

/// What a critical gate holds of the machine: the critical mutex, the heavy and light slots it took, and its ticket,
/// which it keeps until it ends.
pub(crate) struct Machine {
    critical: Option<Held>,
    slots: Vec<Held>,
    ticket: PathBuf,
    /// The mutexes taken from a gate that died holding them, in the order they were taken.
    abandoned: Vec<String>,
    /// Whether the gate was asked for `--alone`: it holds the machine alone, light slots included.
    alone: bool,
    /// The light gates it left room for beside it on the pass that took the machine ([`beside_allowance`]).
    beside: u32,
}

impl Machine {
    /// The names of the heavy and light slots held, in the order they were taken.
    pub(crate) fn names(&self) -> Vec<String> {
        self.slots.iter().map(|slot| slot.name().to_owned()).collect()
    }

    /// The names of the mutexes, slots and the critical mutex, taken from a gate that died holding them.
    pub(crate) fn abandoned(&self) -> &[String] {
        &self.abandoned
    }

    fn holds(&self, name: &str) -> bool {
        self.slots.iter().any(|slot| slot.name() == name)
    }

    /// How many of the slots held are light slots under `root`, by their names.
    fn light_held(&self, root: &ResolvedRoot) -> u32 {
        let light = self
            .slots
            .iter()
            .filter(|slot| slot.name() == root.root.object_name(KernelObject::LightSlot(slot.index())))
            .count();
        u32::try_from(light).unwrap_or(u32::MAX)
    }

    /// Keeps `taken`, saying and noting a takeover from a gate that died holding it.
    fn keep(&mut self, taken: Taken, console: &mut Console) -> Held {
        if taken.abandoned {
            let name = taken.held.name().to_owned();
            acquire::say_taken_abandoned(console, &name);
            self.abandoned.push(name);
        }
        taken.held
    }

    /// Writes the critical mutex's claim, `slots\gate-critical.json`, of kind critical and index 0; the slots get none.
    pub(crate) fn write_claim(&self, of: &ClaimOf<'_>, console: &mut Console) {
        if let Some(critical) = &self.critical {
            slots::write_claim(critical, ClaimKind::Critical, of, console);
        }
    }

    /// Lets the machine go: deletes the critical mutex's claim, releases every slot, then the critical mutex, deletes the
    /// ticket and wakes the waiters.
    pub(crate) fn release(self, root: &ResolvedRoot, survey: &mut Survey, console: &mut Console) {
        let Machine {
            critical,
            slots: held,
            ticket,
            ..
        } = self;
        if let Some(critical) = &critical {
            slots::delete_claim(critical, console);
        }
        drop(held);
        drop(critical);
        wait::delete_ticket(&ticket, console);
        slots::wake_waiters(root, survey, console);
    }
}

/// How a critical gate's wait ended.
pub(crate) enum MachineWaited {
    /// It holds the machine, and found it so.
    Held(Machine, MachineSnapshot),
    /// The console was interrupted; what it took so far is still held.
    Interrupted(Machine),
}

/// A critical gate queued for the machine.
pub(crate) struct MachineWait<'a> {
    root: &'a ResolvedRoot,
    machine: Machine,
    /// The gate's own ticket, rewritten as holding once it holds the machine.
    ticket: Ticket,
    waiter: Waiter,
    processors: usize,
    /// The counts the last pass read.
    counts: HeavyLight,
    /// The other gates' live tickets the last pass read.
    others: Vec<Ticket>,
    /// The gates that held a slot by a live claim as the last pass read them, whose tickets wait for nothing.
    holders: Holders,
    /// Whether no earlier heavy or light gate was left waiting, as last found.
    drained: bool,
    /// The counts file's ignored lines said already this wait.
    said: HashSet<String>,
    said_waiting: bool,
}

impl<'a> MachineWait<'a> {
    /// Queues the arriving critical gate with a ticket of kind critical ([`Waiter::queue`]). A raised interrupt of
    /// `host` ends the wait.
    pub(crate) fn queue(root: &'a ResolvedRoot, arrival: Arrival<'_>, host: Host, console: &mut Console) -> MachineWait<'a> {
        let ticket = Ticket {
            kind: TicketKind::Critical,
            arrived: arrival.arrived,
            pid: arrival.gate.pid,
            start_ms: arrival.gate.start_ms,
            holds: false,
            lends_closed: false,
            alone: arrival.request.alone,
            remote: None,
            expected: arrival.expected.cloned(),
        };
        let waiter = Waiter::queue(root, arrival, &ticket, host, console);
        MachineWait {
            root,
            machine: Machine {
                critical: None,
                slots: Vec::new(),
                ticket: waiter.ticket().to_path_buf(),
                abandoned: Vec::new(),
                alone: arrival.request.alone,
                beside: 0,
            },
            ticket,
            waiter,
            processors: host.processors,
            counts: HeavyLight { heavy: 0, light: 0 },
            others: Vec::new(),
            holders: Holders::new(),
            drained: false,
            said: HashSet::new(),
            said_waiting: false,
        }
    }

    /// Passes until the gate holds the machine, its ticket rewritten as holding ([`MachineWait::hold`]), then reads what
    /// it found; each pass that falls short says the waiting line the first time and the still-waiting line when it is
    /// due, and sleeps on the wake event until the poll or the next still-waiting line. Interrupted once the console
    /// is, with what it took so far.
    pub(crate) fn until_held(mut self, survey: &mut Survey, console: &mut Console) -> MachineWaited {
        loop {
            if self.waiter.interrupted() {
                return MachineWaited::Interrupted(self.machine);
            }
            if self.pass(survey, console) {
                let snapshot = self.snapshot(survey, console);
                return MachineWaited::Held(self.machine, snapshot);
            }
            if self.said_waiting {
                self.say_heartbeat(survey, console);
            } else {
                self.say_waiting(survey, console);
            }
            if !self.waiter.interrupted() {
                self.waiter.sleep(None, console);
            }
        }
    }

    /// One pass; whether the gate now holds the machine. It takes the critical mutex once no critical request that
    /// arrived before it is live, so critical requests run in arrival order; then, until it holds its first slot,
    /// waits until no earlier heavy gate, nor light one while it has a light slot to take, still waits, a remote one or
    /// one whose gate already holds a slot left out; then takes every heavy slot of the counts as they stand and, on
    /// every pass, its light slots ([`MachineWait::take_light`]); once it holds them all, holds the machine when no gate
    /// runs lent its light parent's slot ([`MachineWait::hold`]). A pass that deleted a dead gate's ticket wakes the
    /// other waiters.
    fn pass(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        self.counts = self.read_counts(console);
        let (others, deleted) = survey.others(&self.root.path, console);
        if deleted {
            slots::signal(self.root, &others);
        }
        self.holders = if others.is_empty() {
            Holders::new()
        } else {
            survey.holders(&self.root.path, console)
        };
        self.others = others;
        if self.machine.critical.is_none() && !self.take_critical(survey, console) {
            return false;
        }
        if !self.drained || self.machine.slots.is_empty() {
            let order = self.ticket.queue_order();
            let target = light_target(self.counts.light, self.allowance());
            self.drained = drain_waits_for(&self.others, &self.holders, order, target) == 0;
            if !self.drained {
                return false;
            }
        }
        if !(self.take_slots(survey, console) && self.hold(survey, console)) {
            return false;
        }
        self.machine.beside = self.allowance();
        true
    }

    /// How many light gates may run beside this gate on this pass, by the light count and the live tickets the pass
    /// read, its own included ([`beside_allowance`]): none while a request that holds the machine alone waits or holds.
    fn allowance(&self) -> u32 {
        beside_allowance(self.counts.light, self.others.iter().chain([&self.ticket]))
    }

    /// Holds the machine once it holds every slot it takes: holding the beside mutex, counts the gates that run lent
    /// their light parent's slot by their lend markers, heavy holders it waits for outside the allowance, and with none
    /// rewrites its ticket as holding before it lets the mutex go, so a gate lent a slot after the count sees the ticket
    /// and is not lent ([`slots::try_beside`]). While some run, it rewrites its ticket once as closing its lends
    /// (`lendsClosed`), so no light parent lends its slot again and the drain waits only for those already lent.
    /// Whether it holds the machine; not while the mutex cannot be taken, a lent gate runs, the claims folder cannot be
    /// listed, said once, or the ticket cannot be rewritten as holding ([`MachineWait::rewrite`]).
    fn hold(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        let Some(_beside) = self.try_beside(survey, console) else {
            return false;
        };
        match survey.lent_children(&self.root.path, console) {
            Ok(0) => self.rewrite(true, false, survey, console),
            Ok(_) => {
                if !self.ticket.lends_closed {
                    self.rewrite(false, true, survey, console);
                }
                false
            }
            Err(_) => false,
        }
    }

    /// Rewrites the gate's ticket as holding the machine or not (`holds`) and as closing its lends or not
    /// (`lends_closed`), keeping it as written; whether it was written. A ticket that cannot be written is said once a
    /// wait and keeps what it said before, so the next pass tries again.
    fn rewrite(&mut self, holds: bool, lends_closed: bool, survey: &mut Survey, console: &mut Console) -> bool {
        let ticket = Ticket {
            holds,
            lends_closed,
            ..self.ticket.clone()
        };
        if let Err(error) = self.waiter.rewrite(&ticket) {
            survey.say_once("cannot write the ticket", &self.waiter.ticket().display().to_string(), error, console);
            return false;
        }
        self.ticket = ticket;
        true
    }

    /// The beside mutex, tried once ([`slots::try_beside`]); `None` when another gate holds it, which this gate looks
    /// again for soon, or when it cannot be taken, said once.
    fn try_beside(&mut self, survey: &mut Survey, console: &mut Console) -> Option<OwnedMutexGuard> {
        let tried = slots::try_beside(self.root, survey, console);
        if matches!(tried, BesideTry::Busy) {
            self.waiter.retry_soon();
        }
        tried.held()
    }

    /// Takes the critical mutex unless a live critical request arrived first; whether it holds it.
    fn take_critical(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        let order = self.ticket.queue_order();
        if self.others.iter().any(|ticket| is_earlier(ticket, order, &[TicketKind::Critical])) {
            return false;
        }
        let Some(taken) = slots::try_object(self.root, KernelObject::Critical, 0, survey, console) else {
            return false;
        };
        let held = self.machine.keep(taken, console);
        self.machine.critical = Some(held);
        true
    }

    /// Takes every heavy slot of the counts it does not hold yet, then its light slots ([`MachineWait::take_light`]);
    /// whether it holds them all.
    fn take_slots(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        let mut all = true;
        for index in 0..self.counts.heavy {
            let object = KernelObject::HeavySlot(index);
            all &= self.machine.holds(&self.root.root.object_name(object)) || self.take(object, index, survey, console);
        }
        self.take_light(survey, console) && all
    }

    /// Takes the slot `index` of `object`; whether it took it. A slot taken from a gate that died holding it has that
    /// gate's claim deleted, since the critical gate writes none to replace it, so the next gate to take the slot does
    /// not count the takeover again.
    fn take(&mut self, object: KernelObject, index: u32, survey: &mut Survey, console: &mut Console) -> bool {
        let Some(taken) = slots::try_object(self.root, object, index, survey, console) else {
            return false;
        };
        if taken.abandoned {
            slots::delete_claim(&taken.held, console);
        }
        let held = self.machine.keep(taken, console);
        self.machine.slots.push(held);
        true
    }

    /// Takes free light slots, lowest first, until it holds all but the allowance of the light count
    /// ([`light_target`]), on a pass where no more light gates than the allowance run beside it, whichever slots they
    /// hold ([`MachineWait::running_light`]); whether it holds that many. It counts and takes holding the beside mutex
    /// ([`slots::try_beside`]), as every local light gate counts, takes and claims, so a light gate between its take
    /// and its claim is counted, and the slots left free are the beside slots of this drain. The mutex held by another
    /// gate, or the claims folder that cannot be listed, takes nothing this pass. A pass that took a slot wakes the
    /// waiters once it lets the mutex go.
    fn take_light(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        let allowance = self.allowance();
        let target = light_target(self.counts.light, allowance);
        let mut held = self.machine.light_held(self.root);
        if held >= target {
            return true;
        }
        let Some(beside) = self.try_beside(survey, console) else {
            return false;
        };
        let allowance = usize::try_from(allowance).unwrap_or(usize::MAX);
        if !self.running_light(survey, console).is_ok_and(|running| running <= allowance) {
            return false;
        }
        let before = held;
        for index in 0..self.counts.light {
            if held >= target {
                break;
            }
            let object = KernelObject::LightSlot(index);
            if !self.machine.holds(&self.root.root.object_name(object)) && self.take(object, index, survey, console) {
                held += 1;
            }
        }
        drop(beside);
        if held > before {
            slots::wake_waiters(self.root, survey, console);
        }
        held >= target
    }

    /// How many other light gates run: the light slots of the count some gate has open that this gate does not hold, so
    /// a gate between its take and its claim counts, and the other gates' live light claims past the count, where a
    /// count lowered since they took their slots left them ([`running_light`]); else why the claims folder cannot be
    /// listed, said once.
    fn running_light(&self, survey: &mut Survey, console: &mut Console) -> io::Result<usize> {
        let probe = self.probe_light(survey, console);
        let claims = survey.live_claims(&self.root.path, console)?;
        Ok(running_light(&probe, past_light(&claims, self.counts.light).len()))
    }

    /// The heavy and light counts from the counts file as it stands, each ignored line said once a wait.
    fn read_counts(&mut self, console: &mut Console) -> HeavyLight {
        let counts = v1::read_counts(&v1::counts_path(&self.root.path), self.processors);
        for line in [&counts.heavy.ignored, &counts.light.ignored].into_iter().flatten() {
            if self.said.insert(line.clone()) {
                console.err(line);
            }
        }
        HeavyLight {
            heavy: counts.heavy.count,
            light: counts.light.count,
        }
    }

    /// Says the gate waits for the machine, and why, then raises the test interrupt when asked to.
    fn say_waiting(&mut self, survey: &mut Survey, console: &mut Console) {
        self.said_waiting = true;
        let status = self.status(survey, console);
        console.err(&format!("gate: waiting for {}: {status}", asked(self.machine.alone)));
        self.waiter.said_waiting();
    }

    /// Says the still-waiting line when it is due, with the whole seconds since the gate queued.
    fn say_heartbeat(&mut self, survey: &mut Survey, console: &mut Console) {
        if let Some(waited) = self.waiter.heartbeat_due() {
            let status = self.status(survey, console);
            console.err(&format!(
                "gate: still waiting for {}: {status} ({} s)",
                asked(self.machine.alone),
                waited.as_secs()
            ));
        }
    }

    /// What the gate waits for, as its waiting lines say it, from the tickets and claims the last pass read, one probe
    /// of the slots and one listing of the claims folder ([`Survey::claims_folder`]), each live gate lent its light
    /// parent's slot counted as a heavy holder (none when the claims folder cannot be listed, said once); then, when no
    /// gate is ahead of it ([`eta_blockers`]), when the last holder it waits for ends ([`MachineWait::last_eta`]).
    fn status(&self, survey: &mut Survey, console: &mut Console) -> String {
        let order = self.ticket.queue_order();
        let earlier = earlier_waiters(&self.others, &self.holders, order);
        let probe = self.probe_slots(survey, console);
        let folder = survey
            .claims_folder(&self.root.path, console)
            .inspect_err(|error| {
                survey.say_once(
                    "cannot read the claims in",
                    &v1::slots_dir(&self.root.path).display().to_string(),
                    error,
                    console,
                );
            })
            .ok();
        let mut held = held_of(&probe);
        if let Some(folder) = &folder {
            held.heavy = held.heavy.saturating_add(u32::try_from(folder.lent.len()).unwrap_or(u32::MAX));
        }
        let status = status_text(held, self.allowance(), earlier);
        if eta_blockers(&self.others, &self.holders, order) > 0 {
            return status;
        }
        match folder.and_then(|folder| self.last_eta(&probe, &folder)) {
            Some(eta) => format!("{status}, {eta}"),
            None => status,
        }
    }

    /// When the last holder the gate waits for ends, by `probe` and the claims folder as `folder` lists it: the holders
    /// of the slots busy by the probe and of the light ones past the count by their live claims, as
    /// [`MachineWait::running_light`] counts them, those the drain waits for ([`drained_by`]), and each gate lent its
    /// light parent's slot by its lend marker's `expected` ([`wait::holder_usuals`]). `None` when none is waited for,
    /// or one of them has no usual, a lend marker that cannot be read included.
    fn last_eta(&self, probe: &[SlotProbe], folder: &ClaimsFolder) -> Option<String> {
        let claims: Vec<Claim> = folder.claims.iter().map(|(_, claim)| claim.clone()).collect();
        let busy = waited_for(probe, past_light(&claims, self.counts.light))?;
        let now = Timestamp::now();
        let allowance = usize::try_from(self.allowance()).unwrap_or(usize::MAX);
        let busy = drained_by(&claims, busy, allowance, now)?;
        let mut holders = wait::holder_usuals(&claims, &busy, now)?;
        for marker in &folder.lent {
            let lent = v1::read_claim(marker).ok()?;
            holders.push(wait::usual(&lent, now)?);
        }
        wait::named_eta(holders, Which::Last)
    }

    /// One probe of the heavy and light slots of the counts, its own left out: each slot's claim kind and index, and
    /// whether some gate has its mutex open, `None` for one that cannot be probed, said once.
    fn probe_slots(&self, survey: &mut Survey, console: &mut Console) -> Vec<SlotProbe> {
        let heavy = (0..self.counts.heavy).map(|index| (KernelObject::HeavySlot(index), ClaimKind::Heavy, index));
        let mut probe = self.probe(heavy, survey, console);
        probe.extend(self.probe_light(survey, console));
        probe
    }

    /// One probe of the light slots of the count, as [`MachineWait::probe_slots`] probes them.
    fn probe_light(&self, survey: &mut Survey, console: &mut Console) -> Vec<SlotProbe> {
        let light = (0..self.counts.light).map(|index| (KernelObject::LightSlot(index), ClaimKind::Light, index));
        self.probe(light, survey, console)
    }

    /// One probe of `slots`, each an object with its claim kind and index, its own left out, as
    /// [`MachineWait::probe_slots`] probes them.
    fn probe(&self, slots: impl Iterator<Item = (KernelObject, ClaimKind, u32)>, survey: &mut Survey, console: &mut Console) -> Vec<SlotProbe> {
        slots
            .filter_map(|(object, kind, index)| {
                let name = self.root.root.object_name(object);
                if self.machine.holds(&name) {
                    return None;
                }
                let open = mutex_exists(&name)
                    .inspect_err(|error| survey.say_once("cannot probe", &name, error, console))
                    .ok();
                Some((kind, index, open))
            })
            .collect()
    }

    /// What the gate found as it took the machine: the counts of the pass that took it, the slots other gates hold,
    /// and the other gates' live claims, the remote pool's left out, or why the claims folder cannot be listed.
    fn snapshot(&self, survey: &mut Survey, console: &mut Console) -> MachineSnapshot {
        let folder = v1::slots_dir(&self.root.path);
        let other_claims = survey
            .other_claims(&self.root.path, console)
            .map(|claims| {
                claims
                    .into_iter()
                    .filter(|(_, claim)| claim.kind != ClaimKind::Remote)
                    .map(|(file, claim)| OtherClaim {
                        pid: claim.pid,
                        kind: claim.kind.as_str().to_owned(),
                        index: claim.index,
                        command: claim.command,
                        file,
                    })
                    .collect()
            })
            .map_err(|error| format!("cannot read the claims in {}: {error}", folder.display()));
        MachineSnapshot {
            counts: self.counts,
            other_held: held_of(&self.probe_slots(survey, console)),
            other_claims,
        }
    }
}

/// The live light claims of `claims` on slots past a light count of `light`, where a count lowered since they took
/// their slots left them, each as a claim kind and slot index.
fn past_light(claims: &[Claim], light: u32) -> Vec<(ClaimKind, u32)> {
    claims
        .iter()
        .filter(|claim| claim.kind == ClaimKind::Light && claim.index >= light)
        .map(|claim| (ClaimKind::Light, claim.index))
        .collect()
}

/// Whether `ticket` is a local waiter of one of `kinds` ahead of the place `order`; a remote one never is.
fn is_earlier(ticket: &Ticket, order: (u64, u32), kinds: &[TicketKind]) -> bool {
    kinds.contains(&ticket.kind) && ticket.remote != Some(true) && ticket.queue_order() < order
}

/// Whether `ticket`'s gate still waits: no live claim of `holders` names it. One that holds a slot only failed to
/// delete its ticket.
fn still_waits(ticket: &Ticket, holders: &Holders) -> bool {
    !holders.contains(&(ticket.pid, ticket.start_ms))
}

/// How many of the other gates' tickets `others` the drain of a critical gate at `order` waits for: the earlier local
/// heavy waiters, and the light ones while it has `target` light slots to take ([`light_target`]), a gate of `holders`
/// left out.
fn drain_waits_for(others: &[Ticket], holders: &Holders, order: (u64, u32), target: u32) -> usize {
    let kinds: &[TicketKind] = if target > 0 { &LOCAL_KINDS } else { &[TicketKind::Heavy] };
    others
        .iter()
        .filter(|ticket| is_earlier(ticket, order, kinds) && still_waits(ticket, holders))
        .count()
}

/// How many of the other gates' tickets `others` are earlier local heavy or light waiters than `order`, a critical gate
/// holding the machine and a gate of `holders` left out.
fn earlier_waiters(others: &[Ticket], holders: &Holders, order: (u64, u32)) -> usize {
    others
        .iter()
        .filter(|ticket| !ticket.holds && is_earlier(ticket, order, &LOCAL_KINDS) && still_waits(ticket, holders))
        .count()
}

/// How many light slots a critical gate holds for a light count of `light`: every one but the `allowance` of light
/// gates beside it ([`beside_allowance`]), whichever slots those hold.
fn light_target(light: u32, allowance: u32) -> u32 {
    light.saturating_sub(allowance)
}

/// How many other light gates run by `probe`, a probe of the slots this gate does not hold: its light slots some gate
/// has open, a slot that cannot be probed counted as running so a drain never overtakes one, and `past` light gates on
/// slots past the count.
fn running_light(probe: &[SlotProbe], past: usize) -> usize {
    probe
        .iter()
        .filter(|(kind, _, open)| *kind == ClaimKind::Light && *open != Some(false))
        .count()
        + past
}

/// What a critical gate waits for and holds, as its lines say it: the machine, or the machine alone for a gate asked
/// for `--alone`.
fn asked(alone: bool) -> &'static str {
    if alone {
        "the machine alone (critical)"
    } else {
        "the machine (critical)"
    }
}

/// The `allowance` of light gates that may run beside a critical gate ([`beside_allowance`]), as its lines say it.
fn beside_phrase(allowance: u32) -> String {
    let noun = if allowance == 1 { "light slot" } else { "light slots" };
    format!("{allowance} {noun} beside it")
}

/// The waiting lines' status: the slots other gates hold, the `allowance` of light slots beside it, and the earlier
/// waiters.
fn status_text(held: HeavyLight, allowance: u32, earlier: usize) -> String {
    format!(
        "{} heavy and {} light slots held ({}), {earlier} earlier waiters",
        held.heavy,
        held.light,
        beside_phrase(allowance)
    )
}

/// How many of the other gates' tickets `others` keep a critical gate at `order` from saying when the slots it waits for
/// free: the earlier heavy and light waiters ([`earlier_waiters`]), which take the freed slots first, and the earlier
/// critical requests, waiting or holding the machine, each of which runs its whole command first.
fn eta_blockers(others: &[Ticket], holders: &Holders, order: (u64, u32)) -> usize {
    let critical = others.iter().filter(|ticket| is_earlier(ticket, order, &[TicketKind::Critical])).count();
    earlier_waiters(others, holders, order) + critical
}

/// The slots other gates hold by `probe`, by kind; a slot that could not be probed counts as not held.
fn held_of(probe: &[SlotProbe]) -> HeavyLight {
    let held = |kind: ClaimKind| {
        let count = probe.iter().filter(|(of, _, open)| *of == kind && *open == Some(true)).count();
        u32::try_from(count).unwrap_or(u32::MAX)
    };
    HeavyLight {
        heavy: held(ClaimKind::Heavy),
        light: held(ClaimKind::Light),
    }
}

/// The slots of `probe` some gate holds, then the light slots `past` the count that live claims hold, each as a claim
/// kind and slot index; `None` when a slot of the probe could not be probed.
fn waited_for(probe: &[SlotProbe], past: Vec<(ClaimKind, u32)>) -> Option<Vec<(ClaimKind, u32)>> {
    let mut busy = Vec::new();
    for &(kind, index, open) in probe {
        match open {
            Some(true) => busy.push((kind, index)),
            Some(false) => {}
            None => return None,
        }
    }
    busy.extend(past);
    Some(busy)
}

/// Of the busy slots `busy`, those whose holders a critical gate's drain waits for: every heavy one, and while more
/// light slots are busy than the `allowance` of light gates beside it, the light ones whose holders' usual ends come
/// soonest, all but `allowance` of them, by their live claims `claims` at `now`. `None` when such a light holder has no
/// live claim carrying a usual.
fn drained_by(claims: &[Claim], busy: Vec<(ClaimKind, u32)>, allowance: usize, now: Timestamp) -> Option<Vec<(ClaimKind, u32)>> {
    let (light, mut waited): (Vec<_>, Vec<_>) = busy.into_iter().partition(|(kind, _)| *kind == ClaimKind::Light);
    let over = light.len().saturating_sub(allowance);
    if over == 0 {
        return Some(waited);
    }
    let mut left = Vec::with_capacity(light.len());
    for slot in light {
        let claim = claims.iter().find(|claim| (claim.kind, claim.index) == slot)?;
        left.push((wait::usual(claim, now)?.left, slot));
    }
    left.sort_by(|one, other| one.0.total_cmp(&other.0));
    waited.extend(left.into_iter().take(over).map(|(_, slot)| slot));
    Some(waited)
}

/// The line a critical gate says as it takes `machine` `at`, after waiting `waited` seconds, with what it found: whether
/// it holds the machine alone, and the light gates it left room for beside it.
pub(crate) fn holds_line(at: Timestamp, waited: f64, snapshot: &MachineSnapshot, machine: &Machine) -> String {
    let claims = match &snapshot.other_claims {
        Ok(claims) if claims.is_empty() => "none".to_owned(),
        Ok(claims) => claims
            .iter()
            .map(|claim| format!("{} {} {} {}", claim.pid, claim.kind, claim.index, claim.command))
            .collect::<Vec<_>>()
            .join(", "),
        Err(reason) => format!("unknown ({reason})"),
    };
    let time = jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%S.%7fZ", at).unwrap_or_else(|_| at.to_string());
    let MachineSnapshot { counts, other_held, .. } = snapshot;
    format!(
        "gate: holds {} at {time} after {:.1} s: counts heavy {}, light {}; other gates hold {} heavy and {} light slots ({}); \
         other live claims: {claims}",
        asked(machine.alone),
        (waited * 10.0).round_ties_even() / 10.0,
        counts.heavy,
        counts.light,
        other_held.heavy,
        other_held.light,
        beside_phrase(machine.beside)
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fixed time cannot be read must fail loudly")]
mod tests {
    use std::path::PathBuf;

    use gate_state::v1::{Ticket, TicketKind};
    use gate_state::{HeavyLight, MachineSnapshot, OtherClaim, beside_allowance};

    use gate_state::v1::ClaimKind;
    use jiff::{SignedDuration, Timestamp};

    use super::{
        Holders, Machine, drain_waits_for, drained_by, earlier_waiters, eta_blockers, held_of, holds_line, is_earlier, light_target, running_light,
        status_text, waited_for,
    };
    use crate::slots;
    use crate::wait::scratch::holding;
    use crate::wait::{Which, holder_eta};

    #[test]
    fn the_critical_eta_names_the_last_holder() {
        let now: Timestamp = "2026-10-13T12:00:00Z".parse().expect("a fixed time");
        let long_ago = now.checked_sub(SignedDuration::from_secs(1000)).expect("a moment after the epoch");
        let claims = [
            holding(ClaimKind::Heavy, 0, now, Some(60.0)),
            holding(ClaimKind::Light, 1, now, Some(600.0)),
        ];
        let busy = [(ClaimKind::Heavy, 0), (ClaimKind::Light, 1)];

        assert_eq!(
            holder_eta(&claims, &busy, Which::Last, now).as_deref(),
            Some("the last frees in about 10 min"),
            "the latest end of the slots it waits for"
        );
        let past = [holding(ClaimKind::Heavy, 0, long_ago, Some(240.0))];
        assert_eq!(
            holder_eta(&past, &[(ClaimKind::Heavy, 0)], Which::Last, now).as_deref(),
            Some("the last is 13 min past its usual 4 min, ceiling 15 min")
        );
    }

    #[test]
    fn no_critical_eta_while_an_earlier_waiter_or_critical_request_is_ahead() {
        let own = ticket(TicketKind::Critical, 100, 7);
        let order = own.queue_order();
        let none = Holders::new();

        assert_eq!(
            eta_blockers(&[ticket(TicketKind::Heavy, 50, 8)], &none, order),
            1,
            "an earlier heavy waiter"
        );
        assert_eq!(
            eta_blockers(&[ticket(TicketKind::Critical, 50, 9)], &none, order),
            1,
            "an earlier critical request runs its whole command first"
        );
        let mut holding_machine = ticket(TicketKind::Critical, 40, 10);
        holding_machine.holds = true;
        assert_eq!(eta_blockers(&[holding_machine], &none, order), 1, "one that holds the machine");
        let later = [ticket(TicketKind::Critical, 200, 11), ticket(TicketKind::Heavy, 200, 12)];
        assert_eq!(eta_blockers(&later, &none, order), 0, "gates that came after it");
    }

    #[test]
    fn one_probe_gives_the_held_counts_and_the_slots_a_critical_gate_waits_for() {
        let probe = [
            (ClaimKind::Heavy, 0, Some(true)),
            (ClaimKind::Heavy, 1, Some(false)),
            (ClaimKind::Light, 0, Some(true)),
            (ClaimKind::Light, 1, Some(true)),
            (ClaimKind::Light, 2, None),
        ];

        assert_eq!(held_of(&probe), HeavyLight { heavy: 1, light: 2 }, "an unprobed slot is not held");
        assert_eq!(
            waited_for(&probe[..4], Vec::new()),
            Some(vec![(ClaimKind::Heavy, 0), (ClaimKind::Light, 0), (ClaimKind::Light, 1)]),
            "every busy slot, whatever its index"
        );
        assert_eq!(waited_for(&probe, Vec::new()), None, "a slot could not be probed");
        assert_eq!(running_light(&probe, 0), 3, "an unprobed light slot counts as running");
        assert_eq!(running_light(&probe[..2], 1), 1, "a light claim past the count");
    }

    #[test]
    fn the_drain_waits_for_the_light_holders_that_end_soonest_beyond_the_allowance() {
        let now: Timestamp = "2026-10-13T12:00:00Z".parse().expect("a fixed time");
        let ago = now.checked_sub(SignedDuration::from_secs(100)).expect("a moment after the epoch");
        let claims = [
            holding(ClaimKind::Light, 0, ago, Some(600.0)),
            holding(ClaimKind::Light, 1, now, Some(60.0)),
            holding(ClaimKind::Light, 2, now, Some(300.0)),
            holding(ClaimKind::Heavy, 0, now, Some(900.0)),
        ];
        let heavy = (ClaimKind::Heavy, 0);
        let lights = [(ClaimKind::Light, 0), (ClaimKind::Light, 1), (ClaimKind::Light, 2)];
        let busy = |light: &[(ClaimKind, u32)]| [&[heavy], light].concat();

        assert_eq!(
            drained_by(&claims, busy(&lights), 2, now),
            Some(vec![heavy, (ClaimKind::Light, 1)]),
            "three light gates, two allowed beside it: the one that ends soonest"
        );
        assert_eq!(
            drained_by(&claims, busy(&lights[..2]), 2, now),
            Some(vec![heavy]),
            "two light gates run beside it"
        );
        assert_eq!(
            drained_by(&claims, busy(&lights), 1, now),
            Some(vec![heavy, (ClaimKind::Light, 1), (ClaimKind::Light, 2)])
        );
        assert_eq!(drained_by(&claims[1..], busy(&lights), 2, now), None, "a light holder with no claim");
    }

    #[test]
    fn a_light_gate_past_a_lowered_count_is_named_in_the_critical_wait_line() {
        let now: Timestamp = "2026-10-13T12:00:00Z".parse().expect("a fixed time");
        let claims = [
            holding(ClaimKind::Light, 0, now, Some(600.0)),
            holding(ClaimKind::Light, 1, now, Some(900.0)),
            holding(ClaimKind::Light, 4, now, Some(60.0)),
        ];
        let probe = [
            (ClaimKind::Light, 0, Some(true)),
            (ClaimKind::Light, 1, Some(true)),
            (ClaimKind::Light, 2, Some(false)),
        ];

        let busy = waited_for(&probe, vec![(ClaimKind::Light, 4)]).expect("every slot was probed");
        assert_eq!(busy, [(ClaimKind::Light, 0), (ClaimKind::Light, 1), (ClaimKind::Light, 4)]);
        let waited = drained_by(&claims, busy, 2, now).expect("every light holder has a usual");
        assert_eq!(waited, [(ClaimKind::Light, 4)], "three run, two may: the one past the count ends soonest");
        assert_eq!(
            holder_eta(&claims, &waited, Which::Last, now).as_deref(),
            Some("the last frees in about 60 s")
        );
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
            expected: None,
        }
    }

    /// The light slots a critical gate takes for a light count of `light` with no request to hold the machine alone.
    fn target(light: u32) -> u32 {
        light_target(light, beside_allowance(light, []))
    }

    #[test]
    fn the_critical_status_says_one_light_slot_or_n_light_slots() {
        let held = HeavyLight { heavy: 2, light: 1 };
        let status = |light: u32, earlier: usize| status_text(held, beside_allowance(light, []), earlier);
        assert_eq!(status(1, 3), "2 heavy and 1 light slots held (1 light slot beside it), 3 earlier waiters");
        assert_eq!(
            status(2, 0),
            "2 heavy and 1 light slots held (2 light slots beside it), 0 earlier waiters"
        );
        assert_eq!(
            status(8, 0),
            "2 heavy and 1 light slots held (2 light slots beside it), 0 earlier waiters"
        );
    }

    #[test]
    fn a_critical_gate_holds_every_light_slot_but_the_allowance_beside_it() {
        assert_eq!(target(1), 0, "a count of 1 allows its one light gate beside it");
        assert_eq!(target(2), 0);
        assert_eq!(target(3), 1);
        assert_eq!(target(5), 3);
    }

    #[test]
    fn the_critical_drain_waits_for_earlier_local_waiters_and_never_for_its_own_ticket() {
        let own = ticket(TicketKind::Critical, 100, 7);
        let mut remote = ticket(TicketKind::Heavy, 40, 10);
        remote.remote = Some(true);
        let mut holding = ticket(TicketKind::Critical, 30, 13);
        holding.holds = true;
        let tickets = vec![
            own.clone(),
            ticket(TicketKind::Heavy, 50, 8),
            ticket(TicketKind::Light, 60, 9),
            remote,
            ticket(TicketKind::Heavy, 200, 11),
            ticket(TicketKind::Heavy, 100, 12),
            holding,
        ];
        let order = own.queue_order();
        let none = Holders::new();

        let others = slots::others(tickets, (own.pid, own.start_ms));

        assert_eq!(others.len(), 6, "every ticket but its own");
        assert!(others.iter().all(|ticket| ticket.pid != own.pid), "{others:?}");
        assert_eq!(
            drain_waits_for(&others, &none, order, target(3)),
            2,
            "the earlier heavy and light waiters"
        );
        assert_eq!(
            drain_waits_for(&others, &none, order, target(2)),
            1,
            "no light slot to take beyond the two beside it: the heavy waiter alone"
        );
        assert_eq!(earlier_waiters(&others, &none, order), 2);
        let critical: Vec<u32> = others
            .iter()
            .filter(|ticket| is_earlier(ticket, order, &[TicketKind::Critical]))
            .map(|ticket| ticket.pid)
            .collect();
        assert_eq!(critical, [13], "an earlier critical request, and never its own");
        let mut tie = ticket(TicketKind::Heavy, 100, 6);
        assert!(is_earlier(&tie, order, &[TicketKind::Heavy]), "a tie goes to the lower pid");
        tie.pid = 8;
        assert!(!is_earlier(&tie, order, &[TicketKind::Heavy]));
    }

    #[test]
    fn a_critical_drain_does_not_wait_for_a_gate_that_holds_a_slot_and_kept_its_ticket() {
        let own = ticket(TicketKind::Critical, 100, 7);
        let holder = ticket(TicketKind::Heavy, 50, 8);
        let waiter = ticket(TicketKind::Light, 60, 9);
        let others = vec![holder.clone(), waiter];
        let holders: Holders = [(holder.pid, holder.start_ms)].into_iter().collect();

        assert_eq!(
            drain_waits_for(&others, &holders, own.queue_order(), target(3)),
            1,
            "the light waiter alone"
        );
        assert_eq!(
            drain_waits_for(&others, &holders, own.queue_order(), target(1)),
            0,
            "the heavy holder is no waiter"
        );
        assert_eq!(earlier_waiters(&others, &holders, own.queue_order()), 1);
    }

    #[test]
    fn the_critical_holds_line_lists_the_other_claims_or_none_or_why_not() {
        let at = "2026-10-10T12:00:00.1234567Z".parse().expect("a fixed time");
        let snapshot = |other_claims| MachineSnapshot {
            counts: HeavyLight { heavy: 4, light: 1 },
            other_held: HeavyLight { heavy: 0, light: 1 },
            other_claims,
        };
        let head = "gate: holds the machine (critical) at 2026-10-10T12:00:00.1234567Z after 2.0 s: counts heavy 4, light 1; \
                    other gates hold 0 heavy and 1 light slots (1 light slot beside it); other live claims: ";
        let claim = |pid, command: &str| OtherClaim {
            pid,
            kind: "light".to_owned(),
            index: 0,
            command: command.to_owned(),
            file: "gate-light-slot-0.json".to_owned(),
        };

        let machine = Machine {
            critical: None,
            slots: Vec::new(),
            ticket: PathBuf::new(),
            abandoned: Vec::new(),
            alone: false,
            beside: 1,
        };

        assert_eq!(holds_line(at, 1.96, &snapshot(Ok(Vec::new())), &machine), format!("{head}none"));
        let two = Ok(vec![claim(5, "cargo test"), claim(6, "cargo fmt")]);
        assert_eq!(
            holds_line(at, 2.04, &snapshot(two), &machine),
            format!("{head}5 light 0 cargo test, 6 light 0 cargo fmt")
        );
        let failed = Err("cannot read the claims in C:\\root\\slots: denied".to_owned());
        assert_eq!(
            holds_line(at, 2.0, &snapshot(failed), &machine),
            format!("{head}unknown (cannot read the claims in C:\\root\\slots: denied)")
        );
    }
}

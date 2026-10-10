//! A critical gate's wait for the machine and its hold of it: the passes that take the critical mutex in arrival order,
//! wait for the earlier heavy and light gates to drain and take every heavy and unreserved light slot, the lines it
//! says while it waits and once it holds the machine, the snapshot its `take` journals, and the release of all it took.
//! Its wake event, ticket, sleep and heartbeat are a slot waiter's ([`Waiter`]).

use std::collections::HashSet;
use std::path::PathBuf;

use gate_state::v1::{self, ClaimKind, Ticket, TicketKind};
use gate_state::{HeavyLight, KernelObject, MachineSnapshot, OtherClaim, reserved_light};
use gate_win32::mutex_exists;
use jiff::Timestamp;

use crate::acquire;
use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, ClaimOf, Held, Survey, Taken};
use crate::wait::{self, Arrival, Host, Waiter};

/// The kinds of earlier gate the waiting line counts and a drain with an unreserved light slot waits for.
const LOCAL_KINDS: [TicketKind; 2] = [TicketKind::Heavy, TicketKind::Light];

/// The pid and start, in epoch milliseconds, of the gates that hold a slot by a live claim.
type Holders = HashSet<(u32, u64)>;

/// What a critical gate holds of the machine: the critical mutex, the heavy and unreserved light slots, and its ticket,
/// which it keeps until it ends.
pub(crate) struct Machine {
    critical: Option<Held>,
    slots: Vec<Held>,
    ticket: PathBuf,
    /// The mutexes taken from a gate that died holding them, in the order they were taken.
    abandoned: Vec<String>,
}

impl Machine {
    /// The names of the heavy and unreserved light slots held, in the order they were taken.
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
            remote: None,
            expected: None,
        };
        let waiter = Waiter::queue(root, arrival, &ticket, host, console);
        MachineWait {
            root,
            machine: Machine {
                critical: None,
                slots: Vec::new(),
                ticket: waiter.ticket().to_path_buf(),
                abandoned: Vec::new(),
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

    /// Passes until the gate holds the machine, then rewrites its ticket as holding and reads what it found; each pass
    /// that falls short says the waiting line the first time and the still-waiting line when it is due, and sleeps on
    /// the wake event until the poll or the next still-waiting line. Interrupted once the console is, with what it took
    /// so far.
    pub(crate) fn until_held(mut self, survey: &mut Survey, console: &mut Console) -> MachineWaited {
        loop {
            if self.waiter.interrupted() {
                return MachineWaited::Interrupted(self.machine);
            }
            if self.pass(survey, console) {
                self.ticket.holds = true;
                self.waiter.rewrite(&self.ticket, console);
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
    /// waits until no earlier heavy gate, nor light one while it takes an unreserved light slot, still waits, a remote
    /// one or one whose gate already holds a slot left out; then takes every heavy and unreserved light slot of the
    /// counts as they stand. A pass that deleted a dead gate's ticket wakes the other waiters.
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
            self.drained = drain_waits_for(&self.others, &self.holders, order, self.counts.light) == 0;
            if !self.drained {
                return false;
            }
        }
        self.take_slots(survey, console)
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

    /// Takes every heavy and unreserved light slot of the counts it does not hold yet; whether it holds them all. A slot
    /// taken from a gate that died holding it has that gate's claim deleted, since the critical gate writes none to
    /// replace it, so the next gate to take the slot does not count the takeover again.
    fn take_slots(&mut self, survey: &mut Survey, console: &mut Console) -> bool {
        let mut all = true;
        for (object, index) in machine_slots(self.counts) {
            if self.machine.holds(&self.root.root.object_name(object)) {
                continue;
            }
            let Some(taken) = slots::try_object(self.root, object, index, survey, console) else {
                all = false;
                continue;
            };
            if taken.abandoned {
                slots::delete_claim(&taken.held, console);
            }
            let held = self.machine.keep(taken, console);
            self.machine.slots.push(held);
        }
        all
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
        console.err(&format!("gate: waiting for the machine (critical): {status}"));
        self.waiter.said_waiting();
    }

    /// Says the still-waiting line when it is due, with the whole seconds since the gate queued.
    fn say_heartbeat(&mut self, survey: &mut Survey, console: &mut Console) {
        if let Some(waited) = self.waiter.heartbeat_due() {
            let status = self.status(survey, console);
            console.err(&format!(
                "gate: still waiting for the machine (critical): {status} ({} s)",
                waited.as_secs()
            ));
        }
    }

    /// What the gate waits for, as its waiting lines say it, from the tickets and claims the last pass read.
    fn status(&self, survey: &mut Survey, console: &mut Console) -> String {
        let earlier = earlier_waiters(&self.others, &self.holders, self.ticket.queue_order());
        status_text(self.other_held(survey, console), self.counts.light, earlier)
    }

    /// The heavy and light slots of the counts, its own left out, whose mutex some gate has open. A slot that cannot be
    /// probed is said once and counts as not held.
    fn other_held(&self, survey: &mut Survey, console: &mut Console) -> HeavyLight {
        let heavy = (0..self.counts.heavy).map(KernelObject::HeavySlot);
        let light = (0..self.counts.light).map(KernelObject::LightSlot);
        HeavyLight {
            heavy: self.open_others(heavy, survey, console),
            light: self.open_others(light, survey, console),
        }
    }

    fn open_others(&self, objects: impl Iterator<Item = KernelObject>, survey: &mut Survey, console: &mut Console) -> u32 {
        let mut open = 0;
        for object in objects {
            let name = self.root.root.object_name(object);
            if self.machine.holds(&name) {
                continue;
            }
            match mutex_exists(&name) {
                Ok(true) => open += 1,
                Ok(false) => {}
                Err(error) => survey.say_once("cannot probe", &name, error, console),
            }
        }
        open
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
            other_held: self.other_held(survey, console),
            other_claims,
        }
    }
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
/// heavy waiters, and the light ones while a light count of `light` leaves it an unreserved light slot to take, a gate
/// of `holders` left out.
fn drain_waits_for(others: &[Ticket], holders: &Holders, order: (u64, u32), light: u32) -> usize {
    let kinds: &[TicketKind] = if light > reserved_light(light) {
        &LOCAL_KINDS
    } else {
        &[TicketKind::Heavy]
    };
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

/// The slots a critical gate takes for `counts`, each with its index: every heavy slot and every light slot below the
/// reserved ones, which are the highest `min(2, light count)`.
fn machine_slots(counts: HeavyLight) -> Vec<(KernelObject, u32)> {
    let light = counts.light.saturating_sub(reserved_light(counts.light));
    let heavy = (0..counts.heavy).map(|index| (KernelObject::HeavySlot(index), index));
    heavy.chain((0..light).map(|index| (KernelObject::LightSlot(index), index))).collect()
}

/// How many light slots of a count of `light` are reserved for light gates beside a critical gate, as its lines say it.
fn reserved_phrase(light: u32) -> String {
    let reserved = reserved_light(light);
    let noun = if reserved == 1 { "light slot" } else { "light slots" };
    format!("{reserved} {noun} reserved for light gates beside it")
}

/// The waiting lines' status: the slots other gates hold, the reserved light slots of a count of `light`, and the
/// earlier waiters.
fn status_text(held: HeavyLight, light: u32, earlier: usize) -> String {
    format!(
        "{} heavy and {} light slots held ({}), {earlier} earlier waiters",
        held.heavy,
        held.light,
        reserved_phrase(light)
    )
}

/// The line a critical gate says as it takes the machine `at`, after waiting `waited` seconds, with what it found.
pub(crate) fn holds_line(at: Timestamp, waited: f64, snapshot: &MachineSnapshot) -> String {
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
        "gate: holds the machine (critical) at {time} after {:.1} s: counts heavy {}, light {}; other gates hold {} heavy and {} light \
         slots ({}); other live claims: {claims}",
        (waited * 10.0).round_ties_even() / 10.0,
        counts.heavy,
        counts.light,
        other_held.heavy,
        other_held.light,
        reserved_phrase(counts.light)
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fixed time cannot be read must fail loudly")]
mod tests {
    use gate_state::v1::{Ticket, TicketKind};
    use gate_state::{HeavyLight, KernelObject, MachineSnapshot, OtherClaim};

    use super::{Holders, drain_waits_for, earlier_waiters, holds_line, is_earlier, machine_slots, status_text};
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
    fn the_critical_status_says_one_light_slot_or_n_light_slots() {
        let held = HeavyLight { heavy: 2, light: 1 };
        assert_eq!(
            status_text(held, 1, 3),
            "2 heavy and 1 light slots held (1 light slot reserved for light gates beside it), 3 earlier waiters"
        );
        assert_eq!(
            status_text(held, 2, 0),
            "2 heavy and 1 light slots held (2 light slots reserved for light gates beside it), 0 earlier waiters"
        );
        assert_eq!(
            status_text(held, 8, 0),
            "2 heavy and 1 light slots held (2 light slots reserved for light gates beside it), 0 earlier waiters"
        );
    }

    #[test]
    fn a_critical_drain_takes_every_heavy_slot_and_the_light_slots_below_the_reserved_ones() {
        let slots = |heavy, light| machine_slots(HeavyLight { heavy, light });
        assert_eq!(slots(1, 1), [(KernelObject::HeavySlot(0), 0)]);
        assert_eq!(slots(2, 2), [(KernelObject::HeavySlot(0), 0), (KernelObject::HeavySlot(1), 1)]);
        assert_eq!(slots(1, 3), [(KernelObject::HeavySlot(0), 0), (KernelObject::LightSlot(0), 0)]);
        assert_eq!(
            slots(1, 5),
            [
                (KernelObject::HeavySlot(0), 0),
                (KernelObject::LightSlot(0), 0),
                (KernelObject::LightSlot(1), 1),
                (KernelObject::LightSlot(2), 2)
            ]
        );
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
        assert_eq!(drain_waits_for(&others, &none, order, 3), 2, "the earlier heavy and light waiters");
        assert_eq!(
            drain_waits_for(&others, &none, order, 2),
            1,
            "every light slot reserved: the heavy waiter alone"
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

        assert_eq!(drain_waits_for(&others, &holders, own.queue_order(), 3), 1, "the light waiter alone");
        assert_eq!(
            drain_waits_for(&others, &holders, own.queue_order(), 1),
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
                    other gates hold 0 heavy and 1 light slots (1 light slot reserved for light gates beside it); other live claims: ";
        let claim = |pid, command: &str| OtherClaim {
            pid,
            kind: "light".to_owned(),
            index: 0,
            command: command.to_owned(),
            file: "gate-light-slot-0.json".to_owned(),
        };

        assert_eq!(holds_line(at, 1.96, &snapshot(Ok(Vec::new()))), format!("{head}none"));
        let two = Ok(vec![claim(5, "cargo test"), claim(6, "cargo fmt")]);
        assert_eq!(
            holds_line(at, 2.04, &snapshot(two)),
            format!("{head}5 light 0 cargo test, 6 light 0 cargo fmt")
        );
        let failed = Err("cannot read the claims in C:\\root\\slots: denied".to_owned());
        assert_eq!(
            holds_line(at, 2.0, &snapshot(failed)),
            format!("{head}unknown (cannot read the claims in C:\\root\\slots: denied)")
        );
    }
}

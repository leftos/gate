//! The build servers' priority: the port of the PowerShell gate's B2, which lowers the build servers outside an
//! ordinary gate's job to below normal, and raises those outside a High gate's job, and its own job's, to High. The
//! rule itself is the watchdog's `server_target` and `server_left`, which read no Windows type; this module reads the
//! other gates' facts the guard needs — at most once a pass, and only when a server the pass would move asks — and
//! says the two failure lines, each once: a class that cannot be set, once for the server, and a job that cannot be
//! listed, once a watch.

use std::collections::HashSet;
use std::io;

use gate_state::KernelObject;
use gate_state::v1::{self, ClaimKind, TicketKind};
use gate_watchdog::{PriorityClass, server_left, server_target};
use gate_win32::{OpenedJob, Priority};

use crate::console::Console;
use crate::resolve::ResolvedRoot;
use crate::servers::Classified;
use crate::slots::{Listed, Survey, ThisGate, critical_holding, listed_tickets, others};

/// What one pass leaves alone: whether a critical ticket holds the machine, and the pids in the jobs whose build
/// servers that pass leaves at their own gate's priority.
pub(crate) struct LeftAlone {
    /// Whether a critical ticket holds the machine now.
    pub(crate) holds: bool,
    /// The pids of the jobs listed this pass, nested jobs included.
    pub(crate) job: HashSet<u32>,
}

/// Where a pass reads what it leaves alone: the other gates of this root, or a test's own answer.
pub(crate) trait Guard {
    /// What this pass leaves alone, read now.
    fn read(&mut self, console: &mut Console) -> LeftAlone;
}

/// One pass's guard: read on the first server the pass would move, as the PowerShell gate's `Test-ServerLeft` reads
/// it only after `Get-ServerTarget` gave a class, and kept for the rest of the pass.
struct Pass<'g> {
    guard: &'g mut dyn Guard,
    left: Option<LeftAlone>,
}

impl Pass<'_> {
    /// What this pass leaves alone, read the first time a server asks.
    fn left(&mut self, console: &mut Console) -> &LeftAlone {
        self.left.get_or_insert_with(|| self.guard.read(console))
    }
}

/// The priority calls a pass makes: a server's class, and setting it. Behind a trait so a unit test can make them fail
/// without a machine's processes.
pub(crate) trait Calls {
    /// The priority class of the process `pid`; `None` when no process of that pid runs.
    ///
    /// # Errors
    ///
    /// The system's error when the process exists but cannot be queried.
    fn class_of(&self, pid: u32) -> io::Result<Option<Priority>>;

    /// Sets the priority class of the process `pid`; `None` when no process of that pid runs, which leaves nothing to
    /// set.
    ///
    /// # Errors
    ///
    /// The system's error when the process exists but its class cannot be set.
    fn set(&self, pid: u32, priority: Priority) -> io::Result<Option<()>>;
}

/// The priority calls of this machine.
struct System;

impl Calls for System {
    fn class_of(&self, pid: u32) -> io::Result<Option<Priority>> {
        gate_win32::priority_of(pid)
    }

    fn set(&self, pid: u32, priority: Priority) -> io::Result<Option<()>> {
        gate_win32::set_priority(pid, priority)
    }
}

/// The one `System` every watch's prioritizer makes its calls through.
static SYSTEM: System = System;

/// One watch's build servers' priority: the calls it makes and the servers whose class could not be set.
pub(crate) struct Prioritizer<'a> {
    /// Whether this gate is High — a critical one, or one under a critical parent's slot: the class its servers take.
    high: bool,
    /// The calls this machine answers with.
    calls: &'a dyn Calls,
    /// The pid and start of every server whose class could not be set, as the PowerShell gate's
    /// `$Watch.Unchangeable`: each is named once and never tried again.
    unchangeable: HashSet<(u32, u64)>,
}

impl Prioritizer<'static> {
    /// The prioritizer of a command running `high`.
    pub(crate) fn new(high: bool) -> Prioritizer<'static> {
        Prioritizer {
            high,
            calls: &SYSTEM,
            unchangeable: HashSet::new(),
        }
    }
}

impl Prioritizer<'_> {
    /// One sample's pass over the `servers` it classified: every classified server is moved, the old `MSBuild` nodes
    /// and other gates' compiler servers included, and a High gate's own job's too. `guard` is read at most once, on
    /// the first server the pass would move.
    pub(crate) fn run(&mut self, servers: &[Classified], guard: &mut dyn Guard, console: &mut Console) {
        let mut pass = Pass { guard, left: None };
        for server in servers {
            self.move_one(server, &mut pass, console);
        }
    }

    /// Moves one server, as the PowerShell gate's `Set-ServerPriority` does: its class is read, the guard applied, and
    /// the class it takes set. An ordinary gate leaves its own job's servers as they started; a server that has gone is
    /// passed over silently; one whose class cannot be set is named once and never tried again.
    fn move_one(&mut self, server: &Classified, pass: &mut Pass<'_>, console: &mut Console) {
        if server.inside && !self.high {
            return;
        }
        if self.unchangeable.contains(&(server.pid, server.created)) {
            return;
        }
        let class = match self.calls.class_of(server.pid) {
            Ok(Some(class)) => Some(as_class(class)),
            Ok(None) => return,
            // A class that cannot be read is unknown, as PowerShell reads .NET's failed getter as no class: an ordinary
            // gate leaves the server silently, and a High gate still tries the raise.
            Err(_) => None,
        };
        let Some(target) = server_target(self.high, class) else {
            return;
        };
        let left = pass.left(console);
        if server_left(self.high, class, left.job.contains(&server.pid), left.holds) {
            return;
        }
        if let Err(error) = self.calls.set(server.pid, as_priority(target)) {
            self.could_not(server, &error, console);
        }
    }

    /// Says that a server's class could not be set — `gate: could not lower|raise <name> <pid>: <message>` — once for
    /// its pid and start, which is never tried again.
    fn could_not(&mut self, server: &Classified, error: &io::Error, console: &mut Console) {
        if !self.unchangeable.insert((server.pid, server.created)) {
            return;
        }
        let verb = if self.high { "raise" } else { "lower" };
        console.err(&format!("gate: could not {verb} {} {}: {}", server.name, server.pid, message(error)));
    }
}

/// The other gates' facts a pass reads of this root: the tickets that name the gates holding the machine, the claims
/// of the live gates holding a slot, and the jobs those gates run their commands in — the port of the PowerShell
/// gate's `Get-ServerGuard` and `Get-OtherGateProcess`.
pub(crate) struct OtherGates<'a> {
    /// Whether this gate is High: it lists other live gates' jobs, where any other gate lists the jobs of the critical
    /// gates holding the machine.
    high: bool,
    /// This gate's own pid and start in epoch milliseconds, which name no other gate.
    own: (u32, u64),
    root: &'a ResolvedRoot,
    survey: &'a mut Survey,
    /// The job-listing failure lines said already, once a watch.
    said: HashSet<String>,
}

impl<'a> OtherGates<'a> {
    /// The other gates' facts as the gate `gate` of `root` reads them, which `high` picks between.
    pub(crate) fn new(high: bool, gate: &ThisGate, root: &'a ResolvedRoot, survey: &'a mut Survey) -> OtherGates<'a> {
        OtherGates {
            high,
            own: (gate.pid, gate.start_ms),
            root,
            survey,
            said: HashSet::new(),
        }
    }

    /// Whether a critical ticket under this root holds the machine, and the pids of the gates holding it. Dead tickets
    /// are left in place: a watch reads them without deleting what another gate is still to use.
    fn holders(&mut self, console: &mut Console) -> (bool, Vec<u32>) {
        let dir = v1::tickets_dir(&self.root.path);
        let listed = match listed_tickets(&self.root.path, self.own) {
            Ok(listed) => listed,
            Err(error) => {
                self.survey
                    .say_once("cannot read the tickets in", &dir.display().to_string(), error, console);
                return (false, Vec::new());
            }
        };
        let live = others(listed.iter().filter_map(Listed::live).cloned().collect(), self.own);
        let holding: Vec<u32> = live
            .iter()
            .filter(|ticket| ticket.kind == TicketKind::Critical && ticket.holds)
            .map(|ticket| ticket.pid)
            .collect();
        (critical_holding(&live), holding)
    }

    /// The pids of the other live gates holding a slot of this machine under this root. The critical claim names no
    /// other gate: it is this gate's own, or its critical parent's, whose job holds this gate's own. Nor does a remote
    /// slot's claim, whose command runs on the builder. Both are left out, as the PowerShell gate's `Get-OtherClaim`
    /// leaves out the critical claim's file and the remote pool's.
    fn other_gates(&mut self, console: &mut Console) -> Vec<u32> {
        let dir = v1::slots_dir(&self.root.path);
        match self.survey.other_claims(&self.root.path, console) {
            Ok(claims) => claims
                .into_iter()
                .filter(|(_, claim)| !matches!(claim.kind, ClaimKind::Critical | ClaimKind::Remote))
                .map(|(_, claim)| claim.pid)
                .collect(),
            Err(error) => {
                self.survey
                    .say_once("cannot read the claims in", &dir.display().to_string(), error, console);
                Vec::new()
            }
        }
    }
}

impl Guard for OtherGates<'_> {
    /// The facts this pass reads: `holds`, whether a critical ticket holds the machine, and the pids of the jobs it
    /// lists. A ticket or claim that cannot be read counts as none, said once a wait.
    fn read(&mut self, console: &mut Console) -> LeftAlone {
        let (holds, ids) = if self.high {
            (false, self.other_gates(console))
        } else {
            self.holders(console)
        };
        let root = self.root;
        let job = jobs(&ids, &|id| job_of(root, id), self.high, &mut self.said, console);
        LeftAlone { holds, job }
    }
}

/// The pids in the job of each gate of `ids`, and in the jobs nested inside them, as `open` gives them. A job that
/// cannot be listed is said once a watch, `its build servers may be lowered|raised`, and left out of the guard; a gate
/// with no job of its name has none to list, which is a gate that has gone since its ticket or claim was read.
fn jobs(
    ids: &[u32],
    open: &dyn Fn(u32) -> io::Result<Option<Vec<u32>>>,
    high: bool,
    said: &mut HashSet<String>,
    console: &mut Console,
) -> HashSet<u32> {
    let mut pids = HashSet::new();
    for &id in ids {
        match open(id) {
            Ok(Some(list)) => pids.extend(list),
            Ok(None) => {}
            Err(error) => {
                let moved = if high { "raised" } else { "lowered" };
                let line = format!(
                    "gate: could not list the job of gate {id} ({}); its build servers may be {moved}",
                    message(&error)
                );
                if said.insert(line.clone()) {
                    console.err(&line);
                }
            }
        }
    }
    pids
}

/// The processes in the job the gate `id` runs its command in, nested jobs included; `None` when no job has that name.
///
/// # Errors
///
/// The system's error when the job exists but cannot be opened or queried.
fn job_of(root: &ResolvedRoot, id: u32) -> io::Result<Option<Vec<u32>>> {
    let name = root.root.object_name(KernelObject::Job { gate_pid: id });
    match OpenedJob::open_to_list(&name)? {
        Some(job) => job.process_ids().map(Some),
        None => Ok(None),
    }
}

/// The message a failed priority call prints, as the PowerShell gate's `Win32Exception.Message` carries it: the
/// system's message for the error's code, or the error's own text when it has no code.
fn message(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => gate_win32::win32_message(code.cast_unsigned()),
        None => error.to_string(),
    }
}

/// The watchdog's class for `priority`, whose rule reads no Windows type.
const fn as_class(priority: Priority) -> PriorityClass {
    match priority {
        Priority::Idle => PriorityClass::Idle,
        Priority::BelowNormal => PriorityClass::BelowNormal,
        Priority::Normal => PriorityClass::Normal,
        Priority::AboveNormal => PriorityClass::AboveNormal,
        Priority::High => PriorityClass::High,
        Priority::RealTime => PriorityClass::RealTime,
    }
}

/// The Win32 class for `class`, the rule's own.
const fn as_priority(class: PriorityClass) -> Priority {
    match class {
        PriorityClass::Idle => Priority::Idle,
        PriorityClass::BelowNormal => Priority::BelowNormal,
        PriorityClass::Normal => Priority::Normal,
        PriorityClass::AboveNormal => Priority::AboveNormal,
        PriorityClass::High => Priority::High,
        PriorityClass::RealTime => Priority::RealTime,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::io;

    use gate_win32::Priority;

    use crate::console::Console;
    use crate::servers::Classified;

    use super::{Calls, Guard, LeftAlone, Prioritizer, jobs};

    /// A build server of the samples these tests pass, as the reader classifies one.
    fn server(pid: u32, created: u64, inside: bool) -> Classified {
        Classified {
            pid,
            name: "VBCSCompiler.exe".to_owned(),
            created,
            inside,
        }
    }

    /// Calls whose servers all read as `class` and whose every set fails with `message`.
    struct Failing {
        class: Priority,
        message: &'static str,
    }

    impl Calls for Failing {
        fn class_of(&self, _pid: u32) -> io::Result<Option<Priority>> {
            Ok(Some(self.class))
        }

        fn set(&self, _pid: u32, _priority: Priority) -> io::Result<Option<()>> {
            Err(io::Error::other(self.message))
        }
    }

    /// Calls whose servers all read as `class`, counting the sets made.
    struct Counting {
        class: Priority,
        sets: Cell<u32>,
    }

    impl Calls for Counting {
        fn class_of(&self, _pid: u32) -> io::Result<Option<Priority>> {
            Ok(Some(self.class))
        }

        fn set(&self, _pid: u32, _priority: Priority) -> io::Result<Option<()>> {
            self.sets.set(self.sets.get() + 1);
            Ok(Some(()))
        }
    }

    /// Calls of a machine no server of the sample runs on.
    struct Gone;

    impl Calls for Gone {
        fn class_of(&self, _pid: u32) -> io::Result<Option<Priority>> {
            Ok(None)
        }

        fn set(&self, _pid: u32, _priority: Priority) -> io::Result<Option<()>> {
            Ok(None)
        }
    }

    /// Calls whose servers' class cannot be read, counting the sets made.
    struct Unreadable {
        sets: Cell<u32>,
    }

    impl Calls for Unreadable {
        fn class_of(&self, _pid: u32) -> io::Result<Option<Priority>> {
            Err(io::Error::other("Access is denied."))
        }

        fn set(&self, _pid: u32, _priority: Priority) -> io::Result<Option<()>> {
            self.sets.set(self.sets.get() + 1);
            Ok(Some(()))
        }
    }

    /// A guard that answers `left` every time a pass reads it, counting the reads.
    struct Fixed {
        left: LeftAlone,
        reads: u32,
    }

    impl Guard for Fixed {
        fn read(&mut self, _console: &mut Console) -> LeftAlone {
            self.reads += 1;
            LeftAlone {
                holds: self.left.holds,
                job: self.left.job.clone(),
            }
        }
    }

    /// The guard answering `left`, read no time yet.
    fn fixed(left: LeftAlone) -> Fixed {
        Fixed { left, reads: 0 }
    }

    /// A guard that leaves nothing alone.
    fn alone() -> LeftAlone {
        LeftAlone {
            holds: false,
            job: HashSet::new(),
        }
    }

    /// A prioritizer making its calls through `calls`, which a test makes fail.
    fn prioritizer(high: bool, calls: &dyn Calls) -> Prioritizer<'_> {
        Prioritizer {
            high,
            calls,
            unchangeable: HashSet::new(),
        }
    }

    #[test]
    fn the_priority_failure_line_is_said_once_for_a_server() {
        let failing = Failing {
            class: Priority::Normal,
            message: "Access is denied.",
        };
        let mut priority = prioritizer(false, &failing);
        let mut console = Console::new();
        let servers = [server(7, 42, false), server(7, 42, false)];

        priority.run(&servers, &mut fixed(alone()), &mut console);
        priority.run(&servers, &mut fixed(alone()), &mut console);

        assert_eq!(console.take_notes(), ["gate: could not lower VBCSCompiler.exe 7: Access is denied."]);
    }

    #[test]
    fn the_priority_of_a_pid_a_later_process_took_is_tried_again() {
        let failing = Failing {
            class: Priority::Normal,
            message: "Access is denied.",
        };
        let mut priority = prioritizer(false, &failing);
        let mut console = Console::new();

        priority.run(&[server(7, 42, false), server(7, 43, false)], &mut fixed(alone()), &mut console);

        let notes = console.take_notes();
        assert_eq!(notes.len(), 2, "a later start is another process, tried and named again: {notes:?}");
    }

    #[test]
    fn the_priority_of_a_high_gate_raises_and_of_an_ordinary_one_lowers() {
        let mut console = Console::new();

        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        prioritizer(false, &counting).run(&[server(1, 10, false)], &mut fixed(alone()), &mut console);
        assert_eq!(counting.sets.get(), 1, "an ordinary gate lowers a server above below normal");

        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        prioritizer(true, &counting).run(&[server(1, 10, false)], &mut fixed(alone()), &mut console);
        assert_eq!(counting.sets.get(), 1, "a High gate raises a server below High");

        let counting = Counting {
            class: Priority::High,
            sets: Cell::new(0),
        };
        prioritizer(false, &counting).run(&[server(1, 10, false)], &mut fixed(alone()), &mut console);
        assert_eq!(counting.sets.get(), 1, "and an ordinary gate lowers it again once nothing holds");
    }

    #[test]
    fn the_priority_guard_leaves_a_listed_job_s_server_and_a_high_one_while_a_critical_gate_holds() {
        let mut console = Console::new();

        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        let guarded = LeftAlone {
            holds: false,
            job: HashSet::from([1]),
        };
        prioritizer(false, &counting).run(&[server(1, 10, false)], &mut fixed(guarded), &mut console);
        assert_eq!(counting.sets.get(), 0, "a server in a listed job stays at its own gate's priority");

        let counting = Counting {
            class: Priority::High,
            sets: Cell::new(0),
        };
        let holding = LeftAlone {
            holds: true,
            job: HashSet::new(),
        };
        prioritizer(false, &counting).run(&[server(1, 10, false)], &mut fixed(holding), &mut console);
        assert_eq!(counting.sets.get(), 0, "a server at High stays while a critical gate holds");
    }

    #[test]
    fn the_priority_of_an_ordinary_gate_leaves_its_own_job_alone_and_a_high_one_raises_it() {
        let mut console = Console::new();

        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        prioritizer(false, &counting).run(&[server(1, 10, true)], &mut fixed(alone()), &mut console);
        assert_eq!(counting.sets.get(), 0, "an ordinary gate leaves its own job's servers as they started");

        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        prioritizer(true, &counting).run(&[server(1, 10, true)], &mut fixed(alone()), &mut console);
        assert_eq!(counting.sets.get(), 1, "a High gate raises its own job's servers too");
    }

    #[test]
    fn the_priority_of_a_server_that_has_gone_is_passed_over() {
        let mut priority = prioritizer(false, &Gone);
        let mut console = Console::new();

        priority.run(&[server(1, 10, false)], &mut fixed(alone()), &mut console);

        assert!(console.take_notes().is_empty(), "a server that has gone says nothing");
    }

    #[test]
    fn the_priority_guard_is_not_read_when_no_server_would_move() {
        let counting = Counting {
            class: Priority::BelowNormal,
            sets: Cell::new(0),
        };
        let mut guard = fixed(alone());
        let mut console = Console::new();

        // An ordinary gate moves no server already below normal, nor one of its own job.
        let servers = [server(1, 10, false), server(2, 20, false), server(3, 30, true)];
        prioritizer(false, &counting).run(&servers, &mut guard, &mut console);

        assert_eq!(guard.reads, 0, "a pass with no server to move reads no guard");
        assert_eq!(counting.sets.get(), 0);
    }

    #[test]
    fn the_priority_guard_is_read_once_a_pass_for_every_server_it_moves() {
        let counting = Counting {
            class: Priority::Normal,
            sets: Cell::new(0),
        };
        let mut priority = prioritizer(false, &counting);
        let mut guard = fixed(alone());
        let mut console = Console::new();
        let servers = [server(1, 10, false), server(2, 20, false), server(3, 30, false)];

        priority.run(&servers, &mut guard, &mut console);
        assert_eq!(guard.reads, 1, "one read for the servers of one pass");
        assert_eq!(counting.sets.get(), 3, "every server the pass would move is moved");

        priority.run(&servers, &mut guard, &mut console);
        assert_eq!(guard.reads, 2, "and the next pass reads it again");
    }

    #[test]
    fn the_priority_of_a_server_whose_class_cannot_be_read_is_left_by_an_ordinary_gate() {
        let unreadable = Unreadable { sets: Cell::new(0) };
        let mut guard = fixed(alone());
        let mut console = Console::new();

        prioritizer(false, &unreadable).run(&[server(1, 10, false)], &mut guard, &mut console);

        assert_eq!(unreadable.sets.get(), 0, "an unknown class is not one an ordinary gate lowers");
        assert_eq!(guard.reads, 0, "nor one that asks the guard");
        assert!(console.take_notes().is_empty(), "and the failed read says nothing");
    }

    #[test]
    fn the_priority_of_a_server_whose_class_cannot_be_read_is_raised_by_a_high_gate() {
        let unreadable = Unreadable { sets: Cell::new(0) };
        let mut console = Console::new();

        prioritizer(true, &unreadable).run(&[server(1, 10, false)], &mut fixed(alone()), &mut console);

        assert_eq!(unreadable.sets.get(), 1, "a High gate still tries the raise");
        assert!(console.take_notes().is_empty(), "and the failed read says nothing");
    }

    #[test]
    fn the_priority_job_listing_line_is_said_once_a_watch() {
        let failed = |_id: u32| -> io::Result<Option<Vec<u32>>> { Err(io::Error::other("Access is denied.")) };
        let mut said = HashSet::new();
        let mut console = Console::new();

        let pids = jobs(&[7, 7], &failed, false, &mut said, &mut console);

        assert!(pids.is_empty(), "a job that cannot be listed is left out of the guard");
        assert_eq!(
            console.take_notes(),
            ["gate: could not list the job of gate 7 (Access is denied.); its build servers may be lowered"]
        );
    }

    #[test]
    fn the_priority_job_listing_line_names_the_word_of_the_gate() {
        let failed = |_id: u32| -> io::Result<Option<Vec<u32>>> { Err(io::Error::other("Access is denied.")) };
        let mut said = HashSet::new();
        let mut console = Console::new();

        let pids = jobs(&[9], &failed, true, &mut said, &mut console);

        assert!(pids.is_empty(), "a job that cannot be listed is left out of the guard");
        assert_eq!(
            console.take_notes(),
            ["gate: could not list the job of gate 9 (Access is denied.); its build servers may be raised"]
        );

        // A gate with no job of its name has none to list, which is no failure.
        let none = |_id: u32| -> io::Result<Option<Vec<u32>>> { Ok(None) };
        let mut said = HashSet::new();
        let mut console = Console::new();
        assert!(
            jobs(&[9], &none, false, &mut said, &mut console).is_empty(),
            "a gate with no job of its name holds none"
        );
        assert!(console.take_notes().is_empty(), "a gate with no job says nothing");
    }

    #[test]
    fn the_priority_guard_lists_every_process_of_the_jobs() {
        let held = |id: u32| -> io::Result<Option<Vec<u32>>> { Ok(Some(vec![id, id + 100])) };
        let mut said = HashSet::new();
        let mut console = Console::new();

        let pids = jobs(&[3, 4], &held, true, &mut said, &mut console);

        assert_eq!(pids, HashSet::from([3, 103, 4, 104]));
        assert!(console.take_notes().is_empty(), "every job listed says nothing");
    }
}

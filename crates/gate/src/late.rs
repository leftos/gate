//! The late take of a covered gate: a heavy or light gate whose parent gate's slot covers it watches that parent while
//! its command runs, and when the parent ends first, says so and takes a slot of its own kind for the rest of the run,
//! as soon as one frees, without pausing its command. A live heavy or light covered gate whose command this gate runs
//! inside, and whose own kind covers this gate's, stands in for that parent instead: this gate stays covered by it,
//! under the slot of its kind, and watches it in turn, so one build tree holds one slot. The watch loop drives it one
//! step a wake, on the thread that will release the slot, since a slot's mutex belongs to the thread that took it.

use std::io;
use std::time::{Duration, Instant};

use gate_win32::WaitableProcess;
use jiff::Timestamp;

use crate::console::Console;
use crate::covered::Covered;
use crate::nesting::{Enclosing, Parent, covers, enclosing_of};
use crate::run::Taker;
use crate::slots::{self, Liveness, Pool, Survey, Taken, epoch_ms};
use crate::wait::{Arrival, Host, Wait, Waited};

/// The slot a covered gate took once its parent ended, when it took it, and the parent its `take` named, held until the
/// run's release.
pub(crate) struct LateSlot {
    pub taken: Taken,
    pub took: Instant,
    /// The parent gate whose end sent the run to a slot of its own, or the gate standing in for it, which the late
    /// `take` names and the `release` the run writes afterwards copies; `None` for a run under no parent.
    pub parent_pid: Option<u32>,
}

/// What a covered run's late take is made of: the parent gate whose slot covers it, the pool of the run's own kind,
/// the machine's processors, the run's first arrival in Unix epoch milliseconds, which its ticket keeps, what the take
/// is journalled and claimed with, where the run keeps the slot taken until its release, and the run's covered marker.
pub(crate) struct LateOf<'a> {
    pub parent: Parent,
    pub pool: Pool,
    pub processors: usize,
    pub arrived: u64,
    pub taker: Taker<'a>,
    pub held: &'a mut Option<LateSlot>,
    pub covered: &'a mut Covered,
}

/// How a late take opens a parent gate, or a gate that stands in for one, to watch it: [`open_parent`], or a test's.
type Opener = fn(&Parent) -> Opened<WaitableProcess>;

/// The watch of a covered run's parent gate, and the take of a slot of the run's own kind once that parent has ended.
pub(crate) struct Late<'a> {
    of: LateOf<'a>,
    /// What the late wait reads of the other gates, apart from the watch's own reads.
    survey: Survey,
    state: State<'a>,
    open: Opener,
    /// The gates no stand-in is looked for among, by pid and start: this gate, and each found that could not be watched.
    skip: Vec<(u32, u64)>,
}

/// Where a late take is.
enum State<'a> {
    /// The parent runs, and is looked at through its handle on every step.
    Watching(WaitableProcess),
    /// The parent had ended by the time the watch began: the first step takes it as ended.
    Ended,
    /// The parent ended; the gate waits for a slot, since this moment.
    Waiting { wait: Box<Wait<'a>>, since: Instant },
    /// Nothing more to do: the slot is taken, the parent cannot be watched, or the wait ended with the command.
    Done,
}

/// What the open of the parent gate found.
#[derive(Debug, PartialEq, Eq)]
enum Opened<P> {
    /// The parent, which runs since its marker's start.
    Runs(P),
    /// No process of the parent's pid runs since its marker's start: it has ended.
    Ended,
    /// The parent cannot be watched, for this reason.
    Unwatchable(String),
}

impl<'a> Late<'a> {
    /// Opens the watch of `of`'s parent gate ([`watching`]); `None` when it cannot be watched, which is said, and the run
    /// stays covered by it to its end, its covered marker rewritten so no gate runs under it in that parent's place
    /// ([`Covered::disown`]). A run that wrote no covered marker opens no watch and says so: a gate started inside its
    /// command cannot find it to run under its slot, so a slot it took would hold that gate back.
    pub(crate) fn watch(of: LateOf<'a>, console: &mut Console) -> Option<Late<'a>> {
        Late::watch_by(of, open_parent, console)
    }

    /// [`Late::watch`], each gate watched opened by `open`.
    fn watch_by(of: LateOf<'a>, open: Opener, console: &mut Console) -> Option<Late<'a>> {
        if !of.covered.written() {
            console.err(&of.parent.unwatched_line("no covered marker"));
            return None;
        }
        let Some(state) = watching(&of.parent, open, console) else {
            of.covered.disown(console);
            return None;
        };
        Some(Late {
            survey: Survey::new(of.taker.gate),
            skip: vec![(of.taker.gate.pid, of.taker.gate.start_ms)],
            of,
            state,
            open,
        })
    }

    /// The poll of the wait for a slot while it goes on, which the watch wakes on; `None` otherwise.
    pub(crate) fn polling(&self) -> Option<Duration> {
        match &self.state {
            State::Waiting { wait, .. } => Some(wait.poll()),
            State::Watching(_) | State::Ended | State::Done => None,
        }
    }

    /// One step, which never blocks: looks at whether the parent has ended, and once it has, either stays covered by
    /// the gate that stands in for it or says so and queues for a slot of the run's kind; while the gate waits, takes
    /// one pass of the wait, with its still-waiting line when due. A look at the parent that fails is said once, and
    /// the run stays covered by it, its covered marker rewritten so no gate runs under it ([`Covered::disown`]). Whether this step moved the run off its parent's slot: onto a stand-in's slot of
    /// that gate's own kind, or onto the slot it took of its own.
    pub(crate) fn step(&mut self, console: &mut Console) -> bool {
        let ended = match &self.state {
            State::Watching(process) => match process.wait(Duration::ZERO) {
                Ok(ended) => ended,
                Err(error) => {
                    console.err(&self.of.parent.unwatched_line(&error.to_string()));
                    self.of.covered.disown(console);
                    self.state = State::Done;
                    return false;
                }
            },
            State::Ended => true,
            State::Waiting { .. } => return self.pass(console),
            State::Done => return false,
        };
        ended && self.parent_ended(console)
    }

    /// Ends a wait for a slot still going, as the command has ended: deletes the ticket and wakes the waiters, saying
    /// nothing. A slot already taken stays with the run until its release.
    pub(crate) fn end(&mut self, console: &mut Console) {
        if let State::Waiting { wait, .. } = std::mem::replace(&mut self.state, State::Done) {
            wait.end(console);
            slots::wake_waiters(self.of.taker.root, &mut self.survey, console);
        }
    }

    /// The parent has ended: a live covered gate whose command this gate runs inside, and whose own kind covers the
    /// run's, stands in for it ([`enclosing_of`]), or else the gate takes a slot of its own ([`Late::queue`]). One that
    /// cannot be watched is passed over, and the search goes on without it. Whether the run moved off the parent's slot
    /// at once: always onto a stand-in's, onto its own when the slot was free.
    fn parent_ended(&mut self, console: &mut Console) -> bool {
        loop {
            let taker = &self.of.taker;
            let (root, kind, trusted) = (taker.root, taker.request.slot, taker.request.test_seams.parent_job_trusted);
            let found = enclosing_of(root, kind, &self.skip, trusted, console).filter(|found| covers(found.held, kind));
            let Some(found) = found else {
                return self.queue(console);
            };
            if self.cover_under(found, console) {
                return true;
            }
        }
    }

    /// Stays covered by `found`, which stands in for the ended parent with a slot of its own kind: says so, watches it
    /// in turn, and names it on a take should it end too while the command runs. Whether it does: a gate that cannot be
    /// watched is said and passed over, and one that has ended since it was found is passed over, as a gate that does
    /// not run is.
    fn cover_under(&mut self, found: Enclosing, console: &mut Console) -> bool {
        let parent = Parent::standing_in(self.of.parent.marker.clone(), found);
        let process = match (self.open)(&parent) {
            Opened::Runs(process) => process,
            Opened::Ended => {
                self.skip.push((found.pid, found.start_ms));
                return false;
            }
            Opened::Unwatchable(reason) => {
                console.err(&parent.unwatched_stand_in_line(self.of.parent.pid, &reason));
                self.skip.push((found.pid, found.start_ms));
                return false;
            }
        };
        console.err(&parent.taken_up_line(self.of.parent.pid));
        self.state = State::Watching(process);
        self.of.taker.parent_pid = Some(parent.pid);
        self.of.parent = parent;
        true
    }

    /// Says the parent ended, queues for a slot of the run's kind with the run's first arrival, and takes the first
    /// pass at once. The late wait reads no Ctrl+C of its own: the watch ends on one, and the wait with it. Whether that
    /// first pass took the slot.
    fn queue(&mut self, console: &mut Console) -> bool {
        let taker = &self.of.taker;
        console.err(&self.of.parent.ended_line(taker.request.slot));
        let since = Instant::now();
        let arrival = Arrival {
            gate: taker.gate,
            request: taker.request,
            queued: since,
            arrived: self.of.arrived,
            builder: None,
            after_fallback: false,
            under: None,
            expected: None,
        };
        let host = Host {
            processors: self.of.processors,
            interrupt: None,
        };
        let wait = Wait::queue(taker.root, self.of.pool, arrival, host, console);
        self.state = State::Waiting { wait: Box::new(wait), since };
        self.pass(console)
    }

    /// One pass of the wait, its still-waiting line first when due; a slot taken ends the wait, is kept for the run's
    /// release before anything else, then claimed and journalled with the parent's pid ([`Taker::took`]). Whether this
    /// pass took it.
    fn pass(&mut self, console: &mut Console) -> bool {
        let State::Waiting { wait, since } = &mut self.state else {
            return false;
        };
        wait.say_heartbeat(&mut self.survey, console);
        let Some(waited) = wait.pass(&mut self.survey, console) else {
            return false;
        };
        let since = *since;
        let State::Waiting { wait, .. } = std::mem::replace(&mut self.state, State::Done) else {
            unreachable!("the state was waiting a moment ago")
        };
        let Waited::Taken(taken) = waited else {
            unreachable!("a late wait is under no parent, has no limit and reads no Ctrl+C, so only a take ends it")
        };
        let waited = since.elapsed().as_secs_f64();
        wait.end(console);
        let parent_pid = self.of.taker.parent_pid;
        let slot = self.of.held.insert(LateSlot {
            taken,
            took: Instant::now(),
            parent_pid,
        });
        self.of.taker.took(&mut slot.taken, self.of.pool, waited, &mut self.survey, console);
        true
    }
}

/// The state a watch of `parent` begins in: watching it while it runs, or ended. A parent no longer running, or whose
/// pid another process has taken since, has ended, which the first step finds. A parent that cannot be opened, or whose
/// start cannot be read, is said once: `None`.
fn watching<'a>(parent: &Parent, open: Opener, console: &mut Console) -> Option<State<'a>> {
    match open(parent) {
        Opened::Runs(process) => Some(State::Watching(process)),
        Opened::Ended => Some(State::Ended),
        Opened::Unwatchable(reason) => {
            console.err(&parent.unwatched_line(&reason));
            None
        }
    }
}

/// What the open of `parent` to watch it finds ([`opened`]).
fn open_parent(parent: &Parent) -> Opened<WaitableProcess> {
    let liveness = || slots::gate_liveness(parent.pid, parent.start_ms);
    opened(WaitableProcess::open(parent.pid), parent.start_ms, WaitableProcess::created, liveness)
}

/// What the open of the parent gate started at `start_ms` in Unix epoch milliseconds found, `open` its result and
/// `created` the read of the opened process's start: a process that started at another moment took the parent's pid
/// after it ended. A pid this gate may not open to wait on is judged by `liveness`, [`slots::gate_liveness`] of the
/// parent, which opens it with less access: gone when another process took the pid, and otherwise a live parent this
/// gate may not wait on, as an elevated one, which cannot be watched.
fn opened<P>(
    open: io::Result<Option<P>>,
    start_ms: u64,
    created: impl FnOnce(&P) -> io::Result<Timestamp>,
    liveness: impl FnOnce() -> Liveness,
) -> Opened<P> {
    match open {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => match liveness() {
            Liveness::Gone => Opened::Ended,
            Liveness::Live | Liveness::Unknown(_) => Opened::Unwatchable(error.to_string()),
        },
        Err(error) => Opened::Unwatchable(error.to_string()),
        Ok(None) => Opened::Ended,
        Ok(Some(process)) => match created(&process) {
            Ok(start) if epoch_ms(start) == start_ms => Opened::Runs(process),
            Ok(_) => Opened::Ended,
            Err(error) => Opened::Unwatchable(error.to_string()),
        },
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a late take the test cannot set up must fail the test loudly")]
mod tests {
    use std::io;
    use std::path::Path;

    use gate_args::Slot;
    use gate_state::v1::{self, CoveredMarker};
    use gate_state::{KernelObject, SlotKind};
    use jiff::Timestamp;
    use proc_table::Win32ProcessTable;
    use serde_json::Value;

    use super::{Late, LateOf, LateSlot, Opened, Opener, opened};
    use crate::console::Console;
    use crate::covered::Covered as Marker;
    use crate::journal::RunJournal;
    use crate::log::Log;
    use crate::nesting::Parent;
    use crate::run::Taker;
    use crate::slots::{Liveness, Pool, ThisGate, epoch_ms};
    use crate::wait::scratch::{Holder, Scratch, request};

    /// A parent's start in Unix epoch milliseconds, and that moment as a start read.
    const START: u64 = 1_790_000_000_123;

    fn start() -> Timestamp {
        Timestamp::from_millisecond(i64::try_from(START).expect("a start fits")).expect("a start is a time")
    }

    #[test]
    fn a_parent_is_watched_only_when_its_start_matches_and_ended_when_it_is_gone_or_its_pid_reused() {
        let read = |_: &u32| -> io::Result<Timestamp> { Ok(start()) };
        let asked = std::cell::Cell::new(0);
        let unasked = || {
            asked.set(asked.get() + 1);
            Liveness::Live
        };
        assert_eq!(opened(Ok(Some(7)), START, read, unasked), Opened::Runs(7));
        assert_eq!(opened(Ok(None), START, read, unasked), Opened::Ended, "no process has the pid");
        assert_eq!(
            opened(Ok(Some(7)), START + 1, read, unasked),
            Opened::Ended,
            "a process of another start took the pid"
        );
        let failed = opened(Err(io::Error::other("failed")), START, read, unasked);
        assert_eq!(failed, Opened::Unwatchable("failed".to_owned()), "a parent that cannot be opened");
        let unread = opened(Ok(Some(7)), START, |_| Err(io::Error::other("no times")), unasked);
        assert_eq!(unread, Opened::Unwatchable("no times".to_owned()), "a start that cannot be read");
        assert_eq!(asked.get(), 0, "only a parent denied to this gate is judged by its liveness");
    }

    #[test]
    fn a_parent_denied_to_this_gate_has_ended_only_when_its_liveness_says_it_is_gone() {
        let read = |_: &u32| -> io::Result<Timestamp> { Ok(start()) };
        let denied = || -> io::Result<Option<u32>> { Err(io::Error::from(io::ErrorKind::PermissionDenied)) };
        let reason = io::Error::from(io::ErrorKind::PermissionDenied).to_string();
        assert_eq!(
            opened(denied(), START, read, || Liveness::Gone),
            Opened::Ended,
            "another user's process took the pid"
        );
        assert_eq!(
            opened(denied(), START, read, || Liveness::Live),
            Opened::Unwatchable(reason.clone()),
            "a live parent this gate may not wait on, as an elevated one"
        );
        assert_eq!(
            opened(denied(), START, read, || Liveness::Unknown(io::Error::other("no times"))),
            Opened::Unwatchable(reason),
            "a parent whose start cannot be read counts as running"
        );
    }

    /// A light run of this test process in `scratch`, its log and journal, the parent `parent` that covers it, and its
    /// covered marker.
    struct Covered {
        gate: ThisGate,
        request: gate_args::RunRequest,
        log: Log,
        journal: RunJournal,
        held: Option<LateSlot>,
        parent: Parent,
        marker: Marker,
    }

    impl Covered {
        fn new(scratch: &Scratch, parent_pid: u32, start_ms: u64) -> Covered {
            let request = request(Slot::Light);
            let gate = ThisGate::read(&request).expect("this process's start can be read");
            let journal = RunJournal::open(&scratch.root, &gate);
            let marker = Marker::write(scratch.path(), &gate, parent_pid, &mut Console::new());
            assert!(marker.written(), "the run's covered marker is written");
            Covered {
                gate,
                request,
                log: Log::new(Path::new("out.log")),
                journal,
                held: None,
                marker,
                parent: Parent {
                    held: Slot::Heavy,
                    pid: parent_pid,
                    start_ms,
                    marker: format!("heavy:{parent_pid}:{start_ms}"),
                    unjudged: None,
                    stand_in: false,
                    commit: false,
                },
            }
        }

        /// The late take of this run.
        fn late<'a>(&'a mut self, scratch: &'a Scratch, console: &mut Console) -> Option<Late<'a>> {
            let of = self.late_of(scratch);
            Late::watch(of, console)
        }

        /// The late take of this run, each gate it watches opened by `open`.
        fn late_by<'a>(&'a mut self, scratch: &'a Scratch, open: Opener, console: &mut Console) -> Option<Late<'a>> {
            let of = self.late_of(scratch);
            Late::watch_by(of, open, console)
        }

        /// What this run's late take is made of.
        fn late_of<'a>(&'a mut self, scratch: &'a Scratch) -> LateOf<'a> {
            LateOf {
                parent: self.parent.clone(),
                pool: Pool::Local(SlotKind::Light),
                processors: 4,
                arrived: self.gate.start_ms,
                taker: Taker {
                    root: &scratch.root,
                    gate: &self.gate,
                    request: &self.request,
                    ceiling: 60,
                    log: &self.log,
                    tag: None,
                    journal: &mut self.journal,
                    parent_pid: Some(self.parent.pid),
                    expected: None,
                },
                held: &mut self.held,
                covered: &mut self.marker,
            }
        }
    }

    /// The kinds the covered markers under `scratch` name, in no set order.
    fn marker_kinds(scratch: &Scratch) -> Vec<Option<String>> {
        v1::read_covered(&v1::covered_dir(scratch.path()))
            .expect("the covered folder lists")
            .into_iter()
            .map(|(_, marker)| marker.kind)
            .collect()
    }

    #[test]
    fn a_covered_gate_that_wrote_no_marker_never_watches_its_parent_and_says_so() {
        let scratch = Scratch::new("late-unmarked");
        let mut console = Console::new();
        let mut covered = Covered::new(&scratch, GONE, START);
        covered.marker.end(&mut console);
        let reason = "cannot read the working directory: denied".to_owned();
        covered.marker = Marker::write_as(scratch.path(), Err(&reason), GONE, &mut console);

        assert!(covered.late(&scratch, &mut console).is_none(), "no watch, so no late take");
        assert_eq!(
            console.take_notes(),
            [format!(
                "gate: cannot watch the parent gate {GONE} (no covered marker); this run stays covered by it"
            )]
        );
        assert!(covered.held.is_none(), "no slot is taken");
        assert!(journal(&scratch).is_empty(), "nothing is journalled");
    }

    /// Every gate a late take watches cannot be watched.
    fn unwatchable(_: &Parent) -> Opened<gate_win32::WaitableProcess> {
        Opened::Unwatchable("denied".to_owned())
    }

    #[test]
    fn a_covered_gate_that_cannot_watch_its_parent_rewrites_its_marker_so_no_gate_runs_under_it() {
        let scratch = Scratch::new("late-unwatched");
        let mut console = Console::new();
        let mut covered = Covered::new(&scratch, GONE, START);
        assert_eq!(marker_kinds(&scratch), [Some("light".to_owned())], "the run's own kind first");

        assert!(covered.late_by(&scratch, unwatchable, &mut console).is_none(), "no watch");
        assert_eq!(
            console.take_notes(),
            [format!(
                "gate: cannot watch the parent gate {GONE} (denied); this run stays covered by it"
            )]
        );
        assert_eq!(
            marker_kinds(&scratch),
            [Some("none".to_owned())],
            "a gate that will take no slot of its own stands in for no ended parent"
        );
    }

    /// The ended parent [`GONE`] is found ended; every other gate cannot be watched.
    fn only_the_parent_ended(parent: &Parent) -> Opened<gate_win32::WaitableProcess> {
        if parent.pid == GONE {
            Opened::Ended
        } else {
            Opened::Unwatchable("denied".to_owned())
        }
    }

    #[test]
    fn a_stand_in_that_cannot_be_watched_is_passed_over_and_the_run_takes_its_own_slot() {
        let scratch = Scratch::new("late-unwatched-stand-in");
        let sleeper = Sleeper::start();
        let (stand_in, stand_in_start) = sleeper.gate();
        write_heavy_marker(&scratch, stand_in, stand_in_start);
        let _job = InJobOf::gate(&scratch, stand_in);
        let mut console = Console::new();
        let mut covered = Covered::new(&scratch, GONE, START);
        let mut late = covered
            .late_by(&scratch, only_the_parent_ended, &mut console)
            .expect("a gone parent is watched as ended");

        assert!(late.step(&mut console), "the first step takes the free slot");
        assert_eq!(late.polling(), None, "the slot is taken, so nothing waits");
        drop(late);
        assert_eq!(
            console.take_notes(),
            [
                format!("gate: cannot watch gate {stand_in} (denied) to run under its slot in place of the parent gate {GONE}"),
                format!("gate: the parent gate {GONE} ended; taking a light slot for the rest of this run"),
            ]
        );
        assert!(covered.held.is_some(), "the run took a slot of its own");
    }

    /// A pid no process runs as: Windows gives out multiples of 4 far below this one.
    const GONE: u32 = u32::MAX - 3;

    /// The journal's lines in `scratch`, none when it was not written.
    fn journal(scratch: &Scratch) -> Vec<Value> {
        let text = match std::fs::read_to_string(scratch.path().join("events.jsonl")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            read => read.expect("the journal can be read"),
        };
        text.lines()
            .map(|line| serde_json::from_str(line).expect("a journal line is JSON"))
            .collect()
    }

    #[test]
    fn a_parent_that_runs_is_only_watched() {
        let scratch = Scratch::new("late-runs");
        let mut console = Console::new();
        let own = ThisGate::read(&request(Slot::Light)).expect("this process's start can be read");
        // This test process stands in for the parent: it runs for the whole test.
        let mut covered = Covered::new(&scratch, own.pid, own.start_ms);
        let mut late = covered.late(&scratch, &mut console).expect("this process can be watched");
        assert!(!late.step(&mut console));
        assert!(!late.step(&mut console));
        assert_eq!(late.polling(), None, "no wait while the parent runs");
        late.end(&mut console);
        drop(late);
        assert!(console.take_notes().is_empty(), "nothing is said while the parent runs");
        assert!(covered.held.is_none());
        assert!(journal(&scratch).is_empty(), "nothing is journalled");
    }

    #[test]
    fn a_parent_gone_before_the_watch_takes_a_free_slot_on_the_first_step() {
        let scratch = Scratch::new("late-free");
        let mut console = Console::new();
        let mut covered = Covered::new(&scratch, GONE, START);
        let mut late = covered.late(&scratch, &mut console).expect("a gone parent is watched as ended");
        assert!(late.step(&mut console), "the first step takes the free slot");
        assert_eq!(late.polling(), None, "the slot is taken, so nothing waits");
        drop(late);

        let notes = console.take_notes();
        assert_eq!(
            notes,
            [format!(
                "gate: the parent gate {GONE} ended; taking a light slot for the rest of this run"
            )]
        );
        let slot = covered.held.as_ref().expect("the slot is kept for the release");
        let name = scratch.name(KernelObject::LightSlot(0));
        assert_eq!(slot.taken.held.name(), name);
        let lines = journal(&scratch);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            (&lines[0]["event"], &lines[0]["slots"], &lines[0]["parentPid"]),
            (&Value::from("take"), &Value::from(vec![name]), &Value::from(GONE))
        );
        let claim = v1::claim_path(scratch.path(), KernelObject::LightSlot(0)).expect("a light slot has a claim");
        assert_eq!(v1::read_claim(&claim).expect("the claim is written").pid, covered.gate.pid);
    }

    #[test]
    fn a_full_pool_is_waited_for_and_the_slot_taken_once_it_frees() {
        let scratch = Scratch::new("late-full");
        scratch.one_light_slot();
        let mut console = Console::new();
        let holder = Holder::take(scratch.name(KernelObject::LightSlot(0)));
        let mut covered = Covered::new(&scratch, GONE, START);
        let mut late = covered.late(&scratch, &mut console).expect("a gone parent is watched as ended");
        assert!(!late.step(&mut console));
        assert!(late.polling().is_some(), "the gate waits for the held slot");
        assert!(!late.step(&mut console));
        assert!(late.polling().is_some(), "still held");

        drop(holder);
        assert!(late.step(&mut console), "the freed slot is taken");
        assert_eq!(late.polling(), None, "nothing waits once the slot is taken");
        drop(late);
        assert_eq!(
            console.take_notes(),
            [
                format!("gate: the parent gate {GONE} ended; taking a light slot for the rest of this run"),
                "gate: waiting for a light slot (1 busy)".to_owned()
            ]
        );
        assert!(covered.held.is_some());
        let ticket = v1::ticket_path(scratch.path(), covered.gate.pid, covered.gate.start_ms);
        assert!(!ticket.exists(), "the take deletes the ticket");
    }

    #[test]
    fn a_wait_the_command_outlives_ends_without_a_take() {
        let scratch = Scratch::new("late-end");
        scratch.one_light_slot();
        let mut console = Console::new();
        let holder = Holder::take(scratch.name(KernelObject::LightSlot(0)));
        let mut covered = Covered::new(&scratch, GONE, START);
        let mut late = covered.late(&scratch, &mut console).expect("a gone parent is watched as ended");
        late.step(&mut console);
        assert!(late.polling().is_some(), "the gate waits for the held slot");
        late.end(&mut console);
        assert_eq!(late.polling(), None, "the wait ended with the command");
        late.step(&mut console);
        drop(late);
        drop(holder);

        assert_eq!(console.take_notes().len(), 2, "the parent's line and the waiting line, nothing more");
        assert!(covered.held.is_none(), "no slot is taken");
        let ticket = v1::ticket_path(scratch.path(), covered.gate.pid, covered.gate.start_ms);
        assert!(!ticket.exists(), "the ticket is deleted");
        assert!(journal(&scratch).is_empty(), "nothing is journalled");
    }

    /// The process that started this test, and its start in Unix epoch milliseconds: a live process other than this
    /// one, which outlives the test.
    fn runner() -> (u32, u64) {
        let me = std::process::id();
        let pid = Win32ProcessTable
            .snapshot_without_command_lines()
            .into_iter()
            .find(|process| process.pid == me)
            .expect("this process is in the process table")
            .parent_pid;
        let start = gate_win32::process_start_of(pid)
            .expect("the runner's start can be read")
            .expect("the runner runs");
        (pid, epoch_ms(start))
    }

    /// Writes, under `scratch`, the covered marker of the live process `pid` started at `start_ms`, as a heavy covered
    /// gate's.
    fn write_heavy_marker(scratch: &Scratch, pid: u32, start_ms: u64) {
        let marker = CoveredMarker {
            pid,
            process_start: start_ms,
            parent_pid: GONE,
            command: "cargo build".to_owned(),
            cwd: scratch.path().display().to_string(),
            since: start_ms,
            kind: Some("heavy".to_owned()),
        };
        v1::write_covered(&v1::covered_path(scratch.path(), pid, start_ms), &marker).expect("the marker can be written");
    }

    /// A process that runs until this is dropped, started after the test process's own runner.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        fn start() -> Sleeper {
            let mut command = std::process::Command::new("cmd");
            command
                .args(["/d", "/c", "ping -n 600 127.0.0.1 >nul"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            Sleeper(gate_win32::spawn(&mut command).expect("cmd starts"))
        }

        /// Its pid and its start in Unix epoch milliseconds.
        fn gate(&self) -> (u32, u64) {
            let pid = self.0.id();
            let start = gate_win32::process_start_of(pid).expect("its start can be read").expect("it runs");
            (pid, epoch_ms(start))
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            // A process that has ended already is no failure of the test.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Puts this test process in a job named as the job of the gate `pid` is named under `scratch`, as a gate whose
    /// command started this one would hold it; the job's handle is closed when this drops, the process left in it.
    struct InJobOf(windows_sys::Win32::Foundation::HANDLE);

    impl InJobOf {
        fn gate(scratch: &Scratch, pid: u32) -> InJobOf {
            use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;

            let name: Vec<u16> = scratch.name(KernelObject::Job { gate_pid: pid }).encode_utf16().chain(Some(0)).collect();
            // SAFETY: the name is NUL-terminated and outlives the call, and no security attributes are given.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
            assert!(!job.is_null(), "the job can be created: {}", io::Error::last_os_error());
            // SAFETY: the job's handle was opened above; the current process's pseudo-handle needs no closing.
            let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
            assert_ne!(assigned, 0, "this process joins the job: {}", io::Error::last_os_error());
            InJobOf(job)
        }
    }

    impl Drop for InJobOf {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by this test and is closed once, here.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    #[test]
    fn a_covered_gate_whose_command_holds_this_gate_stands_in_for_the_ended_parent_and_no_slot_is_taken() {
        let scratch = Scratch::new("late-stand-in");
        // The runner started first, so it would be the outermost, but its command does not hold this test: no job of
        // its name exists. The sleeper's job holds this test, so it stands in.
        let (runner, runner_start) = runner();
        write_heavy_marker(&scratch, runner, runner_start);
        let sleeper = Sleeper::start();
        let (stand_in, stand_in_start) = sleeper.gate();
        write_heavy_marker(&scratch, stand_in, stand_in_start);
        let _job = InJobOf::gate(&scratch, stand_in);
        let mut console = Console::new();
        let mut covered = Covered::new(&scratch, GONE, START);
        let mut late = covered.late(&scratch, &mut console).expect("a gone parent is watched as ended");

        assert!(late.step(&mut console), "the run moves onto the stand-in's slot");
        assert_eq!(late.polling(), None, "nothing waits");
        assert!(!late.step(&mut console), "the gate standing in runs, so the next step only watches it");
        drop(late);
        assert_eq!(
            console.take_notes(),
            [format!(
                "gate: the parent gate {GONE} ended; running under the heavy slot of gate {stand_in}"
            )]
        );
        assert!(covered.held.is_none(), "no slot is taken");
        assert!(journal(&scratch).is_empty(), "nothing is journalled");
        let ticket = v1::ticket_path(scratch.path(), covered.gate.pid, covered.gate.start_ms);
        assert!(!ticket.exists(), "the gate never queued");
    }
}

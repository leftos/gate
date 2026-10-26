//! The processes that left a command's job: a packaged desktop app, such as the Store's pwsh, that a process of the job
//! without package identity starts runs in a job of its own, and so does what it starts. A kill looks for them under
//! the job's processes before the job is terminated, while the processes that started them still run, then terminates
//! the job and ends them parents first, each checked through its own handle to be the process the snapshot saw, and
//! makes one more pass for the children they started meanwhile.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

use gate_router::escaped as shown;
use gate_state::{KernelObject, StateRoot};
use gate_win32::{Job, OpenedJob, ProcessTimes, StoppableProcess, WaitableProcess, file_time_now};
use jiff::Timestamp;
use proc_table::{Children, ProcessInfo, ProcessTree, Win32ProcessTable, filetime_to_timestamp};

use crate::stopping::{Node, Opened, failure, open_checked};

/// How many of the processes that left the job the kill's line names.
const NAMED: usize = 5;
/// A gate's image: one among the processes that left the job has its own command's job terminated before it is ended.
const GATE_IMAGE: &str = "gate.exe";
/// Why a look whose snapshot listed no process could not look: the snapshot itself failed.
pub(crate) const EMPTY_SNAPSHOT: &str = "the process snapshot listed no processes";
/// Why a look could not begin: the system time is outside the range a timestamp holds.
pub(crate) const NO_CLOCK: &str = "the system time is outside the range a timestamp holds";

/// One process outside the job, under a process the job holds.
#[derive(Clone, Debug)]
pub(crate) struct Escaped {
    node: Node,
    /// The executable's file name, without its folder.
    image: String,
}

impl Escaped {
    /// The process `process` of a snapshot.
    fn of(process: &ProcessInfo) -> Escaped {
        Escaped {
            node: Node {
                pid: process.pid,
                parent: process.parent_pid,
                created: process.created,
            },
            image: process.name.clone(),
        }
    }

    /// The key that tells it from a process that took its pid since: its pid and its start.
    fn key(&self) -> (u32, Option<Timestamp>) {
        (self.node.pid, self.node.created)
    }
}

/// One process outside the job a kill ended, and when it ended: `None` when that could not be read.
struct Ended {
    process: Escaped,
    exited: Option<Timestamp>,
}

/// Whether a process the snapshot lists under the pid of an ended process is a child that process started.
#[derive(Debug, PartialEq, Eq)]
enum Kin {
    Child,
    /// Another holder of the pid started it.
    Other,
    /// Which it is cannot be told.
    Unknown,
}

/// The job's members, as the two reads around the snapshot listed them.
struct Members {
    /// Every pid either read listed, each once, in the order first read.
    held: Vec<u32>,
    /// The members only the read before the snapshot listed whose pid the snapshot gives to a process that started
    /// after the look began: the member had ended and another process took its pid. Nothing is taken from under them.
    reused: HashSet<u32>,
    /// The members the job remembers and no longer held when the look ended: what each started that the snapshot
    /// lists is taken as a member's is, though it has ended.
    departed: Vec<Departed>,
}

/// A process that was in the job and has ended, as the job remembers it ([`Job::departed`]): its pid, its start, read
/// while it ran in the job, and the stretch of time in which it can have started a process as a member, from the later
/// of its start and the job's creation to its exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Departed {
    pub(crate) pid: u32,
    pub(crate) start: Timestamp,
    pub(crate) from: Timestamp,
    pub(crate) until: Timestamp,
}

/// The members `job` remembers that have ended, those whose pid the job holds again included, with why what it
/// remembers may lack a member, when it may.
pub(crate) fn departed(job: &Job) -> (Vec<Departed>, Option<String>) {
    let (ended, trouble) = job.departed();
    let departed = ended
        .into_iter()
        .map(|departure| Departed {
            pid: departure.pid,
            start: departure.start,
            from: departure.from,
            until: departure.until,
        })
        .collect();
    (departed, trouble)
}

/// Has `job` forget the `walked` members a look that began at `since` and saw `tree` has taken everything under
/// ([`forgettable`]).
pub(crate) fn forget(job: &Job, walked: &[Departed], since: Timestamp, tree: &ProcessTree) {
    job.forget(&forgettable(walked, since, tree));
}

/// The `walked` members a look that began at `since` has taken everything under: those that had ended before it began,
/// so the snapshot `tree` taken after lists every process they started, and they start nothing more. One that ended
/// later can have started a process after the snapshot, and is kept for the next look; and all are kept when the
/// snapshot listed nothing, as a failed one does, so its look took nothing.
fn forgettable(walked: &[Departed], since: Timestamp, tree: &ProcessTree) -> Vec<(u32, Timestamp, Timestamp)> {
    if tree.iter().next().is_none() {
        return Vec::new();
    }
    walked
        .iter()
        .filter(|member| member.until < since)
        .map(|member| (member.pid, member.start, member.until))
        .collect()
}

/// What a kill's `look` finds outside the job, as [`outside`] finds it under the members and under the `earlier`
/// processes, with a line saying why the job's memory may lack a member when its `trouble` says so: the look goes on
/// with the members the memory has.
fn kill_look(look: Look<'_>, trouble: Option<&str>, earlier: &[Escaped], running: impl Fn(u32) -> bool) -> Found {
    let members = members(look.before, &look.after, look.since, look.tree, look.departed);
    let mut found = outside(&members, earlier, look.tree, running);
    found
        .problems
        .extend(trouble.map(|why| could_not_look(&format!("the job's memory failed: {why}"))));
    found
}

/// The line a watch says the first time the job's memory has `trouble`, which `said` then keeps: the watch goes on
/// sampling.
pub(crate) fn memory_line(said: &mut Option<String>, trouble: Option<&str>) -> Option<String> {
    let why = trouble.filter(|_| said.is_none())?;
    *said = Some(why.to_owned());
    Some(format!(
        "gate: the job's memory failed, so a kill may miss a process that left the job under one that ended: {why}"
    ))
}

/// What a kill reaches: the command's job, and the state root that names the job of a gate among the processes that
/// left the job.
#[derive(Clone, Copy)]
pub(crate) struct Reach<'a> {
    pub(crate) job: &'a Job,
    pub(crate) root: &'a StateRoot,
}

/// What looking for the processes that left the job found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The processes outside the job, parents first.
    escaped: Vec<Escaped>,
    /// The pids of the processes under the job whose start, or whose parent's, could not be read, so whether they are
    /// its children could not be told; they are left running.
    unjudged: HashSet<u32>,
    /// A line for each look that failed.
    problems: Vec<String>,
    /// Whether a later look goes under the members the job remembers that have ended, as it goes under the recorded
    /// processes: only where the watch's records are passed, so an end of the run after the command's own exit, which
    /// leaves what it left running outside its job, reaches neither.
    departed: bool,
}

impl Found {
    /// What the watch `recorded` outside the job, as a kill's earlier look: its processes, parents first; a later look
    /// goes under the members the job remembers that have ended too.
    pub(crate) fn recorded(recorded: &Recorded) -> Found {
        Found {
            escaped: recorded.escaped.clone(),
            departed: true,
            ..Found::default()
        }
    }

    /// The pids of the processes outside the job found, in order.
    #[cfg(test)]
    pub(crate) fn pids(&self) -> Vec<u32> {
        self.escaped.iter().map(|process| process.node.pid).collect()
    }

    /// Whether a later look goes under the members the job remembers that have ended.
    #[cfg(test)]
    pub(crate) fn reaches_departed(&self) -> bool {
        self.departed
    }
}

/// What a terminate reaching the escaped processes did.
pub(crate) struct Reached {
    /// The processes outside the job it ended, in the order it ended them, parents first.
    pub(crate) ended: Vec<Escaped>,
    /// A line for each process outside the job it could not end and for each look that failed, then one for the
    /// processes it could not judge.
    pub(crate) problems: Vec<String>,
}

impl Reach<'_> {
    /// What `earlier` found, then what a look now finds that it did not: the processes that left the job under its
    /// members, under the members it remembers that have ended when `earlier` says so, and under each of `earlier`'s
    /// processes the snapshot still lists as itself, though the process that started it has ended. A look that fails
    /// says why in the [`Found`].
    pub(crate) fn look(&self, earlier: Found) -> Found {
        let now = self.find(&earlier.escaped, earlier.departed).unwrap_or_else(|problem| Found {
            problems: vec![could_not_look(&problem)],
            ..Found::default()
        });
        union(earlier, now)
    }

    /// The processes outside the job under the processes it holds, the members read before the snapshot and after
    /// it, under the members it remembers that had ended by the first read when `with_departed`, and under each of the
    /// `earlier` processes still running as itself; nothing, and no snapshot taken, when there is no earlier process,
    /// neither of two reads lists a member and no remembered member is looked under. The remembered members looked
    /// under are forgotten then, as a sample forgets them. The snapshot has no command lines.
    fn find(&self, earlier: &[Escaped], with_departed: bool) -> Result<Found, String> {
        let listed = || self.job.process_ids().map_err(|error| failure("QueryInformationJobObject", &error));
        let remembered = || if with_departed { departed(self.job) } else { (Vec::new(), None) };
        let since = now().ok_or_else(|| NO_CLOCK.to_owned())?;
        let before = listed()?;
        let (known, trouble) = remembered();
        if earlier.is_empty() && before.is_empty() && known.is_empty() && trouble.is_none() && listed()?.is_empty() {
            return Ok(Found::default());
        }
        let snapshot = Win32ProcessTable.snapshot_without_command_lines();
        if snapshot.is_empty() {
            return Err(EMPTY_SNAPSHOT.to_owned());
        }
        // The snapshot takes a while: a process that joined the job meanwhile is a member like any other.
        let after = listed()?;
        // Read after the second read, as a sample reads it: a member that ended meanwhile is among them.
        let (ended, trouble) = remembered();
        let tree = ProcessTree::new(snapshot);
        forget(self.job, &ended, since, &tree);
        let look = Look {
            before,
            after,
            since,
            tree: &tree,
            departed: ended,
        };
        Ok(kill_look(look, trouble.as_deref(), earlier, still_runs))
    }

    /// Terminates the job with `status`, then ends with `status` every process outside it that `earlier` found or a
    /// look now finds, below the job's members and below `earlier`'s processes alike, parents first, each checked
    /// through its own handle: all are terminated before any is waited for, then all are waited for until `wait` has
    /// passed since the first wait began. One more look then ends the children the ended processes started while they
    /// ran since the first look, waited for only until that same deadline.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated; then nothing outside it is ended.
    pub(crate) fn terminate(&self, status: u32, wait: Duration, earlier: Found) -> io::Result<Reached> {
        let found = self.look(earlier);
        self.job.terminate(status)?;
        let seen: HashSet<(u32, Option<Timestamp>)> = found.escaped.iter().map(Escaped::key).collect();
        let mut ending = Ending {
            reach: *self,
            status,
            wait,
            deadline: None,
            problems: found.problems,
            unjudged: found.unjudged,
        };
        let first = ending.end(found.escaped);
        let late = ending.started_since(&first, &seen);
        let second = ending.end(late);
        let ended = first.into_iter().chain(second).map(|ended| ended.process).collect();
        Ok(ending.reached(ended))
    }
}

/// One kill's ending of the processes outside the job, and what it could not do.
struct Ending<'a> {
    reach: Reach<'a>,
    status: u32,
    wait: Duration,
    /// When the kill's waits end: set when its first wait begins, and kept by every later one.
    deadline: Option<Instant>,
    problems: Vec<String>,
    unjudged: HashSet<u32>,
}

impl Ending<'_> {
    /// Terminates each of `escaped` in order that is still the process the snapshot saw, then waits for them all until
    /// the kill's deadline; the ones that ended.
    fn end(&mut self, escaped: Vec<Escaped>) -> Vec<Ended> {
        let mut issued = Vec::new();
        for process in escaped {
            match open_checked(&process.node, Some(self.reach.job)) {
                Opened::Unjudged if still_runs(process.node.pid) => {
                    self.unjudged.insert(process.node.pid);
                }
                // One that no longer runs is no longer this kill's to judge.
                Opened::Gone | Opened::Unjudged => {}
                Opened::Failed(error) => self.problems.push(could_not_end(&process, &error)),
                // One that has ended already is no longer this kill's to end.
                Opened::Same(handle) if matches!(handle.wait(Duration::ZERO), Ok(true)) => {}
                Opened::Same(handle) => {
                    let refused = self.issue(&process, &handle).err();
                    issued.push((process, handle, refused));
                }
            }
        }
        self.await_ended(issued)
    }

    /// Terminates `process`, open as `handle`; a gate's own command's job is terminated first.
    fn issue(&mut self, process: &Escaped, handle: &StoppableProcess) -> io::Result<()> {
        if is_gate(&process.image) {
            self.stop_gate_job(process);
        }
        // A process found ending already counts as terminated.
        handle.terminate(self.status).map(|_| ())
    }

    /// Terminates the job of the gate `gate`, named as `gate stop-tree` names it; a gate with no such job has none.
    fn stop_gate_job(&mut self, gate: &Escaped) {
        let name = self.reach.root.object_name(KernelObject::Job { gate_pid: gate.node.pid });
        let error = match OpenedJob::open(&name) {
            Ok(Some(job)) => job
                .terminate(self.status)
                .err()
                .map(|error| failure(&format!("TerminateJobObject({name})"), &error)),
            Ok(None) => None,
            Err(error) => Some(failure(&format!("OpenJobObjectW({name})"), &error)),
        };
        if let Some(error) = error {
            self.problems.push(could_not_end(gate, &error));
        }
    }

    /// Waits for each process of `issued`, with the error its terminate was refused with if it was, until the kill's
    /// deadline, which the first wait sets; the ones that ended with when each ended, and a line for each that still
    /// ran or could not be waited for.
    fn await_ended(&mut self, issued: Vec<(Escaped, StoppableProcess, Option<io::Error>)>) -> Vec<Ended> {
        let mut ended = Vec::new();
        for (process, handle, refused) in issued {
            let (deadline, left) = wait_left(self.deadline, self.wait, Instant::now());
            self.deadline = Some(deadline);
            match handle.wait(left) {
                // An end that cannot be read leaves the children found under it not judged.
                Ok(true) => ended.push(Ended {
                    exited: handle.exited().ok(),
                    process,
                }),
                Ok(false) => {
                    let why = match refused {
                        Some(error) => failure(&format!("TerminateProcess({})", process.node.pid), &error),
                        None => format!("it was still running {} s after it was terminated", self.wait.as_secs()),
                    };
                    self.problems.push(could_not_end(&process, &why));
                }
                Err(error) => self.problems.push(could_not_end(&process, &failure("WaitForSingleObject", &error))),
            }
        }
        ended
    }

    /// The children of the `ended` processes a new snapshot lists that are none of `seen`, the processes the first
    /// look found: what they started after it.
    fn started_since(&mut self, ended: &[Ended], seen: &HashSet<(u32, Option<Timestamp>)>) -> Vec<Escaped> {
        if ended.is_empty() {
            return Vec::new();
        }
        let snapshot = Win32ProcessTable.snapshot_without_command_lines();
        if snapshot.is_empty() {
            self.problems.push(could_not_look(EMPTY_SNAPSHOT));
            return Vec::new();
        }
        let late = children_since(ended, seen, &ProcessTree::new(snapshot), still_runs);
        self.unjudged.extend(late.unjudged);
        late.escaped
    }

    /// What the ending came to: the `ended` processes, and its problems with the line for those it could not judge.
    fn reached(self, ended: Vec<Escaped>) -> Reached {
        let mut problems = self.problems;
        if !self.unjudged.is_empty() {
            problems.push(could_not_judge(self.unjudged.len()));
        }
        Reached { ended, problems }
    }
}

/// The deadline of a kill's waits and how long a wait that begins at `now` may last: the `deadline` the kill's first
/// wait set, else `wait` from `now`; and what is left of it, zero once it has passed.
pub(crate) fn wait_left(deadline: Option<Instant>, wait: Duration, now: Instant) -> (Instant, Duration) {
    let deadline = deadline.unwrap_or(now + wait);
    (deadline, deadline.saturating_duration_since(now))
}

/// What one sample of a watch looks at for the processes outside the job: the job's members read before the snapshot
/// and after it, when the look began, the snapshot, and the members the job remembers that it no longer held after it.
pub(crate) struct Look<'a> {
    pub(crate) before: Vec<u32>,
    pub(crate) after: Vec<u32>,
    pub(crate) since: Timestamp,
    pub(crate) tree: &'a ProcessTree,
    pub(crate) departed: Vec<Departed>,
}

/// The processes outside the job a watch has recorded, parents first, as its last sample found them, each with the CPU
/// its last read found: what a kill of the run ends, though the process of the job that started them has ended.
#[derive(Debug, Default)]
pub(crate) struct Recorded {
    escaped: Vec<Escaped>,
    /// The last read of each recorded process, by its pid and start: its start as its own times give it, in 100 ns
    /// ticks since 1601, and the CPU time it had used, in 100 ns ticks.
    cpu: HashMap<(u32, Option<Timestamp>), (u64, u64)>,
}

impl Recorded {
    /// Records what the sample's `look` finds outside the job, in place of what was recorded: the descendants of the
    /// job's members, then each recorded process the snapshot still lists as itself, by its pid and start, with what it
    /// started, though the process that started it has ended; parents first. A recorded process the snapshot no longer
    /// lists as itself is dropped, and so is a process whose start or whose parent's is unknown, as one the watch
    /// cannot judge.
    pub(crate) fn record(&mut self, look: Look<'_>) {
        let members = members(look.before, &look.after, look.since, look.tree, look.departed);
        let mut walk = Walk::new(look.tree, &members.held);
        walk.members(&members);
        for process in &self.escaped {
            walk.recorded_root(process);
        }
        self.escaped = walk.escaped;
        let kept: HashSet<(u32, Option<Timestamp>)> = self.escaped.iter().map(Escaped::key).collect();
        self.cpu.retain(|key, _| kept.contains(key));
    }

    /// The CPU time of each recorded process, in 100 ns ticks, by its pid and its start as `times` reads it (100 ns
    /// ticks since 1601). One whose CPU `times` cannot read now as the process recorded (it has exited since the
    /// snapshot, cannot be read, or another process has taken its pid) keeps the CPU its last read found, so a read
    /// that fails once neither drops it nor has the next read count it anew; one no read has found yet is left out.
    pub(crate) fn cpu(&mut self, times: impl Fn(u32) -> io::Result<Option<ProcessTimes>>) -> BTreeMap<(u32, u64), u64> {
        let mut cpu = BTreeMap::new();
        for process in &self.escaped {
            if let Ok(Some(read)) = times(process.node.pid) {
                let started = i64::try_from(read.created).ok().and_then(filetime_to_timestamp);
                if started.is_some() && started == process.node.created {
                    self.cpu.insert(process.key(), (read.created, read.cpu));
                }
            }
            if let Some(&(created, used)) = self.cpu.get(&process.key()) {
                cpu.insert((process.node.pid, created), used);
            }
        }
        cpu
    }

    /// The snapshot `tree`'s entry of each recorded process it lists, parents first.
    pub(crate) fn processes<'t>(&self, tree: &'t ProcessTree) -> Vec<&'t ProcessInfo> {
        self.escaped
            .iter()
            .filter_map(|process| tree.get(process.node.pid).filter(|info| info.created == process.node.created))
            .collect()
    }
}

/// One walk down a snapshot for the processes outside the job: what it has passed and found so far.
struct Walk<'t> {
    tree: &'t ProcessTree,
    /// The snapshot's children of each process, indexed once for every root the walk goes below.
    children: Children<'t>,
    /// The job's members, never outside it.
    held: HashSet<u32>,
    seen: HashSet<u32>,
    /// The processes outside the job found, parents first.
    escaped: Vec<Escaped>,
}

impl<'t> Walk<'t> {
    /// A walk down `tree` that has found nothing yet, past the members `held`.
    fn new(tree: &'t ProcessTree, held: &[u32]) -> Self {
        Walk {
            tree,
            children: tree.children(),
            held: held.iter().copied().collect(),
            seen: HashSet::new(),
            escaped: Vec::new(),
        }
    }

    /// Takes the descendants of each of `members`' roots, then the children of each departed member, as
    /// [`departed_children`] finds them, each with its descendants.
    fn members(&mut self, members: &Members) {
        for member in members.roots() {
            self.under(member);
        }
        for child in departed_children(self.tree, &members.departed) {
            self.judge(child);
            self.under(child.pid);
        }
    }

    /// Takes the descendants of `root` breadth first, so a process's parent has been judged before it.
    fn under(&mut self, root: u32) {
        for process in self.children.descendants(root) {
            self.judge(process);
        }
    }

    /// Takes `process` as outside the job; a member, or a process the walk has passed already, is left as it is.
    fn judge(&mut self, process: &ProcessInfo) {
        if self.held.contains(&process.pid) || !self.seen.insert(process.pid) {
            return;
        }
        self.escaped.push(Escaped::of(process));
    }

    /// Takes the recorded `process` and its descendants while the snapshot lists it as itself, by its pid and start; one
    /// the walk has passed already was taken with its descendants.
    fn recorded_root(&mut self, process: &Escaped) {
        let alive = process.node.created.is_some() && self.tree.get(process.node.pid).is_some_and(|info| info.created == process.node.created);
        if !alive || self.held.contains(&process.node.pid) || !self.seen.insert(process.node.pid) {
            return;
        }
        self.escaped.push(process.clone());
        self.under(process.node.pid);
    }
}

/// Now, on the clock and in the units the snapshot gives start times in: the system time in whole microseconds;
/// `None` when outside the range a timestamp holds.
pub(crate) fn now() -> Option<Timestamp> {
    i64::try_from(file_time_now()).ok().and_then(filetime_to_timestamp)
}

/// Whether a process has the pid `pid` and still runs, so that one not judged is left running: false when none has it
/// or the one that has it has ended, true when that cannot be told.
fn still_runs(pid: u32) -> bool {
    match WaitableProcess::open(pid) {
        Ok(Some(process)) => !matches!(process.wait(Duration::ZERO), Ok(true)),
        Ok(None) => false,
        // It exists but cannot be opened: it may run.
        Err(_) => true,
    }
}

/// The job's members, read `before` the snapshot `tree` and `after` it, the look having begun at `since`, and those it
/// remembers that had `departed` it after it.
fn members(before: Vec<u32>, after: &[u32], since: Timestamp, tree: &ProcessTree, departed: Vec<Departed>) -> Members {
    let started_since = |pid: u32| tree.get(pid).and_then(|process| process.created).is_some_and(|created| created > since);
    let reused = before.iter().copied().filter(|pid| !after.contains(pid) && started_since(*pid)).collect();
    let mut held = before;
    for &pid in after {
        if !held.contains(&pid) {
            held.push(pid);
        }
    }
    Members { held, reused, departed }
}

/// The processes `tree` lists as children of the `departed` members, by the parent pid they name, members in order: each
/// started from its member's `from` to its `until`, both included; and while `tree` lists another process under the
/// member's pid, one that took it since, only those that started before that process did, and none when its start is
/// unknown.
pub(crate) fn departed_children<'t>(tree: &'t ProcessTree, departed: &[Departed]) -> Vec<&'t ProcessInfo> {
    let pids: HashSet<u32> = departed.iter().map(|member| member.pid).collect();
    let mut by_parent: HashMap<u32, Vec<&'t ProcessInfo>> = HashMap::new();
    for process in tree
        .iter()
        .filter(|process| process.pid != process.parent_pid && pids.contains(&process.parent_pid))
    {
        by_parent.entry(process.parent_pid).or_default().push(process);
    }
    let mut found = Vec::new();
    for member in departed {
        let holder = tree.get(member.pid);
        let started = by_parent.get(&member.pid).into_iter().flatten();
        found.extend(started.filter(|child| started_under(member, holder, child.created)).copied());
    }
    found
}

/// Whether a process started at `created` that names the departed `member`'s pid as its parent's is a child it
/// started: from its `from` to its `until`, and, while `holder` has the pid now, before the holder started, unless the
/// holder is the member itself ([`is_itself`]).
fn started_under(member: &Departed, holder: Option<&ProcessInfo>, created: Option<Timestamp>) -> bool {
    let Some(created) = created else {
        return false;
    };
    let before_holder = holder.is_none_or(|holder| is_itself(member, holder) || holder.created.is_some_and(|held| created < held));
    (member.from..=member.until).contains(&created) && before_holder
}

/// Whether `holder`, the process that has the departed `member`'s pid now, is the member itself: started at its start.
fn is_itself(member: &Departed, holder: &ProcessInfo) -> bool {
    holder.created == Some(member.start)
}

impl Members {
    /// The members whose descendants a walk takes: every one but those whose pid another process took.
    fn roots(&self) -> Vec<u32> {
        self.held.iter().copied().filter(|pid| !self.reused.contains(pid)).collect()
    }
}

/// What two looks found together: `earlier`'s processes, then those of `later` it did not find, each told by its pid
/// and start; the processes neither could judge; and each look's problem once.
fn union(earlier: Found, later: Found) -> Found {
    let mut found = earlier;
    let known: HashSet<(u32, Option<Timestamp>)> = found.escaped.iter().map(Escaped::key).collect();
    found
        .escaped
        .extend(later.escaped.into_iter().filter(|process| !known.contains(&process.key())));
    found.unjudged.extend(later.unjudged);
    for problem in later.problems {
        if !found.problems.contains(&problem) {
            found.problems.push(problem);
        }
    }
    found
}

/// The descendants in `tree` of the job's `members`, each once, parents first: the members left out; nothing is taken
/// from under a member whose pid another process took. Each member's descendants come breadth first, so a process's
/// parent has been judged before it. A process under a member or one found whose own start or whose parent's is
/// unknown cannot be judged, and is counted while `running` says a process still has its pid. Each of the `earlier`
/// processes the snapshot still lists as itself, by its pid and start, follows with its descendants, as a member's are
/// taken, though the process that started it has ended.
fn outside(members: &Members, earlier: &[Escaped], tree: &ProcessTree, running: impl Fn(u32) -> bool) -> Found {
    let roots = members.roots();
    let mut walk = Walk::new(tree, &members.held);
    walk.members(members);
    for process in earlier {
        walk.recorded_root(process);
    }
    let Walk { held, seen, escaped, .. } = walk;
    // Under an ended member's pid, only while nothing but the member itself holds it: a stranger's children are not
    // the run's.
    let departed = members
        .departed
        .iter()
        .filter(|member| tree.get(member.pid).is_none_or(|holder| is_itself(member, holder)))
        .map(|member| member.pid);
    let parents: HashSet<u32> = roots
        .iter()
        .copied()
        .chain(departed)
        .chain(escaped.iter().map(|process| process.node.pid))
        .collect();
    let unjudged = tree
        .iter()
        .filter(|process| process.pid != process.parent_pid && parents.contains(&process.parent_pid))
        .filter(|process| !held.contains(&process.pid) && !seen.contains(&process.pid) && unknown_start(tree, process))
        .map(|process| process.pid)
        .filter(|&pid| running(pid))
        .collect();
    Found {
        escaped,
        unjudged,
        ..Found::default()
    }
}

/// Whether `process`'s start or its parent's is unknown in `tree`, so whether it is its parent's child cannot be told.
fn unknown_start(tree: &ProcessTree, process: &ProcessInfo) -> bool {
    process.created.is_none() || tree.get(process.parent_pid).is_some_and(|parent| parent.created.is_none())
}

/// The children in `tree` of the processes `ended` that are none of `seen` and started while their parent ran; one of
/// which that cannot be told is counted as not judged while `running` says a process still has its pid.
fn children_since(ended: &[Ended], seen: &HashSet<(u32, Option<Timestamp>)>, tree: &ProcessTree, running: impl Fn(u32) -> bool) -> Found {
    let mut found = Found::default();
    for process in tree.iter().filter(|process| process.pid != process.parent_pid) {
        let Some(parent) = ended.iter().find(|parent| parent.process.node.pid == process.parent_pid) else {
            continue;
        };
        if seen.contains(&(process.pid, process.created)) {
            continue;
        }
        match kinship(process.created, parent) {
            Kin::Child => found.escaped.push(Escaped::of(process)),
            Kin::Unknown if running(process.pid) => {
                found.unjudged.insert(process.pid);
            }
            Kin::Other | Kin::Unknown => {}
        }
    }
    found
}

/// Whether a process started at `created` that names the pid of `parent` as its parent's is a child `parent` started:
/// it started while `parent` ran, at or after its start and at or before its end. One older than it, or started after
/// it ended, was started by another holder of the pid; without both starts and the end, which it is cannot be told.
fn kinship(created: Option<Timestamp>, parent: &Ended) -> Kin {
    let started = parent.process.node.created;
    let older = created.zip(started).is_some_and(|(created, started)| created < started);
    let later = created.zip(parent.exited).is_some_and(|(created, exited)| created > exited);
    if older || later {
        Kin::Other
    } else if created.is_none() || started.is_none() || parent.exited.is_none() {
        Kin::Unknown
    } else {
        Kin::Child
    }
}

/// Whether `image` is a gate's ([`is_image`]).
pub(crate) fn is_gate(image: &str) -> bool {
    is_image(image, GATE_IMAGE)
}

/// Whether the image name `image` is `wanted`, compared ASCII case-insensitively: the comparison of a gate's image
/// ([`is_gate`]) and of a console host's at a run's end, which spares both.
pub(crate) fn is_image(image: &str, wanted: &str) -> bool {
    image.eq_ignore_ascii_case(wanted)
}

/// The kill's line: the job's `processes` process(es) it terminated, and those of `ended` outside the job, up to
/// [`NAMED`] of them named, each image escaped.
pub(crate) fn terminated_line(processes: u32, ended: &[Escaped]) -> String {
    if ended.is_empty() {
        return format!("gate: terminated the job's {processes} process(es)");
    }
    let named: Vec<String> = ended
        .iter()
        .take(NAMED)
        .map(|process| format!("{} {}", shown(&process.image), process.node.pid))
        .collect();
    let more = ended.len() - named.len();
    let list = if more == 0 {
        named.join(", ")
    } else {
        format!("{}, and {more} more", named.join(", "))
    };
    format!(
        "gate: terminated the job's {processes} process(es) and {} that left it ({list})",
        ended.len()
    )
}

/// The line for a process outside the job that could not be ended, its image escaped.
fn could_not_end(process: &Escaped, error: &str) -> String {
    format!(
        "gate: could not end {} {}, which left the job: {error}",
        shown(&process.image),
        process.node.pid
    )
}

/// The line for a kill that could not look for the processes that left the job.
fn could_not_look(error: &str) -> String {
    format!("gate: could not look for processes that left the job: {error}")
}

/// The line for the `count` processes under the job a kill could not judge.
fn could_not_judge(count: usize) -> String {
    format!("gate: could not judge {count} process(es) under the job; they were left running")
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::io;
    use std::time::{Duration, Instant};

    use gate_watchdog::measure_cpu;
    use gate_win32::ProcessTimes;
    use jiff::Timestamp;
    use proc_table::{ProcessInfo, ProcessTree};

    use super::{
        Departed, EMPTY_SNAPSHOT, Ended, Escaped, Found, Look, Members, Recorded, children_since, could_not_end, could_not_judge, could_not_look,
        forgettable, is_gate, kill_look, members, memory_line, outside, terminated_line, union, wait_left,
    };
    use crate::stopping::Node;

    /// The test moment `second` seconds past 10:00 on the test day.
    fn at(second: u32) -> Timestamp {
        format!("2026-10-01T10:00:0{second}Z").parse().expect("a test timestamp parses")
    }

    /// The members `pids`, none of whose pids another process took, and none remembered that has ended.
    fn held(pids: &[u32]) -> Members {
        Members {
            held: pids.to_vec(),
            reused: HashSet::new(),
            departed: Vec::new(),
        }
    }

    /// The member `pid` the job remembers, which started in the job at second `start` and can have started a process
    /// until second `until`.
    fn gone(pid: u32, start: u32, until: u32) -> Departed {
        Departed {
            pid,
            start: at(start),
            from: at(start),
            until: at(until),
        }
    }

    #[test]
    fn a_look_forgets_only_the_members_that_had_ended_before_it_began() {
        // The look began at second 5: member 2 ended at second 3, before it; member 4 at second 7, after it, and can have
        // started a process after the snapshot; member 6 at second 5, as the look began, and can have too.
        let walked = [gone(2, 1, 3), gone(4, 6, 7), gone(6, 4, 5)];
        let tree = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0)]);

        assert_eq!(
            forgettable(&walked, at(5), &tree),
            vec![(2, at(1), at(3))],
            "members 4 and 6 are kept for the next look"
        );
    }

    #[test]
    fn a_look_whose_snapshot_listed_nothing_forgets_no_member() {
        let walked = [gone(2, 1, 3)];

        assert_eq!(
            forgettable(&walked, at(5), &ProcessTree::new(Vec::new())),
            Vec::new(),
            "a failed snapshot took nothing under the member"
        );
    }

    #[test]
    fn a_child_started_as_its_member_exited_is_taken() {
        // The launcher 2 ran from second 1 to second 8; 3 started in the tick of its exit, 4 a tick after.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 8),
            process(4, 2, "later.exe", 9),
        ]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 1, 8)], &tree);

        assert_eq!(found, vec![3], "the exit is a moment the member reached");
        assert_eq!(recorded, vec![3]);
    }

    #[test]
    fn a_member_put_in_the_job_after_its_start_takes_only_what_it_started_since_the_job_was_created() {
        // The launcher 2 started at second 1 and was put in the job created at second 3; the snapshot still lists it as
        // itself. It started 3 at second 2, before the job, and 4 at second 4.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "fake-tool.exe", 1),
            process(3, 2, "older.exe", 2),
            process(4, 2, "pwsh.exe", 4),
        ]);
        let joined = Departed {
            from: at(3),
            ..gone(2, 1, 8)
        };

        let (found, recorded) = found_and_recorded(&[1], &[joined], &tree);

        assert_eq!(found, vec![4], "nothing from before the job; the holder is told by its start");
        assert_eq!(recorded, vec![4]);
    }

    #[test]
    fn a_member_listed_before_the_snapshot_that_ended_has_its_child_reached_by_the_kill_s_look() {
        // Member 2, listed by the read before the snapshot, started 3 at second 4 and ended at second 5; its pid went to a
        // process outside the job that started at second 6, after the look began.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "other.exe", 6),
            process(3, 2, "pwsh.exe", 4),
        ]);
        let look = Look {
            before: vec![1, 2],
            after: vec![1],
            since: at(5),
            tree: &tree,
            departed: vec![gone(2, 1, 5)],
        };

        let found = kill_look(look, None, &[], all_run);

        assert_eq!(pids(&found), vec![3], "the job's memory reaches what it started");
    }

    #[test]
    fn a_kill_s_look_goes_on_with_the_members_the_job_remembers_when_its_memory_failed() {
        let tree = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0), process(3, 2, "pwsh.exe", 2)]);
        let look = Look {
            departed: vec![gone(2, 1, 8)],
            ..look(&[1], &tree)
        };

        let found = kill_look(look, Some("its drain thread ended on a panic"), &[], all_run);

        assert_eq!(pids(&found), vec![3], "the members it has are still looked under");
        assert_eq!(
            found.problems,
            vec![could_not_look("the job's memory failed: its drain thread ended on a panic")]
        );
    }

    #[test]
    fn a_watch_says_the_job_s_memory_failed_once() {
        let mut said = None;

        assert_eq!(memory_line(&mut said, None), None, "nothing to say");
        assert!(
            memory_line(&mut said, Some("refused")).is_some_and(|line| line.ends_with(": refused")),
            "said"
        );
        assert_eq!(memory_line(&mut said, Some("refused")), None, "said once, and the watch goes on");
    }

    /// What a kill finds outside the job, whose reads list `held` and which remembers `departed`, at `tree`, and what a
    /// sample records there.
    fn found_and_recorded(held: &[u32], departed: &[Departed], tree: &ProcessTree) -> (Vec<u32>, Vec<u32>) {
        let members = members(held.to_vec(), held, at(9), tree, departed.to_vec());
        let found = outside(&members, &[], tree, all_run);
        let mut recorded = Recorded::default();
        let look = Look {
            departed: departed.to_vec(),
            ..look(held, tree)
        };
        recorded.record(look);
        (pids(&found), recorded_pids(&recorded))
    }

    #[test]
    fn a_child_outside_the_job_of_a_remembered_member_that_has_ended_is_found() {
        // Member 1 runs; the launcher 2 joined the job at second 1, started 3 outside it and ended before any look.
        let tree = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0), process(3, 2, "pwsh.exe", 2)]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 1, 8)], &tree);

        assert_eq!(found, vec![3], "the kill reaches the child of the ended launcher");
        assert_eq!(recorded, vec![3], "a sample records it");
    }

    #[test]
    fn a_process_under_a_remembered_member_s_pid_that_it_cannot_have_started_is_not_taken() {
        // The launcher 2 ran from second 3 to second 8; 4 names pid 2 as its parent but is older, so an earlier holder of
        // the pid started it.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 4),
            process(4, 2, "older.exe", 2),
        ]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 3, 8)], &tree);

        assert_eq!(found, vec![3], "only what started while the launcher held its pid");
        assert_eq!(recorded, vec![3]);
    }

    #[test]
    fn a_process_under_a_remembered_member_s_pid_started_after_its_end_is_not_taken() {
        // The launcher 2 ran from second 1 to second 4; then a process that has ended since took its pid and started 5.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 2),
            process(5, 2, "later.exe", 6),
        ]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 1, 4)], &tree);

        assert_eq!(found, vec![3], "nothing started after the launcher's end");
        assert_eq!(recorded, vec![3]);
    }

    #[test]
    fn the_children_of_a_process_outside_the_job_that_took_a_remembered_member_s_pid_are_not_taken() {
        // The launcher 2 ran from second 1 to second 8, having started 3; pid 2 went to a process outside the job, started at
        // second 6, which started 5.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "other.exe", 6),
            process(3, 2, "pwsh.exe", 2),
            process(5, 2, "later.exe", 7),
        ]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 1, 8)], &tree);

        assert_eq!(found, vec![3], "not what the pid's new holder started");
        assert_eq!(recorded, vec![3]);
    }

    #[test]
    fn a_remembered_member_the_snapshot_still_lists_as_itself_has_its_children_taken() {
        // The launcher 2, started at second 1, ended after the snapshot, which lists it with the start it was remembered
        // by, and its child 3.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "fake-tool.exe", 1),
            process(3, 2, "pwsh.exe", 2),
        ]);

        let (found, recorded) = found_and_recorded(&[1], &[gone(2, 1, 8)], &tree);

        assert_eq!(found, vec![3], "the holder of the pid is the launcher itself");
        assert_eq!(recorded, vec![3]);
    }

    #[test]
    fn a_child_of_a_remembered_member_whose_start_is_unknown_is_counted_as_not_judged() {
        let tree = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0), unknown_start(3, 2, "pwsh.exe")]);
        let members = members(vec![1], &[1], at(9), &tree, vec![gone(2, 1, 8)]);

        let found = outside(&members, &[], &tree, all_run);

        assert_eq!(pids(&found), Vec::<u32>::new());
        assert_eq!(found.unjudged, HashSet::from([3]), "as a live member's child is");
    }

    #[test]
    fn a_child_whose_start_is_unknown_of_a_stranger_holding_a_remembered_member_s_pid_is_not_counted() {
        // Member 2 ended; pid 2 went to a process outside the job, started at second 6, whose child 5's start is unknown.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "other.exe", 6),
            unknown_start(5, 2, "later.exe"),
        ]);
        let members = members(vec![1], &[1], at(9), &tree, vec![gone(2, 1, 8)]);

        let found = outside(&members, &[], &tree, all_run);

        assert_eq!(found.unjudged, HashSet::new(), "the stranger's child is no process of the run");
    }

    #[test]
    fn a_child_whose_start_is_unknown_of_a_remembered_member_the_snapshot_lists_as_itself_is_counted() {
        // Member 2, started at second 1, ended after the snapshot, which lists it with that start and its child 5.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 0, "fake-tool.exe", 1),
            unknown_start(5, 2, "pwsh.exe"),
        ]);
        let members = members(vec![1], &[1], at(9), &tree, vec![gone(2, 1, 8)]);

        let found = outside(&members, &[], &tree, all_run);

        assert_eq!(found.unjudged, HashSet::from([5]), "the holder is the member itself");
    }

    /// `process`, ended at second `exited`, or with an end that could not be read.
    fn ended(process: &ProcessInfo, exited: Option<u32>) -> Ended {
        Ended {
            process: Escaped::of(process),
            exited: exited.map(at),
        }
    }

    /// Every process still runs.
    fn all_run(_pid: u32) -> bool {
        true
    }

    #[test]
    fn an_escapee_whose_cpu_one_sample_cannot_read_is_neither_counted_twice_nor_progress_after() {
        // Member 1 started 2 outside the job; its CPU reads 5 000 ticks, then cannot be read, then reads 7 000.
        let tree = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0), process(2, 1, "pwsh.exe", 1)]);
        let mut recorded = Recorded::default();
        let samples: Vec<BTreeMap<(u32, u64), u64>> = [Some(5_000), None, Some(7_000)]
            .into_iter()
            .map(|cpu: Option<u64>| {
                record(&mut recorded, &[1], &tree);
                recorded.cpu(|_| match cpu {
                    Some(cpu) => Ok(Some(ProcessTimes { created: filetime(1), cpu })),
                    None => Err(io::Error::other("the times cannot be read")),
                })
            })
            .collect();

        assert_eq!(
            samples[1],
            BTreeMap::from([((2, filetime(1)), 5_000)]),
            "the missed read keeps the last one"
        );
        let third = measure_cpu(&samples[1], &samples[2]);
        assert!(!third.appeared, "the third sample has it already: no new process");
        assert_eq!(third.gained, 2_000, "only what it used since the read before the missed one");
    }

    fn escaped(image: &str, pid: u32) -> Escaped {
        Escaped {
            node: Node {
                pid,
                parent: 1,
                created: None,
            },
            image: image.to_owned(),
        }
    }

    fn process(pid: u32, parent_pid: u32, name: &str, second: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: name.to_owned(),
            command_line: None,
            created: Some(at(second)),
        }
    }

    fn unknown_start(pid: u32, parent_pid: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            created: None,
            ..process(pid, parent_pid, name, 0)
        }
    }

    fn pids(found: &Found) -> Vec<u32> {
        found.escaped.iter().map(|process| process.node.pid).collect()
    }

    /// The `FILETIME` ticks of the test moment `second`, as a process's own times give its start.
    fn filetime(second: u32) -> u64 {
        u64::try_from(at(second).as_microsecond() * 10 + 116_444_736_000_000_000).expect("a test moment is after 1601")
    }

    /// The look of a sample that began at second 9, whose two reads of the job both listed `members`, at `tree`.
    fn look<'a>(members: &[u32], tree: &'a ProcessTree) -> Look<'a> {
        Look {
            before: members.to_vec(),
            after: members.to_vec(),
            since: at(9),
            tree,
            departed: Vec::new(),
        }
    }

    /// Records the look of a sample at `tree` whose job holds `members`.
    fn record(recorded: &mut Recorded, members: &[u32], tree: &ProcessTree) {
        recorded.record(look(members, tree));
    }

    fn recorded_pids(recorded: &Recorded) -> Vec<u32> {
        recorded.escaped.iter().map(|process| process.node.pid).collect()
    }

    #[test]
    fn a_recorded_escapee_whose_launcher_has_left_is_still_a_root_and_what_it_starts_is_recorded() {
        // Member 1 started a launcher (2) in the job, which started 3 outside it; then the launcher ended, 3 started 4,
        // and 4 started 5.
        let first = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "fake-tool.exe", 1),
            process(3, 2, "pwsh.exe", 2),
        ]);
        let later = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 2),
            process(4, 3, "fake-tool.exe", 3),
            process(5, 4, "fake-tool.exe", 4),
        ]);
        let mut recorded = Recorded::default();

        record(&mut recorded, &[1, 2], &first);
        assert_eq!(recorded_pids(&recorded), vec![3], "the launcher's child outside the job");
        record(&mut recorded, &[1], &later);
        assert_eq!(
            recorded_pids(&recorded),
            vec![3, 4, 5],
            "the escapee whose launcher left, and what it started since, parents first"
        );
    }

    #[test]
    fn a_record_the_snapshot_no_longer_lists_as_itself_is_dropped() {
        // The launcher (2) started 3 and 4 outside the job and ended; then 3 ended, and pid 4 went to a process that
        // started later.
        let first = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "fake-tool.exe", 1),
            process(3, 2, "pwsh.exe", 2),
            process(4, 2, "pwsh.exe", 2),
        ]);
        let later = ProcessTree::new(vec![process(1, 0, "cmd.exe", 0), process(4, 2, "other.exe", 5)]);
        let mut recorded = Recorded::default();

        record(&mut recorded, &[1, 2], &first);
        assert_eq!(recorded_pids(&recorded), vec![3, 4]);
        record(&mut recorded, &[1], &later);
        assert_eq!(
            recorded_pids(&recorded),
            Vec::<u32>::new(),
            "neither the ended one nor the pid's new holder"
        );
    }

    #[test]
    fn what_the_members_start_comes_first_and_every_recorded_process_follows_its_parent() {
        let first = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "fake-tool.exe", 1),
            process(3, 2, "pwsh.exe", 2),
            process(4, 3, "fake-tool.exe", 3),
        ]);
        // The launcher has ended; member 1 started 6 outside the job, which started 7, and 4 started 5.
        let later = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 2),
            process(4, 3, "fake-tool.exe", 3),
            process(5, 4, "fake-tool.exe", 5),
            process(6, 1, "pwsh.exe", 5),
            process(7, 6, "fake-tool.exe", 6),
        ]);
        let mut recorded = Recorded::default();

        record(&mut recorded, &[1, 2], &first);
        assert_eq!(recorded_pids(&recorded), vec![3, 4]);
        record(&mut recorded, &[1], &later);

        assert_eq!(recorded_pids(&recorded), vec![6, 7, 3, 4, 5]);
    }

    #[test]
    fn a_kill_goes_below_a_recorded_escapee_whose_launcher_has_ended_as_below_a_member() {
        // The launcher (2) started 3 outside the job and ended; after the last sample recorded 3, it started 4, which
        // started 5.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(3, 2, "pwsh.exe", 2),
            process(4, 3, "fake-tool.exe", 3),
            process(5, 4, "fake-tool.exe", 4),
        ]);
        let earlier = Found {
            escaped: vec![Escaped::of(&process(3, 2, "pwsh.exe", 2))],
            ..Found::default()
        };

        let now = outside(&held(&[1]), &earlier.escaped, &tree, all_run);
        let found = union(earlier, now);

        assert_eq!(
            pids(&found),
            vec![3, 4, 5],
            "the recorded escapee, its child and its grandchild, parents first"
        );
    }

    #[test]
    fn a_child_that_joined_the_job_between_the_two_reads_is_a_member() {
        // Member 1 started 2, which joined the job after the first read of its members, and 3, which left it.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "fake-tool.exe", 1),
            process(3, 1, "pwsh.exe", 1),
        ]);

        let members = members(vec![1], &[1, 2], at(9), &tree, Vec::new());
        let found = outside(&members, &[], &tree, all_run);

        assert_eq!(
            pids(&found),
            vec![3],
            "the child read in the job after the snapshot is no escaped process"
        );
    }

    #[test]
    fn a_process_under_the_job_whose_start_is_unknown_is_counted_as_not_judged() {
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "pwsh.exe", 1),
            unknown_start(3, 1, "pwsh.exe"),
            unknown_start(4, 2, "fake-tool.exe"),
            unknown_start(5, 9, "other.exe"),
        ]);

        let found = outside(&held(&[1]), &[], &tree, all_run);

        assert_eq!(pids(&found), vec![2]);
        assert_eq!(found.unjudged, HashSet::from([3, 4]), "not a process under no member or escaped one");
    }

    #[test]
    fn a_member_read_only_before_the_snapshot_whose_pid_a_later_process_took_has_nothing_taken_from_under_it() {
        // The look began at second 3. Member 1 runs throughout, with a packaged pwsh (2) outside the job. 4, 7 and 9
        // were members at the first read only: pid 4 now belongs to a process started at second 5, after the look
        // began, with two children (5, and 6 whose start is unknown); 7's start is unknown, with a child (8); 9 started
        // before the look began, so it is the member itself, with a child (10).
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "pwsh.exe", 1),
            process(4, 0, "other.exe", 5),
            process(5, 4, "child.exe", 6),
            unknown_start(6, 4, "child.exe"),
            unknown_start(7, 0, "cmd.exe"),
            process(8, 7, "pwsh.exe", 2),
            process(9, 0, "cmd.exe", 1),
            process(10, 9, "pwsh.exe", 2),
        ]);

        let members = members(vec![1, 4, 7, 9], &[1], at(3), &tree, Vec::new());
        let found = outside(&members, &[], &tree, all_run);

        assert_eq!(pids(&found), vec![2, 10], "nothing under the process that took member 4's pid");
        assert_eq!(
            found.unjudged,
            HashSet::from([8]),
            "the child of the member whose start is unknown, not one under the pid another process took"
        );
    }

    #[test]
    fn a_process_that_no_longer_runs_is_not_counted_as_not_judged() {
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            unknown_start(3, 1, "rustc.exe"),
            unknown_start(4, 1, "link.exe"),
        ]);
        let running = |pid: u32| pid != 4;

        let found = outside(&held(&[1]), &[], &tree, running);

        assert_eq!(found.unjudged, HashSet::from([3]), "the first look: only the one still running");

        let pwsh = ended(&process(2, 1, "pwsh.exe", 1), None);
        let tree = ProcessTree::new(vec![process(5, 2, "rustc.exe", 3), process(6, 2, "link.exe", 3)]);
        let running = |pid: u32| pid != 6;

        let found = children_since(&[pwsh], &HashSet::new(), &tree, running);

        assert_eq!(found.unjudged, HashSet::from([5]), "the second pass: only the one still running");
    }

    #[test]
    fn two_looks_together_keep_the_first_look_s_order_and_add_what_only_the_second_found() {
        let earlier = Found {
            escaped: vec![escaped("pwsh.exe", 2), escaped("fake-tool.exe", 3)],
            unjudged: HashSet::from([7]),
            problems: Vec::new(),
            departed: true,
        };
        let later = Found {
            escaped: vec![escaped("fake-tool.exe", 3), escaped("fake-tool.exe", 4)],
            unjudged: HashSet::from([7, 8]),
            problems: vec![could_not_look(EMPTY_SNAPSHOT)],
            departed: false,
        };

        let found = union(earlier, later);

        assert_eq!(pids(&found), vec![2, 3, 4]);
        assert_eq!(found.unjudged, HashSet::from([7, 8]));
        assert_eq!(found.problems, vec![could_not_look(EMPTY_SNAPSHOT)]);
        assert!(found.departed, "the first look's reach under the ended members is kept");
    }

    #[test]
    fn the_second_pass_takes_the_children_an_ended_process_started_since_the_first_look() {
        let pwsh = ended(&process(2, 1, "pwsh.exe", 1), Some(9));
        let known = Escaped::of(&process(3, 2, "fake-tool.exe", 2));
        let tree = ProcessTree::new(vec![
            process(3, 2, "fake-tool.exe", 2),
            process(5, 2, "fake-tool.exe", 3),
            process(6, 2, "older.exe", 0),
            unknown_start(7, 2, "unknown.exe"),
            process(10, 4, "elsewhere.exe", 3),
        ]);
        let seen = HashSet::from([known.key()]);

        let found = children_since(&[pwsh], &seen, &tree, all_run);

        assert_eq!(pids(&found), vec![5], "not the one already seen, an older one, nor another's child");
        assert_eq!(found.unjudged, HashSet::from([7]));
    }

    #[test]
    fn the_second_pass_takes_only_children_started_while_their_parent_ran() {
        // pwsh 2 ran from second 1 to second 5, and its pid went to another process since; pwsh 3's end could not be
        // read.
        let pwsh = ended(&process(2, 1, "pwsh.exe", 1), Some(5));
        let unread = ended(&process(3, 1, "pwsh.exe", 1), None);
        let tree = ProcessTree::new(vec![
            process(4, 2, "fake-tool.exe", 3),
            process(5, 2, "fake-tool.exe", 5),
            process(6, 2, "other.exe", 7),
            process(7, 3, "fake-tool.exe", 3),
        ]);

        let found = children_since(&[pwsh, unread], &HashSet::new(), &tree, all_run);

        assert_eq!(
            pids(&found),
            vec![4, 5],
            "started while it ran, up to its end; not one the pid's later holder started"
        );
        assert_eq!(found.unjudged, HashSet::from([7]), "the child of the one whose end is unknown");
    }

    #[test]
    fn the_second_pass_waits_only_for_what_is_left_of_the_first_pass_s_deadline() {
        let start = Instant::now();
        let wait = Duration::from_secs(5);

        let (deadline, first) = wait_left(None, wait, start);
        let (kept, second) = wait_left(Some(deadline), wait, start + Duration::from_secs(2));
        let (_, past) = wait_left(Some(deadline), wait, start + Duration::from_secs(6));

        assert_eq!((deadline, first), (start + wait, wait), "the first wait sets the deadline");
        assert_eq!(
            (kept, second),
            (deadline, Duration::from_secs(3)),
            "a later wait keeps it and waits what is left"
        );
        assert_eq!(past, Duration::ZERO, "none once it has passed");
    }

    #[test]
    fn a_gate_is_known_by_its_image() {
        assert!(is_gate("gate.exe"));
        assert!(is_gate("GATE.EXE"));
        assert!(!is_gate("gate-router.exe"));
        assert!(!is_gate("pwsh.exe"));
    }

    #[test]
    fn with_none_outside_the_job_the_line_is_the_job_s_alone() {
        assert_eq!(terminated_line(3, &[]), "gate: terminated the job's 3 process(es)");
    }

    #[test]
    fn one_outside_the_job_is_counted_and_named() {
        assert_eq!(
            terminated_line(1, &[escaped("pwsh.exe", 4100)]),
            "gate: terminated the job's 1 process(es) and 1 that left it (pwsh.exe 4100)"
        );
    }

    #[test]
    fn past_five_outside_the_job_the_rest_are_counted() {
        let ended: Vec<Escaped> = (1..=6).map(|n| escaped("fake-tool.exe", 100 + n)).collect();
        assert_eq!(
            terminated_line(2, &ended),
            concat!(
                "gate: terminated the job's 2 process(es) and 6 that left it (fake-tool.exe 101, fake-tool.exe 102, ",
                "fake-tool.exe 103, fake-tool.exe 104, fake-tool.exe 105, and 1 more)"
            )
        );
    }

    #[test]
    fn a_process_that_could_not_be_ended_has_its_own_line() {
        assert_eq!(
            could_not_end(
                &escaped("pwsh.exe", 4100),
                "TerminateProcess(4100) failed with Win32 error 5: Access is denied."
            ),
            "gate: could not end pwsh.exe 4100, which left the job: TerminateProcess(4100) failed with Win32 error 5: Access is denied."
        );
    }

    #[test]
    fn an_image_name_holding_a_control_character_or_a_bidi_mark_is_shown_escaped() {
        assert_eq!(
            terminated_line(1, &[escaped("pwsh\u{202e}exe.\n", 4100)]),
            r"gate: terminated the job's 1 process(es) and 1 that left it (pwsh\u{202e}exe.\n 4100)"
        );
        assert_eq!(
            could_not_end(&escaped("a\u{7}b.exe", 4100), "it was still running 5 s after it was terminated"),
            r"gate: could not end a\u{7}b.exe 4100, which left the job: it was still running 5 s after it was terminated"
        );
    }

    #[test]
    fn what_could_not_be_looked_at_or_judged_is_said() {
        assert_eq!(
            could_not_look(EMPTY_SNAPSHOT),
            "gate: could not look for processes that left the job: the process snapshot listed no processes"
        );
        assert_eq!(
            could_not_judge(2),
            "gate: could not judge 2 process(es) under the job; they were left running"
        );
    }
}

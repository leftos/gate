//! The processes that left a command's job: a packaged desktop app, such as the Store's pwsh, that a process of the job
//! without package identity starts runs in a job of its own, and so does what it starts. A kill looks for them under
//! the job's processes before the job is terminated, while the processes that started them still run, then terminates
//! the job and ends them parents first, each checked through its own handle to be the process the snapshot saw, and
//! makes one more pass for the children they started meanwhile.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use gate_state::{KernelObject, StateRoot};
use gate_win32::{Job, OpenedJob, StoppableProcess, WaitableProcess, file_time_now};
use jiff::Timestamp;
use proc_table::{ProcessInfo, ProcessTree, Win32ProcessTable, command_line_of, filetime_to_timestamp};

use crate::boxside::payload::escaped as shown;
use crate::servers;
use crate::stopping::{Node, Opened, failure, open_checked};

/// How many of the processes that left the job the kill's line names.
const NAMED: usize = 5;
/// A gate's image: one among the processes that left the job has its own command's job terminated before it is ended.
const GATE_IMAGE: &str = "gate.exe";
/// Why a look whose snapshot listed no process could not look: the snapshot itself failed.
const EMPTY_SNAPSHOT: &str = "the process snapshot listed no processes";
/// Why a look could not begin: the system time is outside the range a timestamp holds.
const NO_CLOCK: &str = "the system time is outside the range a timestamp holds";

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
}

/// What a kill reaches: the command's job, one more compiler server's image the kill leaves running
/// (`--test-server-image`), and the state root that names the job of a gate among the processes that left the job.
#[derive(Clone, Copy)]
pub(crate) struct Reach<'a> {
    pub(crate) job: &'a Job,
    pub(crate) server_image: Option<&'a str>,
    pub(crate) root: &'a StateRoot,
}

/// What looking for the processes that left the job found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The processes outside the job, parents first, the build servers and everything under them left out.
    escaped: Vec<Escaped>,
    /// The pids of the processes under the job whose start, or whose parent's, could not be read, so whether they are
    /// its children could not be told; they are left running.
    unjudged: HashSet<u32>,
    /// A line for each look that failed.
    problems: Vec<String>,
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
    /// Looks for the processes that left the job now; a look that fails says why in the [`Found`].
    pub(crate) fn look(&self) -> Found {
        self.find().unwrap_or_else(|problem| Found {
            problems: vec![could_not_look(&problem)],
            ..Found::default()
        })
    }

    /// The processes outside the job under the processes it holds, the members read before the snapshot and after
    /// it; nothing, and no snapshot taken, when neither of two reads lists a member. The snapshot has no command lines;
    /// the classification reads one only for a `dotnet` host.
    fn find(&self) -> Result<Found, String> {
        let listed = || self.job.process_ids().map_err(|error| failure("QueryInformationJobObject", &error));
        let since = now().ok_or_else(|| NO_CLOCK.to_owned())?;
        let before = listed()?;
        if before.is_empty() && listed()?.is_empty() {
            return Ok(Found::default());
        }
        let snapshot = Win32ProcessTable.snapshot_without_command_lines();
        if snapshot.is_empty() {
            return Err(EMPTY_SNAPSHOT.to_owned());
        }
        // The snapshot takes a while: a process that joined the job meanwhile is a member like any other.
        let after = listed()?;
        let tree = ProcessTree::new(snapshot);
        let members = members(before, &after, since, &tree);
        Ok(outside(
            &members,
            &tree,
            |process| spared(process, self.server_image, command_line_of),
            still_runs,
        ))
    }

    /// Terminates the job with `status`, then ends with `status` every process outside it that `earlier` found or a
    /// look now finds, parents first, each checked through its own handle: all are terminated before any is waited
    /// for, then all are waited for until `wait` has passed since the first wait began. One more look then ends the
    /// children the ended processes started while they ran since the first look, waited for only until that same
    /// deadline. A build server, shared with every other build on the machine, is left running with everything under
    /// it.
    ///
    /// # Errors
    ///
    /// The system's error when the job cannot be terminated; then nothing outside it is ended.
    pub(crate) fn terminate(&self, status: u32, wait: Duration, earlier: Found) -> io::Result<Reached> {
        let found = union(earlier, self.look());
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
        handle.terminate(self.status)
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
        let server_image = self.reach.server_image;
        let late = children_since(
            ended,
            seen,
            &ProcessTree::new(snapshot),
            |process| spared(process, server_image, command_line_of),
            still_runs,
        );
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
fn wait_left(deadline: Option<Instant>, wait: Duration, now: Instant) -> (Instant, Duration) {
    let deadline = deadline.unwrap_or(now + wait);
    (deadline, deadline.saturating_duration_since(now))
}

/// Now, on the clock and in the units the snapshot gives start times in: the system time in whole microseconds;
/// `None` when outside the range a timestamp holds.
fn now() -> Option<Timestamp> {
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

/// The job's members, read `before` the snapshot `tree` and `after` it, the look having begun at `since`.
fn members(before: Vec<u32>, after: &[u32], since: Timestamp, tree: &ProcessTree) -> Members {
    let started_since = |pid: u32| tree.get(pid).and_then(|process| process.created).is_some_and(|created| created > since);
    let reused = before.iter().copied().filter(|pid| !after.contains(pid) && started_since(*pid)).collect();
    let mut held = before;
    for &pid in after {
        if !held.contains(&pid) {
            held.push(pid);
        }
    }
    Members { held, reused }
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

/// The descendants in `tree` of the job's `members`, each once, parents first: the members left out, and so is each
/// process `spared` keeps running, with everything under it; nothing is taken from under a member whose pid another
/// process took. Each member's descendants come breadth first, so a process's parent has been judged before it. A
/// process under a member or one found whose own start or whose parent's is unknown cannot be judged, and is counted
/// while `running` says a process still has its pid.
fn outside(members: &Members, tree: &ProcessTree, spared: impl Fn(&ProcessInfo) -> bool, running: impl Fn(u32) -> bool) -> Found {
    let held: HashSet<u32> = members.held.iter().copied().collect();
    let roots: Vec<u32> = members.held.iter().copied().filter(|pid| !members.reused.contains(pid)).collect();
    let mut seen = HashSet::new();
    let mut kept = HashSet::new();
    let mut escaped = Vec::new();
    for &member in &roots {
        for process in tree.descendants(member) {
            if held.contains(&process.pid) || !seen.insert(process.pid) {
                continue;
            }
            if kept.contains(&process.parent_pid) || spared(process) {
                kept.insert(process.pid);
            } else {
                escaped.push(Escaped::of(process));
            }
        }
    }
    let parents: HashSet<u32> = roots.iter().copied().chain(escaped.iter().map(|process| process.node.pid)).collect();
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
        problems: Vec::new(),
    }
}

/// Whether `process`'s start or its parent's is unknown in `tree`, so whether it is its parent's child cannot be told.
fn unknown_start(tree: &ProcessTree, process: &ProcessInfo) -> bool {
    process.created.is_none() || tree.get(process.parent_pid).is_some_and(|parent| parent.created.is_none())
}

/// The children in `tree` of the processes `ended` that are none of `seen` and started while their parent ran, but
/// those `spared` keeps running; one of which that cannot be told is counted as not judged while `running` says a
/// process still has its pid.
fn children_since(
    ended: &[Ended],
    seen: &HashSet<(u32, Option<Timestamp>)>,
    tree: &ProcessTree,
    spared: impl Fn(&ProcessInfo) -> bool,
    running: impl Fn(u32) -> bool,
) -> Found {
    let mut found = Found::default();
    for process in tree.iter().filter(|process| process.pid != process.parent_pid) {
        let Some(parent) = ended.iter().find(|parent| parent.process.node.pid == process.parent_pid) else {
            continue;
        };
        if seen.contains(&(process.pid, process.created)) {
            continue;
        }
        match kinship(process.created, parent) {
            Kin::Child if !spared(process) => found.escaped.push(Escaped::of(process)),
            Kin::Unknown if running(process.pid) => {
                found.unjudged.insert(process.pid);
            }
            Kin::Child | Kin::Other | Kin::Unknown => {}
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

/// Whether `process` is left running with everything under it: a build server, shared with every other build on the
/// machine, or a `dotnet` host whose command line `read` cannot give, which may be one (an `MSBuild` node or a
/// `VBCSCompiler.dll` host). `server_image` is one more compiler server's image (`--test-server-image`).
fn spared(process: &ProcessInfo, server_image: Option<&str>, read: impl FnOnce(u32) -> Option<String>) -> bool {
    let mut unread = false;
    let server = servers::is_server(&process.name, server_image, || {
        read(process.pid).unwrap_or_else(|| {
            unread = true;
            String::new()
        })
    });
    server || unread
}

/// Whether `image` is a gate's.
fn is_gate(image: &str) -> bool {
    image.eq_ignore_ascii_case(GATE_IMAGE)
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
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use jiff::Timestamp;
    use proc_table::{ProcessInfo, ProcessTree};

    use super::{
        EMPTY_SNAPSHOT, Ended, Escaped, Found, Members, children_since, could_not_end, could_not_judge, could_not_look, is_gate, members, outside,
        spared, terminated_line, union, wait_left,
    };
    use crate::stopping::Node;

    /// The test moment `second` seconds past 10:00 on the test day.
    fn at(second: u32) -> Timestamp {
        format!("2026-10-01T10:00:0{second}Z").parse().expect("a test timestamp parses")
    }

    /// The members `pids`, none of whose pids another process took.
    fn held(pids: &[u32]) -> Members {
        Members {
            held: pids.to_vec(),
            reused: HashSet::new(),
        }
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

    /// No process is a server, and no command line can be read but the dotnet host's.
    fn none(_pid: u32) -> Option<String> {
        None
    }

    #[test]
    fn a_build_server_outside_the_job_is_left_running_with_everything_under_it() {
        // Member 1 started a packaged pwsh (2) with a child (3), a compiler server (4) with a child (5), an MSBuild
        // node (6) under the dotnet host, and a dotnet host whose command line cannot be read (7) with a child (8);
        // all outside the job.
        let tree = ProcessTree::new(vec![
            process(1, 0, "cmd.exe", 0),
            process(2, 1, "pwsh.exe", 1),
            process(3, 2, "fake-tool.exe", 2),
            process(4, 1, "VBCSCompiler.exe", 1),
            process(5, 4, "conhost.exe", 2),
            process(6, 1, "dotnet.exe", 1),
            process(7, 1, "dotnet.exe", 1),
            process(8, 7, "dotnet.exe", 2),
        ]);
        let line = |pid: u32| (pid == 6).then(|| r"dotnet C:\sdk\MSBuild.dll /nodemode:1 /nodeReuse:true".to_owned());

        let found = outside(&held(&[1]), &tree, |process| spared(process, None, line), all_run);

        assert_eq!(
            pids(&found),
            vec![2, 3],
            "the pwsh and its child, parents first; no build server, no unreadable dotnet host, nor what they started"
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

        let members = members(vec![1], &[1, 2], at(9), &tree);
        let found = outside(&members, &tree, |process| spared(process, None, none), all_run);

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

        let found = outside(&held(&[1]), &tree, |process| spared(process, None, none), all_run);

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

        let members = members(vec![1, 4, 7, 9], &[1], at(3), &tree);
        let found = outside(&members, &tree, |process| spared(process, None, none), all_run);

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

        let found = outside(&held(&[1]), &tree, |process| spared(process, None, none), running);

        assert_eq!(found.unjudged, HashSet::from([3]), "the first look: only the one still running");

        let pwsh = ended(&process(2, 1, "pwsh.exe", 1), None);
        let tree = ProcessTree::new(vec![process(5, 2, "rustc.exe", 3), process(6, 2, "link.exe", 3)]);
        let running = |pid: u32| pid != 6;

        let found = children_since(&[pwsh], &HashSet::new(), &tree, |process| spared(process, None, none), running);

        assert_eq!(found.unjudged, HashSet::from([5]), "the second pass: only the one still running");
    }

    #[test]
    fn two_looks_together_keep_the_first_look_s_order_and_add_what_only_the_second_found() {
        let earlier = Found {
            escaped: vec![escaped("pwsh.exe", 2), escaped("fake-tool.exe", 3)],
            unjudged: HashSet::from([7]),
            problems: Vec::new(),
        };
        let later = Found {
            escaped: vec![escaped("fake-tool.exe", 3), escaped("fake-tool.exe", 4)],
            unjudged: HashSet::from([7, 8]),
            problems: vec![could_not_look(EMPTY_SNAPSHOT)],
        };

        let found = union(earlier, later);

        assert_eq!(pids(&found), vec![2, 3, 4]);
        assert_eq!(found.unjudged, HashSet::from([7, 8]));
        assert_eq!(found.problems, vec![could_not_look(EMPTY_SNAPSHOT)]);
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
            process(9, 2, "VBCSCompiler.exe", 3),
            process(10, 4, "elsewhere.exe", 3),
        ]);
        let seen = HashSet::from([known.key()]);

        let found = children_since(&[pwsh], &seen, &tree, |process| spared(process, None, none), all_run);

        assert_eq!(
            pids(&found),
            vec![5],
            "not the one already seen, an older one, a server, nor another's child"
        );
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

        let found = children_since(&[pwsh, unread], &HashSet::new(), &tree, |process| spared(process, None, none), all_run);

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

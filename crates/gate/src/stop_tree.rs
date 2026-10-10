//! `gate stop-tree <pid>`: stops every gate under a process and then the process's tree. One snapshot gives the tree;
//! the job of each process in it that is a gate is terminated first, which ends the processes whose parent chain is
//! already broken, then each process of the tree still running, deepest first, except this one.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use gate_args::USAGE;
use gate_state::{KernelObject, StateRoot};
use gate_win32::{OpenedJob, StoppableProcess, win32_message};
use jiff::Timestamp;
use proc_table::{ProcessTable as _, ProcessTree, Win32ProcessTable};

use crate::answer::USAGE_STATUS;
use crate::console::Console;

/// The status every job and process this stops ends with.
const STOP_STATUS: u32 = 124;
/// How long, in all, the processes a terminated job held are given to end on their own, a job's termination being
/// asynchronous.
const JOB_END: Duration = Duration::from_secs(2);
/// How long a process whose termination failed is given to end before the failure counts.
const TERMINATE_CHECK: Duration = Duration::from_secs(1);

/// One process of the tree, as the snapshot saw it.
#[derive(Debug, PartialEq, Eq)]
struct Node {
    pid: u32,
    created: Option<Timestamp>,
}

/// What a stop did: the gate jobs it terminated, the processes it had to terminate itself, and why it could not stop
/// the rest.
#[derive(Default)]
struct Stopped {
    jobs: u32,
    processes: u32,
    failures: Vec<String>,
}

/// What the root's own process said before the snapshot was taken.
#[derive(Clone, Copy, Debug)]
enum RootState {
    /// No process has the pid, or the one that has it has ended.
    Ended,
    /// The process runs.
    Running,
    /// Its start could not be read, as for a process this one may not query.
    Unreadable,
}

/// The tree a stop works on, or why there is none.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// No live process has the pid.
    NotLive,
    /// The root runs, but the snapshot did not list it, as when the snapshot failed and came back empty.
    Unlisted,
    /// The root first, then its descendants breadth first, so that read backwards it is deepest first.
    Tree(Vec<Node>),
}

/// Stops the gates under `pid` and its tree, their jobs named under `root`, says the failures and the summary through
/// `console`, and returns the exit status: 0, 1 when anything could not be stopped, or the usage status when no live
/// process has `pid`.
pub(crate) fn stop_tree(pid: u32, root: &StateRoot, console: &mut Console) -> u8 {
    let mut stopped = Stopped::default();
    let tree = match find(pid) {
        Found::NotLive => {
            console.err(&format!("gate: stop-tree: no live process has the id {pid}"));
            console.err(USAGE);
            return USAGE_STATUS;
        }
        Found::Unlisted => {
            stopped.failures.push(format!("the process snapshot did not list {pid}"));
            Vec::new()
        }
        Found::Tree(tree) => tree,
    };
    let members = stop_jobs(&tree, root, &mut stopped);
    stop_processes(&tree, &members, &mut stopped);
    for failure in &stopped.failures {
        console.err(&format!("gate: could not stop: {failure}"));
    }
    console.out(&format!(
        "gate: stopped {} gate job(s) and {} process(es) under {pid}",
        stopped.jobs, stopped.processes
    ));
    u8::from(!stopped.failures.is_empty())
}

/// The tree under `pid` in one snapshot, read after the root's own process is asked whether it runs.
fn find(pid: u32) -> Found {
    let state = match gate_win32::process_start_of(pid) {
        Ok(None) => RootState::Ended,
        Ok(Some(_)) => RootState::Running,
        Err(_) => RootState::Unreadable,
    };
    if matches!(state, RootState::Ended) {
        return Found::NotLive;
    }
    found_in(pid, state, &ProcessTree::new(Win32ProcessTable.snapshot()))
}

/// The tree under `pid` in `snapshot`, given what the root's own process said. A process that has ended but whose
/// handle another process holds may still be listed, so `Ended` wins over the snapshot; a root the snapshot does not
/// list is no live process unless its process was seen running, when the snapshot is what failed.
fn found_in(pid: u32, state: RootState, snapshot: &ProcessTree) -> Found {
    let root = match (state, snapshot.get(pid)) {
        (RootState::Ended, _) | (RootState::Unreadable, None) => return Found::NotLive,
        (RootState::Running, None) => return Found::Unlisted,
        (RootState::Running | RootState::Unreadable, Some(root)) => root,
    };
    let nodes = std::iter::once(root).chain(snapshot.descendants(pid)).map(|process| Node {
        pid: process.pid,
        created: process.created,
    });
    Found::Tree(nodes.collect())
}

/// Terminates the job of every process of the tree that is a gate, and returns the ids of the processes those jobs
/// held. A process with no job of its name is no gate, and is passed over silently.
fn stop_jobs(tree: &[Node], root: &StateRoot, stopped: &mut Stopped) -> HashSet<u32> {
    let mut members = HashSet::new();
    for node in tree {
        let name = root.object_name(KernelObject::Job { gate_pid: node.pid });
        match OpenedJob::open(&name) {
            Ok(Some(job)) => stop_job(&job, &mut members, stopped),
            Ok(None) => {}
            Err(error) => stopped.failures.push(failure(&format!("OpenJobObjectW({name})"), &error)),
        }
    }
    members
}

fn stop_job(job: &OpenedJob, members: &mut HashSet<u32>, stopped: &mut Stopped) {
    match job.process_ids() {
        Ok(ids) => members.extend(ids),
        Err(error) => stopped.failures.push(format!(
            "listing the processes of {} failed: {}",
            job.name(),
            failure("QueryInformationJobObject", &error)
        )),
    }
    match job.terminate(STOP_STATUS) {
        Ok(()) => stopped.jobs += 1,
        Err(error) => stopped.failures.push(failure(&format!("TerminateJobObject({})", job.name()), &error)),
    }
}

/// Stops each process of the tree deepest first, except this one. The processes a terminated job held share
/// [`JOB_END`] to end on their own; any other is stopped at once.
fn stop_processes(tree: &[Node], members: &HashSet<u32>, stopped: &mut Stopped) {
    let me = std::process::id();
    let began = Instant::now();
    for node in tree.iter().rev().filter(|node| node.pid != me) {
        stop_process(node, grace(members.contains(&node.pid), began.elapsed()), stopped);
    }
}

/// How long a process is given to end on its own, `elapsed` after the processes began to be stopped: what is left of
/// [`JOB_END`] for a process a terminated job held, which all such processes share, and nothing for any other.
fn grace(member: bool, elapsed: Duration) -> Duration {
    if member { JOB_END.saturating_sub(elapsed) } else { Duration::ZERO }
}

/// Whether the process now behind a pid is the one the snapshot saw: both creation times known and equal, so a pid
/// that went to a process created after the snapshot is passed over.
fn same_process(snapshot: Option<Timestamp>, now: io::Result<Timestamp>) -> bool {
    snapshot.is_some() && now.ok() == snapshot
}

/// Terminates one process of the tree unless it has ended, ends within `grace`, or its pid now belongs to a process
/// created after the snapshot; only a process this had to terminate itself is counted. A failed termination counts as
/// a failure only when the process is still running [`TERMINATE_CHECK`] later.
fn stop_process(node: &Node, grace: Duration, stopped: &mut Stopped) {
    let process = match StoppableProcess::open(node.pid) {
        Ok(Some(process)) => process,
        Ok(None) => return,
        Err(error) => {
            stopped.failures.push(failure(&format!("OpenProcess({})", node.pid), &error));
            return;
        }
    };
    // A wait that fails has not seen the process end, so the process is terminated as one still running is.
    if !same_process(node.created, process.created()) || matches!(process.wait(grace), Ok(true)) {
        return;
    }
    match process.terminate(STOP_STATUS) {
        Ok(()) => stopped.processes += 1,
        Err(error) => {
            if !matches!(process.wait(TERMINATE_CHECK), Ok(true)) {
                stopped.failures.push(failure(&format!("TerminateProcess({})", node.pid), &error));
            }
        }
    }
}

/// `<operation> failed with Win32 error <code>: <message>`, as the PowerShell gate words a failed call.
fn failure(operation: &str, error: &io::Error) -> String {
    match error.raw_os_error().and_then(|code| u32::try_from(code).ok()) {
        Some(code) => format!("{operation} failed with Win32 error {code}: {}", win32_message(code)),
        None => format!("{operation} failed: {error}"),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use std::io;
    use std::time::Duration;

    use jiff::Timestamp;
    use proc_table::{ProcessInfo, ProcessTree};

    use super::{Found, JOB_END, Node, RootState, found_in, grace, same_process};

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("a test timestamp parses")
    }

    fn process(pid: u32, parent_pid: u32, created: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: format!("p{pid}.exe"),
            command_line: None,
            created: Some(ts(created)),
        }
    }

    /// Root 1, its child 2, and 2's child 3.
    fn chain() -> ProcessTree {
        ProcessTree::new(vec![
            process(3, 2, "2026-10-01T10:00:03Z"),
            process(1, 0, "2026-10-01T10:00:01Z"),
            process(2, 1, "2026-10-01T10:00:02Z"),
        ])
    }

    fn node(pid: u32, created: &str) -> Node {
        Node {
            pid,
            created: Some(ts(created)),
        }
    }

    fn chain_tree() -> Found {
        Found::Tree(vec![
            node(1, "2026-10-01T10:00:01Z"),
            node(2, "2026-10-01T10:00:02Z"),
            node(3, "2026-10-01T10:00:03Z"),
        ])
    }

    #[test]
    fn a_listed_root_gives_its_tree_root_first_then_breadth_first() {
        assert_eq!(found_in(1, RootState::Running, &chain()), chain_tree());
        assert_eq!(
            found_in(1, RootState::Unreadable, &chain()),
            chain_tree(),
            "an unreadable start is not judged"
        );
    }

    #[test]
    fn an_ended_root_is_not_live_even_when_listed() {
        assert_eq!(found_in(1, RootState::Ended, &chain()), Found::NotLive);
    }

    #[test]
    fn a_running_root_the_snapshot_misses_is_a_failed_snapshot() {
        assert_eq!(found_in(1, RootState::Running, &ProcessTree::new(Vec::new())), Found::Unlisted);
        assert_eq!(found_in(9, RootState::Running, &chain()), Found::Unlisted);
    }

    #[test]
    fn an_unreadable_root_the_snapshot_misses_is_not_live() {
        assert_eq!(found_in(9, RootState::Unreadable, &chain()), Found::NotLive);
    }

    #[test]
    fn job_members_share_what_is_left_of_the_job_end() {
        assert_eq!(grace(true, Duration::ZERO), JOB_END);
        assert_eq!(grace(true, Duration::from_millis(1_500)), Duration::from_millis(500));
        assert_eq!(grace(true, JOB_END), Duration::ZERO);
        assert_eq!(grace(true, JOB_END * 3), Duration::ZERO, "a spent budget gives no member more");
    }

    #[test]
    fn a_process_no_job_held_gets_no_grace() {
        assert_eq!(grace(false, Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn only_equal_known_creation_times_are_the_same_process() {
        let seen = ts("2026-10-01T10:00:00Z");
        assert!(same_process(Some(seen), Ok(seen)));
        assert!(!same_process(Some(seen), Ok(ts("2026-10-01T10:00:05Z"))), "a reused pid is passed over");
        assert!(!same_process(None, Ok(seen)), "a start the snapshot could not read is never matched");
        assert!(
            !same_process(Some(seen), Err(io::Error::other("unreadable"))),
            "a start that cannot be read now is not matched"
        );
    }
}

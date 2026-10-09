//! The machine's processes: one snapshot of every process with its parent, executable name, command line and start
//! time, read through the [`ProcessTable`] trait so a daemon and the tests supply their own. [`win32`] implements it on
//! 64-bit Windows from a `ToolHelp` snapshot and each process's PEB (the PEB offsets are x64).

#[cfg(all(windows, target_pointer_width = "64"))]
pub mod win32;

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;

#[cfg(all(windows, target_pointer_width = "64"))]
pub use win32::Win32ProcessTable;

/// One process of a [`ProcessTable`] snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process id.
    pub pid: u32,
    /// The id of the process that started it.
    pub parent_pid: u32,
    /// The executable's file name, such as `conhost.exe`.
    pub name: String,
    /// The command line, when it could be read.
    pub command_line: Option<String>,
    /// When the process started, when that could be read.
    pub created: Option<Timestamp>,
}

/// The machine's processes, taken in one snapshot per state.
pub trait ProcessTable {
    /// Every process running now.
    fn snapshot(&self) -> Vec<ProcessInfo>;
}

/// A process table that lists no processes: the table on a platform without a live one, and in tests that need none.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyTable;

impl ProcessTable for EmptyTable {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        Vec::new()
    }
}

/// FILETIME ticks (100 ns since 1601-01-01 UTC) at the Unix epoch.
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// FILETIME ticks (100 ns since 1601-01-01 UTC) as a timestamp in whole microseconds, the 100 ns digit dropped (WMI's
/// `CreationDate` holds whole microseconds, so a start time read either way compares equal). `None` when the moment is
/// outside the range a [`Timestamp`] holds.
#[must_use]
pub fn filetime_to_timestamp(ticks: i64) -> Option<Timestamp> {
    let since_epoch = ticks.checked_sub(FILETIME_UNIX_EPOCH)?;
    Timestamp::from_microsecond(since_epoch.div_euclid(10)).ok()
}

/// A process snapshot indexed by pid, for looking a process up and walking its ancestors.
#[derive(Clone, Debug)]
pub struct ProcessTree {
    processes: Vec<ProcessInfo>,
    by_pid: HashMap<u32, usize>,
}

impl ProcessTree {
    /// The tree of a snapshot; when two processes share a pid, the later one in the snapshot is the one looked up.
    #[must_use]
    pub fn new(processes: Vec<ProcessInfo>) -> Self {
        let mut by_pid = HashMap::new();
        for (position, process) in processes.iter().enumerate() {
            by_pid.insert(process.pid, position);
        }
        Self { processes, by_pid }
    }

    /// The process with this pid, or `None` when the snapshot has none.
    #[must_use]
    pub fn get(&self, pid: u32) -> Option<&ProcessInfo> {
        self.by_pid.get(&pid).and_then(|position| self.processes.get(*position))
    }

    /// Every pid's process once, in snapshot order: when two processes share a pid, only the one [`ProcessTree::get`]
    /// returns is yielded, at its own place in the snapshot.
    pub fn iter(&self) -> impl Iterator<Item = &ProcessInfo> {
        self.processes
            .iter()
            .enumerate()
            .filter(|(position, process)| self.by_pid.get(&process.pid) == Some(position))
            .map(|(_, process)| process)
    }

    /// The process's parent, then the parent's parent, and so on; `pid` itself is never yielded. The walk stops at a
    /// parent pid the snapshot does not have, at a pid already visited (pid reuse can make a cycle), and at a parent
    /// that started after its child (the real parent exited and a newer process took its pid). A process whose start
    /// time or whose parent's start time is unknown is not judged by it.
    pub fn ancestors(&self, pid: u32) -> impl Iterator<Item = &ProcessInfo> {
        let mut visited = HashSet::from([pid]);
        let mut child = self.get(pid);
        std::iter::from_fn(move || {
            let current = child?;
            child = self
                .get(current.parent_pid)
                .filter(|parent| visited.insert(parent.pid) && !started_after(parent, current));
            child
        })
    }
}

/// True when both start times are known and `parent` started after `child`.
fn started_after(parent: &ProcessInfo, child: &ProcessInfo) -> bool {
    match (parent.created, child.created) {
        (Some(parent_created), Some(child_created)) => parent_created > child_created,
        (None, _) | (_, None) => false,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test timestamp that does not parse must fail loudly")]
mod tests {
    use jiff::Timestamp;

    use super::{EmptyTable, FILETIME_UNIX_EPOCH, ProcessInfo, ProcessTable as _, ProcessTree, filetime_to_timestamp};

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

    fn ancestor_pids(tree: &ProcessTree, pid: u32) -> Vec<u32> {
        tree.ancestors(pid).map(|process| process.pid).collect()
    }

    #[test]
    fn filetime_epoch_is_unix_epoch() {
        assert_eq!(filetime_to_timestamp(116_444_736_000_000_000), Some(Timestamp::UNIX_EPOCH));
        assert_eq!(filetime_to_timestamp(116_444_736_000_000_000), Some(ts("1970-01-01T00:00:00Z")));
    }

    #[test]
    fn filetime_before_epoch_floors() {
        let micros = |ticks: i64| filetime_to_timestamp(ticks).map(Timestamp::as_microsecond);
        assert_eq!(micros(FILETIME_UNIX_EPOCH - 1), Some(-1));
        assert_eq!(micros(FILETIME_UNIX_EPOCH - 10), Some(-1));
        assert_eq!(micros(FILETIME_UNIX_EPOCH - 11), Some(-2));
    }

    #[test]
    fn filetime_drops_sub_microsecond_ticks() {
        let at_855 = filetime_to_timestamp(134_353_996_073_727_855).expect("a 2026 start time converts");
        let at_850 = filetime_to_timestamp(134_353_996_073_727_850).expect("a 2026 start time converts");
        let at_860 = filetime_to_timestamp(134_353_996_073_727_860).expect("a 2026 start time converts");
        assert_eq!(at_855, at_850);
        assert_eq!(at_860.as_microsecond() - at_855.as_microsecond(), 1);
        assert_eq!(at_855.as_nanosecond() % 1_000, 0);
    }

    #[test]
    fn filetime_out_of_range_is_none() {
        // i64::MAX ticks is in the year 30828, past jiff's `Timestamp::MAX` (late 9999), which `from_microsecond` rejects.
        let past_max = (i64::MAX - 116_444_736_000_000_000).div_euclid(10);
        assert!(past_max > Timestamp::MAX.as_microsecond());
        assert_eq!(filetime_to_timestamp(i64::MAX), None);
        // i64::MIN ticks cannot have the epoch subtracted at all.
        assert_eq!(filetime_to_timestamp(i64::MIN), None);
    }

    #[test]
    fn ancestors_walks_to_the_root() {
        let tree = ProcessTree::new(vec![
            process(3, 2, "2026-10-01T10:00:02Z"),
            process(1, 0, "2026-10-01T10:00:00Z"),
            process(4, 3, "2026-10-01T10:00:03Z"),
            process(2, 1, "2026-10-01T10:00:01Z"),
        ]);
        assert_eq!(ancestor_pids(&tree, 4), vec![3, 2, 1]);
        assert_eq!(tree.get(2).map(|process| process.name.as_str()), Some("p2.exe"));
    }

    #[test]
    fn ancestors_stops_at_missing_parent() {
        let tree = ProcessTree::new(vec![process(2, 9, "2026-10-01T10:00:01Z"), process(3, 2, "2026-10-01T10:00:02Z")]);
        assert_eq!(ancestor_pids(&tree, 3), vec![2]);
    }

    #[test]
    fn ancestors_stops_on_cycle() {
        let tree = ProcessTree::new(vec![process(10, 20, "2026-10-01T10:00:00Z"), process(20, 10, "2026-10-01T10:00:00Z")]);
        assert_eq!(ancestor_pids(&tree, 10), vec![20]);
        assert_eq!(ancestor_pids(&tree, 20), vec![10]);
    }

    #[test]
    fn ancestors_stops_at_reused_parent_pid() {
        let tree = ProcessTree::new(vec![
            process(1, 0, "2026-10-01T09:00:00Z"),
            process(2, 1, "2026-10-01T11:00:00Z"),
            process(3, 2, "2026-10-01T10:00:00Z"),
        ]);
        assert_eq!(ancestor_pids(&tree, 3), Vec::<u32>::new());
        assert_eq!(ancestor_pids(&tree, 2), vec![1]);
    }

    fn iter_pids(tree: &ProcessTree) -> Vec<u32> {
        tree.iter().map(|process| process.pid).collect()
    }

    #[test]
    fn iter_yields_a_duplicate_pid_once_as_get_does() {
        let tree = ProcessTree::new(vec![
            process(7, 1, "2026-10-01T10:00:00Z"),
            process(8, 1, "2026-10-01T10:00:01Z"),
            process(7, 2, "2026-10-01T10:00:02Z"),
        ]);
        let sevens: Vec<&ProcessInfo> = tree.iter().filter(|process| process.pid == 7).collect();
        assert_eq!(sevens.len(), 1);
        assert_eq!(sevens.first().copied(), tree.get(7));
        assert_eq!(sevens.first().map(|process| process.parent_pid), Some(2));
    }

    #[test]
    fn iter_follows_snapshot_order() {
        let tree = ProcessTree::new(vec![
            process(30, 0, "2026-10-01T10:00:00Z"),
            process(10, 0, "2026-10-01T10:00:00Z"),
            process(20, 0, "2026-10-01T10:00:00Z"),
            process(10, 5, "2026-10-01T10:00:01Z"),
            process(40, 0, "2026-10-01T10:00:00Z"),
        ]);
        assert_eq!(iter_pids(&tree), vec![30, 20, 10, 40]);
    }

    #[test]
    fn iter_of_empty_snapshot_yields_nothing() {
        let tree = ProcessTree::new(Vec::new());
        assert_eq!(iter_pids(&tree), Vec::<u32>::new());
    }

    #[test]
    fn empty_table_lists_no_processes() {
        assert_eq!(EmptyTable.snapshot(), Vec::<ProcessInfo>::new());
    }

    #[test]
    fn ancestors_of_unknown_pid_is_empty() {
        let tree = ProcessTree::new(vec![process(1, 0, "2026-10-01T10:00:00Z")]);
        assert_eq!(ancestor_pids(&tree, 42), Vec::<u32>::new());
        assert!(tree.get(42).is_none());
    }
}

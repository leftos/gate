//! The pause markers: the empty file a gate waiting behind a critical request keeps under the pause folder while it
//! waits, so the gate whose slot it runs under can tell a wait that is progress from one that has stalled, and the
//! reader that gate's watchdog calls back into.

use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gate_state::KernelObject;
use gate_state::v1::{self, Ticket};
use gate_win32::mutex_exists;
use jiff::Timestamp;

use crate::console::Console;
use crate::nesting::Parent;
use crate::resolve::ResolvedRoot;
use crate::slots::{self, Liveness, ThisGate};
use crate::wait::UnderParent;

/// How long a pause marker stays live without being rewritten: a waiting gate rewrites its marker on every pass of its
/// wait, so an older one is the marker of a gate that stopped waiting, or of one that died.
pub(crate) const PAUSE_STALE: Duration = Duration::from_secs(30);

/// [`PAUSE_STALE`] in whole milliseconds, which the markers' write times are read in.
const PAUSE_STALE_MS: u64 = PAUSE_STALE.as_secs() * 1_000;

/// The pause marker a gate keeps while it waits behind a critical request under a parent gate's slot: the file named
/// after both gates, written on every pass of the wait and deleted when the wait ends.
pub(crate) struct Pause {
    /// Where the marker is; `None` for a gate under no parent's slot, which keeps none.
    path: Option<PathBuf>,
    /// Whether a marker is there now: written some pass, and not deleted since.
    written: bool,
    /// Whether a write that failed has been said this wait.
    said_write: bool,
    /// Whether a delete that failed has been said this wait.
    said_delete: bool,
}

impl Pause {
    /// The marker of the gate `gate` waiting under `under`, or none for a gate under no parent's slot.
    pub(crate) fn of(root: &Path, under: Option<UnderParent>, gate: &ThisGate) -> Pause {
        Pause {
            path: under.map(|parent| v1::pause_path(root, parent.pid, parent.start_ms, gate.pid, gate.start_ms)),
            written: false,
            said_write: false,
            said_delete: false,
        }
    }

    /// Writes the marker while `waiting_behind` and deletes it otherwise, once a pass of the wait; nothing is deleted
    /// that was never written. A write or a delete that fails is said once a wait; the wait goes on either way.
    pub(crate) fn refresh(&mut self, waiting_behind: bool, console: &mut Console) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if waiting_behind {
            match write(path) {
                Ok(()) => self.written = true,
                Err(error) if !self.said_write => {
                    self.said_write = true;
                    console.err(&format!("gate: could not write the pause marker {} ({error})", path.display()));
                }
                Err(_) => {}
            }
        } else if self.written {
            match slots::delete(path) {
                Ok(()) => self.written = false,
                Err(error) if !self.said_delete => {
                    self.said_delete = true;
                    console.err(&format!("gate: could not delete the pause marker {} ({error})", path.display()));
                }
                Err(_) => {}
            }
        }
    }

    /// Deletes the marker at the end of the wait, and forgets it, so [`Drop`] has nothing left to do on an ordinary
    /// end however the wait ended.
    pub(crate) fn end(&mut self, console: &mut Console) {
        self.refresh(false, console);
        self.path = None;
    }
}

impl Drop for Pause {
    /// Deletes a marker a panic left behind, [`Pause::end`] having cleared the path on every ordinary end. The line for
    /// a failure is said on stderr alone, the run's console being out of reach by now.
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Err(error) = slots::delete(&path) {
            // Nothing is left to report the failure through but the stream itself; a failure to write there is lost.
            let _ = writeln!(io::stderr(), "gate: could not delete the pause marker {} ({error})", path.display());
        }
    }
}

/// Writes the empty marker at `path`, making its folder when it is missing. The empty write sets the marker's last
/// write time too, since that is what the parent reads to tell a waiting gate from a stuck one, and a write that
/// changes no byte of an already empty file would leave it where it was.
fn write(path: &Path) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    File::create(path)?.set_modified(SystemTime::now())
}

/// The facts one read of the pause markers turns on, so its rule can be tested without a folder, a process table or a
/// clock.
pub(crate) trait PauseFacts {
    /// The names of the entries in the pause folder; `Ok(None)` when it does not exist, and `Err` the line the gate
    /// says when it cannot be read.
    fn names(&mut self) -> Result<Option<Vec<String>>, String>;
    /// When the marker named `name` was last written, in Unix epoch milliseconds; `None` when it is not there any
    /// more, or its time cannot be read, since a marker gone as it is read is one whose gate is not waiting.
    fn written_ms(&mut self, name: &str) -> Option<u64>;
    /// Now, in Unix epoch milliseconds.
    fn now_ms(&mut self) -> u64;
    /// Whether a process of `pid` runs, started at `start_ms`; a start that cannot be read is not that process.
    fn runs(&mut self, pid: u32, start_ms: u64) -> bool;
    /// Whether a waiting ticket of the gate `pid` started at `start_ms` is under the state root; `Err` as
    /// [`PauseFacts::names`] when the waiting folder cannot be read.
    fn ticket_exists(&mut self, pid: u32, start_ms: u64) -> Result<bool, String>;
    /// Whether a critical request is pending, as [`slots::critical_pending`] reads it; `Err` as
    /// [`PauseFacts::names`] when the critical mutex cannot be probed.
    fn critical_pending(&mut self) -> Result<bool, String>;
    /// Whether the process `pid` runs in this gate's job; `Err` as [`PauseFacts::names`] when the job's processes
    /// cannot be listed.
    fn in_job(&mut self, pid: u32) -> Result<bool, String>;
}

/// The watchdog's reads of the pause markers of the gates under this one, and whether a read that failed has been said
/// this watch, which it says once.
pub(crate) struct PauseReader<'a> {
    root: &'a ResolvedRoot,
    /// This gate, as its ticket names it, which is never a child's.
    own: (u32, u64),
    /// The parent whose marker this gate hands its command, as its pid and start, when that parent's slot covers it
    /// or is lent to it: a gate under that command names its markers after the parent.
    hands_on: Option<(u32, u64)>,
    said: bool,
}

impl<'a> PauseReader<'a> {
    /// The reader of the markers under `root` of the gates under `gate`, which hands its command the marker of
    /// `hands_on` when it runs on that parent's slot; it has said nothing yet.
    pub(crate) fn new(root: &'a ResolvedRoot, gate: &ThisGate, hands_on: Option<&Parent>) -> Self {
        PauseReader {
            root,
            own: (gate.pid, gate.start_ms),
            hands_on: hands_on.map(|parent| (parent.pid, parent.start_ms)),
            said: false,
        }
    }

    /// One sample's read at the wall clock `now_ms`, `members` listing the processes in this gate's job: whether a gate
    /// under this one waits behind a critical request ([`live_child_pause_with`]), and the line to say when this is the
    /// first read of the watch that failed. A read that fails counts as not paused, and the next sample reads again.
    pub(crate) fn sample(&mut self, now_ms: u64, members: &dyn Fn() -> io::Result<Vec<u32>>) -> (bool, Option<String>) {
        let mut disk = Disk {
            root: self.root,
            own: self.own,
            now_ms,
            listed: None,
            members,
        };
        match live_child_pause_with(&mut disk, self.own, self.hands_on) {
            Ok(paused) => (paused, None),
            Err(line) if !self.said => {
                self.said = true;
                (false, Some(line))
            }
            Err(_) => (false, None),
        }
    }
}

/// Whether a gate under the gate `own` waits behind a critical request now, out of the markers the pause folder holds:
/// the first marker named after `own`, or after the parent `hands_on` whose marker this gate hands its command, whose
/// child gate waits ([`waits`]); each gate is its pid and start in Unix epoch milliseconds. A marker named after
/// `hands_on` counts only for a child in this gate's job, since a sibling gate the parent started beside this one names
/// it too. `Ok(false)` when none names such a child; `Err` the line to say once when the folder, the tickets, the job
/// or the critical mutex cannot be read.
pub(crate) fn live_child_pause_with(facts: &mut impl PauseFacts, own: (u32, u64), hands_on: Option<(u32, u64)>) -> Result<bool, String> {
    let Some(names) = facts.names()? else {
        return Ok(false);
    };
    let now = facts.now_ms();
    for name in names {
        let Some(marker) = v1::parse_pause_name(&name) else {
            continue;
        };
        let parent = (marker.parent_pid, marker.parent_start_ms);
        let handed = parent != own && Some(parent) == hands_on;
        if parent != own && !handed {
            continue;
        }
        if waits(facts, &name, (marker.pid, marker.start_ms), handed, now)? {
            return facts.critical_pending();
        }
    }
    Ok(false)
}

/// Whether the child gate `child`, its pid and start, of the marker `name` waits at `now`: it still runs with the
/// start in the marker's name, the marker was written within [`PAUSE_STALE`], it still holds its ticket, and, for a
/// marker named after the parent this gate hands its command (`handed`), it runs in this gate's job.
fn waits(facts: &mut impl PauseFacts, name: &str, child: (u32, u64), handed: bool, now: u64) -> Result<bool, String> {
    let (pid, start_ms) = child;
    if !facts.runs(pid, start_ms) {
        return Ok(false);
    }
    // A marker gone as its time is read is one whose gate stopped waiting, not a failed read: the next marker is asked.
    let Some(written) = facts.written_ms(name) else {
        return Ok(false);
    };
    if now.saturating_sub(written) > PAUSE_STALE_MS || !facts.ticket_exists(pid, start_ms)? {
        return Ok(false);
    }
    Ok(!handed || facts.in_job(pid)?)
}

/// The pause facts as the machine has them: the pause folder, the waiting tickets and the critical mutex under `root`,
/// the process table, and the wall clock `now_ms` the caller read.
struct Disk<'a> {
    root: &'a ResolvedRoot,
    /// The gate the read answers for, whose own ticket is never the child's.
    own: (u32, u64),
    now_ms: u64,
    /// The waiting tickets as this read listed them, once it has: one listing answers every ticket and the critical
    /// request it asks about.
    listed: Option<Vec<slots::Listed>>,
    /// Lists the processes in this gate's job.
    members: &'a dyn Fn() -> io::Result<Vec<u32>>,
}

impl PauseFacts for Disk<'_> {
    fn names(&mut self) -> Result<Option<Vec<String>>, String> {
        let folder = v1::paused_dir(&self.root.path);
        names(&folder).map_err(|error| line("the pause markers in", folder.display(), &error))
    }

    fn written_ms(&mut self, name: &str) -> Option<u64> {
        written_ms(&v1::paused_dir(&self.root.path).join(name))
    }

    fn now_ms(&mut self) -> u64 {
        self.now_ms
    }

    fn runs(&mut self, pid: u32, start_ms: u64) -> bool {
        matches!(slots::gate_liveness(pid, start_ms), Liveness::Live)
    }

    fn ticket_exists(&mut self, pid: u32, start_ms: u64) -> Result<bool, String> {
        let names_it = |entry: &slots::Listed| entry.live().is_some_and(|ticket| (ticket.pid, ticket.start_ms) == (pid, start_ms));
        Ok(self.tickets()?.iter().any(names_it))
    }

    fn critical_pending(&mut self) -> Result<bool, String> {
        let root = self.root;
        let live: Vec<Ticket> = self.tickets()?.iter().filter_map(slots::Listed::live).cloned().collect();
        let name = root.root.object_name(KernelObject::Critical);
        // A probe that fails cannot be read as no critical request: the read ends, and the parent pauses nothing.
        let mut failed = None;
        let pending = slots::critical_pending(&live, || match mutex_exists(&name) {
            Ok(open) => open,
            Err(error) => {
                failed = Some(error);
                false
            }
        });
        failed.map_or(Ok(pending), |error| Err(line("the critical mutex", name, &error)))
    }

    fn in_job(&mut self, pid: u32) -> Result<bool, String> {
        (self.members)()
            .map(|members| members.contains(&pid))
            .map_err(|error| format!("gate: could not read the processes in this gate's job ({error}); a gate under this one pauses nothing"))
    }
}

impl Disk<'_> {
    /// The waiting tickets under the state root, listed on this read's first ask ([`Disk::list`]) and kept for the
    /// rest of it.
    fn tickets(&mut self) -> Result<&[slots::Listed], String> {
        let listed = self.listed.take().map_or_else(|| self.list(), Ok)?;
        Ok(self.listed.insert(listed).as_slice())
    }

    /// The waiting tickets under the state root ([`slots::listed_tickets`]), the waiting folder named in the line when
    /// it cannot be read.
    fn list(&self) -> Result<Vec<slots::Listed>, String> {
        let root = self.root;
        let folder = v1::tickets_dir(&root.path);
        slots::listed_tickets(&root.path, self.own).map_err(|error| line("the waiting tickets in", folder.display(), &error))
    }
}

/// The line the caller says once when part of a state root cannot be read: `gate: could not read <what> <subject>
/// (<reason>); a gate under this one pauses nothing`, the subject being the folder or the mutex that failed.
fn line(what: &str, subject: impl Display, error: &dyn Display) -> String {
    format!("gate: could not read {what} {subject} ({error}); a gate under this one pauses nothing")
}

/// The names of the entries in the pause folder `folder`, sorted; `Ok(None)` when the folder is not there.
fn names(folder: &Path) -> io::Result<Option<Vec<String>>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(Some(names))
}

/// When the file at `path` was last written, in Unix epoch milliseconds; `None` when it is not there, or its time
/// cannot be read, since a marker gone as it is read is one whose gate is not waiting.
fn written_ms(path: &Path) -> Option<u64> {
    let written = fs::metadata(path).ok()?.modified().ok()?;
    Some(slots::epoch_ms(Timestamp::try_from(written).ok()?))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot build its state root, its markers or its facts must fail loudly"
)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use gate_state::StateRoot;
    use gate_state::v1;

    use super::{Disk, PauseFacts, live_child_pause_with};
    use crate::resolve::ResolvedRoot;

    /// The gate the reader answers for and the child gate of the marker under it, each with its start in Unix epoch
    /// milliseconds, and the wall clock the read happens at.
    const OWN_PID: u32 = 4242;
    const OWN_START: u64 = 1_759_439_760_571;
    const CHILD_PID: u32 = 5151;
    const CHILD_START: u64 = 1_759_439_800_001;
    const NOW: u64 = 1_759_439_900_000;

    /// The pause facts of one read, known up front: the names the folder lists, the last write of each marker it
    /// holds, the processes that run with the start they run since, the tickets that exist, and whether a critical
    /// request is pending.
    struct Fake {
        names: Result<Option<Vec<String>>, String>,
        /// When each marker was last written; a marker not in it is gone.
        written: HashMap<String, u64>,
        running: HashSet<(u32, u64)>,
        tickets: Result<HashSet<(u32, u64)>, String>,
        pending: Result<bool, String>,
    }

    /// A fake whose one marker is live: the child runs, its ticket exists, it was written a moment ago, and a critical
    /// request is pending.
    fn live() -> Fake {
        let marker = v1::pause_name(OWN_PID, OWN_START, CHILD_PID, CHILD_START);
        Fake {
            names: Ok(Some(vec![marker.clone()])),
            written: HashMap::from([(marker, NOW - 1_000)]),
            running: HashSet::from([(CHILD_PID, CHILD_START)]),
            tickets: Ok(HashSet::from([(CHILD_PID, CHILD_START)])),
            pending: Ok(true),
        }
    }

    impl PauseFacts for Fake {
        fn names(&mut self) -> Result<Option<Vec<String>>, String> {
            self.names.clone()
        }

        fn written_ms(&mut self, name: &str) -> Option<u64> {
            self.written.get(name).copied()
        }

        fn now_ms(&mut self) -> u64 {
            NOW
        }

        fn runs(&mut self, pid: u32, start_ms: u64) -> bool {
            self.running.contains(&(pid, start_ms))
        }

        fn ticket_exists(&mut self, pid: u32, start_ms: u64) -> Result<bool, String> {
            self.tickets.clone().map(|tickets| tickets.contains(&(pid, start_ms)))
        }

        fn critical_pending(&mut self) -> Result<bool, String> {
            self.pending.clone()
        }

        fn in_job(&mut self, pid: u32) -> Result<bool, String> {
            Ok(self.running.iter().any(|&(running, _)| running == pid))
        }
    }

    /// What the reader answers for the gate [`OWN_PID`] under `facts`.
    fn read(facts: &mut Fake) -> Result<bool, String> {
        live_child_pause_with(facts, (OWN_PID, OWN_START), None)
    }

    #[test]
    fn a_live_marker_answers_whether_a_critical_request_is_pending() {
        assert_eq!(
            read(&mut live()),
            Ok(true),
            "a pending critical request is what the marker pauses the parent for"
        );
        let mut idle = Fake {
            pending: Ok(false),
            ..live()
        };
        assert_eq!(read(&mut idle), Ok(false), "a marker with no critical request pending pauses nothing");
    }

    #[test]
    fn a_marker_whose_child_ended_is_not_live() {
        let mut ended = Fake {
            running: HashSet::new(),
            ..live()
        };

        assert_eq!(read(&mut ended), Ok(false));
    }

    #[test]
    fn a_marker_whose_child_pid_a_later_process_reused_is_not_live() {
        let mut reused = Fake {
            running: HashSet::from([(CHILD_PID, CHILD_START + 1)]),
            ..live()
        };

        assert_eq!(
            read(&mut reused),
            Ok(false),
            "the pid runs since another start, and one that cannot be read is no better"
        );
    }

    #[test]
    fn a_marker_of_a_child_that_holds_no_ticket_is_not_live() {
        let mut taken = Fake {
            tickets: Ok(HashSet::new()),
            ..live()
        };

        assert_eq!(read(&mut taken), Ok(false), "the child took its slot and left the queue");
    }

    #[test]
    fn a_marker_is_live_while_it_is_no_older_than_the_stale_window() {
        let marker = v1::pause_name(OWN_PID, OWN_START, CHILD_PID, CHILD_START);
        for (age_ms, expected) in [(0, true), (29_999, true), (30_000, true), (30_001, false)] {
            let mut aged = Fake {
                written: HashMap::from([(marker.clone(), NOW - age_ms)]),
                ..live()
            };

            assert_eq!(read(&mut aged), Ok(expected), "a marker written {age_ms} ms ago");
        }
    }

    #[test]
    fn a_marker_gone_as_it_is_read_is_passed_over_and_the_next_one_answers() {
        // Two children of the same parent wait; the first's marker is gone by the time its write time is read.
        let gone = v1::pause_name(OWN_PID, OWN_START, CHILD_PID - 1, CHILD_START);
        let here = v1::pause_name(OWN_PID, OWN_START, CHILD_PID, CHILD_START);
        let mut facts = Fake {
            names: Ok(Some(vec![gone.clone(), here.clone()])),
            written: HashMap::from([(here, NOW - 1_000)]),
            running: HashSet::from([(CHILD_PID - 1, CHILD_START), (CHILD_PID, CHILD_START)]),
            tickets: Ok(HashSet::from([(CHILD_PID - 1, CHILD_START), (CHILD_PID, CHILD_START)])),
            ..live()
        };

        assert_eq!(read(&mut facts), Ok(true), "the marker gone as it was read is skipped, not an Err");
        assert_eq!(read(&mut facts), Ok(true), "and the write times are asked again on the next read");
    }

    #[test]
    fn a_marker_of_another_parent_gate_is_ignored() {
        let names = [
            v1::pause_name(OWN_PID, OWN_START + 1, CHILD_PID, CHILD_START),
            v1::pause_name(OWN_PID + 1, OWN_START, CHILD_PID, CHILD_START),
        ];
        for name in names {
            let mut other = Fake {
                names: Ok(Some(vec![name.clone()])),
                ..live()
            };

            assert_eq!(read(&mut other), Ok(false), "{name}");
        }
    }

    #[test]
    fn a_name_the_pause_folder_cannot_hold_is_no_marker() {
        for name in ["not-a-marker", "4242-1759439760571-5151.pause", "4242-1759439760571-5151-1.txt"] {
            let mut odd = Fake {
                names: Ok(Some(vec![name.to_owned()])),
                ..live()
            };

            assert_eq!(read(&mut odd), Ok(false), "{name}");
        }
    }

    #[test]
    fn no_pause_folder_pauses_nothing() {
        let mut gone = Fake { names: Ok(None), ..live() };

        assert_eq!(read(&mut gone), Ok(false));
    }

    #[test]
    fn a_read_that_cannot_be_made_ends_the_answer_with_its_line() {
        let line = "gate: could not read the pause markers in C:\\state\\paused (denied); a gate under this one pauses nothing";
        let mut no_folder = Fake {
            names: Err(line.to_owned()),
            ..live()
        };
        assert_eq!(read(&mut no_folder), Err(line.to_owned()));

        let line = "gate: could not read the waiting tickets in C:\\state\\waiting (denied); a gate under this one pauses nothing";
        let mut no_tickets = Fake {
            tickets: Err(line.to_owned()),
            ..live()
        };
        assert_eq!(read(&mut no_tickets), Err(line.to_owned()));

        let line = "gate: could not read the critical mutex Local\\gate-ab12-critical (denied); a gate under this one pauses nothing";
        let mut no_mutex = Fake {
            pending: Err(line.to_owned()),
            ..live()
        };
        assert_eq!(read(&mut no_mutex), Err(line.to_owned()));
    }

    /// A folder for one test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let folder = crate::test_support::temp_dir().join(format!("gate-pause-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the scratch folder can be made");
            Scratch(folder)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The state root at `folder`, its identity taken from the path as a case's is.
    fn state_root(folder: &Path) -> ResolvedRoot {
        ResolvedRoot {
            path: folder.to_owned(),
            root: StateRoot::new(folder.to_str().expect("the scratch folder has a Unicode path"), None),
        }
    }

    /// The pause facts over the state root `root`, read at [`NOW`].
    fn disk(root: &ResolvedRoot) -> Disk<'_> {
        Disk {
            root,
            own: (OWN_PID, OWN_START),
            now_ms: NOW,
            listed: None,
            members: &no_members,
        }
    }

    /// A job that holds no process.
    #[expect(clippy::unnecessary_wraps, reason = "it stands in for the job's lister, which returns a Result")]
    fn no_members() -> std::io::Result<Vec<u32>> {
        Ok(Vec::new())
    }

    /// Makes the file at `path` and sets its last write time to `ms` past the epoch, without sleeping.
    fn write_ms(path: &Path, ms: u64) {
        File::create(path)
            .expect("the marker can be made")
            .set_modified(UNIX_EPOCH + Duration::from_millis(ms))
            .expect("the marker's last write can be set");
    }

    #[test]
    fn the_disk_facts_read_a_real_pause_folder_and_its_markers() {
        let scratch = Scratch::new("pause-folder");
        let root = state_root(&scratch.0);
        let ours = v1::pause_name(OWN_PID, OWN_START, CHILD_PID, CHILD_START);
        let other = v1::pause_name(OWN_PID + 1, OWN_START, CHILD_PID, CHILD_START);

        assert_eq!(disk(&root).names(), Ok(None), "a pause folder that is not there holds none");

        let paused = v1::paused_dir(&scratch.0);
        fs::create_dir_all(&paused).expect("the pause folder can be made");
        write_ms(&paused.join(&other), NOW - 1_000);
        write_ms(&paused.join(&ours), NOW - 2_000);

        assert_eq!(disk(&root).names(), Ok(Some(vec![ours.clone(), other])), "the folder's names, sorted");
        assert_eq!(
            disk(&root).written_ms(&ours),
            Some(NOW - 2_000),
            "a marker's last write, to the millisecond"
        );
        assert_eq!(
            disk(&root).written_ms("no-such-marker.pause"),
            None,
            "a marker gone as it is read is no marker"
        );
    }

    #[test]
    fn a_pause_folder_that_cannot_be_listed_ends_the_read_with_its_line() {
        let scratch = Scratch::new("pause-folder-a-file");
        let root = state_root(&scratch.0);
        let paused = v1::paused_dir(&scratch.0);
        fs::write(&paused, "").expect("the pause folder can be made a file");

        let error = disk(&root).names().expect_err("a folder that is a file cannot be listed");

        assert!(
            error.starts_with(&format!("gate: could not read the pause markers in {} (", paused.display())),
            "{error}"
        );
        assert!(error.ends_with("); a gate under this one pauses nothing"), "{error}");
    }

    /// The times [`NOW`] and the marks around it are read as, so a test can see what a set write time comes back as.
    #[test]
    fn a_set_write_time_is_read_back_as_the_same_millisecond() {
        let scratch = Scratch::new("write-time");
        let path = scratch.0.join("marker.pause");
        let written = SystemTime::UNIX_EPOCH + Duration::from_millis(NOW);

        write_ms(&path, NOW);

        assert_eq!(
            fs::metadata(&path).expect("the marker is there").modified().expect("it has a write time"),
            written
        );
    }
}

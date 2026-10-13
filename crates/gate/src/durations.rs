//! The learned durations as a run uses them: the key its runs are filed under, the expected-time line it says as it
//! takes a slot, its release filed in `durations.json` in the hold of its `release` line, and the store rebuilt from
//! the journal once the run has released when its reads found the store missing, stale or torn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_args::TestSeams;
use gate_router::Place;
use gate_state::{
    DurationKey, Estimate, JournalLock, KeyInputs, KeyPlace, RebuildOwner, RebuildReason, Rebuilt, Recorded, RunFacts, StoreError, StoreRead,
    StoreReadError, abandon_rebuild, begin_rebuild, duration_key, durations_path, place_by_shape, read_store, rebuild_store, record_run,
    swap_rebuilt,
};
use gate_win32::Interrupt;
use jiff::Timestamp;

use crate::console::Console;
use crate::remote::git::{self, Git};
use crate::route::{self, Placed};
use crate::slots::{self, Liveness};

/// The `about` figure, in seconds, from which the expected-time line is in minutes.
const MINUTES_FROM: f64 = 90.0;

/// The line a gate says as it takes its slot for a key with history: `gate: expected about {mean} (usual {p50}, p90
/// {p90}, {runs} runs)`, every figure in whole seconds when the mean is under 90 s, else in whole minutes, each rounded to
/// the nearest and never below 1. An estimate of the family key ends `runs of similar commands)`.
pub(crate) fn expected_line(estimate: &Estimate) -> String {
    let in_seconds = estimate.mean < MINUTES_FROM;
    let shown = |seconds: f64| {
        if in_seconds {
            format!("{} s", seconds.round().max(1.0))
        } else {
            format!("{} min", (seconds / 60.0).round().max(1.0))
        }
    };
    let of = if estimate.family { " of similar commands" } else { "" };
    format!(
        "gate: expected about {} (usual {}, p90 {}, {} runs{of})",
        shown(estimate.mean),
        shown(estimate.p50),
        shown(estimate.p90),
        estimate.runs
    )
}

/// How long each git call that places a folder may take, and how long its readers get once git has exited.
#[derive(Clone, Copy)]
struct GitLimits {
    limit: Duration,
    drain: Duration,
}

impl GitLimits {
    fn of(seams: &TestSeams) -> GitLimits {
        GitLimits {
            limit: git::limit(seams),
            drain: git::drain(seams),
        }
    }

    /// Where the folder `cwd` sits, placed by one git call, or by its path's shape when git does not place it.
    fn place_or_shape(self, cwd: &str) -> KeyPlace {
        let folder = Path::new(cwd);
        let placed = Git::find(folder).ok().and_then(|git| self.place(&git, folder, None));
        placed.unwrap_or_else(|| place_by_shape(cwd))
    }

    /// Where git places `folder`, asked as the routing decision asks it ([`route::read_place`]), the call ended when
    /// `interrupt` is raised: in its worktree, under the main checkout's name. `None` when git fails, says the folder
    /// is in no worktree, does not answer in time, or writes what does not read as a place.
    fn place(self, git: &Git, folder: &Path, interrupt: Option<Interrupt>) -> Option<KeyPlace> {
        let call = git::Call {
            drain: self.drain,
            interrupt,
            ..git::Call::plain(folder, &route::PLACE_ARGS, self.limit)
        };
        route::read_place(git, &call).ok().flatten().as_ref().map(from_route)
    }
}

/// Where the folder `cwd` sits in a key, as a run whose routing decision asked no git places it: by one git call under
/// the git limit of `seams`, or by its path's shape when git does not place it.
pub(crate) fn folder_place(cwd: &str, seams: &TestSeams) -> KeyPlace {
    GitLimits::of(seams).place_or_shape(cwd)
}

/// The system temp root, whose paths a command word loses in a key; `None` when it is empty.
pub(crate) fn temp_root() -> Option<String> {
    Some(std::env::temp_dir().to_string_lossy().into_owned()).filter(|root| !root.is_empty())
}

/// The line that says the store cannot be read, so no expected time is said.
pub(crate) fn unreadable_line(error: &StoreReadError) -> String {
    format!(
        "gate: cannot read {} ({}); expected times are unavailable",
        error.path.display(),
        error.reason
    )
}

/// The key place of a folder the routing decision placed.
fn from_route(place: &Place) -> KeyPlace {
    KeyPlace::InGit {
        repo: place.checkout.clone(),
        sub: place.prefix.clone(),
        worktree_root: place.root.clone(),
    }
}

/// What a run that takes a slot knows of its durations: its key's parts, read once before it queues, and which of its
/// once-a-run lines it has said.
pub(crate) struct RunDurations {
    root: PathBuf,
    folder: KeyPlace,
    command: String,
    temp_root: Option<String>,
    critical: bool,
    owner: RebuildOwner,
    git: GitLimits,
    /// Whether a read of the store found it missing, stale or torn, so the run rebuilds it once it has released.
    rebuild: bool,
    said: Said,
}

/// Which of a run's once-a-run durations lines it has said.
#[derive(Default)]
struct Said {
    unreadable: bool,
    unwritten: bool,
}

/// Who a run is, for its key and its rebuild: the folder and command its journal lines name, whether it is a critical
/// gate, and the gate itself.
pub(crate) struct RunIdentity<'a> {
    pub cwd: &'a str,
    pub command: &'a str,
    pub critical: bool,
    pub owner: RebuildOwner,
}

impl RunDurations {
    /// The durations of a run in the state root `root`: its folder placed as the routing decision `placed` it when the
    /// decision asked git, else by one git call under the run's git limit, and by its path's shape when git does not
    /// place it.
    pub(crate) fn open(root: &Path, run: &RunIdentity<'_>, placed: &Placed, seams: &TestSeams) -> RunDurations {
        let limits = GitLimits::of(seams);
        let folder = match placed {
            Placed::Asked(Some(place)) => from_route(place),
            Placed::Asked(None) => place_by_shape(run.cwd),
            Placed::NotAsked => limits.place_or_shape(run.cwd),
        };
        RunDurations {
            root: root.to_path_buf(),
            folder,
            command: run.command.to_owned(),
            temp_root: temp_root(),
            critical: run.critical,
            owner: run.owner,
            git: limits,
            rebuild: false,
            said: Said::default(),
        }
    }

    /// The run's key on the build box `remote_host` (`user@host`), or here when it is `None`.
    fn key(&self, remote_host: Option<&str>) -> DurationKey {
        duration_key(&KeyInputs {
            folder: &self.folder,
            remote_host,
            critical: self.critical,
            temp_root: self.temp_root.as_deref(),
            command: &self.command,
        })
    }

    /// Says the expected-time line of the run's key on `remote_host`, read from the store with no lock; nothing for a
    /// key without history, a missing store or a newer build's, and a store that cannot be read said once a run.
    pub(crate) fn say_expected(&mut self, remote_host: Option<&str>, console: &mut Console) {
        let store = match read_store(&self.root) {
            StoreRead::Current(store) => store,
            StoreRead::NeedsRebuild(reason) => {
                self.needs_rebuild(reason, console);
                return;
            }
            StoreRead::Unavailable(error) => {
                self.say_unreadable(&error, console);
                return;
            }
            StoreRead::Newer => return,
        };
        if let Some(estimate) = store.estimate(&self.key(remote_host), Timestamp::now()) {
            console.err(&expected_line(&estimate));
        }
    }

    /// What files the run's release on `remote_host` in the journal hold, or `None` when the run reached no verdict.
    pub(crate) fn filing(&self, remote_host: Option<&str>, facts: Option<RunFacts>) -> Option<Filing> {
        let facts = facts?;
        Some(Filing {
            root: self.root.clone(),
            key: self.key(remote_host),
            facts,
        })
    }

    /// Says what filing the release found: a store that cannot be read or written, each once a run; a store that
    /// needs a rebuild is rebuilt once the run has released ([`RunDurations::rebuild`]), and one the release was filed
    /// in, made current since the run's read, needs none.
    pub(crate) fn filed(&mut self, filed: Result<Recorded, StoreError>, console: &mut Console) {
        match filed {
            Ok(Recorded::Written) => self.rebuild = false,
            Ok(Recorded::Newer) => {}
            Ok(Recorded::NeedsRebuild(reason)) => self.needs_rebuild(reason, console),
            Ok(Recorded::Unavailable(error)) => self.say_unreadable(&error, console),
            Err(error) => self.say_unwritten(&error, console),
        }
    }

    /// Rebuilds the store from the journal and the archive when a read of this run asked for it, at most once a run,
    /// and no Ctrl+C raised `interrupt` ([`RunDurations::rebuild_unless`]), git placing each folder.
    pub(crate) fn rebuild(&mut self, lock: &impl JournalLock, interrupt: Option<Interrupt>, console: &mut Console) {
        let git = Git::find(&self.root).ok();
        let limits = self.git;
        let place = |folder: &Path| limits.place(git.as_ref()?, folder, interrupt);
        self.rebuild_unless(lock, || interrupt.is_some_and(Interrupt::is_raised), place, console);
    }

    /// Claims the rebuild ([`RunDurations::claim_rebuild`]), reads the journal outside its hold with `place` placing each
    /// folder that still exists, then swaps the store in under `lock` ([`RunDurations::swap_in`]). Once `stopped` says
    /// so, no folder is placed any more, nothing is swapped in and the claim is given up.
    fn rebuild_unless(
        &mut self,
        lock: &impl JournalLock,
        stopped: impl Fn() -> bool,
        place: impl Fn(&Path) -> Option<KeyPlace>,
        console: &mut Console,
    ) {
        if stopped() || !self.claim_rebuild(lock, console) {
            return;
        }
        let key_place = |folder: &Path| (!stopped() && folder.is_dir()).then(|| place(folder)).flatten();
        let rebuilt = rebuild_store(&self.root, self.temp_root.as_deref(), Timestamp::now(), key_place);
        if stopped() {
            if let Some(note) = abandon_rebuild(&self.root, self.owner) {
                console.err(&format!("gate: {note}"));
            }
            return;
        }
        self.swap_in(lock, &rebuilt, console);
    }

    /// Whether this run rebuilds the store: a read of it asked for a rebuild and none was claimed yet, the store is
    /// still not current nor a newer build's, read again with no lock, and the claim is this run's. Another gate's live
    /// claim, or a marker another process holds open, is a quiet no; a claim that fails otherwise is said.
    fn claim_rebuild(&mut self, lock: &impl JournalLock, console: &mut Console) -> bool {
        if !std::mem::take(&mut self.rebuild) || matches!(read_store(&self.root), StoreRead::Current(_) | StoreRead::Newer) {
            return false;
        }
        // A holder whose liveness cannot be read is taken for live: its rebuild is never taken over under it.
        let holder_live = |pid, start_ms| !matches!(slots::gate_liveness(pid, start_ms), Liveness::Gone);
        match begin_rebuild(&self.root, lock, self.owner, holder_live) {
            Ok(claimed) => claimed,
            Err(error) => {
                console.err(&format!(
                    "gate: cannot rebuild {} ({error}); expected times are unavailable",
                    durations_path(&self.root).display()
                ));
                false
            }
        }
    }

    /// Says what the rebuild's reads had to say, then swaps `rebuilt` in under `lock`, saying what the swap had to say
    /// and a store it could not write; a swap that cannot take `lock` leaves the rebuild to a later run.
    fn swap_in(&mut self, lock: &impl JournalLock, rebuilt: &Rebuilt, console: &mut Console) {
        for note in &rebuilt.notes {
            console.err(&format!("gate: {note}"));
        }
        match swap_rebuilt(&self.root, lock, rebuilt, Timestamp::now(), self.owner) {
            Ok(swap) => {
                for note in &swap.notes {
                    console.err(&format!("gate: {note}"));
                }
            }
            // A journal mutex held elsewhere leaves the rebuild to a later run, as a claim another gate holds does.
            Err(StoreError::Lock(_)) => {}
            Err(error) => self.say_unwritten(&error, console),
        }
    }

    fn needs_rebuild(&mut self, reason: RebuildReason, console: &mut Console) {
        self.rebuild = true;
        if let RebuildReason::Invalid(error) = reason {
            self.say_unreadable(&error, console);
        }
    }

    fn say_unreadable(&mut self, error: &StoreReadError, console: &mut Console) {
        if !std::mem::replace(&mut self.said.unreadable, true) {
            console.err(&unreadable_line(error));
        }
    }

    fn say_unwritten(&mut self, error: &StoreError, console: &mut Console) {
        if std::mem::replace(&mut self.said.unwritten, true) {
            return;
        }
        let (path, reason) = match error {
            StoreError::Write(error) => (error.path.clone(), error.reason.clone()),
            StoreError::Lock(_) | StoreError::Marker { .. } => (durations_path(&self.root), error.to_string()),
        };
        console.err(&format!("gate: could not write {} ({reason})", path.display()));
    }
}

/// A release to file in the store, inside the journal hold of its line.
pub(crate) struct Filing {
    root: PathBuf,
    key: DurationKey,
    facts: RunFacts,
}

impl Filing {
    /// Files the release, its line written at `time`: no git, no journal read.
    pub(crate) fn file(&self, time: Timestamp) -> Result<Recorded, StoreError> {
        record_run(&self.root, &self.key, time, &self.facts)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gate_state::{
        DurationStore, Estimate, Held, JournalLock, KeyPlace, LockError, RebuildOwner, durations_path, place_by_shape, rebuilding_path, write_store,
    };
    use jiff::Timestamp;
    use serde_json::json;

    use super::{GitLimits, RunDurations, Said, expected_line};
    use crate::console::Console;

    fn estimate(mean: f64, p50: f64, p90: f64, runs: usize, family: bool) -> Estimate {
        Estimate {
            p50,
            p90,
            p99: p90,
            adjusted_p99: None,
            mean,
            runs,
            key: "local:: | cargo build".to_owned(),
            family,
        }
    }

    #[test]
    fn under_90_s_the_line_is_in_whole_seconds() {
        assert_eq!(
            expected_line(&estimate(19.4, 18.0, 31.2, 12, false)),
            "gate: expected about 19 s (usual 18 s, p90 31 s, 12 runs)"
        );
        assert_eq!(
            expected_line(&estimate(89.4, 80.0, 120.0, 7, false)),
            "gate: expected about 89 s (usual 80 s, p90 120 s, 7 runs)",
            "every figure in the unit of the about figure"
        );
    }

    #[test]
    fn a_sub_second_command_says_about_1_s() {
        assert_eq!(
            expected_line(&estimate(0.3, 0.2, 0.6, 5, false)),
            "gate: expected about 1 s (usual 1 s, p90 1 s, 5 runs)",
            "no figure is ever 0 s"
        );
    }

    #[test]
    fn from_90_s_the_line_is_in_whole_minutes_never_below_1() {
        assert_eq!(
            expected_line(&estimate(90.0, 20.0, 100.0, 5, false)),
            "gate: expected about 2 min (usual 1 min, p90 2 min, 5 runs)",
            "90 s rounds to 2 min, and 20 s is never 0 min"
        );
        assert_eq!(
            expected_line(&estimate(240.0, 230.0, 380.0, 30, false)),
            "gate: expected about 4 min (usual 4 min, p90 6 min, 30 runs)"
        );
    }

    #[test]
    fn a_family_estimate_says_it_is_of_similar_commands() {
        assert_eq!(
            expected_line(&estimate(20.0, 18.0, 31.0, 41, true)),
            "gate: expected about 20 s (usual 18 s, p90 31 s, 41 runs of similar commands)"
        );
    }

    /// A journal lock no other gate holds.
    struct FreeLock;

    impl JournalLock for FreeLock {
        type Guard<'a> = ();

        fn name(&self) -> &'static str {
            "test-journal"
        }

        fn acquire(&self, _timeout: Duration) -> Result<Held<()>, LockError> {
            Ok(Held { guard: (), abandoned: false })
        }
    }

    /// A state root of its own in the temp folder, holding a journal of one passing release in each of two folders that
    /// exist, its own `first` and `second`; removed when dropped.
    struct Root(PathBuf);

    impl Root {
        #[expect(clippy::expect_used, reason = "a test that cannot build its state root must fail loudly")]
        fn new(test: &str) -> Root {
            let root = std::env::temp_dir().join(format!("gate-durations-{}-{test}", std::process::id()));
            // A folder left by an earlier run of the same pid is cleared first.
            let _ = fs::remove_dir_all(&root);
            let lines: Vec<String> = ["first", "second"]
                .iter()
                .zip(1..)
                .map(|(name, pid)| {
                    let folder = root.join(name);
                    fs::create_dir_all(&folder).expect("the folder can be made");
                    let release = json!({
                        "event": "release", "time": Timestamp::now().to_string(), "pid": pid, "kind": "light",
                        "cwd": folder, "command": "cargo build", "wallSeconds": 1.0, "verdict": "passed",
                    });
                    format!("{release}\n")
                })
                .collect();
            fs::write(root.join("events.jsonl"), lines.concat()).expect("the journal can be written");
            Root(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            // A folder left in the temp folder harms no later test, and a panic in drop would abort the run.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The durations of a run in `root` whose read of the store asked for a rebuild.
    fn rebuilding(root: &Path) -> RunDurations {
        RunDurations {
            root: root.to_path_buf(),
            folder: place_by_shape("C:/work"),
            command: "cargo build".to_owned(),
            temp_root: None,
            critical: false,
            owner: RebuildOwner {
                pid: std::process::id(),
                start_ms: 1,
            },
            git: GitLimits {
                limit: Duration::ZERO,
                drain: Duration::ZERO,
            },
            rebuild: true,
            said: Said::default(),
        }
    }

    #[test]
    fn a_ctrl_c_during_the_rebuild_places_no_more_folders_and_swaps_nothing() {
        let root = Root::new("interrupted");
        let raised = Cell::new(false);
        let asked = Cell::new(0);
        // The Ctrl+C comes while git places the first folder.
        let place = |_: &Path| -> Option<KeyPlace> {
            asked.set(asked.get() + 1);
            raised.set(true);
            None
        };
        let mut console = Console::new();

        rebuilding(&root.0).rebuild_unless(&FreeLock, || raised.get(), place, &mut console);

        assert_eq!(asked.get(), 1, "no folder is placed once the Ctrl+C came");
        assert!(!durations_path(&root.0).exists(), "nothing is swapped in");
        assert!(!rebuilding_path(&root.0).exists(), "the claim is given up");
        assert_eq!(console.take_notes(), Vec::<String>::new(), "an interrupted rebuild says nothing");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot write its store must fail loudly")]
    fn a_store_made_current_since_the_runs_read_is_not_rebuilt() {
        let root = Root::new("current-since");
        write_store(&root.0, &DurationStore::new()).expect("another gate's store can be written");
        let asked = Cell::new(0);
        let place = |_: &Path| -> Option<KeyPlace> {
            asked.set(asked.get() + 1);
            None
        };
        let mut console = Console::new();

        rebuilding(&root.0).rebuild_unless(&FreeLock, || false, place, &mut console);

        assert_eq!(asked.get(), 0, "the journal is not read again");
        assert!(!rebuilding_path(&root.0).exists(), "no rebuild is claimed");
        assert_eq!(console.take_notes(), Vec::<String>::new(), "nothing is said");
    }
}

//! The lock of a worktree a sync-back writes, the main one's or a pair's sibling's: a kernel mutex named for the
//! worktree folder's id ([`KernelObject::Worktree`]), read by opening its root at lock time as the route read it,
//! following links, and refused when it is not the id the route was decided for. The folder stays open, shared for
//! reading and writing but not deletion, as long as its lock is held, so it cannot be renamed or swapped while the run
//! writes it, and two runs that write one folder, under one spelling of its path or two, never write it at once. A run
//! takes its worktrees' locks in the order of their ids, so two runs never each hold one the other waits for, and waits
//! for one another run holds, saying so once, until the attempt's lock wait has passed.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use gate_router::FolderId;
use gate_state::{KernelObject, StateRoot};
use gate_win32::{AcquireError, Interrupt, NamedMutex, OwnedMutexGuard, Share};

use super::client::End;
use crate::route::open_followed;

/// How long each wait for a worktree's lock another run holds lasts before the interrupt and the deadline are looked at
/// again.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// A worktree a run locks: its name in the run, its root, and the id the route read for its folder.
#[derive(Debug, Clone, Copy)]
pub(super) struct Folder<'a> {
    pub(super) name: &'a str,
    pub(super) root: &'a str,
    pub(super) id: FolderId,
}

/// A worktree's folder opened for its lock: its id, the name of its lock, and the open folder.
pub(super) struct Opened {
    id: (u64, [u8; 16]),
    pub(super) lock: String,
    folder: File,
}

/// A worktree's lock held: the mutex's guard and the worktree's folder, held open without delete sharing until both
/// are dropped.
pub(super) struct Locked {
    #[cfg_attr(not(test), expect(dead_code, reason = "held for its drop; only the tests read the lock's name"))]
    guard: OwnedMutexGuard,
    _folder: File,
}

impl Locked {
    /// The name of the mutex held.
    #[cfg(test)]
    pub(super) fn name(&self) -> &str {
        self.guard.name()
    }
}

/// The worktree folder at `root`, opened following links, as the route read its id, without delete sharing; its id and
/// the name of its lock under the state root `state`. Else why its id cannot be read, or, when its id is not
/// `expected`, the one the route was decided for, that it is another folder.
pub(super) fn lock_of(state: &StateRoot, root: &str, expected: FolderId) -> Result<Opened, End> {
    let cannot = |error: std::io::Error| End::Failed(format!("cannot read the id of the worktree {root}: {error}"));
    let folder = open_followed(Path::new(root), Share::NoDelete).map_err(cannot)?;
    let (volume, file) = gate_win32::file_id(&folder).map_err(cannot)?;
    if (FolderId { volume, file }) != expected {
        return Err(End::Failed(format!("the worktree {root} is not the folder the route was decided for")));
    }
    Ok(Opened {
        id: (volume, file),
        lock: state.object_name(KernelObject::Worktree { volume, file }),
        folder,
    })
}

/// The locks of `worktrees` under the state root `state`, each folder opened and checked ([`lock_of`]), taken in the
/// order of their folders' ids, one per folder however many spellings name it, and held with the folders until dropped.
/// A lock another run holds is waited for, `say` given `gate: remote waiting for the worktree <name>: another run holds
/// it` once, until `limit` has passed since the first lock was asked for; a lock left by a holder that ended is taken.
/// The interrupt `interrupt` ends a wait as the run's interrupt; a folder whose id cannot be read or is not the route's,
/// a mutex that cannot be opened or taken, and a wait past `limit` are the run's failure.
pub(super) fn held(
    state: &StateRoot,
    worktrees: &[Folder<'_>],
    limit: Duration,
    interrupt: Option<Interrupt>,
    mut say: impl FnMut(String),
) -> Result<Vec<Locked>, End> {
    let deadline = Instant::now() + limit;
    let mut locks = Vec::with_capacity(worktrees.len());
    for worktree in worktrees {
        locks.push((lock_of(state, worktree.root, worktree.id)?, worktree.name));
    }
    locks.sort_by_key(|(opened, _)| opened.id);
    locks.dedup_by_key(|(opened, _)| opened.id);
    let wait = Wait { deadline, limit, interrupt };
    locks
        .into_iter()
        .map(|(opened, name)| {
            let guard = taken(&opened.lock, name, &wait, &mut say)?;
            Ok(Locked {
                guard,
                _folder: opened.folder,
            })
        })
        .collect()
}

/// How long a wait for a lock may last, and the interrupt that ends it.
struct Wait {
    deadline: Instant,
    limit: Duration,
    interrupt: Option<Interrupt>,
}

/// The mutex `lock` of the worktree `name`, taken at once when it is free, else after the wait [`held`] makes.
fn taken(lock: &str, name: &str, wait: &Wait, say: &mut impl FnMut(String)) -> Result<OwnedMutexGuard, End> {
    let mut step = Duration::ZERO;
    loop {
        let mutex = NamedMutex::open(lock).map_err(|error| End::Failed(format!("cannot open the mutex {lock}: {error}")))?;
        match mutex.acquire_owned(step) {
            Ok(guard) => return Ok(guard),
            Err(AcquireError::Timeout) => {}
            Err(AcquireError::Failed(error)) => return Err(End::Failed(format!("cannot take the mutex {lock}: {error}"))),
        }
        if step.is_zero() {
            say(format!("gate: remote waiting for the worktree {name}: another run holds it"));
        }
        if wait.interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
        let left = wait.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(End::Failed(format!(
                "another run held the worktree {name} for more than {} s",
                wait.limit.as_secs()
            )));
        }
        step = left.min(WAIT_STEP);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folder, mutex or thread cannot be had must fail loudly")]
pub(super) mod tests {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use gate_router::FolderId;
    use gate_state::StateRoot;
    use gate_win32::NamedMutex;

    use super::{Folder, Locked, held, lock_of};
    use crate::remote::client::End;

    /// The state root of the test `name`'s own, so no other test's lock is its.
    fn state(name: &str) -> StateRoot {
        StateRoot::new(&format!(r"C:\gate-worktree-lock-{name}-{}", std::process::id()), None)
    }

    /// A new folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-worktree-lock-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// The worktree `name` at `root`, with the id the route reads for its folder now.
    pub(in crate::remote) fn routed<'a>(name: &'a str, root: &'a str) -> Folder<'a> {
        Folder {
            name,
            root,
            id: crate::route::root_id(root).expect("the folder's id is read"),
        }
    }

    /// The name of the lock of the folder at `root`, as the route reads its id now.
    pub(in crate::remote) fn lock_name(state: &StateRoot, root: &str) -> String {
        lock_of(state, root, routed("", root).id).expect("the folder's id is read").lock
    }

    /// The lock `lock` held by another run, a thread of its own, until the sender it hands back is sent to; the
    /// sync-back's tests hold a worktree's lock through it too.
    pub(in crate::remote) fn held_elsewhere(lock: &str) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (holding, is_held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let lock = lock.to_owned();
        let holder = thread::spawn(move || {
            let mutex = NamedMutex::open(&lock).expect("the lock can be opened");
            let guard = mutex.acquire_owned(Duration::ZERO).expect("the lock is free");
            holding.send(()).expect("the test waits for the hold");
            released.recv().expect("the test lets go");
            drop(guard);
        });
        is_held.recv().expect("the other run holds the lock");
        (release, holder)
    }

    #[test]
    fn a_run_whose_main_is_another_runs_sibling_waits_for_it() {
        let state = state("main");
        let shared = folder("main-shared");
        let other = folder("main-other");
        let (shared_root, other_root) = (shared.to_string_lossy().into_owned(), other.to_string_lossy().into_owned());
        let lock = lock_name(&state, &shared_root);
        let (release, holder) = held_elsewhere(&lock);
        let mut said = Vec::new();

        // This run's main worktree is the folder the other run carries as a sibling, spelled in upper case.
        let upper = shared_root.to_uppercase();
        let worktrees = [routed("app", &upper), routed("tools", &other_root)];
        let guards = held(&state, &worktrees, Duration::from_secs(3600), None, |line| {
            said.push(line);
            // The other run lets go once the wait is said, so the wait ends on its release and not on a clock.
            release.send(()).expect("the other run is still there");
        })
        .expect("every lock is taken");

        holder.join().expect("the other run ends");
        assert_eq!(said, ["gate: remote waiting for the worktree app: another run holds it"]);
        let names: Vec<&str> = guards.iter().map(Locked::name).collect();
        assert!(names.contains(&lock.as_str()) && names.len() == 2, "{names:?}");
        drop(guards);
        std::fs::remove_dir_all(&shared).expect("the folder can be removed");
        std::fs::remove_dir_all(&other).expect("the folder can be removed");
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn two_spellings_of_one_folder_share_one_lock() {
        let state = state("spellings");
        let real = folder("spellings");
        let link = real.with_extension("junction");
        if link.exists() {
            std::fs::remove_dir(&link).expect("an earlier run's junction is removed");
        }
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
        let real_root = real.to_string_lossy().into_owned();
        let link_root = link.to_string_lossy().into_owned();

        let lock = lock_name(&state, &real_root);
        let through = lock_name(&state, &link_root);
        let worktrees = [routed("app", &real_root), routed("app-link", &link_root)];
        let guards = held(&state, &worktrees, Duration::ZERO, None, |_| {}).expect("the lock is taken");

        assert_eq!(through, lock, "the junction names the same folder");
        assert_eq!(guards.len(), 1, "one folder, one lock");
        drop(guards);
        std::fs::remove_dir(&link).expect("the junction can be removed");
        std::fs::remove_dir_all(&real).expect("the folder can be removed");
    }

    #[test]
    fn a_worktree_whose_folder_is_not_the_routes_is_refused_before_its_lock() {
        let state = state("other-id");
        let shared = folder("other-id");
        let root = shared.to_string_lossy().into_owned();
        let mut worktree = routed("app", &root);
        worktree.id = FolderId {
            volume: worktree.id.volume ^ 1,
            ..worktree.id
        };
        let mut said = Vec::new();

        let ended = held(&state, &[worktree], Duration::ZERO, None, |line| said.push(line));

        let refused = format!("the worktree {root} is not the folder the route was decided for");
        assert!(
            matches!(&ended, Err(End::Failed(why)) if *why == refused),
            "{:?}",
            ended.as_ref().map(Vec::len)
        );
        assert!(said.is_empty(), "no lock is waited for: {said:?}");
        std::fs::remove_dir_all(&shared).expect("the folder can be removed");
    }

    #[test]
    fn a_worktree_lock_wait_ends_at_the_attempts_deadline() {
        let state = state("deadline");
        let shared = folder("deadline");
        let root = shared.to_string_lossy().into_owned();
        let lock = lock_name(&state, &root);
        let (release, holder) = held_elsewhere(&lock);
        let mut said = Vec::new();

        let ended = held(&state, &[routed("lib", &root)], Duration::ZERO, None, |line| said.push(line));

        release.send(()).expect("the other run is still there");
        holder.join().expect("the other run ends");
        assert!(
            matches!(&ended, Err(End::Failed(why)) if why == "another run held the worktree lib for more than 0 s"),
            "{:?}",
            ended.as_ref().map(Vec::len)
        );
        assert_eq!(said, ["gate: remote waiting for the worktree lib: another run holds it"]);
        drop(ended);
        std::fs::remove_dir_all(&shared).expect("the folder can be removed");
    }
}

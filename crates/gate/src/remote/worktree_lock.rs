//! The lock of a worktree a sync-back writes, the main one's or a pair's sibling's: a kernel mutex named for the
//! worktree folder's id ([`KernelObject::Worktree`]), read by opening its root at lock time, so two runs that write one
//! folder, under one spelling of its path or two, never write it at once. A run takes its worktrees' locks in the order
//! of their ids, so two runs never each hold one the other waits for, and waits for one another run holds, saying so
//! once, until the attempt's git limit has passed.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use gate_state::{KernelObject, StateRoot};
use gate_win32::{AcquireError, Interrupt, NamedMutex, OwnedMutexGuard};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use super::client::End;

/// How long each wait for a worktree's lock another run holds lasts before the interrupt and the deadline are looked at
/// again.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// The folder at `root` opened for reading, a link or a junction on the way followed, sharing everything.
fn opened(root: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(Path::new(root))
}

/// The id of the worktree folder at `root` and the name of its lock under the state root `state`; else why its id
/// cannot be read.
fn lock_of(state: &StateRoot, root: &str) -> Result<((u64, [u8; 16]), String), End> {
    let unreadable = |error: io::Error| End::Failed(format!("cannot read the id of the worktree folder {root}: {error}"));
    let (volume, file) = opened(root).and_then(|folder| gate_win32::file_id(&folder)).map_err(unreadable)?;
    Ok(((volume, file), state.object_name(KernelObject::Worktree { volume, file })))
}

/// The locks of `worktrees`, each its name and its root, under the state root `state`, taken in the order of their
/// folders' ids, one per folder however many spellings name it, and held until the guards are dropped. A lock another
/// run holds is waited for, `say` given `gate: remote waiting for the worktree <name>: another run holds it` once, until
/// `limit` has passed since the first lock was asked for; a lock left by a holder that ended is taken. The interrupt
/// `interrupt` ends a wait as the run's interrupt; a folder whose id cannot be read, a mutex that cannot be opened or
/// taken, and a wait past `limit` are the run's failure.
pub(super) fn held(
    state: &StateRoot,
    worktrees: &[(&str, &str)],
    limit: Duration,
    interrupt: Option<Interrupt>,
    mut say: impl FnMut(String),
) -> Result<Vec<OwnedMutexGuard>, End> {
    let deadline = Instant::now() + limit;
    let mut locks = Vec::with_capacity(worktrees.len());
    for (name, root) in worktrees {
        let (id, lock) = lock_of(state, root)?;
        locks.push((id, lock, *name));
    }
    locks.sort_by_key(|(id, _, _)| *id);
    locks.dedup_by_key(|(id, _, _)| *id);
    let wait = Wait { deadline, limit, interrupt };
    locks.iter().map(|(_, lock, name)| taken(lock, name, &wait, &mut say)).collect()
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
mod tests {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use gate_state::StateRoot;
    use gate_win32::{NamedMutex, OwnedMutexGuard};

    use super::{held, lock_of};
    use crate::remote::client::End;

    /// The state root of the test `name`'s own, so no other test's lock is its.
    fn state(name: &str) -> StateRoot {
        StateRoot::new(&format!(r"C:\gate-worktree-lock-{name}-{}", std::process::id()), None)
    }

    /// A new folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-worktree-lock-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// The lock `lock` held by another run, a thread of its own, until the sender it hands back is sent to.
    fn held_elsewhere(lock: &str) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
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
        let (_, lock) = lock_of(&state, &shared_root).expect("the folder's id is read");
        let (release, holder) = held_elsewhere(&lock);
        let mut said = Vec::new();

        // This run's main worktree is the folder the other run carries as a sibling, spelled in upper case.
        let worktrees = [("app", shared_root.to_uppercase()), ("tools", other_root.clone())];
        let named: Vec<(&str, &str)> = worktrees.iter().map(|(name, root)| (*name, root.as_str())).collect();
        let guards = held(&state, &named, Duration::from_secs(3600), None, |line| {
            said.push(line);
            // The other run lets go once the wait is said, so the wait ends on its release and not on a clock.
            release.send(()).expect("the other run is still there");
        })
        .expect("every lock is taken");

        holder.join().expect("the other run ends");
        assert_eq!(said, ["gate: remote waiting for the worktree app: another run holds it"]);
        let names: Vec<&str> = guards.iter().map(OwnedMutexGuard::name).collect();
        assert!(names.contains(&lock.as_str()) && names.len() == 2, "{names:?}");
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

        let (_, lock) = lock_of(&state, &real_root).expect("the folder's id is read");
        let (_, through) = lock_of(&state, &link_root).expect("the junction's folder's id is read");
        let guards = held(&state, &[("app", &real_root), ("app-link", &link_root)], Duration::ZERO, None, |_| {}).expect("the lock is taken");

        assert_eq!(through, lock, "the junction names the same folder");
        assert_eq!(guards.len(), 1, "one folder, one lock");
        drop(guards);
        std::fs::remove_dir(&link).expect("the junction can be removed");
        std::fs::remove_dir_all(&real).expect("the folder can be removed");
    }

    #[test]
    fn a_worktree_lock_wait_ends_at_the_attempts_deadline() {
        let state = state("deadline");
        let shared = folder("deadline");
        let root = shared.to_string_lossy().into_owned();
        let (_, lock) = lock_of(&state, &root).expect("the folder's id is read");
        let (release, holder) = held_elsewhere(&lock);
        let mut said = Vec::new();

        let ended = held(&state, &[("lib", &root)], Duration::ZERO, None, |line| said.push(line));

        release.send(()).expect("the other run is still there");
        holder.join().expect("the other run ends");
        assert!(
            matches!(&ended, Err(End::Failed(why)) if why == "another run held the worktree lib for more than 0 s"),
            "{:?}",
            ended.as_ref().map(Vec::len)
        );
        assert_eq!(said, ["gate: remote waiting for the worktree lib: another run holds it"]);
        std::fs::remove_dir_all(&shared).expect("the folder can be removed");
    }
}

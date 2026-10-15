//! The lock of a pair's sibling: a kernel mutex named for a hash of the sibling's root, which a routed run holds around
//! what it does in that sibling's worktree, so two runs that carry one sibling never write it at once. A run takes the
//! locks of its siblings in the order of their roots, so two runs that carry the same siblings never each hold one the
//! other waits for, and waits for one another run holds, saying so once.

use std::time::Duration;

use gate_state::{StateRoot, key_hash};
use gate_win32::{AcquireError, Interrupt, NamedMutex, OwnedMutexGuard};

use super::client::End;

/// How long each wait for a sibling's lock another run holds lasts before the interrupt is looked at again.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// The mutex of the sibling whose root is `root`, under the state root `state`: `Local\gate-sibling-<hash>`, the
/// state root's hash after `gate-` as for every other object but under the default root, and `<hash>` the 16 hex
/// digits of the SHA-256 of the root in lower case that a routed run's key mutex is named by.
fn lock_name(state: &StateRoot, root: &str) -> String {
    let suffix = format!("sibling-{:016x}", key_hash(root));
    match state.hash() {
        None => format!(r"Local\gate-{suffix}"),
        Some(hash) => format!(r"Local\gate-{hash}-{suffix}"),
    }
}

/// The locks of `siblings`, each its name and its root, under the state root `state`, taken in the order of their roots
/// in lower case and held until the guards are dropped. A lock another run holds is waited for, `say` given
/// `gate: remote waiting for sibling <name>: another run holds it` once; a lock left by a holder that ended is taken.
/// The interrupt `interrupt` ends a wait as the run's interrupt; a mutex that cannot be opened or taken is the run's
/// failure.
pub(super) fn held(
    state: &StateRoot,
    siblings: &[(&str, &str)],
    interrupt: Option<Interrupt>,
    mut say: impl FnMut(String),
) -> Result<Vec<OwnedMutexGuard>, End> {
    let mut ordered = siblings.to_vec();
    ordered.sort_by_key(|(_, root)| root.to_lowercase());
    ordered
        .iter()
        .map(|(name, root)| taken(&lock_name(state, root), name, interrupt, &mut say))
        .collect()
}

/// The mutex `lock` of the sibling `name`, taken at once when it is free, else after the wait [`held`] makes.
fn taken(lock: &str, name: &str, interrupt: Option<Interrupt>, say: &mut impl FnMut(String)) -> Result<OwnedMutexGuard, End> {
    let mut wait = Duration::ZERO;
    loop {
        let mutex = NamedMutex::open(lock).map_err(|error| End::Failed(format!("cannot open the mutex {lock}: {error}")))?;
        match mutex.acquire_owned(wait) {
            Ok(guard) => return Ok(guard),
            Err(AcquireError::Timeout) => {}
            Err(AcquireError::Failed(error)) => return Err(End::Failed(format!("cannot take the mutex {lock}: {error}"))),
        }
        if wait.is_zero() {
            say(format!("gate: remote waiting for sibling {name}: another run holds it"));
            wait = WAIT_STEP;
        }
        if interrupt.is_some_and(Interrupt::is_raised) {
            return Err(End::Interrupted);
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose mutex or thread cannot be had must fail loudly")]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use gate_state::StateRoot;
    use gate_win32::{NamedMutex, OwnedMutexGuard};

    use super::{held, lock_name};

    #[test]
    fn sibling_locks_are_taken_in_sorted_order_and_a_held_one_is_waited_for() {
        let state = StateRoot::new(&format!(r"C:\gate-sibling-lock-tests-{}", std::process::id()), None);
        let siblings = [("tools", r"X:\dev\Tools"), ("lib", r"X:\dev\lib")];
        let lib = lock_name(&state, r"X:\dev\lib");
        assert!(lib.starts_with(r"Local\gate-") && lib.contains("-sibling-"), "{lib}");
        assert_eq!(lib, lock_name(&state, r"x:\DEV\LIB"), "a root's case does not change its lock");
        let (holding, is_held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let holder = {
            let lib = lib.clone();
            thread::spawn(move || {
                let mutex = NamedMutex::open(&lib).expect("the lock can be opened");
                let guard = mutex.acquire_owned(Duration::ZERO).expect("the lock is free");
                holding.send(()).expect("the test waits for the hold");
                released.recv().expect("the test lets go");
                drop(guard);
            })
        };
        is_held.recv().expect("the other run holds lib's lock");
        let mut said = Vec::new();

        let guards = held(&state, &siblings, None, |line| {
            said.push(line);
            // The other run lets go once the wait is said, so the wait ends on its release and not on a clock.
            release.send(()).expect("the other run is still there");
        })
        .expect("every lock is taken");

        holder.join().expect("the other run ends");
        let names: Vec<&str> = guards.iter().map(OwnedMutexGuard::name).collect();
        assert_eq!(
            names,
            [lib.as_str(), lock_name(&state, r"X:\dev\Tools").as_str()],
            "lib's root sorts first"
        );
        assert_eq!(said, ["gate: remote waiting for sibling lib: another run holds it"]);
    }
}

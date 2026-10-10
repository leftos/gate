//! Named mutexes: one owner at a time across handles, and an abandoned mutex taken as owned.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot open its mutex or join its thread must fail loudly")]

use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gate_win32::{AcquireError, NamedMutex};

/// A mutex name no other test or process uses.
fn unique_name(test: &str) -> String {
    format!(r"Local\gate-win32-test-{}-{test}", std::process::id())
}

#[test]
fn journal_mutex_is_exclusive_across_handles() {
    let name = unique_name("exclusive");
    let mine = NamedMutex::open(&name).expect("the mutex opens");
    let guard = mine.acquire(Duration::from_secs(5)).expect("a free mutex is taken");
    assert!(!guard.abandoned());
    let (busy, saw_busy) = mpsc::channel();
    let (go, wait_go) = mpsc::channel::<()>();
    let other = thread::spawn(move || {
        let theirs = NamedMutex::open(&name).expect("a second handle opens");
        let refused = matches!(theirs.acquire(Duration::from_millis(100)), Err(AcquireError::Timeout));
        busy.send(refused).expect("the test waits for the answer");
        wait_go.recv().expect("the test says when the mutex is released");
        theirs.acquire(Duration::from_secs(30)).map(|taken| taken.abandoned()).ok()
    });

    assert!(
        saw_busy.recv().expect("the other thread answers"),
        "a held mutex is not taken through another handle"
    );
    drop(guard);
    go.send(()).expect("the other thread waits");
    assert_eq!(
        other.join().expect("the other thread ends"),
        Some(false),
        "once released it is taken through the other handle, not abandoned"
    );
}

#[test]
fn abandoned_mutex_counts_as_owned() {
    let name = unique_name("abandoned");
    // Kept open so the mutex outlives the thread that abandons it.
    let mine = NamedMutex::open(&name).expect("the mutex opens");
    thread::spawn(move || {
        let theirs = NamedMutex::open(&name).expect("a second handle opens");
        // The guard is never dropped, so the thread ends owning the mutex.
        let _held = ManuallyDrop::new(theirs.acquire(Duration::from_secs(5)).expect("a free mutex is taken"));
    })
    .join()
    .expect("the abandoning thread ends");

    let guard = mine.acquire(Duration::from_secs(5)).expect("an abandoned mutex counts as owned");

    assert!(guard.abandoned());
}

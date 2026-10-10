//! Named mutexes: one owner at a time across handles, and an abandoned mutex taken as owned.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot open its mutex or join its thread must fail loudly")]

use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gate_win32::{AcquireError, NamedEvent, NamedMutex, mutex_exists};

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

#[test]
fn an_owned_guard_holds_the_mutex_until_dropped() {
    let name = unique_name("owned-holds");
    let mutex = NamedMutex::open(&name).expect("the mutex opens");
    let guard = mutex.acquire_owned(Duration::from_secs(5)).expect("a free mutex is taken");
    assert!(!guard.abandoned());
    assert_eq!(guard.name(), name);

    let (busy, saw_busy) = mpsc::channel();
    let (go, wait_go) = mpsc::channel::<()>();
    let other = thread::spawn(move || {
        let theirs = NamedMutex::open(&name).expect("a second handle opens");
        let refused = matches!(theirs.acquire(Duration::ZERO), Err(AcquireError::Timeout));
        busy.send(refused).expect("the test waits for the answer");
        wait_go.recv().expect("the test says when the guard is dropped");
        theirs.acquire(Duration::from_secs(30)).map(|taken| taken.abandoned()).ok()
    });

    assert!(saw_busy.recv().expect("the other thread answers"), "a guard that owns its mutex holds it");
    drop(guard);
    go.send(()).expect("the other thread waits");
    assert_eq!(
        other.join().expect("the other thread ends"),
        Some(false),
        "once the owning guard is dropped the mutex is taken through the other handle, not abandoned"
    );
}

#[test]
fn an_owned_guard_reports_an_abandoned_mutex() {
    let name = unique_name("owned-abandoned");
    // Kept open so the mutex outlives the thread that abandons it.
    let mine = NamedMutex::open(&name).expect("the mutex opens");
    thread::spawn(move || {
        let theirs = NamedMutex::open(&name).expect("a second handle opens");
        // The guard is never dropped, so the thread ends owning the mutex.
        let _held = ManuallyDrop::new(theirs.acquire(Duration::from_secs(5)).expect("a free mutex is taken"));
    })
    .join()
    .expect("the abandoning thread ends");

    let guard = mine.acquire_owned(Duration::from_secs(5)).expect("an abandoned mutex counts as owned");

    assert!(guard.abandoned());
}

#[test]
fn a_mutex_whose_dead_owner_left_no_other_handle_comes_back_fresh() {
    let name = unique_name("owned-fresh");
    let thread_name = name.clone();
    thread::spawn(move || {
        let theirs = NamedMutex::open(&thread_name).expect("the mutex opens");
        // Nothing releases the guard, and the `drop` below closes the mutex's only handle, so the kernel frees the
        // mutex rather than leaving it abandoned for the next opener.
        let _held = ManuallyDrop::new(theirs.acquire(Duration::from_secs(5)).expect("a free mutex is taken"));
        drop(theirs);
    })
    .join()
    .expect("the owning thread ends");

    let theirs = NamedMutex::open(&name).expect("a fresh mutex opens under the same name");
    let guard = theirs.acquire_owned(Duration::from_secs(5)).expect("a fresh mutex is taken");

    assert!(!guard.abandoned(), "a mutex whose only handle closed comes back fresh, not abandoned");
}

#[test]
fn a_name_nobody_holds_does_not_exist() {
    let name = unique_name("no-holder");

    assert!(!mutex_exists(&name).expect("the probe answers"), "a name nothing has opened has no mutex");
}

#[test]
fn a_held_mutex_exists_and_a_released_one_closed_by_everyone_does_not() {
    let name = unique_name("held");
    let mine = NamedMutex::open(&name).expect("the mutex opens");
    let guard = mine.acquire_owned(Duration::from_secs(5)).expect("a free mutex is taken");

    assert!(mutex_exists(&name).expect("the probe answers"), "a mutex exists while a handle holds it");
    drop(guard);
    assert!(
        !mutex_exists(&name).expect("the probe answers"),
        "once every handle is closed the mutex is gone"
    );
}

#[test]
fn a_name_held_by_another_object_type_is_an_error() {
    let name = unique_name("wrong-type");
    let _event = NamedEvent::create(&name).expect("the event is created");

    assert!(mutex_exists(&name).is_err(), "a name another object type holds is no mutex");
}

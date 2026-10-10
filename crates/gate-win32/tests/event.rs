//! Named auto-reset events: one waiter woken per set, and a set kept until that waiter wakes.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot open its event must fail loudly")]

use std::time::Duration;

use gate_win32::{EventWait, NamedEvent, NamedMutex};

/// An event name no other test or process uses.
fn unique_name(test: &str) -> String {
    format!(r"Local\gate-win32-test-{}-{test}", std::process::id())
}

#[test]
fn a_set_before_the_wait_is_not_lost() {
    let event = NamedEvent::create(&unique_name("preset")).expect("the event is created");
    event.set().expect("the event is set");

    assert_eq!(event.wait(Duration::ZERO).expect("the wait answers"), EventWait::Signaled);
}

#[test]
fn an_auto_reset_event_wakes_one_wait() {
    let event = NamedEvent::create(&unique_name("one-wake")).expect("the event is created");
    event.set().expect("the event is set");

    assert_eq!(event.wait(Duration::ZERO).expect("the wait answers"), EventWait::Signaled);
    assert_eq!(
        event.wait(Duration::ZERO).expect("the wait answers"),
        EventWait::TimedOut,
        "an auto-reset event keeps one wake"
    );
}

#[test]
fn a_waker_signals_the_waiters_event_through_its_own_handle() {
    let name = unique_name("waker");
    let waited = NamedEvent::create(&name).expect("the event is created");
    let waker = NamedEvent::open_existing(&name).expect("the open answers").expect("the event exists");
    waker.set().expect("the event is set through the waker's own handle");

    assert_eq!(waited.wait(Duration::ZERO).expect("the wait answers"), EventWait::Signaled);
}

#[test]
fn open_existing_does_not_create() {
    let name = unique_name("absent");

    assert!(
        NamedEvent::open_existing(&name).expect("the open answers").is_none(),
        "an unused name opens no event"
    );
}

#[test]
fn a_wait_times_out() {
    let event = NamedEvent::create(&unique_name("timeout")).expect("the event is created");

    assert_eq!(event.wait(Duration::from_millis(10)).expect("the wait answers"), EventWait::TimedOut);
}

#[test]
fn open_existing_on_a_name_held_by_a_mutex_is_an_error() {
    let name = unique_name("mutex-holds");
    let _mutex = NamedMutex::open(&name).expect("the mutex opens");

    assert!(NamedEvent::open_existing(&name).is_err(), "a name another object type holds is no event");
}

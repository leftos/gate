//! The start time of another process: a running one has the start the process reads for itself, a gone one has none.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its child must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a test starts its child directly: what it checks is the child itself"
)]

use std::process::{Child, Command};

use gate_win32::{process_start, process_start_of};

/// A child that exits at once with status 0.
fn quick_child() -> Child {
    Command::new("cmd").args(["/c", "exit 0"]).spawn().expect("cmd starts")
}

/// Waits for `child` to exit with status 0.
fn wait_out(child: &mut Child) {
    let status = child.wait().expect("the child can be waited for");
    assert!(status.success(), "{status}");
}

#[test]
fn this_process_start_matches_process_start() {
    let start = process_start().expect("this process's start can be read");

    assert_eq!(process_start_of(std::process::id()).expect("this process can be queried"), Some(start));
}

#[test]
fn a_pid_that_does_not_exist_has_no_start() {
    // Windows pids are multiples of 4, so no process ever has this one.
    let pid = u32::MAX - 1;

    assert_eq!(process_start_of(pid).expect("a pid no process has is no error"), None);
}

#[test]
fn an_exited_child_has_no_start() {
    let mut child = quick_child();
    wait_out(&mut child);

    assert_eq!(process_start_of(child.id()).expect("an exited child is no error"), None);
}

#[test]
fn a_process_that_exited_259_is_not_running() {
    let mut child = Command::new("cmd").args(["/c", "exit 259"]).spawn().expect("cmd starts");
    let status = child.wait().expect("the child can be waited for");
    assert_eq!(status.code(), Some(259), "{status}");

    // The child is gone, and `Child` still holds its handle, so its exit code, `STILL_ACTIVE`, stays readable.
    assert_eq!(process_start_of(child.id()).expect("an exited child is no error"), None);
}

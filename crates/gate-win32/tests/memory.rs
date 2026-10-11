//! A running process's working set: this process's own is some positive number of bytes, a pid no process has has none,
//! and a process that has exited has none however long its handle is held.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot read a process's memory must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a test starts its child directly: what it checks is the child itself"
)]

use std::process::Command;

use gate_win32::working_set;

#[test]
fn this_process_has_a_working_set() {
    let bytes = working_set(std::process::id()).expect("this process can be opened");

    assert!(bytes.is_some_and(|bytes| bytes > 0), "{bytes:?}");
}

#[test]
fn a_pid_that_does_not_exist_has_no_working_set() {
    // The kernel ignores a pid's low two bits, so this is looked up as 0xFFFFFFFC, which no process has.
    assert_eq!(working_set(u32::MAX - 1).expect("a pid no process has is no error"), None);
}

#[test]
fn an_exited_child_whose_handle_is_held_has_no_working_set() {
    let mut child = Command::new("cmd").args(["/c", "exit 0"]).spawn().expect("cmd starts");
    assert!(child.wait().expect("the child can be waited for").success());

    // The child is gone, and `Child` still holds its handle, so it is open but no longer running.
    assert_eq!(working_set(child.id()).expect("an exited child is no error"), None);
}

#[test]
fn the_working_set_of_an_exited_259_process_is_none() {
    let mut child = Command::new("cmd").args(["/c", "exit 259"]).spawn().expect("cmd starts");
    let status = child.wait().expect("the child can be waited for");
    assert_eq!(status.code(), Some(259), "{status}");

    // The child is gone, and `Child` still holds its handle, so its exit code, `STILL_ACTIVE`, stays readable.
    assert_eq!(working_set(child.id()).expect("an exited child is no error"), None);
}

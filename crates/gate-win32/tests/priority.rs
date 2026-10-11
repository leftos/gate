//! A process's priority class read and set: what the build servers' rule moves a server between.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot start or wait for its child must fail loudly")]
#![expect(
    clippy::disallowed_methods,
    reason = "a test starts its child directly: what it checks is the child's own class"
)]

use std::process::{Child, Command, Stdio};

use gate_win32::{Priority, priority_of, set_priority};

/// A child of this test that lives until it is killed: `ping` is nobody's build server and touches nothing. It is
/// started directly, so killing the child ends `ping` itself.
fn child() -> Child {
    Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .expect("ping starts")
}

#[test]
fn a_child_s_priority_class_is_read_and_set() {
    let mut child = child();
    let pid = child.id();

    // A child of this test starts at whatever class this test does, so the case sets every class it reads.
    assert!(
        priority_of(pid).expect("the child can be queried").is_some(),
        "a running child has a class"
    );
    assert_eq!(set_priority(pid, Priority::Normal).expect("the class can be set"), Some(()));
    assert_eq!(priority_of(pid).expect("the child can be queried"), Some(Priority::Normal));
    assert_eq!(set_priority(pid, Priority::BelowNormal).expect("the class can be set"), Some(()));
    assert_eq!(priority_of(pid).expect("the child can be queried"), Some(Priority::BelowNormal));
    assert_eq!(set_priority(pid, Priority::High).expect("the class can be set"), Some(()));
    assert_eq!(priority_of(pid).expect("the child can be queried"), Some(Priority::High));
    assert_eq!(set_priority(pid, Priority::Idle).expect("the class can be set"), Some(()));
    assert_eq!(priority_of(pid).expect("the child can be queried"), Some(Priority::Idle));

    child.kill().expect("the child can be killed");
    child.wait().expect("the killed child can be waited for");
}

#[test]
fn a_pid_no_process_has_has_no_priority_class() {
    // The kernel ignores a pid's low two bits, so this is looked up as 0xFFFFFFFC, which no process has.
    let gone = u32::MAX - 1;
    assert_eq!(priority_of(gone).expect("a pid no process has is no error"), None);
    assert_eq!(set_priority(gone, Priority::High).expect("a pid no process has is no error"), None);
}

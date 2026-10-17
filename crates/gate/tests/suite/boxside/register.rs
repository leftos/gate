//! `gate box register-desktop`, the registration of the desktop helper's task, through its seam: the success line and
//! exit 0, the failure line and exit 1, never a read of stdin, and the box usage for a `--stdin` after it. The seam stands
//! in for the Task Scheduler, so no case touches this machine's tasks.

use super::{BOX_USAGE, bare_box_command};
use crate::support::{Case, Outcome, finish, start_with_stdin};

/// `gate box` with `args` after `box`, run to its end with its stdin left open, so a box that read it would never end.
fn register(case: &Case, args: &[&str]) -> Outcome {
    let mut words = vec!["box"];
    words.extend_from_slice(args);
    let (running, stdin) = start_with_stdin(bare_box_command(case, &words));
    let outcome = running.finish();
    drop(stdin);
    outcome
}

#[test]
fn the_seam_s_ok_says_the_task_is_registered_for_the_running_user() {
    let case = Case::new("register_ok");
    let user = gate_win32::current_user_sid().expect("the running user is read");

    let outcome = register(&case, &["--test-box-register", "ok", "register-desktop"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("gate-register: registered \\gate\\desktop-helper for {user}\n"),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

#[test]
fn the_seam_s_fail_says_why_and_exits_1() {
    let case = Case::new("register_fail");

    let outcome = register(&case, &["--test-box-register=fail", "register-desktop"]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate-register: failed: the test seam refused\n", "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

#[test]
fn register_desktop_with_stdin_is_the_box_usage() {
    let case = Case::new("register_stdin");

    let outcome = finish(bare_box_command(
        &case,
        &["box", "--test-box-register", "ok", "register-desktop", "--stdin"],
    ));

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stderr, BOX_USAGE, "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
}

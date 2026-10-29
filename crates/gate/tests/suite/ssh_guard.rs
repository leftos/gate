//! The suite's ssh guard: a copy of the stand-in, named `ssh.exe` and put first on every case's `PATH`, so a case that
//! reaches ssh without naming a stand-in of its own runs the guard, which fails at once, never the machine's own ssh.

use crate::routing::{heavy_words_with_limit, routed_case};
use crate::support::{finish, log_texts};

/// What the suite's ssh guard says when a case reaches it: fake-ssh with no steps file named, so a guard replaced by any
/// other ssh fails the case rather than passing it.
const SSH_GUARD_LINE: &str = "fake-ssh: GATE_FAKE_SSH_STEPS: environment variable not found";

/// What the gate says of the guard's own exit status 2, which this machine's ssh never gives: the preflight reached the
/// guard and the gate read its answer.
const GUARD_ANSWERED_LINE: &str = "gate: not routed: builder has no gate box that takes this run (preflight status 2)";

/// A routed case that names no remote seam reaches the ssh first on its PATH: the suite's guard, which fails the
/// preflight at once, never the machine's own ssh, which on a builder whose ssh blocks writing to a piped stderr holds
/// the preflight to its stall window.
#[test]
fn a_routed_case_without_a_remote_seam_reaches_the_suite_s_ssh_guard() {
    let case = routed_case("routing-ssh-guard");

    let outcome = finish(case.gate(&heavy_words_with_limit("out.log", &[], &[r".\tool.exe", "out", "ran here"])));

    let log = log_texts(&case.folder().join("out.log"));
    assert!(log.contains(SSH_GUARD_LINE), "the guard itself answered the preflight: {log} {outcome:?}");
    assert!(log.contains(GUARD_ANSWERED_LINE), "the gate read the guard's answer: {log} {outcome:?}");
    assert!(!log.contains("the preflight stalled"), "{log}");
}

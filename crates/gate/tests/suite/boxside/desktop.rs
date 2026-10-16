//! A run that asks for the builder's desktop, on the box: the console's session refused before the checkout, the
//! builder's desktop lock waited for after it, each refusal's `desktop` tag and exit 75 with no back commits line, and
//! the payloads refused with exit 2.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::{
    box_call, field, gate_box, lines_starting, pair_payload, pair_snapshot, payload as request_line, refused_line, run_box, run_box_with_seams,
    snapshot, started_line, stopped_line_for,
};
use crate::support::{Case, FAKE_TOOL, HeldDesktop, KeptDesktop, Outcome, box_payload, desktop_payload, desktop_seams};

/// The box's prefix of every line refusing a run the builder's desktop cannot take.
const NO_DESKTOP: &str = "gate: not routed: the builder has no desktop session: ";

/// A snapshot no refusal before the checkout ever fetches.
const UNFETCHED: &str = "0123456789abcdef0123456789abcdef01234567";

/// The command of every desktop case: it never runs, as no run gets past the stage.
const COMMAND: &[&str] = &[FAKE_TOOL, "out", "ran"];

/// The cap on every desktop wait of a case, in milliseconds.
const DESKTOP_MS: &str = "50";

/// The line the box says when another run held the builder's desktop for the whole of a one-second wait.
const BUSY_FOR_1_S: &str = "gate: not routed: the builder's desktop stayed busy for 1 s";

/// The line the box says once when another run holds the builder's desktop as it starts waiting.
const BUSY_WAIT: &str = "gate: remote waiting for the builder's desktop: another run holds it";

/// The line the box says when it takes the builder's desktop from a run that ended holding it.
const ABANDONED: &str = "gate: remote took the builder's desktop from a run that ended without releasing it";

/// `gate box run` of `payload` with the desktop seams of `case`: the fault `ok` and the console session `session`.
fn run_desktop(case: &Case, payload: &Value, session: &str) -> Outcome {
    let seams = desktop_seams(case, "ok", session, DESKTOP_MS);
    let seams: Vec<&str> = seams.iter().map(String::as_str).collect();
    run_box_with_seams(case, payload, &seams)
}

/// The desktop payload of `case`'s work folder `k` with a snapshot no refusal before the checkout fetches, and the
/// folder its work folder's lock is taken in made, as a preflight leaves it.
fn unfetched_payload(case: &Case) -> Value {
    let mut payload = desktop_payload(case, "k", COMMAND, 30);
    payload["Snapshot"] = json!(UNFETCHED);
    let lock_folder = Path::new(field(&payload, "Lock")).parent().expect("the lock is in a folder").to_owned();
    fs::create_dir_all(lock_folder).expect("the work folders' folder can be made");
    payload
}

/// Checks `outcome` is the box's desktop refusal of `payload` saying `line`: exit 75, the `desktop` tag once and
/// `line` right after it, no start marker and no back commits line.
fn refused_with(outcome: &Outcome, payload: &Value, line: &str) {
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let tag = refused_line(payload, "desktop");
    assert_eq!(
        lines_starting(outcome, "gate-remote-refused:"),
        [tag.as_str()],
        "one desktop tag: {outcome:?}"
    );
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let at = lines.iter().position(|said| *said == tag).expect("the tag is said");
    assert_eq!(lines.get(at + 1).copied(), Some(line), "the refusal follows its tag: {outcome:?}");
    assert!(lines_starting(outcome, &started_line(payload)).is_empty(), "no start marker: {outcome:?}");
    assert!(
        lines_starting(outcome, "gate: remote back commits:").is_empty(),
        "no back commits line: {outcome:?}"
    );
}

/// Checks the console session `session` refuses a desktop run before the checkout with `why`.
fn console_refuses(name: &str, session: &str, why: &str) {
    let case = Case::new(name);
    let payload = unfetched_payload(&case);

    let outcome = run_desktop(&case, &payload, session);

    refused_with(&outcome, &payload, &format!("{NO_DESKTOP}{why}"));
}

#[test]
fn nobody_logged_on_refuses_desktop_before_the_checkout() {
    console_refuses("desktop_nobody", "none", "nobody is logged on at the console");
}

#[test]
fn a_locked_console_refuses_desktop() {
    console_refuses("desktop_locked", "locked", "the console session is locked");
}

#[test]
fn another_user_at_the_console_refuses_desktop() {
    console_refuses("desktop_other_user", "other-user", "another user is logged on at the console");
}

#[test]
fn a_disconnected_console_refuses_desktop() {
    console_refuses("desktop_disconnected", "disconnected", "the console session is disconnected");
}

#[test]
fn a_desktop_refusal_carries_the_tag_and_exit_75_and_no_back_commits_line_on_a_pair() {
    let case = Case::new("desktop_pair_refused");
    let mut payload = pair_payload(&case, &["lib"], COMMAND);
    payload["Desktop"] = json!(true);
    payload["DesktopWait"] = json!(1);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);

    let before = run_desktop(&case, &payload, "none");
    refused_with(&before, &payload, &format!("{NO_DESKTOP}nobody is logged on at the console"));

    let held = HeldDesktop::hold(&case);
    let after = run_desktop(&case, &payload, "active");
    drop(held);
    refused_with(&after, &payload, BUSY_FOR_1_S);
}

#[test]
fn a_desktop_run_past_the_stage_gets_the_placeholder_refusal() {
    let case = Case::new("desktop_placeholder");
    let mut payload = desktop_payload(&case, "k", COMMAND, 30);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let placeholder = format!("{NO_DESKTOP}the desktop helper did not start: the box's hand-off is not built yet");
    // Open across both runs, so a lock the first box left taken would be found abandoned by the second.
    let _kept = KeptDesktop::open(&case);

    for run in ["first", "second"] {
        let outcome = run_desktop(&case, &payload, "active");

        refused_with(&outcome, &payload, &placeholder);
        assert!(
            lines_starting(&outcome, BUSY_WAIT).is_empty(),
            "the {run} run found the lock free: {outcome:?}"
        );
        assert!(
            lines_starting(&outcome, ABANDONED).is_empty(),
            "the {run} run found the lock released, not abandoned: {outcome:?}"
        );
        assert!(!outcome.stdout.lines().any(|line| line == "ran"), "the command never ran: {outcome:?}");
    }
}

#[test]
fn a_desktop_wait_says_the_busy_line_once_and_an_abandoned_lock_is_said() {
    let case = Case::new("desktop_wait_lines");
    let mut payload = desktop_payload(&case, "k", COMMAND, 1);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let held = HeldDesktop::hold(&case);
    let busy = run_desktop(&case, &payload, "active");
    drop(held);
    refused_with(&busy, &payload, BUSY_FOR_1_S);
    assert_eq!(lines_starting(&busy, BUSY_WAIT), [BUSY_WAIT], "the busy line, once: {busy:?}");

    HeldDesktop::abandon(&case);
    let abandoned = run_desktop(&case, &payload, "active");
    assert_eq!(
        lines_starting(&abandoned, ABANDONED),
        [ABANDONED],
        "the abandoned lock, said once: {abandoned:?}"
    );
    refused_with(
        &abandoned,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper did not start: the box's hand-off is not built yet"),
    );
}

#[test]
fn a_stop_during_the_desktop_wait_on_a_pair_says_back_commits_failed() {
    let case = Case::new("desktop_stop_in_wait");
    let mut payload = pair_payload(&case, &["lib"], COMMAND);
    payload["Desktop"] = json!(true);
    payload["DesktopWait"] = json!(600);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);
    let held = HeldDesktop::hold(&case);
    let seams = desktop_seams(&case, "ok", "active", DESKTOP_MS);
    let mut args = vec!["box"];
    args.extend(seams.iter().map(String::as_str));
    args.push("run");

    let running = box_call(&case, &args, &request_line(&payload)).start();
    running.wait_for_stderr(BUSY_WAIT);
    let stopped = gate_box(&case, "stop", &request_line(&payload), None);
    let outcome = running.finish();
    drop(held);

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop during the wait fails the run: {outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    assert!(said.contains(&stopped_line_for(field(&payload, "Job")).as_str()), "{outcome:?}");
    assert!(said.contains(&"gate: remote back commits: failed"), "{outcome:?}");
    assert!(lines_starting(&outcome, "gate-remote-refused:").is_empty(), "no desktop tag: {outcome:?}");
    assert!(
        lines_starting(&outcome, &started_line(&payload)).is_empty(),
        "no start marker: {outcome:?}"
    );
}

#[test]
fn a_desktop_command_that_cannot_start_never_waits_for_the_desktop() {
    let case = Case::new("desktop_cannot_start");
    let _held = HeldDesktop::hold(&case);

    let mut missing = desktop_payload(&case, "k", &["gate-no-such-program-anywhere"], 1);
    snapshot(&case, &mut missing, &[("write.cmd", "@echo x>new.txt\r\n")]);
    let not_found = run_desktop(&case, &missing, "active");
    assert_eq!(not_found.code, Some(127), "{not_found:?}");
    assert_eq!(
        lines_starting(&not_found, "gate: could not start gate-no-such-program-anywhere").len(),
        1,
        "{not_found:?}"
    );

    let mut batch = desktop_payload(&case, "k", &[r".\write.cmd", "%PATH%"], 1);
    batch["Snapshot"] = missing["Snapshot"].clone();
    let refused = run_desktop(&case, &batch, "active");
    assert_eq!(refused.code, Some(2), "{refused:?}");
    assert_eq!(
        lines_starting(&refused, "gate: a .cmd or .bat command cannot take a word"),
        ["gate: a .cmd or .bat command cannot take a word holding a line break or a %NAME% cmd.exe would expand: %PATH%"],
        "{refused:?}"
    );

    for outcome in [&not_found, &refused] {
        assert!(lines_starting(outcome, BUSY_WAIT).is_empty(), "never waited for the desktop: {outcome:?}");
        assert!(lines_starting(outcome, "gate-remote-refused:").is_empty(), "no desktop tag: {outcome:?}");
    }
    assert!(
        !Path::new(field(&batch, "Work")).join("new.txt").exists(),
        "the batch command did not run"
    );
}

#[test]
fn desktop_true_without_a_wait_exits_2() {
    let case = Case::new("desktop_no_wait");
    let mut payload = box_payload(&case, "k", COMMAND);
    payload["Snapshot"] = json!(UNFETCHED);
    payload["Desktop"] = json!(true);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        lines_starting(&outcome, "gate: box payload "),
        ["gate: box payload lacks DesktopWait"],
        "{outcome:?}"
    );
}

#[test]
fn a_warm_up_that_asks_for_the_desktop_exits_2() {
    let case = Case::new("desktop_warm_up");
    let mut payload = unfetched_payload(&case);
    payload.as_object_mut().expect("the payload is an object").remove("Command");
    payload["WarmCommands"] = json!([COMMAND]);
    payload["WarmHash"] = json!("5f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0");
    payload["Timeout"] = json!(600);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        lines_starting(&outcome, "gate: box payload "),
        ["gate: box payload asks a warm-up for the desktop"],
        "{outcome:?}"
    );
}

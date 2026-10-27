//! `gate drain` on the PC: each form driven through the stand-in ssh to the real `gate box drain` on a builder root in
//! the case's folder, as `gate check --builder` reaches its builder.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::routing::CONTROL_C_EXIT;
use crate::support::{
    Case, FAKE_SSH, GATE, GATE_VARIABLE, Outcome, ROOT_WALK_STOP_VARIABLE, STEPS_VARIABLE, beside, calls, finish, shown, ssh_steps, start,
    write_remote_json,
};

/// The variable whose words the stand-in gives the box's `drain` before its verb.
const BOX_SEAMS_VARIABLE: &str = "GATE_FAKE_SSH_BOX_SEAMS";
/// The box seam every drain call of a case takes: the box under test reads no command line of the elevated gates
/// around it, which its drained test would count.
const READABLE_ONLY: &str = "--test-box-drain-readable-only true";
/// A fixed time a case's box calls start from, in Unix seconds: 08:00 UTC, so a barrier raised then lapses at 08:30.
const T: u64 = 1_800_000_000;
/// The routing file of a drain case: `builder-1` the host a drain goes to, `builder-2` another builder it lists.
const ROUTING: &str = r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}}"#;
/// The steps the stand-in answers a case's calls with: more than any wait of a case makes.
static ANSWERS: [&str; 2000] = ["ok"; 2000];
/// The list of a wait on one live session.
const ONE_RUN: &str = "gate: drain: waiting for 1 run(s) on the builder:";
/// The line a Ctrl+C ends a wait with.
const INTERRUPTED: &str = "gate: drain: interrupted; the barrier stands; gate drain --lift lifts it, gate drain resumes the wait";
/// The line of a wait whose call the builder did not answer, `fail-host` refusing it.
const NOT_ANSWERED: &str = "gate: drain: the builder did not answer: ssh: connect to host the builder port 22: Connection refused; still trying";

/// A case whose routing file lists `builder-1` and `builder-2`, with a folder for the stand-in's files.
fn drain_case(name: &str) -> Case {
    let case = Case::new(name);
    write_remote_json(&case, ROUTING);
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    case
}

/// The case's builder root.
fn root_of(case: &Case) -> PathBuf {
    case.folder().join("builder")
}

/// The case's local transport: the builder root spelled through a folder that is not there, which the PC's
/// `std::path::absolute` reads as the root itself, resolving the `..` without looking, so the drain's own command line
/// never names the root the box's drained test looks for.
fn transport(case: &Case) -> String {
    format!("local:{}", case.folder().join("elsewhere").join("..").join("builder").display())
}

/// `gate drain` with `extra` after its seams, through the stand-in of the steps file `steps`, its box calls given
/// `box_seams`, polling every 20 ms.
fn gate_drain(case: &Case, extra: &[&str], steps: &Path, box_seams: &str) -> Command {
    let transport = transport(case);
    let mut args = vec![
        "drain",
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-poll-ms",
        "20",
    ];
    args.extend_from_slice(extra);
    let mut command = case.gate(&args);
    command
        .env(STEPS_VARIABLE, steps)
        .env(GATE_VARIABLE, GATE)
        .env(BOX_SEAMS_VARIABLE, box_seams)
        .env(ROOT_WALK_STOP_VARIABLE, case.folder());
    command
}

/// [`gate_drain`] run to its end.
fn drained(case: &Case, extra: &[&str], steps: &Path, box_seams: &str) -> Outcome {
    finish(gate_drain(case, extra, steps, box_seams))
}

/// `gate box drain` of `action` on the case's builder root, `seams` before its verb, to its end, which passes.
fn box_drain(case: &Case, action: &str, seams: &[&str]) {
    let root = root_of(case);
    fs::create_dir_all(&root).expect("the builder root can be made");
    let request = STANDARD.encode(json!({ "Root": shown(&root), "Action": action }).to_string());
    let stop = shown(&case.folder());
    let mut args = vec!["box", "--test-box-root-walk-stop", &stop, "--test-box-drain-readable-only", "true"];
    args.extend_from_slice(seams);
    args.extend(["drain", "--stdin"]);
    let mut child = Command::new(GATE)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the box's drain can be started");
    let mut stdin = child.stdin.take().expect("the drain's stdin is piped");
    writeln!(stdin, "{request}").expect("the drain's request can be written");
    drop(stdin);
    let output = child.wait_with_output().expect("the box's drain ends");
    assert!(output.status.success(), "drain {action}: {output:?}");
}

/// The Unix second now.
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("the clock is after 1970").as_secs()
}

/// A live session on the case's builder, as an admitted box process holds one: its record, called long past its lease,
/// and its holder held while the returned file lives.
fn live_session(case: &Case) -> File {
    let sessions = root_of(case).join("drain").join("sessions");
    fs::create_dir_all(&sessions).expect("the sessions folder can be made");
    let session = json!({ "Pc": "pc1", "Repo": "app", "Command": "gate run cargo test", "Called": now() - 1000 });
    fs::write(sessions.join("s1"), session.to_string()).expect("the session record can be written");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(sessions.join(format!("s1.{}", std::process::id())))
        .expect("the holder can be held")
}

/// The generation of the barrier on the case's builder; 0 while it cannot be read.
fn generation(case: &Case) -> u64 {
    let text = fs::read_to_string(root_of(case).join("drain").join("barrier")).unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|barrier| barrier["Generation"].as_u64())
        .unwrap_or(0)
}

/// Asserts `stderr` holds the list of the one live session [`live_session`] makes.
fn assert_listed(stderr: &str) {
    let lines: Vec<&str> = stderr.lines().collect();
    let at = lines.iter().position(|line| *line == ONE_RUN).expect("the list is said");
    let session = lines.get(at + 1).copied().unwrap_or_default();
    assert!(
        session.starts_with("gate:   pc1  app  ") && session.ends_with(" s  gate run cargo test"),
        "{stderr}"
    );
}

#[test]
fn a_raise_with_nothing_running_says_raised_then_drained() {
    let case = drain_case("drain_raise_nothing_running");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &[], &steps, &format!("{READABLE_ONLY} --test-box-now {T}"));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: drain: barrier raised on the builder; it lapses at 08:30 UTC unless gate drain runs again\n\
         gate: drain: the builder is drained; gate drain --lift lifts it (it lapses at 08:30 UTC; gate drain re-raises it)\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["drain"], "one raise answered drained");
}

#[test]
fn a_raise_lists_a_live_session_then_says_drained_once_it_ends() {
    let case = drain_case("drain_raise_live_session");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);
    let running = start(gate_drain(&case, &[], &steps, READABLE_ONLY));

    running.wait_for_stderr(ONE_RUN);
    drop(holder);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_listed(&outcome.stderr);
    let last = outcome.stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("gate: drain: the builder is drained; gate drain --lift lifts it (it lapses at "),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr.matches(ONE_RUN).count(), 1, "an unchanged list is said once: {outcome:?}");
}

#[test]
fn a_status_with_no_barrier_says_none_and_open() {
    let case = drain_case("drain_status_no_barrier");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--status"], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: drain: no barrier stands on the builder\ngate: drain: 0 waiting ticket(s)\ngate: drain: state open\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["drain"]);
    assert!(!root_of(&case).exists(), "a status makes no root");
}

#[test]
fn a_status_under_a_standing_barrier_says_when_it_lapses() {
    let case = drain_case("drain_status_standing");
    box_drain(&case, "raise", &["--test-box-now", &T.to_string()]);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--status"], &steps, &format!("{READABLE_ONLY} --test-box-now {}", T + 10));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: drain: a barrier stands on the builder; it lapses at 08:30 UTC\n\
         gate: drain: 0 waiting ticket(s)\ngate: drain: state drained\n",
        "{outcome:?}"
    );
    assert_eq!(generation(&case), 1, "a status raises nothing");
}

#[test]
fn a_status_after_a_lapse_says_when_the_barrier_lapsed() {
    let case = drain_case("drain_status_lapsed");
    let raised_at = T.to_string();
    box_drain(&case, "raise", &["--test-box-drain-lapse-ms", "60000", "--test-box-now", &raised_at]);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let seams = format!("{READABLE_ONLY} --test-box-drain-lapse-ms 60000 --test-box-now {}", T + 60);
    let outcome = drained(&case, &["--status"], &steps, &seams);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: drain: the last barrier lapsed at 08:01 UTC without a lift\n\
         gate: drain: 0 waiting ticket(s)\ngate: drain: state open\n",
        "{outcome:?}"
    );
}

#[test]
fn a_lift_says_how_many_waiting_runs_start() {
    let case = drain_case("drain_lift_tickets");
    box_drain(&case, "raise", &["--test-box-now", &T.to_string()]);
    let tickets = root_of(&case).join("drain").join("tickets");
    fs::create_dir_all(&tickets).expect("the tickets folder can be made");
    for (name, session) in [("00000001-s1", "s1"), ("00000002-s2", "s2")] {
        let ticket = json!({ "Session": session, "Polled": T });
        fs::write(tickets.join(name), ticket.to_string()).expect("a ticket can be written");
    }
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--lift"], &steps, &format!("{READABLE_ONLY} --test-box-now {}", T + 5));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: drain: barrier lifted; 2 waiting run(s) start in arrival order\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["drain", "drain"], "a status, then the lift");
    assert!(!root_of(&case).join("drain").join("barrier").exists(), "the barrier is gone");
}

#[test]
fn a_lift_with_no_barrier_says_there_is_nothing_to_lift() {
    let case = drain_case("drain_lift_nothing");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--lift"], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: drain: no barrier stands on the builder; nothing to lift\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), ["drain"], "the status alone: nothing is lifted");
}

#[test]
fn ctrl_c_during_the_wait_leaves_the_barrier_standing() {
    let case = drain_case("drain_interrupted");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let interrupted = drained(&case, &["--test-drain-interrupt-after-list"], &steps, READABLE_ONLY);
    drop(holder);
    let status = drained(&case, &["--status"], &steps, READABLE_ONLY);

    assert_eq!(interrupted.code, Some(CONTROL_C_EXIT), "{interrupted:?}");
    assert_listed(&interrupted.stderr);
    assert_eq!(interrupted.stderr.lines().last(), Some(INTERRUPTED), "{interrupted:?}");
    assert_eq!(status.code, Some(0), "{status:?}");
    let first = status.stderr.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("gate: drain: a barrier stands on the builder; it lapses at "),
        "{status:?}"
    );
}

#[test]
fn ctrl_c_before_the_first_raise_is_answered_says_a_barrier_may_stand() {
    let case = drain_case("drain_interrupted_before_raise");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--test-drain-interrupt-before-raise"], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: drain: interrupted before the builder answered the raise; a barrier may stand; gate drain --status says, \
         gate drain --lift lifts it\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn a_builder_whose_box_does_not_know_drain_is_too_old() {
    let case = drain_case("drain_too_old");
    let usage = "say-exit 2 usage: gate box preflight|run|stop|boot|outputs|inflight|inputs|lfs --stdin";
    let steps = ssh_steps(&case, "ssh", &[usage]);

    let outcome = drained(&case, &[], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: drain: the builder's gate box does not know drain (status 2); run gate push\n",
        "{outcome:?}"
    );
}

#[test]
fn a_builder_unreachable_at_the_first_call_fails_at_once() {
    let case = drain_case("drain_unreachable");
    let steps = ssh_steps(&case, "ssh", &["fail-host"]);

    let outcome = drained(&case, &[], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate: drain: the builder could not be reached: ssh: connect to host the builder port 22: Connection refused\n",
        "{outcome:?}"
    );
    assert!(
        !outcome.stderr.to_lowercase().contains("builder-1"),
        "no line names the host: {outcome:?}"
    );
    assert_eq!(calls(&steps), ["drain"], "one call, never retried");
}

#[test]
fn a_builder_that_fails_mid_wait_is_said_once_then_drained_once_it_answers() {
    let case = drain_case("drain_fails_mid_wait");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let mut answers = vec!["ok", "fail-host"];
    answers.extend_from_slice(&ANSWERS);
    let steps = ssh_steps(&case, "ssh", &answers);
    let running = start(gate_drain(&case, &[], &steps, READABLE_ONLY));

    running.wait_for_stderr(NOT_ANSWERED);
    drop(holder);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(NOT_ANSWERED).count(), 1, "{outcome:?}");
    let last = outcome.stderr.lines().last().unwrap_or_default();
    assert!(last.starts_with("gate: drain: the builder is drained; "), "{outcome:?}");
}

#[test]
fn a_builder_silent_past_the_barrier_s_lapse_ends_the_wait() {
    let case = drain_case("drain_lapsed_mid_wait");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let mut answers = vec!["ok"];
    answers.extend(std::iter::repeat_n("fail-host", 2000));
    let steps = ssh_steps(&case, "ssh", &answers);
    let seams = format!("{READABLE_ONLY} --test-box-drain-lapse-ms 2000");

    let outcome = drained(&case, &["--test-drain-raise-ms", "600000"], &steps, &seams);
    drop(holder);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_listed(&outcome.stderr);
    assert_eq!(outcome.stderr.matches(NOT_ANSWERED).count(), 1, "{outcome:?}");
    let last = outcome.stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("gate: drain: the builder has not answered and the barrier lapsed at ")
            && last.ends_with(" UTC; gate drain raises it again"),
        "{outcome:?}"
    );
}

#[test]
fn a_barrier_lifted_while_the_wait_runs_ends_it() {
    let case = drain_case("drain_lifted_mid_wait");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);
    let running = start(gate_drain(&case, &[], &steps, READABLE_ONLY));

    running.wait_for_stderr(ONE_RUN);
    let lifted = drained(&case, &["--lift"], &steps, READABLE_ONLY);
    let outcome = running.finish();
    drop(holder);

    assert_eq!(lifted.code, Some(0), "{lifted:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        outcome.stderr.lines().last(),
        Some("gate: drain: the barrier was lifted or lapsed while gate drain waited; gate drain raises it again"),
        "{outcome:?}"
    );
}

#[test]
fn a_long_wait_says_it_still_waits() {
    let case = drain_case("drain_still_waiting");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);
    let running = start(gate_drain(&case, &["--test-heartbeat-ms", "50"], &steps, READABLE_ONLY));

    running.wait_for_stderr("gate: drain: still waiting for 1 run(s) (");
    drop(holder);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_listed(&outcome.stderr);
}

#[test]
fn a_drain_with_no_routing_file_says_so_as_push_does() {
    let case = Case::new("drain_no_remote_json");
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &[], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let routing = case.local_app_data().join("gate").join("remote.json");
    assert_eq!(outcome.stderr, format!("drain: no remote.json at {}\n", shown(&routing)), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn a_drain_of_another_listed_builder_goes_to_it() {
    let case = drain_case("drain_other_builder");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--builder", "builder-2"], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines = fs::read_to_string(beside(&steps, ".lines")).expect("the stand-in's calls can be read");
    let hosts: Vec<&str> = lines.lines().filter_map(|line| line.split_once(' ').map(|(host, _)| host)).collect();
    assert_eq!(hosts, ["builder-2"], "{outcome:?}");
}

#[test]
fn the_wait_raises_the_barrier_again() {
    let case = drain_case("drain_re_raise");
    box_drain(&case, "raise", &[]);
    let holder = live_session(&case);
    let steps = ssh_steps(&case, "ssh", &ANSWERS);
    let running = start(gate_drain(&case, &["--test-drain-raise-ms", "50"], &steps, READABLE_ONLY));

    // The case's raise, the wait's own, then one again.
    running.wait_until("a second raise of the wait", || generation(&case) >= 3);
    drop(holder);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let raised = outcome.stderr.matches("gate: drain: barrier raised on the builder;").count();
    assert_eq!(raised, 1, "the raised line is said after the first raise alone: {outcome:?}");
}

#[test]
fn a_builder_remote_json_does_not_list_is_a_usage_error() {
    let case = drain_case("drain_unlisted_builder");
    let steps = ssh_steps(&case, "ssh", &ANSWERS);

    let outcome = drained(&case, &["--builder", "builder-3"], &steps, READABLE_ONLY);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate drain: builder-3 is not a builder remote.json lists\n",
        "{outcome:?}"
    );
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

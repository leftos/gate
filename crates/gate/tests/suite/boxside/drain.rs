//! `gate box drain --stdin`: the builder's drain barrier raised, read and lifted under a root in the case's folder,
//! every time from the fixed clock `--test-box-now`.

use std::fs;
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use super::{ReadBack, box_call, field, gate_box, icacls, passed, payload, refused_line, snapshot};
use crate::support::{Case, FAKE_TOOL, Outcome, UNTIL_KILLED, box_payload, build_version, junction, shown};

/// The fixed time every call of a case starts from, in Unix seconds.
const T: u64 = 1_800_000_000;

/// `gate box drain` of `action` on the root `root` at the Unix second `now`, with `seams` before the verb, to its end.
fn drain(case: &Case, root: &Path, action: &str, now: u64, seams: &[&str]) -> Outcome {
    let now = now.to_string();
    // The box under test reads no command line of the elevated gates around it, which the drained test would count.
    let mut args = vec!["box", "--test-box-now", &now, "--test-box-drain-readable-only", "true"];
    args.extend_from_slice(seams);
    args.push("drain");
    box_call(case, &args, &payload(&json!({ "Root": shown(root), "Action": action }))).finish()
}

/// The case's builder root, `remote` under its folder.
fn root_of(case: &Case) -> PathBuf {
    case.folder().join("remote")
}

/// The JSON of the drain's file `name` under `root`.
fn record(root: &Path, name: &str) -> Value {
    let text = fs::read_to_string(root.join("drain").join(name)).expect("the drain's record can be read");
    serde_json::from_str(&text).expect("the drain's record is JSON")
}

/// The status lines of a standing barrier raised at `raised` with the default lapse, nothing going.
fn drained_at(raised: u64) -> String {
    format!(
        "gate: drain barrier {raised} {}\ngate: drain tickets 0\ngate: drain state drained\n",
        raised + 1800
    )
}

/// A process the case started, killed and waited for when the case ends.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

#[test]
fn drain_status_on_a_missing_root_reads_open() {
    let case = Case::new("drain_status_on_a_missing_root_reads_open");
    let root = root_of(&case);

    let outcome = drain(&case, &root, "status", T, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: drain barrier none\ngate: drain tickets 0\ngate: drain state open\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(!root.exists(), "a status makes no root");
}

#[test]
fn drain_raise_then_status_reads_drained_with_nothing_running() {
    let case = Case::new("drain_raise_then_status_reads_drained_with_nothing_running");
    let root = root_of(&case);

    let raised = drain(&case, &root, "raise", T, &[]);
    let status = drain(&case, &root, "status", T + 10, &[]);

    assert_eq!(raised.code, Some(0), "{raised:?}");
    assert_eq!(raised.stdout, drained_at(T), "{raised:?}");
    assert_eq!(raised.stderr, "", "{raised:?}");
    assert_eq!(status.code, Some(0), "{status:?}");
    assert_eq!(status.stdout, drained_at(T), "the barrier still stands: {status:?}");
    assert_eq!(record(&root, "barrier"), json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }));
}

#[test]
fn drain_raise_twice_bumps_the_generation_and_moves_the_lapse() {
    let case = Case::new("drain_raise_twice_bumps_the_generation_and_moves_the_lapse");
    let root = root_of(&case);

    let first = drain(&case, &root, "raise", T, &[]);
    let second = drain(&case, &root, "raise", T + 60, &[]);

    assert_eq!(first.stdout, drained_at(T), "{first:?}");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(second.stdout, drained_at(T + 60), "the lapse moves with the raise: {second:?}");
    assert_eq!(record(&root, "barrier"), json!({ "Raised": T + 60, "Lapses": T + 1860, "Generation": 2 }));
}

#[test]
fn drain_lift_reads_open_and_says_lifted() {
    let case = Case::new("drain_lift_reads_open_and_says_lifted");
    let root = root_of(&case);

    let raised = drain(&case, &root, "raise", T, &[]);
    let lifted = drain(&case, &root, "lift", T + 5, &[]);

    assert_eq!(raised.code, Some(0), "{raised:?}");
    assert_eq!(lifted.code, Some(0), "{lifted:?}");
    assert_eq!(
        lifted.stdout, "gate: drain barrier none\ngate: drain tickets 0\ngate: drain state open\n",
        "{lifted:?}"
    );
    assert_eq!(lifted.stderr, "", "{lifted:?}");
    assert_eq!(record(&root, "lifted"), json!({ "At": T + 5 }), "the lift is recorded with its time");
    assert!(!root.join("drain").join("barrier").exists(), "the barrier is gone");
}

#[test]
fn a_raised_barrier_lapses_after_the_lapse_seam() {
    let case = Case::new("a_raised_barrier_lapses_after_the_lapse_seam");
    let root = root_of(&case);
    let lapse = ["--test-box-drain-lapse-ms", "60000"];

    let raised = drain(&case, &root, "raise", T, &lapse);
    let before = drain(&case, &root, "status", T + 59, &lapse);
    let after = drain(&case, &root, "status", T + 60, &lapse);

    assert_eq!(raised.code, Some(0), "{raised:?}");
    assert_eq!(
        before.stdout,
        format!("gate: drain barrier {T} {}\ngate: drain tickets 0\ngate: drain state drained\n", T + 60),
        "{before:?}"
    );
    assert_eq!(after.code, Some(0), "{after:?}");
    assert_eq!(
        after.stdout,
        format!("gate: drain barrier lapsed {}\ngate: drain tickets 0\ngate: drain state open\n", T + 60),
        "{after:?}"
    );
    assert_eq!(record(&root, "lapsed"), json!({ "At": T + 60 }));
    assert!(!root.join("drain").join("barrier").exists(), "a lapsed barrier is replaced by its lapse");
}

/// The case's `drain\sessions` folder under `root`, made.
fn sessions_of(root: &Path) -> PathBuf {
    let sessions = root.join("drain").join("sessions");
    fs::create_dir_all(&sessions).expect("the sessions folder can be made");
    sessions
}

#[test]
fn a_live_holder_keeps_the_drain_waiting_and_names_its_session() {
    let case = Case::new("a_live_holder_keeps_the_drain_waiting_and_names_its_session");
    let root = root_of(&case);
    let raised = drain(&case, &root, "raise", T, &[]);
    assert_eq!(raised.code, Some(0), "{raised:?}");
    let sessions = sessions_of(&root);
    let session = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "gate run cargo test", "Called": T - 1000 });
    fs::write(sessions.join("s1"), session.to_string()).expect("the session record can be written");
    let holder = sessions.join(format!("s1.{}", std::process::id()));
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&holder)
        .expect("the holder can be held, as an admitted box process holds it");

    let outcome = drain(&case, &root, "status", T, &[]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!(
            "gate: drain barrier {T} {}\ngate: drain session 1000 {{\"Pc\":\"pc1\",\"Repo\":\"repo1\",\"Command\":\"gate run cargo test\"}}\ngate: drain tickets 0\ngate: drain state waiting\n",
            T + 1800
        ),
        "{outcome:?}"
    );
    assert!(sessions.join("s1").exists(), "a live session's record stays");
    assert!(holder.exists(), "a held holder stays");
}

#[test]
fn a_session_past_its_lease_with_no_holder_is_removed_by_status() {
    let case = Case::new("a_session_past_its_lease_with_no_holder_is_removed_by_status");
    let root = root_of(&case);
    let raised = drain(&case, &root, "raise", T, &[]);
    assert_eq!(raised.code, Some(0), "{raised:?}");
    let sessions = sessions_of(&root);
    let session = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "gate run x", "Called": T - 120 });
    fs::write(sessions.join("s1"), session.to_string()).expect("the session record can be written");
    fs::write(sessions.join("s1.4242"), "").expect("a dead process's holder can be written");

    let outcome = drain(&case, &root, "status", T, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, drained_at(T), "{outcome:?}");
    assert!(!sessions.join("s1").exists(), "the dead session's record is removed");
    assert!(!sessions.join("s1.4242").exists(), "the dead holder is removed");
}

#[test]
fn a_process_naming_the_root_keeps_the_drain_waiting() {
    let case = Case::new("a_process_naming_the_root_keeps_the_drain_waiting");
    let root = root_of(&case);
    let raised = drain(&case, &root, "raise", T, &[]);
    assert_eq!(raised.code, Some(0), "{raised:?}");
    let named = shown(&root.join("k.git"));
    let started = Started(
        Command::new(FAKE_TOOL)
            .args(["out", &named, "sleep", UNTIL_KILLED])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fake tool starts"),
    );
    let pid = started.0.id();

    let outcome = drain(&case, &root, "status", T, &[]);

    drop(started);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!(
            "gate: drain barrier {T} {}\ngate: drain process {pid} git\ngate: drain tickets 0\ngate: drain state waiting\n",
            T + 1800
        ),
        "{outcome:?}"
    );
}

#[test]
fn a_planted_drain_folder_is_moved_aside() {
    let case = Case::new("a_planted_drain_folder_is_moved_aside");
    let root = root_of(&case);
    let planted = root.join("drain");
    fs::create_dir_all(&planted).expect("a planted drain folder can be made");
    fs::write(planted.join("barrier"), r#"{"Raised":1,"Lapses":99999999999,"Generation":7}"#).expect("a planted barrier can be written");

    let outcome = drain(&case, &root, "raise", T, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, drained_at(T), "the planted barrier is not read: {outcome:?}");
    assert_eq!(record(&root, "barrier"), json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }));
    assert_eq!(
        moved_aside(&root),
        ["drain.gate-moved-1"],
        "the planted folder is moved aside beside the new one"
    );
}

/// The names under `root` that something at `drain` was moved aside to.
fn moved_aside(root: &Path) -> Vec<String> {
    fs::read_dir(root)
        .expect("the root can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("drain.gate-moved-"))
        .collect()
}

#[test]
fn a_junction_at_drain_is_moved_aside_and_the_raise_goes_on() {
    let case = Case::new("a_junction_at_drain_is_moved_aside_and_the_raise_goes_on");
    let root = root_of(&case);
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&root).expect("the root can be made");
    fs::create_dir_all(&elsewhere).expect("the junction's target can be made");
    junction(&root.join("drain"), &elsewhere);

    let outcome = drain(&case, &root, "raise", T, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, drained_at(T), "{outcome:?}");
    assert_eq!(record(&root, "barrier"), json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }));
    assert_eq!(moved_aside(&root), ["drain.gate-moved-1"], "the junction is moved aside as itself");
    let target: Vec<_> = fs::read_dir(&elsewhere).expect("the target can be listed").collect();
    assert!(target.is_empty(), "nothing is written through the junction: {target:?}");
}

#[test]
fn a_drain_folder_with_an_explicit_dacl_is_moved_aside_and_the_raise_goes_on() {
    let case = Case::new("a_drain_folder_with_an_explicit_dacl_is_moved_aside_and_the_raise_goes_on");
    let root = root_of(&case);
    let planted = root.join("drain");
    fs::create_dir_all(&planted).expect("a planted drain folder can be made");
    icacls(&planted, &["/inheritance:d", "/q"]);

    let outcome = drain(&case, &root, "raise", T, &[]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, drained_at(T), "{outcome:?}");
    assert_eq!(record(&root, "barrier"), json!({ "Raised": T, "Lapses": T + 1800, "Generation": 1 }));
    assert_eq!(moved_aside(&root), ["drain.gate-moved-1"], "the folder of its own DACL is moved aside");
}

/// `gate box <verb>` of `payload` at the Unix second `now`, `input` written to its stdin after the request line, which
/// is then closed; to its end.
fn verb_at(case: &Case, verb: &str, payload: &Value, now: u64, input: &str) -> Outcome {
    verb_at_with(case, verb, payload, (now, input), &[])
}

/// [`verb_at`] at the Unix second and with the input `at`, with the environment variables `env` set on the box.
fn verb_at_with(case: &Case, verb: &str, payload: &Value, at: (u64, &str), env: &[(&str, &str)]) -> Outcome {
    let (now, input) = at;
    let now = now.to_string();
    let args = ["box", "--test-box-now", &now, "--test-box-drain-readable-only", "true", verb];
    let mut call = box_call(case, &args, &self::payload(payload));
    call.envs(env.iter().copied());
    let (running, mut stdin) = call.start_with_stdin();
    // A box that refuses before it reads its input may close its end first; what it said is the case's to assert.
    drop(stdin.write_all(input.as_bytes()));
    drop(stdin);
    running.finish_read()
}

/// `payload` naming the drain session `id`.
fn sessioned(payload: &Value, id: &str) -> Value {
    let mut sessioned = payload.clone();
    sessioned["Session"] = json!(id);
    sessioned
}

/// The payload of a run of `fake-tool out ran` in the work folder `k` under the case's root.
fn run_payload(case: &Case) -> Value {
    box_payload(case, "k", &[FAKE_TOOL, "out", "ran"])
}

/// What a call of `payload` refused by the drain says on stderr after anything before it: the `drain` tag, its place
/// `place` and the barrier raised at `raised` with the default lapse.
fn refusal(payload: &Value, place: &str, raised: u64) -> String {
    format!(
        "{}\ngate: drain place {place}\ngate: drain barrier {raised} {}\n",
        refused_line(payload, "drain"),
        raised + 1800
    )
}

/// The version line a refused preflight says first.
fn version_line(case: &Case) -> String {
    format!("gate: remote version {}\n", build_version(case))
}

/// Raises the barrier on `root` at `now`.
fn raised(case: &Case, root: &Path, now: u64) {
    let outcome = drain(case, root, "raise", now, &[]);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn a_preflight_under_a_barrier_refuses_after_its_version_line_and_queues_its_session() {
    let case = Case::new("a_preflight_under_a_barrier_refuses_after_its_version_line_and_queues_its_session");
    let root = root_of(&case);
    let base = run_payload(&case);
    raised(&case, &root, T);

    let first = verb_at(&case, "preflight", &sessioned(&base, "s1"), T + 1, "");
    let second = verb_at(&case, "preflight", &sessioned(&base, "s2"), T + 2, "");

    assert_eq!(first.code, Some(75), "{first:?}");
    assert_eq!(first.stdout, version_line(&case), "{first:?}");
    assert_eq!(first.stderr, refusal(&base, "1", T), "{first:?}");
    assert_eq!(record(&root, r"tickets\00000001-s1"), json!({ "Session": "s1", "Polled": T + 1 }));
    assert_eq!(second.code, Some(75), "{second:?}");
    assert_eq!(second.stdout, version_line(&case), "{second:?}");
    assert_eq!(second.stderr, refusal(&base, "2", T), "{second:?}");
    assert_eq!(record(&root, r"tickets\00000002-s2"), json!({ "Session": "s2", "Polled": T + 2 }));
    assert!(!root.join("work").join("k").exists(), "a refused preflight readies nothing");
}

#[test]
fn a_no_ticket_preflight_under_a_barrier_refuses_and_takes_no_place() {
    let case = Case::new("a_no_ticket_preflight_under_a_barrier_refuses_and_takes_no_place");
    let root = root_of(&case);
    let base = run_payload(&case);
    let mut asked = sessioned(&base, "s1");
    asked["NoTicket"] = json!(true);
    raised(&case, &root, T);

    let outcome = verb_at(&case, "preflight", &asked, T + 1, "");

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, version_line(&case), "{outcome:?}");
    assert_eq!(outcome.stderr, refusal(&base, "1", T), "{outcome:?}");
    let tickets: Vec<_> = fs::read_dir(root.join("drain").join("tickets"))
        .expect("the tickets can be listed")
        .collect();
    assert!(tickets.is_empty(), "no ticket is taken: {tickets:?}");
}

#[test]
fn a_preflight_without_a_session_passes_with_no_barrier_and_is_refused_under_one() {
    let case = Case::new("a_preflight_without_a_session_passes_with_no_barrier_and_is_refused_under_one");
    let root = root_of(&case);
    let base = run_payload(&case);

    let open = verb_at(&case, "preflight", &base, T, "");
    let left_no_drain = !root.join("drain").exists();
    raised(&case, &root, T + 1);
    let refused = verb_at(&case, "preflight", &base, T + 2, "");

    assert_eq!(open.code, Some(0), "{open:?}");
    assert!(left_no_drain, "a preflight with no session on a root with no drain makes none");
    assert_eq!(refused.code, Some(75), "{refused:?}");
    assert_eq!(refused.stdout, version_line(&case), "{refused:?}");
    assert_eq!(refused.stderr, refusal(&base, "none", T + 1), "{refused:?}");
}

#[test]
fn a_preflight_whose_drain_fails_says_its_version_line_first() {
    let case = Case::new("a_preflight_whose_drain_fails_says_its_version_line_first");
    let root = root_of(&case);
    let base = run_payload(&case);
    raised(&case, &root, T);
    // A sessions folder the box did not make, which it refuses to write in, holding a session live at the call.
    let sessions = sessions_of(&root);
    let session = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "", "Job": "", "Called": T });
    fs::write(sessions.join("s1"), session.to_string()).expect("the session record can be written");

    let outcome = verb_at(&case, "preflight", &sessioned(&base, "s1"), T + 1, "");

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, version_line(&case), "the version line comes first: {outcome:?}");
    assert!(outcome.stderr.starts_with("gate: drain failed: "), "{outcome:?}");
    assert!(!root.join("work").join("k").exists(), "a failed preflight readies nothing");
}

#[test]
fn a_waiting_session_is_admitted_by_its_preflight_after_the_lift() {
    let case = Case::new("a_waiting_session_is_admitted_by_its_preflight_after_the_lift");
    let root = root_of(&case);
    let asked = sessioned(&run_payload(&case), "s1");
    raised(&case, &root, T);
    let waiting = verb_at(&case, "preflight", &asked, T + 1, "");
    assert_eq!(waiting.code, Some(75), "{waiting:?}");
    let lifted = drain(&case, &root, "lift", T + 2, &[]);
    assert_eq!(lifted.code, Some(0), "{lifted:?}");

    let admitted = verb_at(&case, "preflight", &asked, T + 3, "");

    assert_eq!(admitted.code, Some(0), "{admitted:?}");
    assert_eq!(record(&root, r"sessions\s1")["Called"], json!(T + 3), "the admitted session is recorded");
    assert!(!root.join("drain").join("tickets").join("00000001-s1").exists(), "its ticket is given up");
}

#[test]
fn a_run_of_a_session_not_live_under_a_barrier_is_refused_drain() {
    let case = Case::new("a_run_of_a_session_not_live_under_a_barrier_is_refused_drain");
    let root = root_of(&case);
    let mut base = run_payload(&case);
    snapshot(&case, &mut base, &[("a.txt", "a\n")]);
    raised(&case, &root, T);

    let outcome = verb_at(&case, "run", &sessioned(&base, "s9"), T + 1, "");

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert!(outcome.stderr.ends_with(&refusal(&base, "1", T)), "{outcome:?}");
    assert!(
        !root.join("drain").join("tickets").join("00000001-s9").exists(),
        "only a preflight queues"
    );
}

/// `payload` asking for the step `diff`, its work folder readied by a preflight, as every route's first call does.
fn diff_payload(case: &Case) -> Value {
    let mut payload = run_payload(case);
    payload["Step"] = json!("diff");
    passed(&gate_box(case, "preflight", &self::payload(&payload), None));
    payload
}

#[test]
fn an_inputs_call_of_a_session_not_live_under_a_barrier_is_refused_before_its_manifest_is_read() {
    let case = Case::new("an_inputs_call_of_a_session_not_live_under_a_barrier_is_refused_before_its_manifest_is_read");
    let root = root_of(&case);
    let base = diff_payload(&case);
    raised(&case, &root, T);

    // A manifest that does not read, which fails the call with status 1 once it is read.
    let outcome = verb_at(&case, "inputs", &sessioned(&base, "s9"), T + 1, "{not json\n");

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, refusal(&base, "1", T), "the manifest is never read: {outcome:?}");
    for stages in ["inputs-stage", "inputs-stage-medium"] {
        assert!(!root.join(stages).exists(), "no stage folder is made: {stages}");
    }
    let tickets: Vec<_> = fs::read_dir(root.join("drain").join("tickets"))
        .expect("the tickets can be listed")
        .collect();
    assert!(tickets.is_empty(), "only a preflight queues: {tickets:?}");
}

#[test]
fn an_lfs_call_of_a_session_not_live_under_a_barrier_is_refused_drain_not_lfs() {
    let case = Case::new("an_lfs_call_of_a_session_not_live_under_a_barrier_is_refused_drain_not_lfs");
    let root = root_of(&case);
    let base = diff_payload(&case);
    raised(&case, &root, T);

    // The box judges its elevation, as the lfs cases that test it do: the suite runs unelevated, so a call that reached
    // the elevation check would be refused `lfs`.
    let check = [("GATE_TEST_BOX_ELEVATION", "check")];

    let outcome = verb_at_with(&case, "lfs", &sessioned(&base, "s9"), (T + 1, "not a manifest\n"), &check);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, refusal(&base, "1", T), "{outcome:?}");
}

#[test]
fn a_session_admitted_at_its_preflight_runs_under_a_barrier_raised_after_it() {
    let case = Case::new("a_session_admitted_at_its_preflight_runs_under_a_barrier_raised_after_it");
    let root = root_of(&case);
    let mut base = run_payload(&case);
    snapshot(&case, &mut base, &[("a.txt", "a\n")]);
    let asked = sessioned(&base, "s1");
    let admitted = verb_at(&case, "preflight", &asked, T, "");
    assert_eq!(admitted.code, Some(0), "{admitted:?}");
    let called = record(&root, r"sessions\s1")["Called"].as_u64().expect("the record holds Called");
    assert!(called >= T, "the session is called no earlier than its preflight: {called}");
    raised(&case, &root, T + 10);

    let outcome = verb_at(&case, "run", &asked, T + 20, "");

    assert_eq!(outcome.code, Some(0), "a session live within its lease runs: {outcome:?}");
    assert!(!outcome.stderr.contains("gate: drain"), "no drain refusal is said: {outcome:?}");
    assert!(!outcome.stderr.contains(&refused_line(&base, "drain")), "{outcome:?}");
}

#[test]
fn a_run_under_a_root_that_is_a_junction_or_a_file_fails_its_drain() {
    let case = Case::new("a_run_under_a_root_that_is_a_junction_or_a_file_fails_its_drain");
    let root = root_of(&case);
    let mut base = run_payload(&case);
    // A snapshot the run never reaches: the payload reads, and the drain fails before any checkout.
    base["Snapshot"] = json!("a".repeat(40));
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the junction's target can be made");

    junction(&root, &elsewhere);
    let through_junction = verb_at(&case, "run", &sessioned(&base, "s1"), T, "");
    fs::remove_dir(&root).expect("the junction can be removed");
    fs::write(&root, "not a folder").expect("a file can be written at the root");
    let at_file = verb_at(&case, "run", &sessioned(&base, "s1"), T, "");

    for (what, outcome) in [("a junction", &through_junction), ("a file", &at_file)] {
        assert_eq!(outcome.code, Some(1), "{what}: {outcome:?}");
        let said = |line: &str| line.starts_with("gate: drain failed: ") && line.ends_with("is a link, a junction or not a folder");
        assert!(outcome.stderr.lines().any(said), "{what}: the drain failure is said: {outcome:?}");
    }
    let target: Vec<_> = fs::read_dir(&elsewhere).expect("the target can be listed").collect();
    assert!(target.is_empty(), "nothing is written through the junction: {target:?}");
}

#[test]
fn a_run_s_session_record_holds_its_job_and_its_commands_joined() {
    let case = Case::new("a_run_s_session_record_holds_its_job_and_its_commands_joined");
    let root = root_of(&case);
    let mut base = run_payload(&case);
    base.as_object_mut().expect("the payload is an object").remove("Command");
    base["WarmCommands"] = json!([[FAKE_TOOL, "out", "a"], [FAKE_TOOL, "out", "b"]]);
    base["WarmHash"] = json!("5f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0");
    base["Timeout"] = json!(600);
    snapshot(&case, &mut base, &[("a.txt", "a\n")]);

    let outcome = verb_at(&case, "run", &sessioned(&base, "s1"), T, "");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let session = record(&root, r"sessions\s1");
    assert_eq!(session["Job"], json!(field(&base, "Job")), "{session}");
    assert_eq!(session["Command"], json!(format!("{FAKE_TOOL} out a && {FAKE_TOOL} out b")), "{session}");
}

/// The payload of the `outputs` verb for the run of `fake-tool out ran`'s archive, and the archive's path, written
/// with `bytes`.
fn outputs_payload(case: &Case, bytes: &[u8]) -> (Value, PathBuf) {
    let mut payload = run_payload(case);
    let name = format!("k-{}.tar.zst", field(&payload, "Nonce"));
    let archive = Path::new(field(&payload, "OutputFolder")).join(&name);
    payload["Name"] = json!(name);
    fs::create_dir_all(archive.parent().expect("the archive has a folder")).expect("the outputs folder can be made");
    fs::write(&archive, bytes).expect("the archive can be written");
    (payload, archive)
}

#[test]
fn outputs_of_a_dead_session_under_a_barrier_sends_the_archive_and_keeps_it() {
    let case = Case::new("outputs_of_a_dead_session_under_a_barrier_sends_the_archive_and_keeps_it");
    let root = root_of(&case);
    let (payload, archive) = outputs_payload(&case, b"archive bytes");
    raised(&case, &root, T);

    let outcome = verb_at(&case, "outputs", &sessioned(&payload, "s9"), T + 1, "");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.raw_stdout == b"archive bytes", "the archive is sent: {outcome:?}");
    assert_eq!(outcome.stderr, "gate: remote outputs kept: the builder is drained\n", "{outcome:?}");
    assert!(archive.exists(), "the archive is kept");
}

#[test]
fn outputs_of_a_live_session_under_a_barrier_renews_it_and_removes_the_archive() {
    let case = Case::new("outputs_of_a_live_session_under_a_barrier_renews_it_and_removes_the_archive");
    let root = root_of(&case);
    let (mut payload, archive) = outputs_payload(&case, b"archive bytes");
    payload["Pc"] = json!("pc1");
    payload["Repo"] = json!("repo1");
    let asked = sessioned(&payload, "s1");
    let admitted = verb_at(&case, "preflight", &asked, T, "");
    assert_eq!(admitted.code, Some(0), "{admitted:?}");
    raised(&case, &root, T + 10);

    let outcome = verb_at(&case, "outputs", &asked, T + 30, "");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.raw_stdout == b"archive bytes", "the archive is sent: {outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(!archive.exists(), "the archive is removed as today");
    assert_eq!(
        record(&root, r"sessions\s1"),
        json!({ "Pc": "pc1", "Repo": "repo1", "Command": "", "Job": "", "Called": T + 30 }),
        "the session the preflight recorded is renewed, its other fields kept"
    );
}

#[test]
fn outputs_whose_drain_cannot_be_read_sends_the_archive_keeps_it_and_says_so() {
    let case = Case::new("outputs_whose_drain_cannot_be_read_sends_the_archive_keeps_it_and_says_so");
    let root = root_of(&case);
    let (payload, archive) = outputs_payload(&case, b"archive bytes");
    raised(&case, &root, T);
    // A sessions folder the box did not make, which it refuses to write in, holding a session live at the call.
    let sessions = sessions_of(&root);
    let session = json!({ "Pc": "pc1", "Repo": "repo1", "Command": "", "Job": "", "Called": T });
    fs::write(sessions.join("s1"), session.to_string()).expect("the session record can be written");

    let outcome = verb_at(&case, "outputs", &sessioned(&payload, "s1"), T + 1, "");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.raw_stdout == b"archive bytes", "the archive is sent: {outcome:?}");
    assert!(outcome.stderr.starts_with("gate: drain failed: "), "{outcome:?}");
    assert_eq!(outcome.stderr.lines().count(), 1, "{outcome:?}");
    assert!(archive.exists(), "the archive is kept");
}

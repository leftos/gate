//! `gate box drain --stdin`: the builder's drain barrier raised, read and lifted under a root in the case's folder,
//! every time from the fixed clock `--test-box-now`.

use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use super::{box_call, icacls, payload};
use crate::support::{Case, FAKE_TOOL, Outcome, UNTIL_KILLED, junction, shown};

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

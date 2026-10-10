//! The pause markers: the file a gate waits behind while it waits for a slot under a parent gate whose slot does not
//! cover it, rewritten on every pass of that wait and gone once the wait ends.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gate_state::v1;

use crate::support::{
    Case, FAKE_TOOL, GATE, Outcome, Running, critical_words, heavy_words, run_words, shown, wait_for_journal_line, wait_until, write_counts,
};

/// The seam that keeps a wait's passes and its still-waiting lines close together.
const POLL: [&str; 2] = ["--test-poll-ms", "20"];
/// As [`POLL`], with a still-waiting line every 200 ms, so a case can count several passes without a clock of its own.
const HEARTBEATS: [&str; 4] = ["--test-poll-ms", "20", "--test-heartbeat-ms", "200"];
/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();
/// The prefix of a heavy gate's still-waiting line.
const STILL_WAITING: &str = "gate: still waiting for a heavy slot";
/// The prefix of the line a gate says when its pause marker cannot be written.
const CANNOT_WRITE: &str = "gate: could not write the pause marker";
/// The prefix of a critical gate's holds line.
const HOLDS: &str = "gate: holds the machine (critical) at ";

/// Starts `case`'s critical gate holding the machine on the case's `go` file, and returns the file and the gate.
fn holding_critical(case: &Case) -> (PathBuf, Running) {
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    critical.wait_for_stderr(HOLDS);
    (go, critical)
}

/// Lets the holder of the `go` file go.
fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

fn assert_passed(outcome: &Outcome) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The names in `case`'s pause folder, sorted; empty when the folder is not there.
fn markers(case: &Case) -> Vec<String> {
    let Ok(entries) = fs::read_dir(v1::paused_dir(&case.root())) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("a pause marker lists").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The start of the process `pid` in Unix epoch milliseconds, or `None` when it does not run.
fn started(pid: u32) -> Option<u64> {
    gate_win32::process_start_of(pid)
        .ok()
        .flatten()
        .map(|start| start.as_millisecond().unsigned_abs())
}

/// The pause marker in `case`'s root of a child of the gate `parent` started at `parent_start`, or `None` when the
/// folder holds none for it.
fn marker_of(case: &Case, parent: u32, parent_start: u64) -> Option<v1::PauseMarker> {
    let names = markers(case);
    assert!(names.len() <= 1, "the pause folder holds at most one marker: {names:?}");
    names
        .first()
        .and_then(|name| v1::parse_pause_name(name))
        .filter(|marker| marker.parent_pid == parent && marker.parent_start_ms == parent_start)
}

/// When the file at `path` was last written; `None` when it is gone.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// What the command of the parent gate logging to `log` has said on its stderr: a gate starts its command with the
/// command's stderr going to the `.err` file beside its log, and moves those bytes into the log once the command has
/// ended, so both are read.
fn child_stderr(case: &Case, log: &str) -> String {
    let read = |path: PathBuf| fs::read_to_string(path).unwrap_or_default();
    format!("{}{}", read(case.folder().join(log)), read(case.folder().join(format!("{log}.err"))))
}

/// Returns once the child gate of the parent logging to `log` has said `needle` on its stderr.
fn wait_for_child_stderr(case: &Case, log: &str, needle: &str) {
    wait_until(&format!("{needle:?} on the child's stderr"), || child_stderr(case, log).contains(needle));
}

/// The heavy child gate logging to `log`, as an outer gate's command words.
fn child_gate(log: &'static str, seams: &[&'static str]) -> Vec<&'static str> {
    let mut child = vec![GATE];
    child.extend(heavy_words(log, seams, &[FAKE_TOOL, "exit", "0"]));
    child
}

/// The words of a light parent gate logging to `log`, running `command` with the case's own seams.
fn parent_words(log: &'static str, command: &[&'static str]) -> Vec<&'static str> {
    run_words(log, &POLL, command)
}

#[test]
fn a_child_waiting_behind_a_holding_critical_gate_keeps_a_pause_marker_until_it_takes_a_slot() {
    let case = Case::new("a_child_waiting_behind_a_holding_critical_gate_keeps_a_pause_marker_until_it_takes_a_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    // The parent runs on the one light slot, which the drain reserves: it lends its heavy child nothing, so the child
    // waits behind the critical request and keeps the marker.
    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &POLL)));
    let parent_pid = parent.id();
    let barrier = format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}");
    wait_for_child_stderr(&case, "parent.log", &barrier);
    let parent_start = started(parent_pid).expect("the parent gate runs");

    wait_until("the pause marker of the waiting child, naming a child that runs", || {
        marker_of(&case, parent_pid, parent_start).is_some_and(|marker| started(marker.pid) == Some(marker.start_ms))
    });
    let marker = marker_of(&case, parent_pid, parent_start).expect("the marker is there, and names its child");
    assert_eq!(
        marker.parent_pid, parent_pid,
        "the marker names the parent whose slot the child runs under"
    );
    assert_eq!(marker.parent_start_ms, parent_start);

    let_go(&go);
    wait_until("the pause marker going as the child takes its slot", || {
        marker_of(&case, parent_pid, parent_start).is_none()
    });
    assert_passed(&parent.finish());
    assert_passed(&critical.finish());
}

#[test]
fn a_pause_marker_is_rewritten_every_pass_while_the_child_waits() {
    let case = Case::new("a_pause_marker_is_rewritten_every_pass_while_the_child_waits");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &POLL)));
    let parent_pid = parent.id();
    wait_for_child_stderr(
        &case,
        "parent.log",
        &format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"),
    );
    let parent_start = started(parent_pid).expect("the parent gate runs");

    wait_until("the pause marker of the waiting child", || {
        marker_of(&case, parent_pid, parent_start).is_some()
    });
    let marker = marker_of(&case, parent_pid, parent_start).expect("the marker is there");
    let path = v1::paused_dir(&case.root()).join(v1::pause_name(marker.parent_pid, marker.parent_start_ms, marker.pid, marker.start_ms));
    let first = modified(&path).expect("the marker has a last write");

    wait_until("the marker written again on a later pass", || modified(&path) != Some(first));

    let_go(&go);
    assert_passed(&parent.finish());
    assert_passed(&critical.finish());
}

#[test]
fn a_pause_marker_that_cannot_be_written_is_said_once() {
    let case = Case::new("a_pause_marker_that_cannot_be_written_is_said_once");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();
    // The pause folder is a regular file, so no marker can be written under it.
    fs::write(v1::paused_dir(&case.root()), "").expect("the pause folder can be made a file");

    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &HEARTBEATS)));
    wait_for_child_stderr(
        &case,
        "parent.log",
        &format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"),
    );
    wait_until("several passes of the child's wait", || {
        child_stderr(&case, "parent.log").matches(STILL_WAITING).count() >= 3
    });
    let text = child_stderr(&case, "parent.log");

    assert_eq!(text.matches(CANNOT_WRITE).count(), 1, "the failing write is said once a wait:\n{text}");

    let_go(&go);
    assert_passed(&parent.finish());
    assert_passed(&critical.finish());
}

#[test]
fn a_pause_marker_is_deleted_when_the_wait_is_interrupted() {
    let case = Case::new("a_pause_marker_is_deleted_when_the_wait_is_interrupted");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    // The child queues behind the critical request, writes its pause marker, and is interrupted as it says it waits.
    let mut seams = POLL.to_vec();
    seams.push("--test-interrupt-queued");
    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &seams)));
    let outcome = parent.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    let barrier = format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}");
    assert!(
        child_stderr(&case, "parent.log").contains(&barrier),
        "the child queued behind the critical request"
    );
    assert!(markers(&case).is_empty(), "the interrupted child's pause marker is gone");

    let_go(&go);
    assert_passed(&critical.finish());
}

#[test]
fn a_pause_marker_is_deleted_when_the_child_is_lent_its_parents_slot() {
    let case = Case::new("a_pause_marker_is_deleted_when_the_child_is_lent_its_parents_slot");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_word = shown(&go);
    // A heavy holder keeps the critical request waiting, so the child has a barrier to write its marker behind.
    let holder = case.start(&heavy_words("holder.log", &POLL, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));
    let critical = case.start(&critical_words("critical.log", &POLL, &[FAKE_TOOL, "exit", "0"]));
    critical.wait_for_stderr("gate: waiting for the machine (critical)");

    // The parent's one light slot is a reserved one, so it lends the child nothing yet.
    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &POLL)));
    let parent_pid = parent.id();
    let parent_start = started(parent_pid).expect("the parent gate runs");
    wait_until("the pause marker of the waiting child", || {
        marker_of(&case, parent_pid, parent_start).is_some()
    });

    // Raising the light count past that slot makes it an unreserved one, which the parent lends while the critical
    // request drains.
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":3}"#);
    wait_until("the pause marker gone as the child is lent its parent's slot", || {
        marker_of(&case, parent_pid, parent_start).is_none()
    });
    assert_passed(&parent.finish());
    let lent = format!("gate: running under the light slot of gate {parent_pid} while a critical request drains; taking no slot");
    assert!(
        child_stderr(&case, "parent.log").contains(&lent),
        "the child left the queue for its parent's slot"
    );

    let_go(&go);
    assert_passed(&holder.finish());
    assert_passed(&critical.finish());
}

//! The pause markers: the file a gate waits behind while it waits for a slot under a parent gate whose slot does not
//! cover it, rewritten on every pass of that wait and gone once the wait ends.

use std::fs::{self, File};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use std::{process, thread};

use gate_state::v1::{self, Ticket, TicketKind};
use regex::Regex;

use crate::routing::{remote_claim, routed_case, routed_words, stub};
use crate::support::{
    Case, FAKE_TOOL, GATE, Outcome, Running, critical_words, events, heavy_words, log_texts, none_words, run_words, shown, wait_until, write_counts,
};

/// The seam that keeps a wait's passes and its still-waiting lines close together.
const POLL: [&str; 2] = ["--test-poll-ms", "20"];
/// As [`POLL`], with an hour's stall window, so a gate holding a slot through a case never stalls however long the case
/// lasts on a loaded machine.
const HOLDER: [&str; 4] = ["--test-poll-ms", "20", "--stall", "3600"];
/// How often a case rewrites a pause marker it stands in for a waiting gate with, well inside the 30 s after which a
/// watchdog reads a marker as stale.
const MARKER_REWRITE: Duration = Duration::from_secs(1);
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
/// The seams of the gate whose watchdog watches it while a heavy gate under it waits: a 5 s stall window, whose CPU
/// floor of 1% of a core (50 ms) takes several of Windows's 15.6 ms CPU ticks to reach, with a sample every 100 ms.
const PARENT_WATCH: [&str; 6] = ["--test-poll-ms", "20", "--test-stall-ms", "5000", "--test-sample-ms", "100"];
/// The seams of the waiting child gate: a still-waiting line every 6 s, longer than the parent's stall window, so that
/// line can never hold the window off by itself, and a slow poll, so its waiting costs almost no CPU.
const CHILD_WAIT: [&str; 4] = ["--test-poll-ms", "200", "--test-heartbeat-ms", "6000"];
/// The seams of a parent gate whose every sample fails, so its watchdog goes by wall time alone, sampled every 100 ms.
const SAMPLER_FAILS: [&str; 5] = ["--test-poll-ms", "20", "--test-sampler-fail", "--test-sample-ms", "100"];
/// The seams of a parent gate sampled every 100 ms.
const SAMPLED: [&str; 4] = ["--test-poll-ms", "20", "--test-sample-ms", "100"];
/// The prefix of the line a gate says when its watchdog cannot list the pause folder.
const CANNOT_READ: &str = "gate: could not read the pause markers in ";
/// The seams of a gate whose hung command its watchdog stalls within one case: a 1.5 s stall window, sampled every
/// 100 ms.
const HUNG_WATCH: [&str; 6] = ["--test-poll-ms", "20", "--test-stall-ms", "1500", "--test-sample-ms", "100"];
/// The start of a passed line, up to its whole wall seconds and the `(` of its load note: the one spelling of the line
/// every reader below builds its pattern on.
const PASSED_WALL: &str = r"gate: passed in (\d+) s \(";
/// The seams of a nested gate whose own watchdog stalls its hung command within one case: a 15 s stall window, three
/// of the [`PARENT_WATCH`] windows of the gate above it, with the default 5 s sampling and a slow poll, so its own
/// watching costs too little CPU to hold the parent's window open by itself.
const NESTED_HUNG: [&str; 4] = ["--test-poll-ms", "200", "--test-stall-ms", "15000"];
/// How long a case keeps a nested gate quiet under its parent: over two of the [`PARENT_WATCH`] stall windows.
const QUIET: Duration = Duration::from_secs(12);

/// Starts `case`'s critical gate holding the machine on the case's `go` file, and returns the file and the gate.
fn holding_critical(case: &Case) -> (PathBuf, Running) {
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let critical = case.start(&critical_words("critical.log", &HOLDER, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
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

/// Creates the file at `path`, or empties it, and sets its last write to now, as a waiting gate writes its pause marker.
fn touch(path: &Path) {
    File::create(path)
        .expect("the marker can be written")
        .set_modified(SystemTime::now())
        .expect("the marker's last write can be set");
}

/// When the file at `path` was last written; `None` when it is gone.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// What the command of the parent gate logging to `log` has said: a gate writes its command's stderr lines to its
/// log as they are read, so the log's texts are read.
fn child_stderr(case: &Case, log: &str) -> String {
    log_texts(&case.folder().join(log))
}

/// Returns once the child gate of the parent logging to `log` has said `needle` on its stderr.
fn wait_for_child_stderr(case: &Case, log: &str, needle: &str) {
    wait_until(&format!("{needle:?} on the child's stderr"), || child_stderr(case, log).contains(needle));
}

/// Waits on the parent gate's own `wait_until`, and on a failed wait says what the child under it said by then, so a red
/// run shows how far the child got.
#[expect(clippy::panic, reason = "a wait that failed fails the case again, with what the child said beside it")]
fn wait_showing_child(parent: &Running, case: &Case, log: &str, what: &str, check: impl FnMut() -> bool) {
    let Err(payload) = catch_unwind(AssertUnwindSafe(|| parent.wait_until(what, check))) else {
        return;
    };
    let message = payload.downcast_ref::<String>().map_or("the wait failed", String::as_str);
    panic!("{message}\nwhat the child said by then:\n{}", child_stderr(case, log));
}

/// The gate's own passed line in `text`, the last whole line that starts with [`PASSED_WALL`]: a gate says its passed
/// line last, so a passed line printed in the tail of its log cannot stand in for it, and a line that stops matching the
/// format fails the reader's own `expect` rather than reading a line under it.
fn passed_line(text: &str) -> &str {
    let pattern = Regex::new(&format!(r"(?m)^{PASSED_WALL}")).expect("the pattern is valid");
    let start = pattern.find_iter(text).last().expect("a passed line of the gate's own").start();
    let line = &text[start..];
    &line[..line.find('\n').unwrap_or(line.len())]
}

/// The wall and load-adjusted whole seconds of the gate's own passed line in `text`, which comes after the tail of its
/// log, a tail that can hold the passed line of a gate under it.
fn passed_seconds(text: &str) -> (u64, u64) {
    let pattern = Regex::new(&format!("{PASSED_WALL}load-adjusted (\\d+) s")).expect("the pattern is valid");
    let last = pattern.captures(passed_line(text)).expect("a passed line with its load-adjusted seconds");
    let seconds = |index: usize| -> u64 {
        last.get(index)
            .and_then(|found| found.as_str().parse().ok())
            .expect("a passed line's seconds are whole")
    };
    (seconds(1), seconds(2))
}

/// The whole wall seconds of the gate's own passed line in `text`, whatever its load note says: a gate whose sampler
/// failed says its load-adjusted seconds are unknown.
fn passed_wall(text: &str) -> u64 {
    let pattern = Regex::new(PASSED_WALL).expect("the pattern is valid");
    pattern
        .captures(passed_line(text))
        .and_then(|found| found[1].parse().ok())
        .expect("a passed line's whole wall seconds")
}

/// Asserts that the gate whose own passed line ends `text` left most of its run out of its load-adjusted clock, as the
/// time a gate under it waited behind a critical request is. Load only lowers the load-adjusted seconds, so this is red
/// only on a mostly idle machine, where an unpaused run's clock keeps up with its wall time. Any live nested gate in the
/// job is stall progress, so a parent whose child waits is not STALLED: this and [`assert_release_less_paused`] are the
/// case's red signals.
fn assert_wait_left_out(text: &str) {
    let (wall, adjusted) = passed_seconds(text);
    assert!(
        adjusted * 2 <= wall,
        "the wait behind the critical request is paused time, left out of the load-adjusted clock ({adjusted} s of {wall} s):\n{text}"
    );
}

/// Asserts that the release line `case`'s journal holds for the gate `pid`, whose own passed line ends `text`, left out of
/// its `wallSeconds` at least half of `waited`: the stretch the case itself saw the child wait behind the critical
/// request, from the pause marker it saw to the still-waiting line it names, with the critical gate holding throughout.
/// A parent starved across an edge of that stretch books the whole stretch as unpaused, since `Watch::wall_only` counts
/// a stretch as paused only when its sample saw the pause (`gate-watchdog`'s `watch.rs`), which the half of `waited`
/// this asks for leaves room for; the gates' start-up and the child's take and exit after the release, which load
/// stretches without bound, are never compared. The passed line's whole seconds round the wall time half to even, so the
/// wall is taken half a second short.
fn assert_release_less_paused(case: &Case, pid: u32, text: &str, waited: Duration) {
    let wall = passed_wall(text);
    let wall = u32::try_from(wall).map(f64::from).expect("a passed line's wall seconds fit in 32 bits");
    let released = events(case)
        .into_iter()
        .find(|event| event["event"] == "release" && event["pid"] == pid)
        .expect("the gate journalled its release");
    let released_wall = released["wallSeconds"].as_f64().expect("the release has its wall seconds");
    let left_out = wall - 0.5 - released_wall;
    let waited = waited.as_secs_f64();
    assert!(
        left_out * 2.0 >= waited,
        "the release's wallSeconds leave out the wait behind the critical request: at least {left_out:.3} s of {wall} s left \
         out, under half the {waited:.3} s the case saw the child wait: {released}"
    );
}

/// Sets the ceiling in a gate's `words` to `seconds`: the value after the first `--timeout`, which is the outer gate's,
/// since a gate's command and its words come after its own.
fn set_timeout<'a>(words: &mut [&'a str], seconds: &'a str) {
    let flag = words.iter().position(|word| *word == "--timeout").expect("the words name a ceiling");
    words[flag + 1] = seconds;
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

    // The critical gate holds the machine, so the parent lends its heavy child nothing on its light slot, and the child
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
    // A critical request of this test process that holds the machine: a parent lends nothing while one holds it, so
    // the child waits behind it and keeps its marker.
    let start = gate_win32::process_start_of(process::id())
        .expect("this process's start can be read")
        .expect("this process runs");
    let start_ms = start.as_millisecond().unsigned_abs();
    let mut request = Ticket {
        kind: TicketKind::Critical,
        arrived: 0,
        pid: process::id(),
        start_ms,
        holds: true,
        lends_closed: false,
        alone: false,
        remote: None,
        pool: None,
        pinned: None,
        reason: None,
        expected: None,
    };
    let ticket = v1::ticket_path(&case.root(), process::id(), start_ms);
    v1::write_ticket(&ticket, &request).expect("the critical ticket can be written");

    let parent = case.start(&parent_words("parent.log", &child_gate("child.log", &POLL)));
    let parent_pid = parent.id();
    let parent_start = started(parent_pid).expect("the parent gate runs");
    wait_until("the pause marker of the waiting child", || {
        marker_of(&case, parent_pid, parent_start).is_some()
    });

    // The request still drains once it no longer holds the machine, and the parent lends its slot, whichever index it
    // is, while it does.
    request.holds = false;
    v1::write_ticket(&ticket, &request).expect("the critical ticket can be rewritten");
    wait_until("the pause marker gone as the child is lent its parent's slot", || {
        marker_of(&case, parent_pid, parent_start).is_none()
    });
    assert_passed(&parent.finish());
    let lent = format!("gate: running under the light slot of gate {parent_pid} while a critical request drains; taking no slot");
    assert!(
        child_stderr(&case, "parent.log").contains(&lent),
        "the child left the queue for its parent's slot"
    );
    fs::remove_file(&ticket).expect("the critical ticket can be deleted");
}

#[test]
fn a_parent_whose_child_waits_behind_a_critical_request_is_not_stalled() {
    let case = Case::new("a_parent_whose_child_waits_behind_a_critical_request_is_not_stalled");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    // The critical gate holds the machine, so the parent lends its heavy child nothing on its light slot, and the child
    // waits behind the critical request and keeps its pause marker. That marker is not the only stall progress the
    // parent's watchdog sees: any live nested gate in its job is progress too, though the child's still-waiting line
    // comes every 6 s, longer than the parent's 5 s window.
    let child = child_gate("child.log", &CHILD_WAIT);
    let mut parent = case.start(&run_words("parent.log", &PARENT_WATCH, &child));
    let parent_pid = parent.id();
    wait_for_child_stderr(
        &case,
        "parent.log",
        &format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"),
    );
    let parent_start = started(parent_pid).expect("the parent gate runs");
    wait_until("the pause marker of the waiting child, naming a child that runs", || {
        marker_of(&case, parent_pid, parent_start).is_some_and(|marker| started(marker.pid) == Some(marker.start_ms))
    });
    let marker_seen = Instant::now();

    // The child's second still-waiting line is 12 s into its wait, over two of the parent's stall windows; a parent
    // that ended before the line was said is the failure this case is about, so the wait ends with it either way.
    wait_until("the child's still-waiting line said again, or the parent ending", || {
        child_stderr(&case, "parent.log").matches(STILL_WAITING).count() >= 2 || !parent.is_running()
    });
    let waited = marker_seen.elapsed();
    let waited_out = parent.is_running();

    let_go(&go);
    let outcome = parent.finish();
    assert!(waited_out, "the parent gate was killed while its child waited: {outcome:?}");
    assert_passed(&outcome);
    assert!(
        !outcome.stderr.contains("STALLED"),
        "a parent whose child waits behind a critical request is not stalled: {outcome:?}"
    );
    assert_wait_left_out(&outcome.stdout);
    assert_release_less_paused(&case, parent_pid, &outcome.stdout, waited);
    assert_passed(&critical.finish());
}

#[test]
fn a_covered_gate_whose_grandchild_waits_behind_a_critical_request_is_not_stalled() {
    let case = Case::new("a_covered_gate_whose_grandchild_waits_behind_a_critical_request_is_not_stalled");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    // The light parent's slot covers the light gate it runs, which hands its own command the parent's marker: the heavy
    // grandchild waits under the parent's light slot, and names its pause marker after the parent, not after the
    // covered gate whose watchdog this case is about.
    let mut covered = vec![GATE];
    covered.extend(run_words("covered.log", &PARENT_WATCH, &child_gate("grandchild.log", &CHILD_WAIT)));
    let mut parent = case.start(&parent_words("parent.log", &covered));
    let parent_pid = parent.id();
    wait_for_child_stderr(
        &case,
        "covered.log",
        &format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"),
    );
    let parent_start = started(parent_pid).expect("the parent gate runs");
    wait_until("the pause marker of the waiting grandchild, named after the parent", || {
        marker_of(&case, parent_pid, parent_start).is_some_and(|marker| started(marker.pid) == Some(marker.start_ms))
    });

    // As in the case above, the grandchild's second still-waiting line gives the case a stretch of its wait to measure.
    // The live covered gate under this one is stall progress, so this gate cannot be STALLED while the grandchild waits;
    // the clock assertions are the case's red signal.
    wait_until("the grandchild's still-waiting line said again, or the gates ending", || {
        child_stderr(&case, "covered.log").matches(STILL_WAITING).count() >= 2 || !parent.is_running()
    });
    let waited_out = parent.is_running();

    let_go(&go);
    let outcome = parent.finish();
    let covered_log = log_texts(&case.folder().join("covered.log"));
    assert!(waited_out, "the covered gate was killed while its grandchild waited: {covered_log}");
    assert_passed(&outcome);
    assert!(
        !covered_log.contains("STALLED"),
        "a covered gate whose grandchild waits behind a critical request is not stalled: {covered_log}"
    );
    assert_wait_left_out(&covered_log);
    assert_passed(&critical.finish());
}

#[test]
fn a_parent_whose_sampler_failed_leaves_its_child_s_paused_wait_out_of_its_wall_time() {
    let case = Case::new("a_parent_whose_sampler_failed_leaves_its_child_s_paused_wait_out_of_its_wall_time");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();

    // With its sampler failed the parent goes by wall time alone: its backstop kills at five ceilings of wall time less
    // the time it counted paused, the paused time its release leaves out of `wallSeconds`. The ceiling stays far past the
    // hang guard, so no kill races the child's start-up, which counts as wall time until its first pass writes the
    // marker; the backstop's arithmetic on the paused time is the watchdog's own unit case.
    let child = child_gate("child.log", &CHILD_WAIT);
    let parent = case.start(&run_words("parent.log", &SAMPLER_FAILS, &child));
    let parent_pid = parent.id();
    let barrier = format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}");
    wait_showing_child(&parent, &case, "parent.log", &format!("{barrier:?} on the child's stderr"), || {
        child_stderr(&case, "parent.log").contains(&barrier)
    });
    let parent_start = started(parent_pid).expect("the parent gate runs");
    wait_showing_child(
        &parent,
        &case,
        "parent.log",
        "the pause marker of the waiting child, naming a child that runs",
        || marker_of(&case, parent_pid, parent_start).is_some_and(|marker| started(marker.pid) == Some(marker.start_ms)),
    );
    let marker_seen = Instant::now();

    // The child's third still-waiting line is 18 s into its wait.
    wait_showing_child(&parent, &case, "parent.log", "the child's third still-waiting line", || {
        child_stderr(&case, "parent.log").matches(STILL_WAITING).count() >= 3
    });
    let waited = marker_seen.elapsed();

    let_go(&go);
    let outcome = parent.finish();
    assert_passed(&outcome);
    assert!(
        outcome.stderr.contains("gate: sampler failed: "),
        "the parent's sampler failed: {outcome:?}"
    );
    assert_release_less_paused(&case, parent_pid, &outcome.stdout, waited);
    assert_passed(&critical.finish());
}

#[test]
fn a_covered_gate_is_not_paused_by_a_sibling_waiting_under_its_parent() {
    let case = Case::new("a_covered_gate_is_not_paused_by_a_sibling_waiting_under_its_parent");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);

    // The covered gate's command chatters until the case has written the sibling's marker, so its stall clock cannot
    // run before the marker is there to be read; then it hangs without output or CPU for longer than any case, so only
    // the stall ends it.
    let chatter = case.folder().join("chatter");
    let chatter_word = shown(&chatter);
    let mut covered = vec![GATE];
    covered.extend(run_words(
        "covered.log",
        &HUNG_WATCH,
        &[FAKE_TOOL, "chatter", &chatter_word, "sleep", "590000"],
    ));
    let parent = case.start(&run_words("parent.log", &POLL, &covered));
    let parent_pid = parent.id();
    wait_for_child_stderr(&case, "parent.log", &format!("gate: running under the light slot of gate {parent_pid}"));
    let parent_start = started(parent_pid).expect("the parent gate runs");

    // A sibling the parent started beside the covered gate waits behind the critical request under the parent's slot:
    // this test process stands in for it, with a waiting ticket and a pause marker named after the parent.
    let sibling = (process::id(), started(process::id()).expect("this process runs"));
    let arrived = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past the epoch");
    let ticket = Ticket {
        kind: TicketKind::Heavy,
        arrived: u64::try_from(arrived.as_millis()).expect("the arrival fits in milliseconds"),
        pid: sibling.0,
        start_ms: sibling.1,
        holds: false,
        lends_closed: false,
        alone: false,
        remote: None,
        pool: None,
        pinned: None,
        reason: None,
        expected: None,
    };
    v1::write_ticket(&v1::ticket_path(&case.root(), sibling.0, sibling.1), &ticket).expect("the ticket can be written");
    let paused = v1::paused_dir(&case.root());
    fs::create_dir_all(&paused).expect("the pause folder can be made");
    let marker = paused.join(v1::pause_name(parent_pid, parent_start, sibling.0, sibling.1));
    touch(&marker);

    // The sibling keeps its marker fresh, as a waiting gate rewrites its own on every pass, until the parent ends: a
    // covered gate that counted it would stay paused and never stall.
    let finished = AtomicBool::new(false);
    let outcome = thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                touch(&marker);
                thread::sleep(MARKER_REWRITE);
            }
        });
        fs::write(&chatter, "").expect("the chatter file can be written");
        let outcome = parent.finish();
        finished.store(true, Ordering::Relaxed);
        outcome
    });
    let_go(&go);
    let covered_log = log_texts(&case.folder().join("covered.log"));
    assert_eq!(outcome.code, Some(124), "the covered gate's hung command fails its parent: {outcome:?}");
    assert!(
        covered_log.contains("gate: STALLED: "),
        "a sibling's wait under the parent is not the covered gate's progress: {covered_log}"
    );
    assert_passed(&critical.finish());
}

#[test]
fn a_pause_folder_the_watchdog_cannot_read_is_said_once() {
    let case = Case::new("a_pause_folder_the_watchdog_cannot_read_is_said_once");
    write_counts(&case.root(), r#"{"v":1,"heavy":1,"light":1}"#);
    let (go, critical) = holding_critical(&case);
    let critical_pid = critical.id();
    // The pause folder is a regular file, so no read of it can list the markers.
    fs::write(v1::paused_dir(&case.root()), "").expect("the pause folder can be made a file");

    // The parent samples every 100 ms and its child says it still waits every 200 ms, so several of the parent's reads
    // fail before the child's fifth still-waiting line.
    let parent = case.start(&run_words("parent.log", &SAMPLED, &child_gate("child.log", &HEARTBEATS)));
    wait_for_child_stderr(
        &case,
        "parent.log",
        &format!("gate: waiting for a heavy slot behind the critical request of gate {critical_pid}"),
    );
    wait_until("several passes of the child's wait", || {
        child_stderr(&case, "parent.log").matches(STILL_WAITING).count() >= 5
    });

    let_go(&go);
    let outcome = parent.finish();
    assert_passed(&outcome);
    let said: Vec<&str> = outcome.stderr.lines().filter(|line| line.starts_with(CANNOT_READ)).collect();
    assert_eq!(said.len(), 1, "the failing read is said once a watch: {outcome:?}");
    assert!(
        said.iter().all(|line| line.ends_with("; a gate under this one pauses nothing")),
        "{said:?}"
    );
    assert_passed(&critical.finish());
}

#[test]
fn a_none_gate_whose_routed_child_is_quiet_for_longer_than_its_stall_window_is_not_stalled() {
    let case = routed_case("a_none_gate_whose_routed_child_is_quiet_for_longer_than_its_stall_window_is_not_stalled");
    let go = case.folder().join("go");
    let steps = shown(&stub(&case, &[&format!("wait {}", shown(&go)), "status 0"]));

    // The heavy child routes beside its no-slot parent, takes the remote slot and waits on the go file: no output, next
    // to no CPU and no new process, as a hook's routed build is under a commit's gate.
    let mut child = vec![GATE];
    child.extend(routed_words("child.log", &steps, &[], &[r".\tool.exe", "out", "hi"]));
    let mut parent = case.start(&none_words("parent.log", &PARENT_WATCH, &child));
    wait_until("the routed child holding the remote slot, or the parent ending", || {
        remote_claim(&case).exists() || !parent.is_running()
    });
    assert!(
        remote_claim(&case).exists(),
        "the child routed and holds the remote slot: {}",
        child_stderr(&case, "parent.log")
    );

    // A parent that ended before two of its stall windows passed is the failure this case is about.
    let held = Instant::now();
    wait_until("two of the parent's stall windows passing, or the parent ending", || {
        held.elapsed() >= QUIET || !parent.is_running()
    });
    let waited_out = parent.is_running();

    let_go(&go);
    let outcome = parent.finish();
    assert!(waited_out, "the parent gate was killed while its routed child ran: {outcome:?}");
    assert_passed(&outcome);
    assert!(
        !outcome.stderr.contains("STALLED"),
        "a no-slot gate whose routed child is quiet is not stalled: {outcome:?}"
    );
}

#[test]
fn a_none_gate_whose_nested_gate_hangs_is_still_ended_by_the_nested_gate_s_own_stall() {
    let case = Case::new("a_none_gate_whose_nested_gate_hangs_is_still_ended_by_the_nested_gate_s_own_stall");

    // The nested gate's command hangs without output or CPU for longer than any case, so only a stall ends it; the
    // parent's window is a third of the nested gate's, so a parent that did not count the nested gate would stall first.
    let mut child = vec![GATE];
    child.extend(run_words("child.log", &NESTED_HUNG, &[FAKE_TOOL, "sleep", "590000"]));
    let outcome = case.start(&none_words("parent.log", &PARENT_WATCH, &child)).finish();

    assert!(
        !outcome.stderr.contains("STALLED"),
        "the parent itself is not stalled while its nested gate lives: {outcome:?}"
    );
    assert_eq!(outcome.code, Some(124), "the parent ends with its command's status: {outcome:?}");
    let child_log = log_texts(&case.folder().join("child.log"));
    assert!(
        child_log.contains("gate: STALLED: "),
        "the nested gate's own watchdog ends its hung command: {child_log}"
    );
}

#[test]
fn a_none_gate_whose_nested_gate_keeps_running_is_still_ended_by_its_own_ceiling() {
    let case = Case::new("a_none_gate_whose_nested_gate_keeps_running_is_still_ended_by_its_own_ceiling");
    let go = case.folder().join("go");
    let go_word = shown(&go);

    // The nested gate never stalls and its command waits on a go file the case writes only after the parent ended: only
    // the parent's 3 s ceiling, or its backstop on a starved machine, can end the parent, whose 1.5 s stall window the
    // nested gate holds off.
    let mut child = vec![GATE];
    child.extend(run_words("child.log", &HOLDER, &[FAKE_TOOL, "wait", &go_word, "exit", "0"]));
    let mut words = none_words("parent.log", &HUNG_WATCH, &child);
    set_timeout(&mut words, "3");
    let outcome = case.start(&words).finish();
    let_go(&go);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line.starts_with("gate: TIMED OUT after ") || line.starts_with("gate: BACKSTOP: ")),
        "the parent's own ceiling ends it: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("STALLED"), "{outcome:?}");
}

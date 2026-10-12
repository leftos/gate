//! Ctrl+C, raised by the gate on itself through its test seams: a running command still running 5 s later has its job
//! terminated and the gate exits as Ctrl+C does; a queued gate deletes its ticket, journals the abandon and exits so.

use std::fs;
use std::time::Duration;

use gate_state::v1::ticket_path;
use serde_json::Value;

use crate::support::{
    Case, Escapee, FAKE_TOOL, assert_in_order, events, release, run_words, said_pid, shown, wait_for_journal_line, wait_until, write_counts,
};

/// The status Windows reports for a process Ctrl+C ended, `STATUS_CONTROL_C_EXIT`, as a signed exit code.
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32.cast_signed();

#[test]
fn a_command_still_running_5_s_after_a_ctrl_c_has_its_job_terminated() {
    let case = Case::new("a_command_still_running_5_s_after_a_ctrl_c_has_its_job_terminated");
    let seams = ["--test-sample-ms", "100", "--test-interrupt-after-samples", "1"];

    // The seam raises only the gate's own interrupt: the command never sees a Ctrl+C, so it is still running 5 s later.
    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "out", "hi", "sleep", "30000"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &["gate: interrupted; stopping the command", "gate: FAILED (status -1073741510)"],
    );
    let release = release(&case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from("failed"), &Value::from(CONTROL_C_EXIT)),
        "{release}"
    );
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    let pid = said_pid(&log, "pid ");
    // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
    wait_until(&format!("the interrupted command {pid} ending"), || {
        gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
    });
}

#[test]
#[ignore = "needs the machine alone: run with -Slot critical and --ignored"]
fn a_ctrl_c_ends_a_child_outside_the_job_whose_launcher_has_ended() {
    let case = Case::new("a_ctrl_c_ends_a_child_outside_the_job_whose_launcher_has_ended");
    // The launcher lives a second, so a sample sees the escapee under it; the interrupt comes 30 samples in, at least
    // 3 s, when nothing in the job is the escapee's parent.
    let escapee = Escapee::new(&case);
    let command = escapee.launched_command();
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let seams = ["--test-sample-ms", "100", "--test-interrupt-after-samples", "30", "--test-job-breakaway"];

    let mut running = case.start(&run_words("out.log", &seams, &words));
    let (pid, away) = escapee.open(|| !running.is_running());
    let launcher = escapee.launcher_ended();
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert!(
        matches!(away.wait(Duration::ZERO), Ok(true)),
        "the escapee {pid}, whose launcher had ended, was ended at the interrupt: {outcome:?}"
    );
    assert!(
        matches!((launcher.exited(), away.exited()), (Ok(left), Ok(ended)) if left < ended),
        "the launcher had ended before the interrupt ended the escapee {pid}: {outcome:?}"
    );
}

#[test]
fn a_ctrl_c_in_the_queue_abandons_the_wait() {
    let case = Case::new("a_ctrl_c_in_the_queue_abandons_the_wait");
    write_counts(&case.root(), r#"{"v":1,"light":1}"#);
    let go = case.folder().join("go");
    let go_shown = shown(&go);
    let holder = case.start(&run_words("held.log", &[], &[FAKE_TOOL, "out", "holding", "wait", &go_shown]));
    wait_for_journal_line(&case.root(), &format!(r#""event":"take","pid":{},"#, holder.id()));

    let queued = case.start(&run_words("queued.log", &["--test-interrupt-queued"], &[FAKE_TOOL, "exit", "0"]));
    let pid = queued.id();
    let outcome = queued.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &["gate: waiting for a light slot", "gate: interrupted while waiting for a light slot"],
    );
    let own: Vec<Value> = events(&case).into_iter().filter(|event| event["pid"] == pid).collect();
    let names: Vec<&str> = own.iter().map(|event| event["event"].as_str().expect("an event has a name")).collect();
    assert_eq!(names, ["queue", "abandon"], "{own:?}");
    assert!(own[1]["waitedSeconds"].is_number(), "{own:?}");
    let tickets = ticket_path(&case.root(), pid, 0);
    let folder = tickets.parent().expect("a ticket has a folder");
    let left = fs::read_dir(folder)
        .expect("the tickets folder can be listed")
        .map(|entry| entry.expect("a ticket entry can be read").file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&format!("{pid}-")))
        .count();
    assert_eq!(left, 0, "the abandoning gate's ticket is gone");

    let mut holder = holder;
    assert!(holder.is_running(), "the holder runs on");
    fs::write(&go, "").expect("the go file can be made");
    let held = holder.finish();
    assert_eq!(held.code, Some(0), "{held:?}");
}

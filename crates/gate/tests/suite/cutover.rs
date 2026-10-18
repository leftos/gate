//! `gate cutover`: the counts copy, the flag, the lines it says and the words it refuses; the drain over live PowerShell
//! gates, stood in for by waiting helper processes whose claims and tickets name their real pid and start; a Ctrl+C and
//! the rerun that resumes; and the launcher's hand-over once the cutover is done. Every case writes under its own
//! LOCALAPPDATA, never the machine's, and no case asserts how long anything took.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::launcher::{install_gate, lines, refused_words, run_direct, run_launcher};
use crate::routing::CONTROL_C_EXIT;
use crate::support::{
    Case, FAKE_TOOL, Running, USAGE, assert_in_order, finish, powershell_folder, shown, write_powershell_claim, write_powershell_ticket,
};

/// `gate cutover` with a short confirmation window, so a drain with nothing to wait for ends soon; no case asserts how
/// long it took.
const CUTOVER: [&str; 3] = ["cutover", "--test-confirm-ms", "50"];
/// `gate cutover` as [`CUTOVER`] runs it, polling every 20 ms and saying it still waits every 100 ms, so a case waiting
/// on a gate soon reads a still-waiting line; no case asserts when one comes.
const WAITING: [&str; 7] = ["cutover", "--test-confirm-ms", "50", "--test-poll-ms", "20", "--test-heartbeat-ms", "100"];
/// The line a cutover with a gate to wait for starts its list with.
const ONE_GATE: &str = "gate: cutover: waiting for 1 PowerShell gate to finish:\n";
/// The line a cutover says once it has written the flag.
const FLAG_WRITTEN: &str = "gate: cutover: flag written; new gates now start gate.exe\n";

/// The cutover flag under `case`'s LOCALAPPDATA.
fn flag(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("cutover.flag")
}

/// The PowerShell gate's counts file under `case`'s LOCALAPPDATA.
fn powershell_counts(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("slot-counts.json")
}

/// `gate.exe`'s counts file in the default root under `case`'s LOCALAPPDATA.
fn v1_counts(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("v1").join("slot-counts.json")
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("the file has a folder")).expect("the file's folder can be made");
    fs::write(path, text).expect("the file can be written");
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the file can be read")).expect("the file is JSON")
}

/// The two lines a finished cutover ends with.
fn take_back(case: &Case) -> String {
    format!(
        "gate: cutover: done; every gate now runs gate.exe\n\
         gate: cutover: to go back to the PowerShell gate, delete {}\n",
        flag(case).display()
    )
}

/// The three lines a cutover with nothing to wait for says, after any said before them.
fn handed_over(case: &Case) -> String {
    format!("{FLAG_WRITTEN}{}", take_back(case))
}

/// A live process standing in for a PowerShell gate, `fake-tool wait <go>`, which ends once its go file exists; killed
/// when a case drops it unreleased.
struct Holder {
    child: Child,
    go: PathBuf,
    /// When its process started, as its claim or ticket records it.
    start: Timestamp,
}

impl Holder {
    /// Starts a holder whose go file is `<name>.go` in the case folder.
    fn start(case: &Case, name: &str) -> Holder {
        let go = case.folder().join(format!("{name}.go"));
        let mut child = Command::new(FAKE_TOOL)
            .arg("wait")
            .arg(&go)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the holder starts");
        let read = gate_win32::process_start_of(child.id());
        if !matches!(read, Ok(Some(_))) {
            // Not yet a Holder, so its drop would not end it: end it before the case fails, so it is not left waiting.
            let ended = child.kill().and_then(|()| child.wait());
            let _ = writeln!(io::stderr(), "the holder's start cannot be read; ending it gave {ended:?}");
        }
        let start = read.expect("the holder's start can be read").expect("the holder runs");
        Holder { child, go, start }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Its pid and process start, as a claim or ticket of its names them.
    fn gate(&self) -> (u32, Timestamp) {
        (self.pid(), self.start)
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("the holder's status can be read").is_none()
    }

    /// Lets it go and waits for it to end.
    fn release(mut self) {
        fs::write(&self.go, "").expect("the go file can be made");
        let status = self.child.wait().expect("the holder can be waited for");
        assert!(status.success(), "the holder ended with {status}");
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        if let Err(error) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            // A panic in drop would abort the run; the holder ends by itself once its wait gives up.
            let _ = writeln!(io::stderr(), "could not kill the holder {}: {error}", self.child.id());
        }
    }
}

/// The first whole line of `text` starting with one of `prefixes`, its line end included; `None` until one has ended.
fn line_starting(text: &str, prefixes: &[&str]) -> Option<String> {
    text.split_inclusive('\n')
        .find(|line| line.ends_with('\n') && prefixes.iter().any(|prefix| line.starts_with(prefix)))
        .map(str::to_owned)
}

/// Waits for the cutover's line starting with one of `prefixes` and then for a still-waiting line for `count` gates
/// after it, the cutover seen running each time: the evidence that it waits on the gate the line names. Returns the line.
fn named_while_waiting(cutover: &mut Running, prefixes: &[&str], count: &str) -> String {
    let text = cutover.wait_for_stderr_while_running(&format!("a line starting {prefixes:?}"), |text| line_starting(text, prefixes).is_some());
    let line = line_starting(&text, prefixes).expect("checked above");
    let still = format!("gate: cutover: still waiting for {count} (");
    cutover.wait_for_stderr_while_running(&format!("{still:?} after {line:?}"), |text| {
        text.find(&line).is_some_and(|at| text[at + line.len()..].contains(&still))
    });
    line
}

#[test]
fn cutover_waits_on_a_live_powershell_claim_and_names_it() {
    let case = Case::new("cutover-claim");
    let holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    let claim = write_powershell_claim(&case, "gate-slot-0", holder.gate(), "cargo build --release", holder.start);
    let mut cutover = case.start(&WAITING);

    let named = named_while_waiting(&mut cutover, &[&format!("gate:   pid {pid}  ")], "1 PowerShell gate");
    assert_eq!(named, format!("gate:   pid {pid}  heavy slot 0  cargo build --release\n"));
    assert!(flag(&case).is_file(), "the flag is written before the drain");
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = cutover.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            FLAG_WRITTEN,
            ONE_GATE,
            &named,
            "gate: cutover: still waiting for 1 PowerShell gate (",
            &format!("gate: cutover: pid {pid} finished (0 left)\n"),
        ],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn cutover_waits_on_a_live_powershell_ticket_and_reads_its_command_line() {
    let case = Case::new("cutover-ticket");
    let holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    write_powershell_ticket(&case, holder.gate(), "heavy", Timestamp::now());
    let command = proc_table::command_line_of(pid).expect("the holder's command line can be read");
    assert!(command.contains(&shown(&holder.go)), "not the holder's command line: {command}");
    let mut cutover = case.start(&WAITING);

    let named = named_while_waiting(&mut cutover, &[&format!("gate:   pid {pid}  ")], "1 PowerShell gate");
    assert_eq!(
        named,
        format!("gate:   pid {pid}  waiting (heavy)  {command}\n"),
        "a ticket names no command, so the line reads the process's"
    );
    holder.release();
    let outcome = cutover.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[FLAG_WRITTEN, ONE_GATE, &named, &format!("gate: cutover: pid {pid} finished (0 left)\n")],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn cutover_ignores_stale_claims_and_tickets() {
    let case = Case::new("cutover-stale");
    let an_hour_before = |start: Timestamp| {
        start
            .checked_sub(SignedDuration::from_hours(1))
            .expect("an hour before a holder started is a moment")
    };
    let ended = Holder::start(&case, "ended");
    // Started before the ended holder's pid is freed, so it cannot be given that pid.
    let mut live = Holder::start(&case, "live");
    assert_ne!(live.pid(), ended.pid(), "two running holders have two pids");
    // An hour off its real start too, so a process given the freed pid later still does not match the claim.
    let ended_start = an_hour_before(ended.start);
    write_powershell_claim(&case, "gate-slot-0", (ended.pid(), ended_start), "ended", ended_start);
    ended.release();
    let other_start = an_hour_before(live.start);
    let reused = (live.pid(), other_start);
    write_powershell_claim(&case, "gate-light-slot-1", reused, "reused", other_start);
    write_powershell_ticket(&case, reused, "light", other_start);
    write(&powershell_folder(&case).join("slots").join("gate-slot-2.json"), "not json");

    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "{FLAG_WRITTEN}gate: cutover: cannot read 1 claim or ticket file under {}; they are not waited on\n{}",
            powershell_folder(&case).display(),
            take_back(&case)
        ),
        "no gate of an ended pid or of a pid whose start differs is waited on or named"
    );
    assert!(live.is_running(), "the live process is not the gate its claim and ticket name");
    live.release();
}

#[test]
fn cutover_names_a_powershell_gate_that_appears_after_the_flag() {
    let case = Case::new("cutover-late");
    let first = Holder::start(&case, "first");
    let late = Holder::start(&case, "late");
    let (first_pid, late_pid) = (first.pid(), late.pid());
    write_powershell_claim(&case, "gate-slot-0", first.gate(), "cargo build", first.start);
    let mut cutover = case.start(&WAITING);

    named_while_waiting(&mut cutover, &[&format!("gate:   pid {first_pid}  ")], "1 PowerShell gate");
    write_powershell_claim(&case, "gate-slot-1", late.gate(), "cargo test --workspace", Timestamp::now());
    let named = named_while_waiting(
        &mut cutover,
        &[
            &format!("gate: cutover: late PowerShell gate pid {late_pid} "),
            &format!("gate: cutover: also waiting for pid {late_pid} "),
        ],
        "2 PowerShell gates",
    );
    assert_eq!(
        named,
        format!("gate: cutover: late PowerShell gate pid {late_pid} started after the flag: cargo test --workspace\n"),
        "a claim taken after the flag is a late arrival"
    );
    first.release();
    late.release();
    let outcome = cutover.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, ONE_GATE, &named]);
    for pid in [first_pid, late_pid] {
        assert!(
            outcome.stderr.contains(&format!("gate: cutover: pid {pid} finished (")),
            "{pid} is not said to finish: {}",
            outcome.stderr
        );
    }
    assert_eq!(outcome.stderr.matches(&format!("pid {late_pid} started")).count(), 1, "named once");
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn an_interrupted_cutover_leaves_the_flag_and_a_rerun_resumes() {
    let case = Case::new("cutover-interrupted");
    write(&powershell_counts(&case), r#"{"heavy":3}"#);
    let mut holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    write_powershell_claim(&case, "gate-light-slot-1", holder.gate(), "dotnet test", holder.start);
    let named = format!("gate:   pid {pid}  light slot 1  dotnet test\n");

    let interrupted = case.run(&[&CUTOVER[..], &["--test-cutover-interrupt-after-list"]].concat());

    assert_eq!(interrupted.code, Some(CONTROL_C_EXIT), "{interrupted:?}");
    assert_in_order(&interrupted.stderr, &[FLAG_WRITTEN, ONE_GATE, &named]);
    assert!(
        interrupted
            .stderr
            .ends_with("gate: cutover: interrupted; the flag stays set, and running gate cutover again resumes the wait\n"),
        "{}",
        interrupted.stderr
    );
    assert!(flag(&case).is_file(), "the flag stays set");
    assert!(holder.is_running());
    assert_eq!(json_of(&v1_counts(&case)), json!({"v": 1, "heavy": 3}));
    let edited = r#"{"v":1,"heavy":5}"#;
    write(&v1_counts(&case), edited);

    let mut rerun = case.start(&WAITING);
    let again = named_while_waiting(&mut rerun, &[&format!("gate:   pid {pid}  ")], "1 PowerShell gate");
    assert_eq!(again, named, "the rerun waits on the same gate");
    holder.release();
    let outcome = rerun.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[FLAG_WRITTEN, ONE_GATE, &named, &format!("gate: cutover: pid {pid} finished (0 left)\n")],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert_eq!(
        fs::read_to_string(v1_counts(&case)).expect("the v1 counts can be read"),
        edited,
        "the rerun keeps the edit"
    );
}

#[test]
fn cutover_then_the_launcher_hands_over() {
    let case = Case::new("cutover-launcher");
    let cutover = case.run(&CUTOVER);
    assert_eq!(cutover.code, Some(0), "{cutover:?}");
    let exe = install_gate(&case);
    let handed = run_launcher(&case, &refused_words());
    let direct = run_direct(&exe, &case, &refused_words());

    assert_eq!(handed.code, Some(2), "{handed:?}");
    assert_eq!(handed.code, direct.code, "{handed:?} against {direct:?}");
    assert_eq!(
        lines(&handed.stderr),
        lines(&direct.stderr),
        "not the status and output gate.exe gives directly"
    );
}

#[test]
fn cutover_with_nothing_to_wait_for_writes_the_flag_and_exits_0() {
    let case = Case::new("cutover-empty");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert!(flag(&case).is_file(), "the flag is written");
    assert!(!v1_counts(&case).exists(), "no PowerShell counts, so no v1 counts");
}

#[test]
fn cutover_copies_the_powershell_slot_counts_when_the_v1_file_is_absent() {
    let case = Case::new("cutover-copy");
    write(&powershell_counts(&case), r#"{"heavy":3}"#);
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case), "a copy says nothing");
    assert_eq!(json_of(&v1_counts(&case)), json!({"v": 1, "heavy": 3}));
    assert_eq!(
        fs::read_to_string(powershell_counts(&case)).expect("the PowerShell counts can be read"),
        r#"{"heavy":3}"#,
        "the PowerShell file is untouched"
    );
}

#[test]
fn a_rerun_keeps_a_v1_counts_file_edited_since() {
    let case = Case::new("cutover-rerun");
    write(&powershell_counts(&case), r#"{"heavy":3,"light":2}"#);
    assert_eq!(case.run(&CUTOVER).code, Some(0));
    let edited = r#"{"v":1,"heavy":5}"#;
    write(&v1_counts(&case), edited);
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert_eq!(fs::read_to_string(v1_counts(&case)).expect("the v1 counts can be read"), edited);
    assert!(flag(&case).is_file());
}

#[test]
fn powershell_counts_gate_state_rejects_are_ignored_with_a_line() {
    let case = Case::new("cutover-ignored");
    write(&powershell_counts(&case), "{heavy: 4}");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let (first, rest) = outcome.stderr.split_once('\n').expect("more than one line");
    let prefix = format!("gate: cutover: ignored {}: ", powershell_counts(&case).display());
    assert!(first.starts_with(&prefix), "{first}");
    assert!(first.ends_with("; gates use the default counts"), "{first}");
    assert_eq!(rest, handed_over(&case));
    assert!(!v1_counts(&case).exists(), "nothing is copied");
    assert!(flag(&case).is_file());
}

#[test]
fn cutover_refuses_an_extra_word_and_state_dir_and_writes_nothing() {
    let case = Case::new("cutover-words");
    for (words, problems) in [
        (&["cutover", "now"][..], "gate: unexpected argument now\n"),
        (
            &["cutover", "--state-dir", "elsewhere"][..],
            "gate: unknown option --state-dir\ngate: unexpected argument elsewhere\n",
        ),
    ] {
        let outcome = case.run(words);
        assert_eq!(outcome.code, Some(2), "{words:?}: {outcome:?}");
        assert_eq!(outcome.stderr, format!("{problems}{USAGE}"), "{words:?}");
        assert!(!case.local_app_data().exists(), "{words:?} writes nothing");
    }
}

#[test]
fn cutover_ignores_gate_state_dir() {
    let case = Case::new("cutover-state-dir");
    write(&powershell_counts(&case), r#"{"light":2}"#);
    let mut command = case.gate(&CUTOVER);
    command.env("GATE_STATE_DIR", case.root());
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, handed_over(&case));
    assert!(flag(&case).is_file());
    assert_eq!(json_of(&v1_counts(&case)), json!({"v": 1, "light": 2}));
    assert_eq!(case.root_entries(), Vec::<String>::new(), "nothing lands in GATE_STATE_DIR");
}

#[test]
fn cutover_without_local_app_data_exits_2() {
    let case = Case::new("cutover-no-appdata");
    for value in [None, Some("")] {
        let mut command = case.gate(&["cutover"]);
        match value {
            None => command.env_remove("LOCALAPPDATA"),
            Some(value) => command.env("LOCALAPPDATA", value),
        };
        let outcome = finish(command);
        assert_eq!(outcome.code, Some(2), "{value:?}: {outcome:?}");
        assert_eq!(outcome.stderr, "gate: cutover needs LOCALAPPDATA\n", "{value:?}");
        assert!(!case.local_app_data().exists());
    }
}

#[test]
fn cutover_with_a_file_in_place_of_the_gate_folder_exits_1_without_a_flag() {
    let case = Case::new("cutover-unwritable");
    write(&case.local_app_data().join("gate"), "not a folder");
    let outcome = case.run(&CUTOVER);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.starts_with("gate: cutover: cannot write the cutover flag: "),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.stderr.lines().count(), 1, "{}", outcome.stderr);
    assert!(case.local_app_data().join("gate").is_file(), "the file is left as it was");
}

#[test]
fn cutover_prints_its_request() {
    let case = Case::new("cutover-request");
    let outcome = case.run(&[
        "cutover",
        "--test-poll-ms",
        "20",
        "--test-heartbeat-ms=40",
        "--test-confirm-ms",
        "60",
        "--test-cutover-interrupt-after-list",
        "--test-print-request",
    ]);
    assert_eq!(
        outcome.request(),
        json!({
            "mode": "cutover",
            "localAppData": case.local_app_data().to_string_lossy(),
            "testSeams": {"pollMs": 20, "heartbeatMs": 40, "confirmMs": 60, "interruptAfterList": true},
        })
    );
    assert_eq!(
        case.run(&["cutover", "--test-print-request"]).request()["testSeams"],
        json!({"pollMs": null, "heartbeatMs": null, "confirmMs": null, "interruptAfterList": false})
    );
    assert!(!case.local_app_data().exists(), "printing the request writes nothing");
}

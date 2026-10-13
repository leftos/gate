//! `gate remote-cutover`: the drain marker and the flag, the drain over this PC's routed gates, stood in for by a
//! waiting helper process whose remote-slot claim names its real pid and start, the builder rounds through the suite's
//! stand-in for ssh, which runs this gate's own `box inflight` against a builder root in the case folder, a Ctrl+C and
//! the rerun that resumes, a real Ctrl+C sent to the drain's console during a builder call, the one drain at a time its
//! mutex keeps, the markers it takes over and those it refuses, the end that removes the marker unless it names another
//! live drain, and the end of a drain whose flag is deleted. Every case drains its own state root (`--state-dir`), so no PowerShell root is read, and no case asserts
//! how long anything took.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{self, Child, Command, Stdio};
use std::time::Duration;

use gate_state::{KernelObject, StateRoot};
use gate_win32::{NamedMutex, OwnedMutexGuard};
use jiff::Timestamp;
use serde_json::json;

use crate::routing::{CONTROL_C_EXIT, send_ctrl_c_to_the_console_of};
use crate::support::{Case, FAKE_TOOL, GATE, assert_in_order, finish, shown, start, start_in_own_console, wait_until, write_remote_json};

/// The suite's stand-in for ssh, which `--test-remote-ssh` names.
const FAKE_SSH: &str = env!("CARGO_BIN_EXE_fake-ssh");
/// The builder the routing file names.
const HOST: &str = "builder";
/// How many `ok` steps a case's stand-in has: more than the builder rounds any case makes.
const OK_STEPS: usize = 5000;
/// The line the drain says once it has written the marker and the flag.
const FLAG_WRITTEN: &str = "gate: remote-cutover: flag written; routed runs now wait for the drain\n";
/// The line a drain with one routed gate to wait for starts its list with.
const ONE_GATE: &str = "gate: remote-cutover: waiting for 1 routed gate to finish:\n";
/// The line a builder holding one run says.
const ONE_IN_FLIGHT: &str = "gate: remote-cutover: builder: 1 run(s) in flight\n";
/// The line an interrupted drain ends with.
const INTERRUPTED: &str =
    "gate: remote-cutover: interrupted; the flag and the drain marker stay, and running gate remote-cutover again resumes the drain\n";

/// The remote-cutover flag in `case`'s state root.
fn flag(case: &Case) -> PathBuf {
    case.root().join("remote-cutover.flag")
}

/// The drain marker in `case`'s state root.
fn marker(case: &Case) -> PathBuf {
    case.root().join("remote-cutover.draining")
}

/// The builder's root, a folder in the case folder, which the local transport asks `gate box inflight` about.
fn builder_root(case: &Case) -> PathBuf {
    case.folder().join("builder")
}

/// The stand-in's steps file.
fn steps(case: &Case) -> PathBuf {
    case.folder().join("ssh.steps")
}

/// Writes the stand-in's steps, `first` and then [`OK_STEPS`] `ok` steps, each a line.
fn write_steps(case: &Case, first: &[&str]) {
    let lines: Vec<&str> = first.iter().copied().chain(std::iter::repeat_n("ok", OK_STEPS)).collect();
    fs::write(steps(case), lines.join("\n")).expect("the steps can be written");
}

/// How many `gate box inflight` calls the stand-in took, from the log it keeps beside its steps.
fn inflight_calls(case: &Case) -> usize {
    let mut log = steps(case).into_os_string();
    log.push(".log");
    fs::read_to_string(PathBuf::from(log)).map_or(0, |text| text.lines().filter(|verb| *verb == "inflight").count())
}

/// A routing file naming the one builder [`HOST`]; its path.
fn write_builder(case: &Case) -> PathBuf {
    write_remote_json(case, &json!({ "host": HOST, "repos": {} }).to_string())
}

/// Writes the drain marker in `case`'s state root naming the process `pid` that started at `start`, as a drain does.
fn write_marker(case: &Case, pid: u32, start: Timestamp) {
    let start_ms = u64::try_from(start.as_millisecond()).expect("a process starts after 1970");
    let since_ms = u64::try_from(Timestamp::now().as_millisecond()).expect("now is after 1970");
    gate_state::v1::write_drain_marker(&case.root(), pid, start_ms, since_ms).expect("the marker can be written");
}

/// The pid the drain marker in `case`'s state root names.
fn marker_pid(case: &Case) -> u32 {
    gate_state::v1::read_drain_record(&marker(case))
        .expect("the marker can be read")
        .expect("the marker stands")
        .pid
}

/// This test process's pid and start: a live process that holds no drain.
fn own_process() -> (u32, Timestamp) {
    let pid = process::id();
    let start = gate_win32::process_start_of(pid)
        .expect("this process's start can be read")
        .expect("this process runs");
    (pid, start)
}

/// Takes the mutex a drain holds under `case`'s state root, as a drain that runs holds it, until it is dropped.
fn hold_drain_mutex(case: &Case) -> OwnedMutexGuard {
    let name = StateRoot::new(&shown(&case.root()), None).object_name(KernelObject::RemoteCutover);
    NamedMutex::open(&name)
        .expect("the drain mutex can be opened")
        .acquire_owned(Duration::ZERO)
        .expect("the drain mutex is free")
}

/// The line a run refused by a live drain whose marker names `pid` says.
fn drains_already(pid: u32) -> String {
    format!("gate: remote-cutover: pid {pid} drains already; let it finish, or end it and run gate remote-cutover again\n")
}

/// The line a run refused by a drain whose marker is missing, cannot be read or names no live drainer says.
fn drains_unwritten(case: &Case) -> String {
    format!(
        "gate: remote-cutover: another run drains already and has not written its marker {} yet; let it finish and run \
         gate remote-cutover again\n",
        marker(case).display()
    )
}

/// The line a drain whose flag was deleted while it drained ends with.
fn flag_deleted(case: &Case) -> String {
    format!(
        "gate: remote-cutover: the flag {} was deleted during the drain; routed runs use the bridge\n",
        flag(case).display()
    )
}

/// The two lines a finished remote cutover ends with.
fn take_back(case: &Case) -> String {
    format!(
        "gate: remote-cutover: done; routed runs use the Rust client\n\
         gate: remote-cutover: to go back to the bridge, delete {}\n",
        flag(case).display()
    )
}

/// `gate remote-cutover` on `case`'s state root, polling every 20 ms with a 50 ms confirmation window, its builders
/// asked through the stand-in, which runs this gate's `box`; `extra` after.
fn remote_cutover(case: &Case, extra: &[&str]) -> Command {
    let transport = format!("local:{}", builder_root(case).display());
    let mut command = case.gate_without_state_dir(&["remote-cutover", "--state-dir"]);
    command
        .arg(case.root())
        .args(["--test-poll-ms", "20", "--test-confirm-ms", "50", "--test-remote-transport", &transport])
        .args(["--test-remote-ssh", FAKE_SSH])
        .args(extra)
        .env("GATE_FAKE_SSH_STEPS", steps(case))
        .env("GATE_FAKE_SSH_GATE", GATE);
    command
}

/// Holds the builder's lock `app-1.lock` as a run does, open with no sharing, until the file is dropped.
fn hold_lock(case: &Case) -> File {
    let work = builder_root(case).join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(work.join("app-1.lock"))
        .expect("the lock can be held")
}

/// A live process standing in for a routed gate, `fake-tool wait <go>`, which ends once its go file exists; killed when
/// a case drops it unreleased.
struct Holder {
    child: Child,
    go: PathBuf,
    /// When its process started, as its claim records it.
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

/// Writes the claim a routed `gate.exe` run holding remote slot 0 writes in `case`'s state root, naming `holder` as its
/// gate, running `command`; its path.
fn write_remote_claim(case: &Case, holder: &Holder, command: &str) -> PathBuf {
    let claim = json!({
        "v": 1,
        "kind": "remote",
        "index": 0,
        "pid": holder.pid(),
        "processStart": holder.start.to_string(),
        "started": holder.start.to_string(),
        "cwd": case.folder().to_string_lossy(),
        "command": command,
        "log": "out.log",
        "timeoutSeconds": 900.0,
        "remote": true,
        "host": HOST,
        "key": "app-1",
    });
    let folder = case.root().join("slots");
    fs::create_dir_all(&folder).expect("the claims folder can be made");
    let path = folder.join("gate-remote-slot-0.json");
    fs::write(&path, claim.to_string()).expect("the claim can be written");
    path
}

#[test]
fn waits_on_a_live_routed_claim_and_names_it() {
    let case = Case::new("remote-cutover-claim");
    let holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    let claim = write_remote_claim(&case, &holder, "cargo build --release");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {pid}  cargo build --release\n");
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    assert!(flag(&case).is_file(), "the flag is written before the drain");
    assert!(marker(&case).is_file(), "the marker stands while the drain waits");
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            FLAG_WRITTEN,
            ONE_GATE,
            &named,
            &format!("gate: remote-cutover: pid {pid} finished (0 left)\n"),
        ],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn waits_while_a_builder_reports_a_run_in_flight() {
    let case = Case::new("remote-cutover-builder");
    write_builder(&case);
    write_steps(&case, &[]);
    let lock = hold_lock(&case);
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{ONE_IN_FLIGHT:?}"), |text| text.contains(ONE_IN_FLIGHT));
    assert!(marker(&case).is_file(), "the marker stands while a builder holds a run");
    drop(lock);
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, ONE_IN_FLIGHT]);
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(
        inflight_calls(&case) >= 2,
        "the builder is asked again once it has said a run is in flight"
    );
}

#[test]
fn a_builder_without_inflight_holds_the_drain_and_says_so() {
    let case = Case::new("remote-cutover-no-verb");
    write_builder(&case);
    write_steps(
        &case,
        &["say-exit 2 usage: gate box preflight|run|stop|boot|outputs <base64 JSON payload>"],
    );

    let outcome = finish(remote_cutover(&case, &[]));

    let cannot = "gate: remote-cutover: builder cannot say what runs there (status 2); run gate push\n";
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, cannot]);
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(
        inflight_calls(&case) >= 2,
        "a builder that cannot say holds the drain until it is asked again and answers"
    );
}

#[test]
fn an_interrupt_leaves_the_flag_and_marker_and_a_rerun_resumes() {
    let case = Case::new("remote-cutover-interrupted");
    let mut holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    let claim = write_remote_claim(&case, &holder, "dotnet test");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {pid}  dotnet test\n");

    let interrupted = finish(remote_cutover(&case, &["--test-cutover-interrupt-after-list"]));

    assert_eq!(interrupted.code, Some(CONTROL_C_EXIT), "{interrupted:?}");
    assert_in_order(&interrupted.stderr, &[FLAG_WRITTEN, ONE_GATE, &named]);
    assert!(interrupted.stderr.ends_with(INTERRUPTED), "{}", interrupted.stderr);
    assert!(flag(&case).is_file(), "the flag stays");
    assert!(marker(&case).is_file(), "the marker stays");
    assert!(holder.is_running());

    let mut rerun = start(remote_cutover(&case, &[]));
    rerun.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = rerun.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            FLAG_WRITTEN,
            ONE_GATE,
            &named,
            &format!("gate: remote-cutover: pid {pid} finished (0 left)\n"),
        ],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the rerun's end removes the marker");
}

#[test]
fn an_interrupt_during_the_last_builder_round_keeps_the_flag_and_marker() {
    let case = Case::new("remote-cutover-round-interrupted");
    write_builder(&case);
    write_steps(&case, &[]);

    let outcome = finish(remote_cutover(&case, &["--test-cutover-interrupt-after-round"]));

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{FLAG_WRITTEN}{INTERRUPTED}"));
    assert!(inflight_calls(&case) >= 1, "the builder was asked, and said 0");
    assert!(flag(&case).is_file(), "the flag stays");
    assert!(marker(&case).is_file(), "the marker stays");
}

#[test]
fn a_second_run_while_one_drains_is_refused_and_names_it() {
    let case = Case::new("remote-cutover-second");
    let holder = Holder::start(&case, "holder");
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {}  cargo build\n", holder.pid());
    let mut first = start(remote_cutover(&case, &[]));
    first.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));

    // A second run that is not refused lists the holder and is interrupted, so it ends at once instead of draining.
    let second = finish(remote_cutover(&case, &["--test-cutover-interrupt-after-list"]));

    assert_eq!(second.code, Some(1), "{second:?}");
    assert_eq!(second.stderr, drains_already(first.id()));
    assert_eq!(marker_pid(&case), first.id(), "the marker still names the first run");
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = first.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the first run's end removes its marker");
}

#[test]
fn an_unreadable_entry_holds_the_drain_and_names_it() {
    let case = Case::new("remote-cutover-unreadable");
    let folder = case.root().join("slots");
    fs::create_dir_all(&folder).expect("the claims folder can be made");
    let claim = folder.join("gate-remote-slot-0.json");
    fs::write(&claim, "not a claim").expect("the claim can be written");
    write_steps(&case, &[]);
    let named = format!("gate:   {}, which cannot be read; remove it once no gate uses it\n", claim.display());
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    assert!(marker(&case).is_file(), "the marker stands while an entry cannot be read");
    fs::remove_file(&claim).expect("the unreadable claim can be removed");
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            FLAG_WRITTEN,
            ONE_GATE,
            &named,
            &format!("gate: remote-cutover: {} is gone (0 left)\n", claim.display()),
        ],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn a_gate_that_comes_while_the_builders_are_asked_starts_the_drain_over() {
    let case = Case::new("remote-cutover-over");
    write_builder(&case);
    let go = case.folder().join("asked.go");
    let held = format!("hold {}", go.display());
    write_steps(&case, &[held.as_str()]);
    let mut drain = start(remote_cutover(&case, &[]));

    wait_until("the builder is asked", || inflight_calls(&case) >= 1);
    let holder = Holder::start(&case, "holder");
    let pid = holder.pid();
    let claim = write_remote_claim(&case, &holder, "cargo test");
    fs::write(&go, "").expect("the go file can be made");
    let named = format!("gate:   pid {pid}  cargo test\n");
    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stderr,
        &[
            FLAG_WRITTEN,
            ONE_GATE,
            &named,
            &format!("gate: remote-cutover: pid {pid} finished (0 left)\n"),
        ],
    );
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(inflight_calls(&case) >= 2, "the builder is asked again once the drain has started over");
}

#[test]
fn a_builder_that_never_answers_holds_the_drain_until_its_bound() {
    let case = Case::new("remote-cutover-silent");
    write_builder(&case);
    let never = format!("hold {}", case.folder().join("never.go").display());
    write_steps(&case, &[never.as_str()]);

    let outcome = finish(remote_cutover(&case, &["--test-remote-bound-ms", "1000"]));

    let silent = "gate: remote-cutover: builder cannot say what runs there (no answer in 1 s)\n";
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, silent]);
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(
        inflight_calls(&case) >= 2,
        "a builder that gave no answer holds the drain until it is asked again and answers"
    );
}

#[test]
fn a_builder_that_exits_0_without_the_count_line_holds_the_drain() {
    let case = Case::new("remote-cutover-no-count");
    write_builder(&case);
    write_steps(&case, &["exit 0"]);

    let outcome = finish(remote_cutover(&case, &[]));

    let cannot = "gate: remote-cutover: builder cannot say what runs there (status 0); run gate push\n";
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, cannot]);
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(
        inflight_calls(&case) >= 2,
        "a builder that cannot say holds the drain until it is asked again and answers"
    );
}

#[test]
fn done_removes_the_marker_and_keeps_the_flag() {
    let case = Case::new("remote-cutover-done");
    write_builder(&case);
    write_steps(&case, &[]);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("{FLAG_WRITTEN}{}", take_back(&case)));
    assert!(inflight_calls(&case) >= 1, "the builder is asked");
    assert!(!marker(&case).exists(), "the marker is removed");
    assert!(flag(&case).is_file(), "the flag stays");
}

#[test]
fn a_ctrl_c_during_a_builder_call_ends_the_call_and_keeps_the_flag_and_marker() {
    let case = Case::new("remote-cutover-ctrl-c");
    write_builder(&case);
    let never = format!("hold {}", case.folder().join("never.go").display());
    write_steps(&case, &[never.as_str()]);
    // The call's bound is a hang guard: only the Ctrl+C can end the call before it, however loaded the machine is.
    let drain = start_in_own_console(remote_cutover(&case, &["--test-remote-bound-ms", "300000"]));

    wait_until("the builder is asked", || inflight_calls(&case) >= 1);
    let sent = send_ctrl_c_to_the_console_of(drain.id());
    assert!(sent.status.success(), "the Ctrl+C is sent: {sent:?}");
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    // A call its bound ended would have said the builder gave no answer.
    assert_eq!(outcome.stderr, format!("{FLAG_WRITTEN}{INTERRUPTED}"));
    assert_eq!(inflight_calls(&case), 1, "no builder is asked after the Ctrl+C");
    assert!(flag(&case).is_file(), "the flag stays");
    assert!(marker(&case).is_file(), "the marker stays");
}

#[test]
fn a_run_while_another_holds_the_drain_is_refused_and_names_the_marker_s_pid() {
    let case = Case::new("remote-cutover-held");
    let _drain = hold_drain_mutex(&case);
    let (pid, start) = own_process();
    write_marker(&case, pid, start);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr, drains_already(pid));
    assert_eq!(marker_pid(&case), pid, "the marker is left as it was");
    assert!(!flag(&case).exists(), "a refused run writes no flag");
}

#[test]
fn a_run_while_another_holds_the_drain_and_its_marker_cannot_be_read_is_refused_and_names_the_marker() {
    let case = Case::new("remote-cutover-held-unread");
    let _drain = hold_drain_mutex(&case);
    // A folder at the marker's name: its open is refused, as a file another handle holds can be.
    fs::create_dir(marker(&case)).expect("a folder can be made at the marker's name");

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr, drains_unwritten(&case));
    assert!(!flag(&case).exists(), "a refused run writes no flag");
}

#[test]
fn a_run_while_another_holds_the_drain_and_its_marker_names_a_dead_drainer_names_no_pid() {
    let case = Case::new("remote-cutover-held-stale");
    let _drain = hold_drain_mutex(&case);
    // The marker a dead drain left, which the run holding the mutex has not overwritten yet.
    let holder = Holder::start(&case, "dead");
    let (pid, start) = (holder.pid(), holder.start);
    holder.release();
    write_marker(&case, pid, start);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr, drains_unwritten(&case));
    assert_eq!(marker_pid(&case), pid, "the marker is left as it was");
    assert!(!flag(&case).exists(), "a refused run writes no flag");
}

#[test]
fn two_runs_started_together_drain_one_at_a_time() {
    let case = Case::new("remote-cutover-together");
    let holder = Holder::start(&case, "holder");
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let mut runs = [start(remote_cutover(&case, &[])), start(remote_cutover(&case, &[]))];

    // The holder keeps the drain going, so a run that ends before it is let go was refused.
    wait_until("one of the two runs ends", || runs.iter_mut().any(|run| !run.is_running()));
    let [mut first, second] = runs;
    let (refused, mut drainer) = if first.is_running() { (second, first) } else { (first, second) };
    assert!(drainer.is_running(), "one of the two runs drains");
    let refused = refused.finish();

    assert_eq!(refused.code, Some(1), "{refused:?}");
    // A run refused before the drainer has written its marker finds none to name.
    assert!(
        refused.stderr == drains_already(drainer.id()) || refused.stderr == drains_unwritten(&case),
        "{refused:?}"
    );
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drainer.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the drainer's end removes its marker");
}

#[test]
fn a_dead_drainer_s_marker_is_taken_over() {
    let case = Case::new("remote-cutover-dead");
    let holder = Holder::start(&case, "dead");
    let (pid, start) = (holder.pid(), holder.start);
    holder.release();
    write_marker(&case, pid, start);
    write_builder(&case);
    write_steps(&case, &[]);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{FLAG_WRITTEN}{}", take_back(&case)));
    assert!(!marker(&case).exists(), "the marker taken over is removed at the end");
}

#[test]
fn a_marker_whose_live_pid_started_at_another_moment_is_taken_over() {
    let case = Case::new("remote-cutover-reused");
    // This test process lives, but not since the start the marker names, as a pid reused since its drainer died.
    let (pid, start) = own_process();
    let earlier = Timestamp::from_millisecond(start.as_millisecond() - 1000).expect("a second before this process's start is a moment");
    write_marker(&case, pid, earlier);
    write_builder(&case);
    write_steps(&case, &[]);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, format!("{FLAG_WRITTEN}{}", take_back(&case)));
    assert!(!marker(&case).exists(), "the marker taken over is removed at the end");
}

#[test]
fn a_marker_naming_a_live_drainer_is_refused_though_the_mutex_is_free() {
    let case = Case::new("remote-cutover-live-other");
    // This test process lives since the start the marker names and holds no mutex, as a drain in another logon session,
    // or one that reaches the state root by another spelling, holds another one.
    let (pid, start) = own_process();
    write_marker(&case, pid, start);
    write_builder(&case);
    write_steps(&case, &[]);

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr, drains_already(pid));
    assert_eq!(marker_pid(&case), pid, "the live drainer's marker is left");
    assert!(!flag(&case).exists(), "a refused run writes no flag");
}

#[test]
fn a_flag_that_cannot_be_written_leaves_no_marker() {
    let case = Case::new("remote-cutover-flag-unwritable");
    // A folder at the flag's name: opening it for writing is refused.
    fs::create_dir(flag(&case)).expect("a folder can be made at the flag's name");

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let cannot = format!("gate: remote-cutover: cannot write the flag {}: ", flag(&case).display());
    assert!(outcome.stderr.starts_with(&cannot), "{}", outcome.stderr);
    assert_eq!(outcome.stderr.lines().count(), 1, "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the marker the run wrote is removed");
}

#[test]
fn a_marker_that_cannot_be_read_when_the_drain_starts_is_refused() {
    let case = Case::new("remote-cutover-unread-start");
    fs::write(marker(&case), "not a marker").expect("the marker can be written");

    let outcome = finish(remote_cutover(&case, &[]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let start = format!("gate: remote-cutover: cannot read the drain marker {}: ", marker(&case).display());
    assert!(outcome.stderr.starts_with(&start), "{}", outcome.stderr);
    assert!(
        outcome
            .stderr
            .ends_with("; no other run drains, so delete it and run gate remote-cutover again\n"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fs::read_to_string(marker(&case)).expect("the marker stands"), "not a marker");
    assert!(!flag(&case).exists(), "a refused run writes no flag");
}

#[test]
fn a_marker_rewritten_to_name_a_dead_drainer_is_removed() {
    let case = Case::new("remote-cutover-rewritten-dead");
    let holder = Holder::start(&case, "holder");
    let other = holder.pid();
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {other}  cargo build\n");
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    // The holder ends before the drain does, so the drainer the marker now names is dead at the end.
    write_marker(&case, other, holder.start);
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the marker naming a dead drainer is removed");
}

#[test]
fn a_marker_rewritten_to_name_a_live_drainer_is_left_to_it() {
    let case = Case::new("remote-cutover-rewritten-live");
    let holder = Holder::start(&case, "holder");
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {}  cargo build\n", holder.pid());
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    // This test process lives since the start the marker now names, through the drain's end.
    let (other, other_start) = own_process();
    write_marker(&case, other, other_start);
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let left = format!(
        "gate: remote-cutover: drained, but the drain marker {} now names pid {other}, whose drain removes it\n",
        marker(&case).display()
    );
    assert!(outcome.stderr.ends_with(&left), "{}", outcome.stderr);
    assert_eq!(marker_pid(&case), other, "the marker naming a live drainer is left");
}

#[test]
fn a_flag_deleted_during_the_drain_ends_it_without_done() {
    let case = Case::new("remote-cutover-flag-deleted");
    let holder = Holder::start(&case, "holder");
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {}  cargo build\n", holder.pid());
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    fs::remove_file(flag(&case)).expect("the flag can be deleted");
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(outcome.stderr.ends_with(&flag_deleted(&case)), "{}", outcome.stderr);
    assert!(!outcome.stderr.contains("done;"), "{}", outcome.stderr);
    assert!(!marker(&case).exists(), "the drain's marker is removed");
    assert!(!flag(&case).exists(), "the deleted flag is not written again");
}

#[test]
fn a_marker_that_cannot_be_read_at_the_end_is_left_and_the_drain_fails() {
    let case = Case::new("remote-cutover-unread-end");
    let holder = Holder::start(&case, "holder");
    let claim = write_remote_claim(&case, &holder, "cargo build");
    write_steps(&case, &[]);
    let named = format!("gate:   pid {}  cargo build\n", holder.pid());
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{named:?}"), |text| text.contains(&named));
    // A folder in the marker's place: its open is refused for the whole end, as a file another handle holds can be.
    fs::remove_file(marker(&case)).expect("the marker can be removed");
    fs::create_dir(marker(&case)).expect("a folder can be made at the marker's name");
    fs::remove_file(&claim).expect("the claim can be removed, as its gate does when it ends");
    holder.release();
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let left = format!(
        "gate: remote-cutover: cannot read the drain marker {}: it exists but reads as none; it is left\n",
        marker(&case).display()
    );
    assert!(outcome.stderr.ends_with(&left), "{}", outcome.stderr);
    assert!(!outcome.stderr.contains("done;"), "{}", outcome.stderr);
    assert!(marker(&case).is_dir(), "what stands at the marker's name is left");
}

#[test]
fn a_routing_file_gone_since_the_drain_began_holds_the_drain() {
    let case = Case::new("remote-cutover-routing-gone");
    let routing = write_builder(&case);
    write_steps(&case, &[]);
    let lock = hold_lock(&case);
    let mut drain = start(remote_cutover(&case, &[]));

    drain.wait_for_stderr_while_running(&format!("{ONE_IN_FLIGHT:?}"), |text| text.contains(ONE_IN_FLIGHT));
    fs::remove_file(&routing).expect("the routing file can be removed");
    let gone = format!(
        "gate: remote-cutover: {} is gone since the drain began; no builder can be asked until it is back\n",
        routing.display()
    );
    drain.wait_for_stderr_while_running(&format!("{gone:?}"), |text| text.contains(&gone));
    write_builder(&case);
    drop(lock);
    let outcome = drain.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(&outcome.stderr, &[FLAG_WRITTEN, ONE_IN_FLIGHT, &gone]);
    assert!(outcome.stderr.ends_with(&take_back(&case)), "{}", outcome.stderr);
}

#[test]
fn a_relative_localappdata_is_refused() {
    let case = Case::new("remote-cutover-relative");
    let mut command = remote_cutover(&case, &[]);
    command.env("LOCALAPPDATA", "relative");

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: remote-cutover needs LOCALAPPDATA to be an absolute folder, not relative\n"
    );
    assert!(!marker(&case).exists(), "no marker is written");
    assert!(!flag(&case).exists(), "no flag is written");
}

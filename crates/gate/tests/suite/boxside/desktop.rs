//! A run that asks for the builder's desktop, on the box: the console's session refused before the checkout, the
//! builder's desktop lock waited for after it, each refusal's `desktop` tag and exit 75 with no back commits line, the
//! payloads refused with exit 2, and the command handed to the seam's stand-in helper and run in the run's job.

use std::fs;
use std::path::Path;

use gate_win32::{OpenedJob, StoppableProcess};
use proc_table::{ProcessTable, Win32ProcessTable};
use regex::Regex;
use serde_json::{Value, json};

use super::{
    BoxCall, ENGINE_ENV_NAME, GIT_BIN_STAND_IN, QUIET_CLIENT_GONE_GUARD, STALE_ENGINE_ENV, adjusted_seconds, bare_box_call, box_call, ended_within,
    engine_env, fake_godot, fake_unreal, field, gate_box, godot_root, let_go, lines_starting, pair_payload, pair_snapshot, path_for_run, path_godot,
    payload as request_line, refused_line, run_box, snapshot, started_line, stopped_line_for, unreal_root,
};
use crate::support::{
    Case, Closed, FAKE_TOOL, HeldDesktop, KeptDesktop, Outcome, box_payload, build_version, copy_tool, desktop_payload, desktop_seams, junction,
    shown, wait_until, wait_until_closed,
};

/// The box's prefix of every line refusing a run the builder's desktop cannot take.
const NO_DESKTOP: &str = "gate: not routed: the builder has no desktop session: ";

/// A snapshot no refusal before the checkout ever fetches.
const UNFETCHED: &str = "0123456789abcdef0123456789abcdef01234567";

/// The command of every desktop case that gives none of its own: it says `ran` once it runs.
const COMMAND: &[&str] = &[FAKE_TOOL, "out", "ran"];

/// The cap on every desktop wait of a case that ends before the hand-off, in milliseconds.
const DESKTOP_MS: &str = "50";

/// The cap on every desktop wait of a case whose run hands its command to the stand-in helper, in milliseconds: large
/// enough for a real helper's start and connect on a busy machine, and never reached by a passing run.
const HAND_OFF_MS: &str = "10000";

/// `NORMAL_PRIORITY_CLASS`, which the helper gives the command although it runs below normal itself.
const NORMAL_PRIORITY: &str = "priority 0x20";

/// The line the box says when another run held the builder's desktop for the whole of a one-second wait.
const BUSY_FOR_1_S: &str = "gate: not routed: the builder's desktop stayed busy for 1 s";

/// The line the box says once when another run holds the builder's desktop as it starts waiting.
const BUSY_WAIT: &str = "gate: remote waiting for the builder's desktop: another run holds it";

/// The line the box says when it takes the builder's desktop from a run that ended holding it.
const ABANDONED: &str = "gate: remote took the builder's desktop from a run that ended without releasing it";

/// The stand-in helper's seam that names the input desktop in place of the one it reads.
const INPUT_DESKTOP_VARIABLE: &str = "GATE_DESKTOP_TEST_INPUT_DESKTOP";

/// The input desktop the helper must be on for the box to hand it the command.
const DEFAULT_DESKTOP: &str = "Default";

/// `gate box` with `args` after `box` and the request line `request`, not yet started, the box's environment naming
/// [`DEFAULT_DESKTOP`] as the stand-in helper's input desktop ([`INPUT_DESKTOP_VARIABLE`]): so its `Hello` names
/// `Default` in whatever session the suite runs, a builder's session 0 among them, while the helper still judges the name.
fn desktop_box_call(case: &Case, args: &[&str], request: &str) -> BoxCall {
    let mut call = box_call(case, args, request);
    call.env(INPUT_DESKTOP_VARIABLE, DEFAULT_DESKTOP);
    call
}

/// `gate box run` of `payload` with the desktop seams of `case`: the fault `mode`, the console session `session` and the
/// cap `ms` on its desktop waits.
fn run_desktop(case: &Case, payload: &Value, mode: &str, session: &str, ms: &str) -> Outcome {
    let seams = desktop_seams(case, mode, session, ms);
    let mut args = vec!["box"];
    args.extend(seams.iter().map(String::as_str));
    args.push("run");
    desktop_box_call(case, &args, &request_line(payload)).finish()
}

/// `gate box run` of `payload` handed to the stand-in helper with the fault `mode`, at an active console.
fn handed_off(case: &Case, payload: &Value, mode: &str) -> Outcome {
    desktop_call(case, payload, mode, &[]).finish()
}

/// `gate box run` of `payload` with the desktop seams of a hand-off with the fault `mode` at an active console, and the
/// box's `extra` seams, not yet started.
fn desktop_call(case: &Case, payload: &Value, mode: &str, extra: &[&str]) -> BoxCall {
    let seams = desktop_seams(case, mode, "active", HAND_OFF_MS);
    let mut args = vec!["box"];
    args.extend(seams.iter().map(String::as_str));
    args.extend_from_slice(extra);
    args.push("run");
    desktop_box_call(case, &args, &request_line(payload))
}

/// The desktop payload of `case`'s work folder `k` running `command`, its snapshot made.
fn ready_payload(case: &Case, command: &[&str]) -> Value {
    let mut payload = desktop_payload(case, "k", command, 600);
    snapshot(case, &mut payload, &[("a.txt", "a\n")]);
    payload
}

/// Checks `outcome` ran its command: no refusal tag, no back commits line, and the start marker said once.
fn ran_its_command(outcome: &Outcome, payload: &Value) {
    assert!(lines_starting(outcome, "gate-remote-refused:").is_empty(), "no refusal: {outcome:?}");
    assert!(
        lines_starting(outcome, "gate: remote back commits: failed").is_empty(),
        "no back commits failure: {outcome:?}"
    );
    let marker = started_line(payload);
    assert_eq!(lines_starting(outcome, &marker), [marker.as_str()], "the marker, once: {outcome:?}");
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

    let outcome = run_desktop(&case, &payload, "ok", session, DESKTOP_MS);

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

    let before = run_desktop(&case, &payload, "ok", "none", DESKTOP_MS);
    refused_with(&before, &payload, &format!("{NO_DESKTOP}nobody is logged on at the console"));

    let held = HeldDesktop::hold(&case);
    let after = run_desktop(&case, &payload, "ok", "active", DESKTOP_MS);
    drop(held);
    refused_with(&after, &payload, BUSY_FOR_1_S);
}

#[test]
fn a_desktop_run_refused_before_its_command_leaves_no_command_record() {
    let case = Case::new("desktop_refused_no_record");
    let mut payload = desktop_payload(&case, "k", COMMAND, 1);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let held = HeldDesktop::hold(&case);
    let outcome = run_desktop(&case, &payload, "ok", "active", DESKTOP_MS);
    drop(held);

    refused_with(&outcome, &payload, BUSY_FOR_1_S);
    let record = format!("{}.command", field(&payload, "Work"));
    assert!(!Path::new(&record).exists(), "a command that never started leaves no record: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_desktop_run_releases_the_builders_desktop_when_it_ends() {
    let case = Case::new("desktop_released");
    let payload = ready_payload(&case, COMMAND);
    // Open across both runs, so a lock the first box left taken would be found abandoned by the second.
    let _kept = KeptDesktop::open(&case);

    for run in ["first", "second"] {
        let outcome = handed_off(&case, &payload, "ok");

        assert_eq!(outcome.code, Some(0), "the {run} run ran its command: {outcome:?}");
        assert_eq!(outcome.stdout, "ran\n", "the {run} run's command output: {outcome:?}");
        assert!(
            lines_starting(&outcome, BUSY_WAIT).is_empty(),
            "the {run} run found the lock free: {outcome:?}"
        );
        assert!(
            lines_starting(&outcome, ABANDONED).is_empty(),
            "the {run} run found the lock released, not abandoned: {outcome:?}"
        );
    }
}

#[test]
fn a_desktop_run_prints_the_marker_after_the_command_is_adopted_and_before_its_output() {
    let case = Case::new("desktop_marker");
    let payload = ready_payload(&case, &[FAKE_TOOL, "err", "first", "out", "x"]);

    let outcome = handed_off(&case, &payload, "ok");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    ran_its_command(&outcome, &payload);
    let marker = started_line(&payload);
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let at = |wanted: &str| lines.iter().position(|line| *line == wanted);
    assert!(
        at(&marker).zip(at("first")).is_some_and(|(marker, first)| marker < first),
        "the marker comes before the command's first line: {outcome:?}"
    );
}

#[test]
fn a_desktop_run_passes_the_commands_status_and_output() {
    let case = Case::new("desktop_status");
    let payload = ready_payload(&case, &[FAKE_TOOL, "out", "ran", "err", "e", "exit", "7"]);

    let outcome = handed_off(&case, &payload, "ok");

    assert_eq!(outcome.code, Some(7), "the command's own status: {outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert!(outcome.stderr.lines().any(|line| line == "e"), "the command's stderr: {outcome:?}");
    ran_its_command(&outcome, &payload);
}

/// Whether the input desktop, as the suite reads it in its own process, is `Default`: an unreadable one is not, as the
/// helper reads it.
fn on_default_desktop() -> bool {
    gate_win32::input_desktop_name().is_ok_and(|name| name == DEFAULT_DESKTOP)
}

/// The one hand-off whose stand-in helper reads the input desktop itself, no name given in its place: on the `Default`
/// input desktop, as the suite reads it in its own process, the run is handed off and runs its command; on any other,
/// a builder's session 0 among them, the box refuses it as a helper not on the desktop. When the suite's readings
/// before and after the run differ, as a lock screen or a UAC prompt between them makes them, the helper may have read
/// either, so either outcome passes, each with its full checks.
#[test]
fn the_real_helper_checks_the_input_desktop() {
    let case = Case::new("desktop_real_check");
    let payload = ready_payload(&case, COMMAND);
    let before = on_default_desktop();
    let mut call = desktop_call(&case, &payload, "ok", &[]);
    call.env_remove(INPUT_DESKTOP_VARIABLE);

    let outcome = call.finish();

    let after = on_default_desktop();
    let on_default = if before == after { before } else { outcome.code == Some(0) };
    if on_default {
        assert_eq!(outcome.code, Some(0), "handed off on the Default desktop: {outcome:?}");
        assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
        ran_its_command(&outcome, &payload);
    } else {
        refused_with(
            &outcome,
            &payload,
            &format!("{NO_DESKTOP}the desktop helper is not on the console's Default desktop"),
        );
    }
}

#[test]
fn a_desktop_command_is_in_the_runs_job_at_normal_priority() {
    let case = Case::new("desktop_job");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let payload = ready_payload(&case, &[FAKE_TOOL, "priority", "pid", "wait", &go_word]);

    let running = desktop_call(&case, &payload, "ok", &[]).start();
    running.wait_for_stdout("pid ");
    let members = OpenedJob::open_to_list(field(&payload, "Job"))
        .expect("the job can be opened")
        .expect("the job exists while its command waits")
        .process_ids()
        .expect("the job's processes are listed");
    // Named while the command still waits, so no pid of the job has been reused yet.
    let names: Vec<String> = Win32ProcessTable
        .snapshot_without_command_lines()
        .into_iter()
        .filter(|process| members.contains(&process.pid))
        .map(|process| process.name)
        .collect();
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let pid: u32 = outcome
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("pid "))
        .and_then(|pid| pid.trim().parse().ok())
        .expect("the command printed its pid");
    assert!(members.contains(&pid), "the run's job holds the command: {members:?} {outcome:?}");
    let commands = names.iter().filter(|name| name.eq_ignore_ascii_case("fake-tool.exe")).count();
    assert_eq!(commands, 1, "the command, once: {names:?}");
    assert!(
        names
            .iter()
            .all(|name| name.eq_ignore_ascii_case("fake-tool.exe") || name.eq_ignore_ascii_case("conhost.exe")),
        "beside the command, the job holds only its console host, no console donor: {names:?} {outcome:?}"
    );
    assert!(
        outcome.stdout.lines().any(|line| line == NORMAL_PRIORITY),
        "normal priority, though the stand-in helper runs below normal: {outcome:?}"
    );
}

#[test]
fn a_desktop_command_gets_the_boxs_environment_and_working_folder() {
    let case = Case::new("desktop_environment");
    let mut payload = ready_payload(
        &case,
        &[
            FAKE_TOOL,
            "env",
            "Logging__EventLog__LogLevel__Default",
            "env",
            "GIT_CONFIG_COUNT",
            "env",
            "NuGetAudit",
            "cwd",
        ],
    );
    payload["Env"] = json!({ "NuGetAudit": "false" });
    // The box inherits no config entries, whatever the environment this suite runs in holds.
    let mut command = desktop_call(&case, &payload, "ok", &[]);
    command.env_remove("GIT_CONFIG_COUNT");

    let outcome = command.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(
        lines.get(..3),
        Some(["None", "1", "false"].as_slice()),
        "the Event Log provider off, git's safe.directory entry, the caller's pair: {outcome:?}"
    );
    let cwd = lines.get(3).and_then(|line| line.strip_prefix("cwd ")).expect("the cwd line");
    assert!(
        cwd.eq_ignore_ascii_case(field(&payload, "Work")),
        "the command runs in the work folder: {outcome:?}"
    );
}

/// A desktop run with a `GodotVersion` resolves a Godot console word as any run does: to the root's install of that
/// version, never the `PATH`'s, with `GODOT_PATH` naming that install.
#[test]
fn a_desktop_run_runs_the_godot_install_of_the_chosen_version() {
    let case = Case::new("desktop_godot_chosen");
    let root = godot_root(&case);
    fake_godot(&root.join("4.3.0"), "4.3.stable.mono.official.y\n");
    fake_godot(&root.join("4.7.2"), "4.7.2.stable.mono.official.x\n");
    let mut payload = ready_payload(&case, &["godot_CONSOLE.exe", "--version", "env", "GODOT_PATH"]);
    payload["GodotVersion"] = json!("4.7.2");
    let mut command = desktop_call(&case, &payload, "ok", &[]);
    command.env("GATE_GODOT_ROOT", &root);
    command.env("PATH", path_for_run(&[path_godot(&case)]));
    command.env_remove("GODOT_PATH");

    let outcome = command.finish();

    let install = root.join("4.7.2").join("Godot_console.exe");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("4.7.2.stable.mono.official.x\n{}\n", install.display()),
        "{outcome:?}"
    );
    ran_its_command(&outcome, &payload);
    assert!(!root.join("4.3.0").join("ran").exists(), "the other install ran");
}

/// A desktop run with an `UnrealVersion` gives its command `UE_ROOT` naming the root's engine of that version, over
/// the one the box inherited, as any run does.
#[test]
fn a_desktop_run_sets_ue_root_for_the_chosen_unreal() {
    let case = Case::new("desktop_unreal_chosen");
    let root = unreal_root(&case);
    fake_unreal(&root.join("5.7.3"), "5.7.3");
    fake_unreal(&root.join("5.8.0"), "5.8.0");
    let mut payload = ready_payload(&case, &[FAKE_TOOL, "env", "UE_ROOT"]);
    payload["UnrealVersion"] = json!("5.8.0");
    let mut command = desktop_call(&case, &payload, "ok", &[]);
    command.env("GATE_UNREAL_ROOT", &root);
    command.env("UE_ROOT", r"C:\stale\UE_5.0");

    let outcome = command.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, format!("{}\n", root.join("5.8.0").display()), "{outcome:?}");
    ran_its_command(&outcome, &payload);
}

/// A desktop run with an `EngineEnv` gives its command the variable it names naming the chosen engine, over the value
/// the box inherited, as any run does.
#[test]
fn a_desktop_run_sets_its_engine_env_names_for_the_chosen_unreal() {
    let case = Case::new("desktop_engine_env");
    let root = unreal_root(&case);
    fake_unreal(&root.join("5.8.0"), "5.8.0");
    let mut payload = ready_payload(&case, &[FAKE_TOOL, "env", ENGINE_ENV_NAME]);
    payload["UnrealVersion"] = json!("5.8.0");
    payload["EngineEnv"] = engine_env();
    let mut command = desktop_call(&case, &payload, "ok", &[]);
    command.env("GATE_UNREAL_ROOT", &root);
    command.env(ENGINE_ENV_NAME, STALE_ENGINE_ENV);

    let outcome = command.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, format!("{}\n", root.join("5.8.0").display()), "{outcome:?}");
    ran_its_command(&outcome, &payload);
}

/// A desktop run's command gets Git's `bin` at the front of its `PATH`, as any other run's does.
#[test]
fn a_desktop_run_puts_git_s_bin_first_on_its_command_s_path() {
    let case = Case::new("desktop_git_bin");
    let bin = case.folder().join("fake-git").join("bin");
    fs::create_dir_all(&bin).expect("the fake bin can be made");
    let payload = ready_payload(&case, &[FAKE_TOOL, "env", "PATH"]);
    let mut command = desktop_call(&case, &payload, "ok", &[]);
    command.env(GIT_BIN_STAND_IN, &bin);

    let outcome = command.finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    ran_its_command(&outcome, &payload);
    let printed = outcome.stdout.lines().next().expect("the command prints its PATH");
    assert_eq!(printed.split(';').next(), bin.to_str(), "Git's bin is first: {outcome:?}");
}

#[test]
fn a_desktop_run_has_no_donor_and_ignores_the_console_and_token_seams() {
    let case = Case::new("desktop_no_donor");
    let payload = ready_payload(&case, COMMAND);
    let extra = ["--test-box-console", "fail-before", "--test-box-token", "refused"];

    let outcome = desktop_call(&case, &payload, "ok", &extra).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    ran_its_command(&outcome, &payload);
    assert!(
        lines_starting(&outcome, "gate: remote could not give the command a UTF-8 console").is_empty(),
        "no console donor, so no fallback from one: {outcome:?}"
    );
    assert!(
        !outcome.stderr.contains("own token"),
        "no restricted token, so no start with the box's own: {outcome:?}"
    );
}

#[test]
fn a_refused_child_report_leaves_nothing_running() {
    let case = Case::new("desktop_lie_handle");
    let mark = case.folder().join("marked");
    let mark_word = shown(&mark);
    let record = case.folder().join("children");
    let payload = ready_payload(&case, &[FAKE_TOOL, "mark", &mark_word, "out", "ran"]);
    // The stand-in records its child and runs in no job of the box's, so only the box's own ending ends the child.
    let mut call = desktop_call(&case, &payload, "lie-handle", &[]);
    call.env("GATE_DESKTOP_TEST_RECORD", &record);

    let outcome = call.finish();

    let left = left_by(&record);
    assert!(!left.recorded.is_empty(), "the stand-in created a child: {outcome:?}");
    assert!(
        left.children.is_empty(),
        "no child of the stand-in outlives the run: {:?} {outcome:?}",
        left.children
    );
    assert!(
        !left.stand_in_running,
        "the stand-in itself does not outlive the run: {} created {} {outcome:?}",
        left.stand_in, left.stand_in_created
    );
    refused_with(
        &outcome,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper failed: the command it started is not the process it reported: {INVALID_HANDLE}"),
    );
    assert!(!mark.exists(), "the command never ran: {outcome:?}");
}

/// The system's error for a handle value the helper's table does not hold, as the box says it.
const INVALID_HANDLE: &str = "The handle is invalid. (os error 6)";

/// What a run whose stand-in helper recorded to a record left running.
struct Left {
    /// The pids of the children the stand-in recorded.
    recorded: Vec<u32>,
    /// The recorded children still running.
    children: Vec<u32>,
    /// The stand-in's own pid, as the box recorded it.
    stand_in: u32,
    /// The stand-in's creation time, as the box recorded it.
    stand_in_created: jiff::Timestamp,
    /// Whether the stand-in itself still runs: a process at its pid with its creation time.
    stand_in_running: bool,
}

/// Reads what the run recorded at `record`, the stand-in's pid and creation time and each child's pid, and finds which
/// of them still run: a child by its pid, the stand-in as its parent and a creation time no earlier than the stand-in's,
/// the stand-in by its pid and creation time, none of it read from a command line, which a snapshot may not hold. Every
/// one found is ended before this returns, so a case that fails leaves nothing behind.
fn left_by(record: &Path) -> Left {
    let lines = fs::read_to_string(record).expect("the box recorded the stand-in");
    let (stand_in, stand_in_created) = lines
        .lines()
        .find_map(|line| line.strip_prefix("stand-in "))
        .and_then(|line| line.split_once(' '))
        .expect("the box recorded the stand-in's pid and creation time");
    let stand_in: u32 = stand_in.parse().expect("the stand-in's pid");
    let stand_in_created: jiff::Timestamp = stand_in_created.parse().expect("the stand-in's creation time");
    let recorded: Vec<u32> = lines
        .lines()
        .filter(|line| !line.starts_with("stand-in "))
        .map(|line| line.parse().expect("a recorded pid"))
        .collect();
    let snapshot = Win32ProcessTable
        .try_snapshot_without_command_lines()
        .expect("the process table can be listed");
    // The parent and the creation-time bound with the pid, so another process at a reused pid is never taken for a child.
    let children: Vec<u32> = snapshot
        .iter()
        .filter(|process| recorded.contains(&process.pid))
        .filter(|process| process.parent_pid == stand_in && process.created.is_some_and(|created| created >= stand_in_created))
        .map(|process| process.pid)
        .collect();
    // The pid and the creation time together, so another case's stand-in at a reused pid is never taken for this one.
    let stand_in_left = snapshot
        .iter()
        .any(|process| process.pid == stand_in && process.created == Some(stand_in_created));
    for pid in &children {
        if let Ok(Some(process)) = StoppableProcess::open(*pid) {
            let _ = process.terminate(1);
        }
    }
    if stand_in_left
        && let Ok(Some(process)) = StoppableProcess::open(stand_in)
        && process.created().ok() == Some(stand_in_created)
    {
        let _ = process.terminate(1);
    }
    Left {
        recorded,
        children,
        stand_in,
        stand_in_created,
        stand_in_running: stand_in_left,
    }
}

#[test]
fn a_helper_that_exits_before_hello_refuses_desktop_with_its_exit_why() {
    let case = Case::new("desktop_exit_before_hello");
    let payload = ready_payload(&case, COMMAND);

    let outcome = handed_off(&case, &payload, "exit-before-hello");

    refused_with(
        &outcome,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper failed: the box closed the control pipe"),
    );
}

#[test]
fn a_helper_of_another_build_refuses_desktop_naming_both_versions() {
    let case = Case::new("desktop_wrong_version");
    let payload = ready_payload(&case, COMMAND);

    let outcome = handed_off(&case, &payload, "wrong-version");

    let ours = build_version(&case);
    refused_with(
        &outcome,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper runs gate 0.0.0+fault, the box {ours}"),
    );
}

#[test]
fn a_helper_that_dies_after_creating_the_child_leaves_no_suspended_process() {
    let case = Case::new("desktop_exit_after_create");
    let mark = case.folder().join("marked");
    let mark_word = shown(&mark);
    let record = case.folder().join("children");
    let payload = ready_payload(&case, &[FAKE_TOOL, "mark", &mark_word, "out", "ran"]);
    // The stand-in records its child and runs in no job of the box's, so only the box's sweep ends the child it left.
    let mut call = desktop_call(&case, &payload, "exit-after-create", &[]);
    call.env("GATE_DESKTOP_TEST_RECORD", &record);

    let outcome = call.finish();

    let left = left_by(&record);
    assert!(!left.recorded.is_empty(), "the stand-in created a child: {outcome:?}");
    assert!(
        left.children.is_empty(),
        "no suspended child of the stand-in outlives the run: {:?} {outcome:?}",
        left.children
    );
    assert!(
        !left.stand_in_running,
        "the stand-in itself does not outlive the run: {} created {} {outcome:?}",
        left.stand_in, left.stand_in_created
    );
    refused_with(
        &outcome,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper failed: a system call failed in the helper"),
    );
    assert!(!mark.exists(), "the command never ran: {outcome:?}");
}

/// Checks a run whose helper task the seam answers as `mode` is refused with `line`.
fn task_refuses(name: &str, mode: &str, line: &str) {
    let case = Case::new(name);
    let payload = ready_payload(&case, COMMAND);

    let outcome = handed_off(&case, &payload, mode);

    refused_with(&outcome, &payload, &format!("{NO_DESKTOP}{line}"));
}

#[test]
fn a_helper_task_that_is_not_registered_refuses_desktop() {
    task_refuses(
        "desktop_task_not_registered",
        "task-not-registered",
        "the desktop helper task is not registered (gate push registers it)",
    );
}

#[test]
fn a_helper_task_set_up_wrong_refuses_desktop_naming_what_differs() {
    task_refuses(
        "desktop_task_set_up_wrong",
        "task-set-up-wrong",
        "the desktop helper task is set up wrong: the desktop seam's definition",
    );
}

#[test]
fn a_helper_task_whose_start_failed_refuses_desktop() {
    task_refuses(
        "desktop_task_failed",
        "task-failed",
        "the desktop helper did not start: the desktop seam's start failed",
    );
}

#[test]
fn a_helper_task_that_gave_no_pid_refuses_desktop() {
    task_refuses(
        "desktop_task_no_pid",
        "task-no-pid",
        "the desktop helper did not start: the Task Scheduler gave no process id",
    );
}

#[test]
fn a_helper_that_hangs_refuses_desktop_after_the_message_limit() {
    let case = Case::new("desktop_hang");
    let payload = ready_payload(&case, COMMAND);

    let outcome = handed_off(&case, &payload, "hang");

    refused_with(
        &outcome,
        &payload,
        &format!("{NO_DESKTOP}the desktop helper did not start: it did not answer in time"),
    );
}

#[test]
fn a_desktop_run_parks_after_the_command_is_adopted_and_before_it_runs() {
    let case = Case::new("desktop_park");
    let park = case.folder().join("park");
    let parked = case.folder().join("park.parked");
    let mark = case.folder().join("marked");
    let (park_word, mark_word) = (shown(&park), shown(&mark));
    let payload = ready_payload(&case, &[FAKE_TOOL, "mark", &mark_word, "out", "ran"]);
    let running = desktop_call(&case, &payload, "ok", &["--test-box-park-launch", &park_word]).start();
    wait_until("the box parked before the command's marker", || parked.exists());

    let members = OpenedJob::open_to_list(field(&payload, "Job"))
        .expect("the job can be opened")
        .expect("the job exists while the box is parked")
        .process_ids()
        .expect("the job's processes are listed");
    let ran_before = mark.exists();
    let_go(&park);
    let outcome = running.finish();

    assert!(
        !members.is_empty(),
        "the command was adopted into the run's job before the park: {outcome:?}"
    );
    assert!(!ran_before, "the parked command had not run: {outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    ran_its_command(&outcome, &payload);
}

#[test]
fn a_desktop_command_the_helper_cannot_create_is_a_start_failure_after_the_marker() {
    let case = Case::new("desktop_not_created");
    let mut payload = desktop_payload(&case, "k", &[r".\bad.exe"], 30);
    snapshot(&case, &mut payload, &[("bad.exe", "not a program\r\n")]);

    let outcome = handed_off(&case, &payload, "ok");

    assert_eq!(outcome.code, Some(127), "a command that could not be created: {outcome:?}");
    assert!(lines_starting(&outcome, "gate-remote-refused:").is_empty(), "no refusal: {outcome:?}");
    let marker = started_line(&payload);
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let at_marker = lines.iter().position(|line| *line == marker);
    let at_failure = lines.iter().position(|line| line.starts_with("gate: could not start"));
    assert!(
        at_marker.zip(at_failure).is_some_and(|(marker, failure)| marker < failure),
        "the marker, then the start failure: {outcome:?}"
    );
}

#[test]
fn a_desktop_command_reached_through_a_junction_runs() {
    let case = Case::new("desktop_junction");
    let real = case.folder().join("real");
    fs::create_dir_all(&real).expect("the program's folder is made");
    copy_tool(&case, r"real\fake-tool.exe");
    let current = case.folder().join("current");
    junction(&current, &real);
    let program = shown(&current.join("fake-tool.exe"));
    let payload = ready_payload(&case, &[&program, "out", "ran"]);

    let outcome = handed_off(&case, &payload, "ok");

    assert_eq!(
        outcome.code,
        Some(0),
        "the program the junction reaches is the one the box sent: {outcome:?}"
    );
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    ran_its_command(&outcome, &payload);
}

#[test]
fn a_desktop_wait_says_the_busy_line_once_and_an_abandoned_lock_is_said() {
    let case = Case::new("desktop_wait_lines");
    let mut payload = desktop_payload(&case, "k", COMMAND, 1);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let held = HeldDesktop::hold(&case);
    let busy = run_desktop(&case, &payload, "ok", "active", DESKTOP_MS);
    drop(held);
    refused_with(&busy, &payload, BUSY_FOR_1_S);
    assert_eq!(lines_starting(&busy, BUSY_WAIT), [BUSY_WAIT], "the busy line, once: {busy:?}");

    HeldDesktop::abandon(&case);
    let abandoned = handed_off(&case, &payload, "ok");
    assert_eq!(
        lines_starting(&abandoned, ABANDONED),
        [ABANDONED],
        "the abandoned lock, said once: {abandoned:?}"
    );
    assert_eq!(abandoned.code, Some(0), "the run took the lock and ran: {abandoned:?}");
    ran_its_command(&abandoned, &payload);
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

    let running = desktop_box_call(&case, &args, &request_line(&payload)).start();
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
fn a_second_desktop_run_waits_for_the_first_and_then_runs() {
    let case = Case::new("desktop_waits_then_runs");
    let payload = ready_payload(&case, COMMAND);
    let held = HeldDesktop::hold(&case);

    let running = desktop_call(&case, &payload, "ok", &[]).start();
    running.wait_for_stderr(BUSY_WAIT);
    drop(held);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(0), "the run took the desktop once it was free: {outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(lines_starting(&outcome, BUSY_WAIT), [BUSY_WAIT], "the busy line, once: {outcome:?}");
    let marker = started_line(&payload);
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let at = |wanted: &str| lines.iter().position(|line| *line == wanted);
    assert!(
        at(BUSY_WAIT).zip(at(&marker)).is_some_and(|(busy, marker)| busy < marker),
        "the marker comes after the busy line: {outcome:?}"
    );
}

#[test]
fn a_desktop_wait_is_capped_at_the_runs_timeout() {
    let case = Case::new("desktop_wait_capped");
    let mut payload = desktop_payload(&case, "k", COMMAND, 600);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    // The run's ceiling, which the box reads as `Timeout`, caps its wait for the desktop below the wait it asked for.
    payload["Timeout"] = json!(1);
    let held = HeldDesktop::hold(&case);

    let outcome = run_desktop(&case, &payload, "ok", "active", DESKTOP_MS);
    drop(held);

    refused_with(&outcome, &payload, BUSY_FOR_1_S);
    assert_eq!(lines_starting(&outcome, BUSY_WAIT), [BUSY_WAIT], "the busy line, once: {outcome:?}");
}

#[test]
fn box_stop_during_the_desktop_wait_is_stopped_before_the_command_started() {
    let case = Case::new("desktop_stop_in_wait_one_repo");
    let mut payload = desktop_payload(&case, "k", COMMAND, 600);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let held = HeldDesktop::hold(&case);
    let running = desktop_call(&case, &payload, "ok", &[]).start();
    running.wait_for_stderr(BUSY_WAIT);

    let stopped = gate_box(&case, "stop", &request_line(&payload), None);
    let outcome = running.finish();
    drop(held);

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop during the wait fails the run: {outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    assert!(said.contains(&stopped_line_for(field(&payload, "Job")).as_str()), "{outcome:?}");
    assert!(
        !said.iter().any(|line| line.starts_with("gate: remote back commits:")),
        "a single repository has no back commits line: {outcome:?}"
    );
    assert!(lines_starting(&outcome, "gate-remote-refused:").is_empty(), "no desktop tag: {outcome:?}");
    assert!(
        lines_starting(&outcome, &started_line(&payload)).is_empty(),
        "no start marker: {outcome:?}"
    );
}

/// A client gone while the run waits for the builder's desktop ends the wait: the heartbeat, which writes a line every
/// period while no command is watched, finds the closed stderr, and the box ends with 255 before it starts anything.
#[test]
fn a_closed_stderr_pipe_during_the_desktop_wait_ends_it_with_255_and_starts_nothing() {
    let case = Case::new("desktop_gone_in_wait");
    let payload = ready_payload(&case, COMMAND);
    let held = HeldDesktop::hold(&case);
    let seams = desktop_seams(&case, "ok", "active", HAND_OFF_MS);
    let mut args = vec!["box", "--test-heartbeat-ms", "200"];
    args.extend(seams.iter().map(String::as_str));
    args.push("run");
    let mut call = bare_box_call(&case, &args, &request_line(&payload));
    call.env(INPUT_DESKTOP_VARIABLE, DEFAULT_DESKTOP);
    let (running, stderr) = call.start_with_closable_stderr();
    running.wait_for_stderr(BUSY_WAIT);

    stderr.close();
    let outcome = ended_within(running, QUIET_CLIENT_GONE_GUARD);
    drop(held);

    assert_eq!(outcome.code, Some(255), "a closed connection exits 255: {outcome:?}");
    assert_eq!(outcome.stdout, "", "stdout is empty, so the command never ran: {outcome:?}");
    assert!(
        lines_starting(&outcome, &started_line(&payload)).is_empty(),
        "no start marker before stderr closed: {outcome:?}"
    );
    let stopped = gate_box(&case, "stop", &request_line(&payload), None);
    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(stopped.stdout, "", "no job was left to stop: {stopped:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_stop_during_a_desktop_run_ends_it() {
    let case = Case::new("desktop_stop_in_run");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    // `pid` first, so the case can name the command's process; then it waits for a go this case never writes.
    let payload = ready_payload(&case, &[FAKE_TOOL, "pid", "wait", &go_word]);
    let running = desktop_call(&case, &payload, "ok", &[]).start();
    let printed = running.wait_for_stdout("pid ");
    let pid: u32 = printed
        .lines()
        .find_map(|line| line.strip_prefix("pid "))
        .and_then(|pid| pid.trim().parse().ok())
        .expect("the command printed its pid");
    // The pid and the creation time together, so another case's process at a reused pid is never taken for this one.
    let created = Win32ProcessTable
        .snapshot()
        .into_iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.created);
    assert!(created.is_some(), "the command's creation time can be read: {pid}");

    let stopped = gate_box(&case, "stop", &request_line(&payload), None);

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(
        stopped.stdout,
        format!("gate: remote stopped job {}\n", field(&payload, "Job")),
        "{stopped:?}"
    );
    assert_eq!(stopped.stderr, "", "{stopped:?}");
    wait_until("the stopped desktop command ending", || {
        !Win32ProcessTable
            .snapshot()
            .iter()
            .any(|process| process.pid == pid && process.created == created)
    });
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(124), "the stopped command's status: {outcome:?}");
    ran_its_command(&outcome, &payload);
}

#[test]
fn a_stop_while_the_desktop_command_is_parked_says_it_was_stopped_before_start() {
    let case = Case::new("desktop_stop_parked");
    let park = case.folder().join("park");
    let parked = case.folder().join("park.parked");
    let mark = case.folder().join("marked");
    let (park_word, mark_word) = (shown(&park), shown(&mark));
    let payload = ready_payload(&case, &[FAKE_TOOL, "mark", &mark_word, "out", "ran"]);
    let running = desktop_call(&case, &payload, "ok", &["--test-box-park-launch", &park_word]).start();
    running.wait_until("the box parked before the command's marker", || parked.exists());

    let stopped = gate_box(&case, "stop", &request_line(&payload), None);
    let_go(&park);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop while the command is parked fails the run: {outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    assert!(said.contains(&stopped_line_for(field(&payload, "Job")).as_str()), "{outcome:?}");
    let marker = started_line(&payload);
    assert_eq!(
        lines_starting(&outcome, &marker),
        [marker.as_str()],
        "the marker was said before the stop look: {outcome:?}"
    );
    assert!(!mark.exists(), "the command never ran: {outcome:?}");
}

#[test]
fn a_desktop_command_that_cannot_start_never_waits_for_the_desktop() {
    let case = Case::new("desktop_cannot_start");
    let _held = HeldDesktop::hold(&case);

    let mut missing = desktop_payload(&case, "k", &["gate-no-such-program-anywhere"], 1);
    snapshot(&case, &mut missing, &[("write.cmd", "@echo x>new.txt\r\n")]);
    let not_found = run_desktop(&case, &missing, "ok", "active", DESKTOP_MS);
    assert_eq!(not_found.code, Some(127), "{not_found:?}");
    assert_eq!(
        lines_starting(&not_found, "gate: could not start gate-no-such-program-anywhere").len(),
        1,
        "{not_found:?}"
    );

    let mut batch = desktop_payload(&case, "k", &[r".\write.cmd", "%PATH%"], 1);
    batch["Snapshot"] = missing["Snapshot"].clone();
    let refused = run_desktop(&case, &batch, "ok", "active", DESKTOP_MS);
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

#[test]
fn a_killed_box_ends_its_desktop_command_and_the_next_run_takes_the_abandoned_lock() {
    let case = Case::new("desktop_killed_box");
    let record = case.folder().join("children");
    // A sleep longer than the suite's hang guard, so nothing but the run's job can end the command inside the case.
    let payload = ready_payload(&case, &[FAKE_TOOL, "pid", "sleep", "620000"]);
    // Open across both runs, so the lock the killed box held is still there for the second run to find abandoned.
    let _kept = KeptDesktop::open(&case);
    // The record seam keeps the stand-in helper in no job of the box's, so the command is only ever in the run's own
    // job, and only that job's kill-on-close can end it once the box is gone.
    let mut call = desktop_call(&case, &payload, "ok", &[]);
    call.env("GATE_DESKTOP_TEST_RECORD", &record);
    let running = call.start();
    let pid = printed_pid(&running.wait_for_stdout("pid "));
    let created = started_at(pid);
    assert!(created.is_some(), "the command's creation time can be read: {pid}");

    // As a lost connection's cut kills the box while it holds the builder's desktop and the run's job.
    let killed = running.kill();

    // The run's job went with the box's last handle on it, so the next run can take the same name.
    wait_until("the killed box's desktop job ending", || {
        OpenedJob::open_to_list(field(&payload, "Job")).expect("the job can be opened").is_none()
    });
    wait_until_all_ended(&[(pid, created)], "the killed box's desktop command ending");
    let left = left_by(&record);
    assert!(!left.recorded.is_empty(), "the stand-in started the command: {killed:?}");
    assert!(
        left.children.is_empty(),
        "no child of the stand-in outlives the box: {:?} {killed:?}",
        left.children
    );
    assert!(
        !left.stand_in_running,
        "the stand-in itself does not outlive the box: {} created {} {killed:?}",
        left.stand_in, left.stand_in_created
    );

    let mut next = payload.clone();
    next["Command"] = json!(COMMAND);
    let outcome = handed_off(&case, &next, "ok");

    assert_eq!(outcome.code, Some(0), "the next run took the lock and ran: {killed:?} {outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    assert_eq!(
        lines_starting(&outcome, ABANDONED),
        [ABANDONED],
        "the abandoned lock, said once: {outcome:?}"
    );
    assert!(
        lines_starting(&outcome, BUSY_WAIT).is_empty(),
        "the next run found the lock abandoned, not held: {outcome:?}"
    );
}

#[test]
fn the_ceiling_kills_a_desktop_command() {
    let case = Case::new("desktop_ceiling_kill");
    let mut payload = desktop_payload(&case, "k", &[FAKE_TOOL, "sleep", "10000"], 30);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    payload["Timeout"] = json!(1);
    payload["Host"] = json!("builder");
    let outcome = desktop_call(&case, &payload, "ok", &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]).finish();

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let kill = Regex::new(
        r"^gate: TIMED OUT on builder after 1 s of load-adjusted time \(wall \d+ s, machine free 100% on average\); killed .+ and its children$",
    )
    .expect("the pattern compiles");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let at = said.iter().position(|line| kill.is_match(line));
    assert!(at.is_some(), "the kill line: {outcome:?}");
    let at = at.expect("it is there");
    assert_eq!(
        said.get(at + 1).copied(),
        Some(format!("gate-remote-killed: {} timed-out", field(&payload, "Nonce")).as_str()),
        "the kill tag right after the kill line: {outcome:?}"
    );
    assert!(adjusted_seconds(&outcome, &payload) >= 1.0, "the clock reached the ceiling: {outcome:?}");
}

#[test]
fn a_closed_stdout_pipe_stops_a_desktop_job_and_exits_255() {
    let case = Case::new("desktop_closed_stdout");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let payload = ready_payload(&case, &[FAKE_TOOL, "err", "started", "wait", &go_word, "out", "x", "sleep", "10000"]);
    let running = desktop_call(&case, &payload, "ok", &[]).start_with_closed(Closed::Stdout);
    running.wait_for_stderr("started");
    let_go(&go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(255), "a closed connection exits 255: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote the client closed the connection; stopped job "),
        "{outcome:?}"
    );
}

#[test]
fn a_closed_stderr_pipe_stops_a_desktop_job_and_exits_255() {
    let case = Case::new("desktop_closed_stderr");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let payload = ready_payload(&case, &[FAKE_TOOL, "out", "started", "wait", &go_word, "err", "x", "sleep", "10000"]);
    let running = desktop_call(&case, &payload, "ok", &[]).start_with_closed(Closed::Stderr);
    running.wait_for_stdout("started");
    let_go(&go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(255), "a closed connection exits 255: {outcome:?}");
}

/// The pid of the command that printed its `pid <n>` line to `stdout`.
fn printed_pid(stdout: &str) -> u32 {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("pid "))
        .and_then(|pid| pid.trim().parse().ok())
        .expect("the command printed its pid")
}

/// The creation time of the process `pid`, read now, so a process at a reused pid is never taken for it.
fn started_at(pid: u32) -> Option<jiff::Timestamp> {
    Win32ProcessTable
        .try_snapshot_without_command_lines()
        .expect("the process table can be listed")
        .into_iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.created)
}

/// Waits until no process of `ended`, each a pid and the creation time read while it ran, still runs: another process
/// at a reused pid is never taken for one of them.
fn wait_until_all_ended(ended: &[(u32, Option<jiff::Timestamp>)], what: &str) {
    wait_until(what, || {
        let snapshot = Win32ProcessTable
            .try_snapshot_without_command_lines()
            .expect("the process table can be listed");
        ended
            .iter()
            .all(|(pid, created)| !snapshot.iter().any(|process| process.pid == *pid && process.created == *created))
    });
}

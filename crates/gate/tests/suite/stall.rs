//! The watch: a command with no output and no CPU for the stall window is killed with the STALLED line, its job's
//! processes terminated and the run failed with status 124, and a run that passes journals its load-adjusted seconds.

use std::fs;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate_win32::StoppableProcess;
use regex::Regex;
use serde_json::Value;

use crate::support::{Case, FAKE_TOOL, assert_in_order, release, run_words, said_pid, shown, wait_until, wait_until_closed};

#[test]
fn a_silent_idle_command_is_killed_as_stalled() {
    let case = Case::new("a_silent_idle_command_is_killed_as_stalled");
    let seams = ["--test-stall-ms", "1500", "--test-sample-ms", "100"];

    // `sleep` blocks without a tick of CPU, where `wait` would poll.
    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "out", "hi", "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let stalled = Regex::new(&format!(
        concat!(
            r"(?m)^gate: STALLED: no output and no CPU for \d+ s \(machine free \d+% on average\); ",
            r"killed {} pid out hi sleep 30000 and its children$"
        ),
        regex::escape(FAKE_TOOL)
    ))
    .expect("the pattern is valid");
    let found = stalled.find(&outcome.stderr);
    assert!(found.is_some(), "no STALLED line on stderr: {outcome:?}");
    let line = found.expect("checked above").as_str();
    let after = ["gate: terminated the job's 1 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&[line][..], &after[..]].concat());
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_in_order(&log, &[&["hi\n", line][..], &after[..]].concat());

    let release = release(&case);
    assert_eq!(
        (&release["verdict"], &release["status"]),
        (&Value::from("stalled"), &Value::from(124)),
        "{release}"
    );
    let pid = said_pid(&log, "pid ");
    // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
    wait_until(&format!("the killed command {pid} ending"), || {
        gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
    });
}

#[test]
fn a_stalled_command_is_killed_with_its_grandchild() {
    let case = Case::new("a_stalled_command_is_killed_with_its_grandchild");
    // Never created: the grandchild waits on it until the kill ends it.
    let go = shown(&case.folder().join("go"));
    // The command's own pid, its `pid` line, the grandchild's pid and its `spawned` line each restart the stall
    // window, so the stall can only come once both processes are in the job and the command has gone quiet.
    let seams = ["--test-stall-ms", "3000", "--test-sample-ms", "100"];

    let outcome = case.run(&run_words("out.log", &seams, &[FAKE_TOOL, "pid", "spawn-wait", &go, "sleep", "30000"]));

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let after = ["gate: terminated the job's 2 process(es)", "gate: FAILED (status 124)"];
    assert_in_order(&outcome.stderr, &[&["gate: STALLED: "][..], &after[..]].concat());
    assert_eq!(release(&case)["verdict"], "stalled");
    let log_path = case.folder().join("out.log");
    wait_until_closed(&log_path);
    let log = fs::read_to_string(&log_path).expect("the log was written");
    for prefix in ["pid ", "spawned "] {
        let pid = said_pid(&log, prefix);
        // A terminated process leaves its job before its object is signalled, so its end is waited for, not read once.
        wait_until(&format!("the killed process {pid} ({prefix:?}) ending"), || {
            gate_win32::process_start_of(pid).is_ok_and(|start| start.is_none())
        });
    }
}

/// What a case that starts a packaged program ends on every path out of it, a panic before the program's pid is read
/// included: the files the processes wait on, `opened`, `go` and `release`, are written, and the program, once opened,
/// is terminated, so no process of the case waits out fake-tool's own limit.
struct Packaged {
    process: Option<StoppableProcess>,
    files: [PathBuf; 3],
}

impl Drop for Packaged {
    fn drop(&mut self) {
        // Each fails only when there is nothing left to end: the program has ended, or the case's folder is gone.
        for file in &self.files {
            let _ = fs::write(file, "");
        }
        if let Some(process) = &self.process {
            let _ = process.terminate(1);
        }
    }
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`, the attribute an app execution alias has.
const REPARSE_POINT: u32 = 0x400;

/// Whether the first `pwsh.exe` on `PATH` is the Store's app execution alias: a reparse point in
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`, which starts the packaged pwsh with its package identity. A `pwsh.exe` run
/// straight from the Store's install folder has no package identity, so it does not count. The alias does not open as
/// a file, so its own entry is read.
fn pwsh_is_packaged() -> bool {
    let (Some(path), Some(local)) = (std::env::var_os("PATH"), std::env::var_os("LOCALAPPDATA")) else {
        return false;
    };
    let aliases = PathBuf::from(local).join("Microsoft").join("WindowsApps");
    let first = std::env::split_paths(&path).find_map(|folder| {
        let candidate = folder.join("pwsh.exe");
        fs::symlink_metadata(&candidate).ok().map(|entry| (folder, entry))
    });
    first.is_some_and(|(folder, entry)| same_folder(&folder, &aliases) && entry.file_attributes() & REPARSE_POINT != 0)
}

/// Whether two folders are the same, as Windows compares paths: ignoring case and a trailing separator.
fn same_folder(left: &Path, right: &Path) -> bool {
    let text = |path: &Path| path.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    text(left) == text(right)
}

#[test]
#[expect(clippy::print_stderr, reason = "a case this machine cannot test says so in its output")]
fn a_stall_kill_ends_a_packaged_app_the_command_started() {
    if !pwsh_is_packaged() {
        eprintln!("skipped: pwsh on PATH is not a packaged app");
        return;
    }
    let case = Case::new("a_stall_kill_ends_a_packaged_app_the_command_started");
    // None of these exists until the case writes it: the packaged program chatters into the log until `opened`, so the
    // stall cannot come before the case holds a handle to it, then waits on `go`; the command holds it on `release`.
    let opened = case.folder().join("opened");
    let go = case.folder().join("go");
    let release_path = case.folder().join("release");
    let release_file = shown(&release_path);
    let pid_file = case.folder().join("packaged.pid");
    let mut packaged = Packaged {
        process: None,
        files: [opened.clone(), go.clone(), release_path],
    };
    let program = format!("& '{FAKE_TOOL}' chatter '{}' wait '{}'", shown(&opened), shown(&go));
    // `pwsh` on PATH is a packaged app. fake-tool, in the job and with no package identity, starts it, and Windows
    // creates it in a job of its own, outside the command's.
    let seams = ["--test-stall-ms", "3000", "--test-sample-ms", "100"];
    let command = [
        FAKE_TOOL,
        "spawn-pid",
        &shown(&pid_file),
        &release_file,
        "pwsh",
        "-NoProfile",
        "-Command",
        &program,
    ];

    let running = case.start(&run_words("out.log", &seams, &command));
    let mut pid = None;
    wait_until("the packaged program's pid", || {
        pid = fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<u32>().ok());
        pid.is_some()
    });
    let pid = pid.expect("checked above");
    let process = packaged.process.insert(
        StoppableProcess::open(pid)
            .expect("the program is opened")
            .expect("it runs: it chatters until `opened`"),
    );
    fs::write(&opened, "").expect("`opened` is written");
    let outcome = running.finish();
    // A program the kill reached has ended with the gate's 124 by now; one it missed ends on its own once released,
    // with fake-tool's status 0, rather than when fake-tool's `wait` gives up.
    fs::write(&go, "").expect("`go` is written");

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(release(&case)["verdict"], "stalled");
    let killed = Regex::new(&format!(
        r"(?m)^gate: terminated the job's 1 process\(es\) and 2 that left it \(pwsh\.exe {pid}, fake-tool\.exe \d+\)$"
    ))
    .expect("the pattern is valid");
    assert!(killed.is_match(&outcome.stderr), "no kill line naming pwsh.exe {pid} first: {outcome:?}");
    wait_until(&format!("the packaged program {pid} ending"), || {
        matches!(process.wait(Duration::ZERO), Ok(true))
    });
    let code = process.exit_code().expect("its status is read");
    assert_eq!(code, 124, "the kill did not reach the packaged program {pid}");
}

#[test]
fn a_passed_run_journals_its_load_adjusted_seconds() {
    let case = Case::new("a_passed_run_journals_its_load_adjusted_seconds");

    let outcome = case.run(&run_words("out.log", &[], &[FAKE_TOOL, "out", "hi"]));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let release = release(&case);
    assert!(release["adjustedSeconds"].is_number(), "{release}");
    assert_eq!(release["verdict"], "passed", "{release}");
}

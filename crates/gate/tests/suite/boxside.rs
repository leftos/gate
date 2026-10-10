//! The hidden `gate box`, run as the builder runs it over ssh: no `--state-dir`, every path in the base64 JSON payload,
//! all of them under the case's folder. git must be on PATH; a case about dotnet sets the gate's PATH itself.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_win32::{EnvironmentBlock, Job, Launch};
use jiff::{SignedDuration, Timestamp};
use regex::Regex;
use serde_json::{Value, json};

use crate::support::{
    Case, Closed, FAKE_TOOL, GIT_REDIRECTS, Outcome, Running, allow_breakaway, box_payload, finish, shown, start, start_with_closed, wait_until,
    wait_until_closed,
};

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs <base64 JSON payload>\n";

/// The payload word for `json`, as the remote client encodes it.
fn payload(json: &Value) -> String {
    STANDARD.encode(json.to_string())
}

/// The heartbeat period every box call of the suite passes unless it says otherwise: long enough that no beat lands in
/// a case's output however slow a loaded machine makes the run.
const QUIET_HEARTBEAT_MS: &str = "2147483647";

/// `gate box` with `args` after `box`, its heartbeat period [`QUIET_HEARTBEAT_MS`], without the git variables the
/// suite may have inherited.
fn box_command(case: &Case, args: &[&str]) -> Command {
    let mut words = vec!["box", "--test-heartbeat-ms", QUIET_HEARTBEAT_MS];
    words.extend_from_slice(args.strip_prefix(&["box"][..]).unwrap_or(args));
    bare_box_command(case, &words)
}

/// The gate with `args` as they are, no seam added, without the git variables the suite may have inherited.
fn bare_box_command(case: &Case, args: &[&str]) -> Command {
    let mut command = case.gate_without_state_dir(args);
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    command
}

/// `gate box <verb> <payload>` with `PATH` set to `path` when given.
fn gate_box(case: &Case, verb: &str, payload: &str, path: Option<&OsString>) -> Outcome {
    let mut command = box_command(case, &["box", verb, payload]);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    finish(command)
}

/// A preflight's two folders under the case folder, the bare repository's parent not yet made.
struct Folders {
    bare: String,
    work: String,
}

impl Folders {
    fn new(case: &Case) -> Folders {
        let remote = case.folder().join("remote");
        let text = |path: PathBuf| path.to_str().expect("the case folder is Unicode").to_owned();
        Folders {
            bare: text(remote.join("k.git")),
            work: text(remote.join("k")),
        }
    }

    fn payload(&self) -> String {
        payload(&json!({"Key": "k", "Bare": self.bare, "Work": self.work, "Lock": format!("{}.lock", self.work), "Job": "unused"}))
    }

    fn alternates_path(&self) -> PathBuf {
        Path::new(&self.work).join(".git").join("objects").join("info").join("alternates")
    }

    fn alternates(&self) -> Vec<u8> {
        fs::read(self.alternates_path()).expect("the alternates file can be read")
    }

    /// What preflight writes as the alternates file.
    fn written_alternates(&self) -> Vec<u8> {
        format!("{}/objects\n", self.bare.replace('\\', "/")).into_bytes()
    }
}

/// `git` with `args`, which must succeed; its stdout less the line end.
fn git(args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    let output = command.args(args).output().expect("git starts; the box cases need git on PATH");
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim_end().to_owned()
}

/// The folder on the suite's PATH that holds git.exe.
fn git_folder() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .find(|folder| folder.join("git.exe").is_file())
        .expect("git.exe is on PATH; the box cases need it")
}

/// A PATH of `folders`, then git's folder.
fn path_with_git(folders: &[PathBuf]) -> OsString {
    std::env::join_paths(folders.iter().cloned().chain([git_folder()])).expect("the folders join into a PATH")
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// Checks a passed preflight said no `gate: ` line on stderr (git's own hints may be there) and returns its stdout lines.
fn passed(outcome: &Outcome) -> Vec<&str> {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stderr.lines().any(|line| line.starts_with("gate: ")), "{outcome:?}");
    lines(&outcome.stdout)
}

#[test]
fn boot_prints_one_round_trip_utc_line() {
    let case = Case::new("boot_prints_one_round_trip_utc_line");
    let shape = Regex::new(r"^gate: remote boot (\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{7}Z)$").expect("the pattern compiles");
    let mut times = Vec::new();
    for _ in 0..2 {
        let outcome = gate_box(&case, "boot", &payload(&json!({})), None);

        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        assert_eq!(outcome.stderr, "");
        let printed = lines(&outcome.stdout);
        assert_eq!(printed.len(), 1, "one line: {outcome:?}");
        let captures = shape
            .captures(printed[0])
            .expect("the boot line is round-trip UTC with seven fractional digits");
        times.push(captures[1].parse::<Timestamp>().expect("the boot time parses"));
    }

    assert!(
        times[1].duration_since(times[0]).abs() <= SignedDuration::from_secs(30),
        "two boot times of one boot are within the client's slack: {times:?}"
    );
}

#[test]
fn preflight_creates_the_bare_repo_and_work_folder() {
    let case = Case::new("preflight_creates_the_bare_repo_and_work_folder");
    let folders = Folders::new(&case);

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    let printed = passed(&outcome);
    assert_eq!(printed.len(), 4, "{outcome:?}");
    assert_eq!(printed[0], format!("gate: remote created {}", folders.bare));
    assert_eq!(printed[1], format!("gate: remote version {}", env!("CARGO_PKG_VERSION")));
    assert!(printed[2].starts_with("gate: remote boot "), "{outcome:?}");
    assert!(printed[3].starts_with("gate: remote sdks "), "{outcome:?}");
    assert!(Path::new(&folders.work).join(".git").is_dir());
    assert_eq!(git(&["-C", &folders.work, "config", "--get", "remote.origin.url"]), folders.bare);
    assert_eq!(git(&["-C", &folders.work, "config", "--local", "--get", "core.autocrlf"]), "false");
    assert_eq!(git(&["-C", &folders.work, "config", "--local", "--get", "core.eol"]), "lf");
    assert_eq!(folders.alternates(), folders.written_alternates());
    assert!(!case.local_app_data().exists(), "the box reads and writes nothing under LOCALAPPDATA");
    assert_eq!(case.folder_entries(), ["remote"], "the box writes only where the payload says");
}

#[test]
fn preflight_keeps_alternates_with_other_content_and_rewrites_an_empty_one() {
    let case = Case::new("preflight_keeps_alternates_with_other_content");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));

    fs::write(folders.alternates_path(), "C:/other/objects\n").expect("the alternates file can be written");
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    assert_eq!(folders.alternates(), b"C:/other/objects\n", "other content is kept");

    fs::write(folders.alternates_path(), "").expect("the alternates file can be emptied");
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    assert_eq!(folders.alternates(), folders.written_alternates(), "an empty file is written again");
}

#[test]
fn preflight_a_failing_git_step_exits_with_its_status() {
    let case = Case::new("preflight_a_failing_git_step");
    let folders = Folders::new(&case);
    fs::create_dir_all(&folders.work).expect("the work folder can be made");
    fs::write(Path::new(&folders.work).join(".git"), "junk").expect("a junk .git file can be written");
    let shape = Regex::new(r"(?m)^gate: remote git [a-z .-]+ failed \(status (-?\d+)\)$").expect("the pattern compiles");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    let captures = shape.captures(&outcome.stderr).expect("a git step's failure line");
    let status: i32 = captures[1].parse().expect("the status is a number");
    assert_ne!(status, 0, "{outcome:?}");
    assert_eq!(outcome.code, Some(status), "the gate exits with the status it printed: {outcome:?}");
    assert!(
        !outcome.stdout.contains("gate: remote version"),
        "no trailing lines after a failure: {outcome:?}"
    );
}

#[test]
fn preflight_without_git_says_the_step_could_not_start() {
    let case = Case::new("preflight_without_git");
    let folders = Folders::new(&case);
    let empty = case.folder().join("empty");
    fs::create_dir_all(&empty).expect("the empty folder can be made");

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&empty.into_os_string()));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    let line = outcome.stderr.trim_end();
    let message = line
        .strip_prefix("gate: remote git init --bare failed: ")
        .expect("the start failure names the step");
    assert!(!message.is_empty() && !message.contains("(status"), "{outcome:?}");
}

#[test]
fn preflight_cannot_create_the_folder_above_work() {
    let case = Case::new("preflight_cannot_create_the_folder_above_work");
    let folders = Folders::new(&case);
    let remote = case.folder().join("remote");
    fs::write(&remote, "a file where the folder goes").expect("the blocking file can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome
            .stderr
            .starts_with(&format!("gate: remote could not create the folder {}: ", remote.display())),
        "{outcome:?}"
    );
}

#[test]
fn preflight_says_why_a_failing_dotnet_gives_no_sdks_line() {
    let case = Case::new("preflight_failing_dotnet");
    let folders = Folders::new(&case);
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(stub.join("dotnet.cmd"), "@exit /b 3\r\n").expect("the dotnet stub can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stdout.contains("gate: remote sdks"), "{outcome:?}");
    assert!(
        lines(&outcome.stdout).last().is_some_and(|line| line.starts_with("gate: remote boot ")),
        "{outcome:?}"
    );
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == "gate: remote could not read the SDK list: dotnet --list-sdks exited with status 3"),
        "{outcome:?}"
    );
}

#[test]
fn preflight_refuses_a_path_not_absolute_on_a_drive() {
    let case = Case::new("preflight_refuses_a_path_not_absolute_on_a_drive");
    let folders = Folders::new(&case);
    let newline = format!("{}\nx", folders.bare);
    let refused = [
        ("Bare", "-bare", folders.work.as_str()),
        ("Bare", newline.as_str(), folders.work.as_str()),
        ("Work", folders.bare.as_str(), r"remote\k"),
    ];
    for (key, bare, work) in refused {
        let value = if key == "Bare" { bare } else { work };

        let outcome = gate_box(&case, "preflight", &payload(&json!({"Bare": bare, "Work": work})), None);

        assert_eq!(outcome.code, Some(2), "{outcome:?}");
        assert_eq!(outcome.stdout, "", "{key} {value:?}");
        assert_eq!(
            outcome.stderr,
            format!("gate: box payload {key} is not an absolute path on a drive: {value:?}\n")
        );
        assert_eq!(case.folder_entries(), Vec::<String>::new(), "nothing is made for {key} {value:?}");
    }
}

#[test]
fn preflight_twice_creates_nothing_the_second_time() {
    let case = Case::new("preflight_twice_creates_nothing_the_second_time");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    let alternates = folders.alternates();

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    let printed = passed(&outcome);
    assert!(!printed.iter().any(|line| line.starts_with("gate: remote created")), "{outcome:?}");
    assert_eq!(printed[0], format!("gate: remote version {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(folders.alternates(), alternates);
}

#[test]
fn preflight_repairs_a_missing_origin() {
    let case = Case::new("preflight_repairs_a_missing_origin");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    git(&["-C", &folders.work, "remote", "remove", "origin"]);

    passed(&gate_box(&case, "preflight", &folders.payload(), None));

    assert_eq!(git(&["-C", &folders.work, "config", "--get", "remote.origin.url"]), folders.bare);
}

#[test]
fn preflight_sdks_line_from_dotnet() {
    let case = Case::new("preflight_sdks_line_from_dotnet");
    let folders = Folders::new(&case);
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(
        stub.join("dotnet.cmd"),
        "@echo off\r\necho 8.0.404 [C:\\Program Files\\dotnet\\sdk]\r\necho 9.0.100 [C:\\Program Files\\dotnet\\sdk]\r\n",
    )
    .expect("the dotnet stub can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[stub])));

    assert_eq!(passed(&outcome).last(), Some(&"gate: remote sdks 8.0.404,9.0.100"), "{outcome:?}");
}

#[test]
fn preflight_sdks_none_without_dotnet() {
    let case = Case::new("preflight_sdks_none_without_dotnet");
    let folders = Folders::new(&case);

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[])));

    assert_eq!(passed(&outcome).last(), Some(&"gate: remote sdks none"), "{outcome:?}");
}

#[test]
fn preflight_version_line_is_the_package_version() {
    let case = Case::new("preflight_version_line_is_the_package_version");
    let folders = Folders::new(&case);
    let version = case.run(&["--version"]);
    let number = version.stdout.trim_end().strip_prefix("gate ").expect("--version prints gate <number>");

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert!(
        passed(&outcome).contains(&format!("gate: remote version {number}").as_str()),
        "{outcome:?}"
    );
}

#[test]
fn box_usage_and_payload_errors_exit_2() {
    let case = Case::new("box_usage_and_payload_errors_exit_2");
    let folders = Folders::new(&case);
    for args in [&["box"][..], &["box", "launch", "e30="]] {
        let outcome = finish(bare_box_command(&case, args));

        assert_eq!(outcome.code, Some(2), "{args:?}");
        assert_eq!(outcome.stdout, "", "{args:?}");
        assert_eq!(outcome.stderr, BOX_USAGE, "{args:?}: the box usage alone");
    }

    let not_base64 = finish(bare_box_command(&case, &["box", "preflight", "not-base64!"]));
    assert_eq!(not_base64.code, Some(2));
    assert_eq!(not_base64.stdout, "");
    assert!(not_base64.stderr.starts_with("gate: box payload is not base64 JSON: "), "{not_base64:?}");
    assert_eq!(lines(&not_base64.stderr).len(), 1, "{not_base64:?}");

    let not_an_object = finish(bare_box_command(&case, &["box", "preflight", &payload(&json!(["Bare", "Work"]))]));
    assert_eq!(not_an_object.code, Some(2));
    assert_eq!(not_an_object.stdout, "");
    assert_eq!(not_an_object.stderr, "gate: box payload is not base64 JSON: the JSON is not an object\n");

    let lacking = finish(bare_box_command(&case, &["box", "preflight", &payload(&json!({"Bare": folders.bare}))]));
    assert_eq!(lacking.code, Some(2));
    assert_eq!(lacking.stdout, "");
    assert_eq!(lacking.stderr, "gate: box payload lacks Work\n");
    assert!(!Path::new(&folders.bare).exists(), "nothing is made for a payload the box refuses");

    assert!(!case.run(&["--help"]).stdout.contains("box"), "gate box is hidden from the usage");
}

#[test]
fn box_outputs_is_not_built_yet() {
    let case = Case::new("box_outputs_is_not_built_yet");

    let outcome = gate_box(&case, "outputs", &payload(&json!({"Key": "k"})), None);

    assert_eq!(outcome.code, Some(70));
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "gate: box outputs is not built yet\n");
}

/// The payload's text at `key`.
fn field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().expect("the payload holds the key as text")
}

/// `payload` with the last four hex digits of its job's nonce set to `run`, for a later run in one case.
fn renamed(payload: &Value, run: u16) -> Value {
    let mut renamed = payload.clone();
    let job = field(payload, "Job");
    renamed["Job"] = json!(format!("{}{run:04x}", &job[..job.len() - 4]));
    renamed
}

/// The names in the work folder of `payload`.
fn work_entries(payload: &Value) -> Vec<String> {
    fs::read_dir(field(payload, "Work"))
        .expect("the work folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .collect()
}

/// Readies the payload's work folder with a preflight, then commits `files` in the case's source repository, pushes
/// the commit to `refs/gate/<key>` in the bare repository, as the client sends a snapshot, and records the commit as
/// the payload's `Snapshot`; the commit.
fn snapshot(case: &Case, payload: &mut Value, files: &[(&str, &str)]) -> String {
    allow_breakaway();
    passed(&gate_box(case, "preflight", &self::payload(payload), None));
    let commit = push_snapshot(case, payload, files);
    payload["Snapshot"] = json!(commit);
    commit
}

/// Commits `files` in the case's source repository and pushes the commit to `refs/gate/<key>` in the payload's bare
/// repository, as the client sends a snapshot, leaving the payload's `Snapshot` as it is; the commit. The payload's
/// work folder must have had a preflight, which makes the bare repository; this makes the source repository.
fn push_snapshot(case: &Case, payload: &Value, files: &[(&str, &str)]) -> String {
    let source = case.folder().join("source");
    let source_text = source.to_str().expect("the case folder is Unicode").to_owned();
    if !source.exists() {
        git(&["init", "-q", &source_text]);
        git(&["-C", &source_text, "config", "core.autocrlf", "false"]);
    }
    for (name, content) in files {
        let file = source.join(name);
        fs::create_dir_all(file.parent().expect("a file has a folder")).expect("the file's folder can be made");
        fs::write(file, content).expect("a snapshot file can be written");
    }
    git(&["-C", &source_text, "add", "-A"]);
    git(&[
        "-C",
        &source_text,
        "-c",
        "user.name=gate suite",
        "-c",
        "user.email=suite@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "snapshot",
    ]);
    let target = format!("HEAD:refs/gate/{}", field(payload, "Key"));
    git(&["-C", &source_text, "push", "-q", "--force", field(payload, "Bare"), &target]);
    git(&["-C", &source_text, "rev-parse", "HEAD"])
}

/// Whether the bare repository of `payload` has the ref `name`.
fn has_ref(payload: &Value, name: &str) -> bool {
    let mut command = Command::new("git");
    for variable in GIT_REDIRECTS {
        command.env_remove(variable);
    }
    let bare = format!("--git-dir={}", field(payload, "Bare"));
    let output = command
        .args([bare.as_str(), "rev-parse", "--verify", "-q", name])
        .output()
        .expect("git starts; the box cases need git on PATH");
    output.status.success()
}

/// `gate box run` of `payload`, to its end.
fn run_box(case: &Case, payload: &Value) -> Outcome {
    gate_box(case, "run", &self::payload(payload), None)
}

/// `gate box run` of `payload`, started.
fn start_box(case: &Case, payload: &Value) -> Running {
    start(box_command(case, &["box", "run", &self::payload(payload)]))
}

fn let_go(go: &Path) {
    fs::write(go, "").expect("the go file can be made");
}

/// The pid in the `spawned <pid>` line of `stdout`.
fn spawned_pid(stdout: &str) -> u32 {
    let line = stdout.lines().find_map(|line| line.strip_prefix("spawned ")).expect("a spawned line");
    line.parse().expect("the spawned line holds a pid")
}

/// Waits until the process `pid` has ended.
fn wait_until_gone(pid: u32) {
    wait_until(&format!("process {pid} ending"), || {
        gate_win32::process_start_of(pid).expect("the process can be looked for").is_none()
    });
}

#[test]
fn box_run_a_second_run_on_a_held_lock_is_busy() {
    let case = Case::new("box_run_busy");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut held = box_payload(&case, "k", &[FAKE_TOOL, "err", "started", "wait", &go_word]);
    snapshot(&case, &mut held, &[("a.txt", "a\n")]);
    let first = start_box(&case, &held);
    first.wait_for_stderr("started");

    let second = run_box(&case, &renamed(&held, 1));

    assert_eq!(second.code, Some(75), "{second:?}");
    assert_eq!(second.stdout, "");
    assert_eq!(second.stderr, "gate: remote work folder k is busy\n");
    let_go(&go);
    let outcome = first.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    wait_until_closed(Path::new(field(&held, "Lock")));
}

#[test]
fn box_run_refuses_a_subfolder_outside_the_work_folder_and_deletes_the_back_ref() {
    let case = Case::new("box_run_subfolder_outside");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    let commit = snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let kelvin = "..\\\u{212A}\\x";
    let subs = [
        r"C:\elsewhere",
        "C:x",
        r"\rooted",
        r"\\server\share",
        r"\\?\C:\x",
        "/x",
        r"..\k-sibling",
        "sub/../../x",
        kelvin,
    ];
    for sub in subs {
        git(&[
            &format!("--git-dir={}", field(&payload, "Bare")),
            "update-ref",
            "refs/gate-back/k",
            &commit,
        ]);
        payload["Subfolder"] = json!(sub);

        let outcome = run_box(&case, &payload);

        assert_eq!(outcome.code, Some(2), "{sub}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{sub}");
        assert_eq!(
            outcome.stderr,
            format!("gate: remote subfolder {sub} is not a folder inside the work folder\n")
        );
        assert!(!has_ref(&payload, "refs/gate-back/k"), "{sub}: the back ref is deleted");
        assert_eq!(work_entries(&payload), [".git"], "{sub}: nothing is checked out");
    }
}

#[test]
fn box_run_runs_the_command_in_its_subfolder() {
    let case = Case::new("box_run_subfolder");
    let mut payload = box_payload(&case, "k", &[r".\x.cmd"]);
    payload["Subfolder"] = json!("sub");
    snapshot(&case, &mut payload, &[("sub/x.cmd", "@cd\r\n@echo y>made.txt\r\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let folder = Path::new(field(&payload, "Work")).join("sub");
    assert!(
        outcome.stdout.trim_end().eq_ignore_ascii_case(&folder.to_string_lossy()),
        "the command runs in Work\\sub: {outcome:?}"
    );
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    assert_eq!(git(&[&bare, "show", "refs/gate-back/k:sub/made.txt"]), "y");
}

#[test]
fn box_run_refuses_an_identity_name_outside_the_git_identity() {
    let case = Case::new("box_run_identity_refused");
    let mut payload = box_payload(&case, "k", &[r".\write.cmd"]);
    snapshot(&case, &mut payload, &[("write.cmd", "@echo x>new.txt\r\n")]);
    payload["Identity"] = json!({
        "GIT_AUTHOR_NAME": "a",
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "core.fsmonitor",
        "GIT_CONFIG_VALUE_0": "C:/x/evil.exe",
    });

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        "gate: box payload Identity may set only GIT_AUTHOR_* and GIT_COMMITTER_* names: GIT_CONFIG_COUNT\n"
    );
    assert_eq!(work_entries(&payload), [".git"], "nothing is checked out or run");
}

#[test]
fn box_run_git_runs_no_hook_the_command_wrote() {
    let case = Case::new("box_run_hooks");
    let marker = case.folder().join("marker");
    let hook = format!("#!/bin/sh\necho ran > '{}'\n", shown(&marker).replace('\\', "/"));
    let mut payload = box_payload(&case, "k", &[r".\hooks.cmd"]);
    let script = "@copy /y hooks-src\\pre-push .git\\hooks\\pre-push >nul\r\n\
                  @copy /y hooks-src\\post-checkout .git\\hooks\\post-checkout >nul\r\n\
                  @echo x>new.txt\r\n";
    snapshot(
        &case,
        &mut payload,
        &[("hooks.cmd", script), ("hooks-src/pre-push", &hook), ("hooks-src/post-checkout", &hook)],
    );

    let first = run_box(&case, &payload);
    assert_eq!(first.code, Some(0), "{first:?}");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the first run pushed its back commit");
    let second = run_box(&case, &renamed(&payload, 1));

    assert_eq!(second.code, Some(0), "{second:?}");
    let hooks = Path::new(field(&payload, "Work")).join(".git").join("hooks");
    assert!(
        hooks.join("pre-push").exists() && hooks.join("post-checkout").exists(),
        "the command wrote its hooks"
    );
    assert!(!marker.exists(), "no hook ran");
}

#[test]
fn box_run_git_runs_no_hook_written_beside_the_work_folder() {
    let case = Case::new("box_run_hooks_beside");
    let marker = case.folder().join("marker");
    let hook = format!("#!/bin/sh\necho ran > '{}'\n", shown(&marker).replace('\\', "/"));
    let mut payload = box_payload(&case, "k", &[r".\hooks.cmd"]);
    let script = "@mkdir ..\\k.hooks 2>nul\r\n\
                  @copy /y hooks-src\\pre-push ..\\k.hooks\\pre-push >nul\r\n\
                  @echo x>new.txt\r\n";
    snapshot(&case, &mut payload, &[("hooks.cmd", script), ("hooks-src/pre-push", &hook)]);

    let first = run_box(&case, &payload);
    assert_eq!(first.code, Some(0), "{first:?}");
    let second = run_box(&case, &renamed(&payload, 1));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(
        Path::new(&format!("{}.hooks", field(&payload, "Work"))).join("pre-push").exists(),
        "the command wrote its hook"
    );
    assert!(has_ref(&payload, "refs/gate-back/k"), "the run pushed its back commit");
    assert!(!marker.exists(), "no hook ran");
}

#[test]
fn box_run_refuses_a_work_folder_ending_in_a_separator() {
    let case = Case::new("box_run_work_separator");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let work = format!("{}\\", field(&payload, "Work"));
    payload["Work"] = json!(work);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, format!("gate: box payload Work must not end with a separator: {work}\n"));
}

#[test]
fn box_run_refuses_an_index_or_lock_other_than_the_work_folders() {
    let case = Case::new("box_run_index_lock_refused");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let precious = case.folder().join("precious.txt");
    fs::write(&precious, "keep").expect("the precious file can be written");
    let mut index = payload.clone();
    index["Index"] = json!(shown(&precious));

    let outcome = run_box(&case, &index);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: box payload Index must be {}.index\n", field(&payload, "Work"))
    );
    assert_eq!(fs::read_to_string(&precious).expect("the precious file is read"), "keep");

    let mut lock = payload.clone();
    lock["Lock"] = json!(shown(&case.folder().join("NUL")));

    let outcome = run_box(&case, &lock);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: box payload Lock must be {}.lock\n", field(&payload, "Work"))
    );
}

#[test]
fn box_run_a_failed_sync_back_turns_a_pass_into_status_1() {
    let case = Case::new("box_run_sync_back_failed");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    fs::create_dir_all(field(&payload, "Index")).expect("a folder can block the index copy");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with("gate: remote sync-back failed: ")),
        "{outcome:?}"
    );
}

#[test]
fn box_run_a_failed_fetch_exits_with_gits_status() {
    let case = Case::new("box_run_fetch_failed");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    passed(&gate_box(&case, "preflight", &self::payload(&payload), None));
    // Well formed, but the fetch fails first, so no commit is ever looked for.
    payload["Snapshot"] = json!("0000000000000000000000000000000000000000");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(128), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.lines().any(|line| line == "gate: remote fetch failed (status 128)"),
        "{outcome:?}"
    );
}

#[test]
fn box_run_refuses_a_job_name_already_held() {
    let case = Case::new("box_run_job_held");
    let mut payload = box_payload(&case, "k", &[r".\write.cmd"]);
    snapshot(&case, &mut payload, &[("write.cmd", "@echo x>new.txt\r\n")]);
    let _held = Job::create(field(&payload, "Job")).expect("the job is created");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: the job {} already exists; a run's job name must be new\n", field(&payload, "Job"))
    );
    assert!(!Path::new(field(&payload, "Work")).join("new.txt").exists(), "the command did not run");
    assert!(!has_ref(&payload, "refs/gate-back/k"), "no back commit");
}

#[test]
fn box_stop_refuses_a_job_name_not_of_its_work_folder() {
    let case = Case::new("box_stop_foreign_job");
    let go = case.folder().join("go");
    let name = format!(r"Local\gate-job-suite-{}", std::process::id());
    let job = Job::create(&name).expect("the job is created");
    let launch = Launch {
        application: PathBuf::from(FAKE_TOOL),
        command_line: format!("\"{FAKE_TOOL}\" wait \"{}\"", shown(&go)).encode_utf16().collect(),
        directory: case.folder(),
        environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
        log: case.folder().join("held.log"),
        err_log: case.folder().join("held.log.err"),
        priority: None,
    };
    let held = launch
        .start_suspended()
        .expect("the held command starts")
        .assign_and_resume(&job)
        .expect("it is assigned and resumed");
    let mut payload = box_payload(&case, "k", &[]);
    payload["Job"] = json!(name);

    let outcome = gate_box(&case, "stop", &self::payload(&payload), None);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: box payload Job is not this work folder's job name: {name}\n")
    );
    assert_eq!(held.wait(Duration::ZERO).expect("the held command is looked at"), None, "it lives");
    let_go(&go);
    assert_eq!(held.wait(Duration::from_secs(600)).expect("the held command ends"), Some(0));
}

#[test]
fn box_run_refuses_a_subfolder_the_snapshot_lacks() {
    let case = Case::new("box_run_subfolder_missing");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    payload["Subfolder"] = json!("nope");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "gate: remote subfolder nope does not exist in the snapshot\n");
}

#[test]
fn box_run_checks_out_the_snapshot_keeping_ignored_files_and_removing_untracked_ones() {
    let case = Case::new("box_run_checkout");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let work = PathBuf::from(field(&payload, "Work"));
    let first = snapshot(&case, &mut payload, &[("a.txt", "a\n"), (".gitignore", "ignored.txt\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]), first);
    assert_eq!(fs::read_to_string(work.join("a.txt")).expect("a.txt is checked out"), "a\n");
    assert!(!has_ref(&payload, "refs/gate-back/k"), "an unchanged tree leaves no back ref");

    fs::write(work.join("ignored.txt"), "build output").expect("an ignored file can be written");
    fs::write(work.join("untracked.txt"), "stray").expect("an untracked file can be written");
    let second = snapshot(&case, &mut payload, &[("a.txt", "b\n")]);

    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]), second);
    assert_eq!(fs::read_to_string(work.join("a.txt")).expect("a.txt is checked out"), "b\n");
    assert!(work.join("ignored.txt").exists(), "an ignored file survives");
    assert!(!work.join("untracked.txt").exists(), "an untracked file is removed");
}

#[test]
fn box_run_passes_the_commands_whole_status() {
    let case = Case::new("box_run_status");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "300"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);
    assert_eq!(outcome.code, Some(300), "{outcome:?}");

    let mut all_ones = renamed(&payload, 1);
    all_ones["Command"] = json!([FAKE_TOOL, "exit", "-1"]);
    let outcome = run_box(&case, &all_ones);
    assert_eq!(outcome.code, Some(-1), "0xFFFFFFFF passes whole: {outcome:?}");
}

#[test]
fn box_run_forwards_each_stream_line_by_line_ending_each_line_lf() {
    let case = Case::new("box_run_forwarding");
    let mut payload = box_payload(
        &case,
        "k",
        &[FAKE_TOOL, "out", "a1", "err", "e1", "out", "a2", "bytes", "620d0a630d640a", "err", "e2"],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "a1\na2\nb\nc\nd\n", "CR LF and a lone CR each end a line");
    assert_eq!(outcome.stderr, "e1\ne2\n");
}

#[test]
fn box_run_beats_while_a_busy_command_runs() {
    let case = Case::new("box_run_beats");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "err", "started", "burn", &go_word, "exit", "7"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let running = start(bare_box_command(
        &case,
        &["box", "--test-heartbeat-ms", "30", "run", &self::payload(&payload)],
    ));
    // Beats come before the command too, and one may slip in between its first line and the watch; of two after that
    // line, the second came a period after the watch began, while the command burned.
    let beats_after_started = |stderr: &str| {
        stderr.split_once("started\n").map_or(0, |(_, after)| {
            after.lines().filter(|line| line.starts_with("gate-remote-heartbeat: ")).count()
        })
    };
    running.wait_for_stderr_where("two heartbeats after the command's first line", |stderr| beats_after_started(stderr) >= 2);
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(7), "the command's own status: {outcome:?}");
    assert_eq!(outcome.stdout, "burning\n", "no heartbeat on stdout: {outcome:?}");
    let beat = Regex::new(r"^gate-remote-heartbeat: \d+$").expect("the pattern compiles");
    let beats: Vec<&str> = outcome.stderr.lines().filter(|line| line.contains("gate-remote-heartbeat")).collect();
    assert!(!beats.is_empty(), "{outcome:?}");
    for line in beats {
        assert!(beat.is_match(line), "a heartbeat is a whole line of its own: {line:?} in {outcome:?}");
    }
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_stops_reading_output_a_leftover_process_holds_open() {
    let case = Case::new("box_run_drain");
    let never = case.folder().join("never");
    let never_word = shown(&never);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "spawn-wait", &never_word, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "the run still ends: {outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == "gate: remote command's output is still held open by a leftover process; stopped reading"),
        "{outcome:?}"
    );
    let_go(&never);
    wait_until_gone(spawned_pid(&outcome.stdout));
}

#[test]
fn box_run_a_closed_stdout_pipe_stops_the_job_and_exits_255() {
    let case = Case::new("box_run_closed_stdout");
    let go = case.folder().join("go");
    let never = case.folder().join("never");
    let go_word = shown(&go);
    let never_word = shown(&never);
    let script = format!("@echo x>new.txt\r\n@\"{FAKE_TOOL}\" err started wait \"{go_word}\" out x wait \"{never_word}\"\r\n");
    let mut payload = box_payload(&case, "k", &[r".\run.cmd"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n"), ("run.cmd", &script)]);

    let running = start_with_closed(Closed::Stdout, box_command(&case, &["box", "run", &self::payload(&payload)]));
    running.wait_for_stderr("started");
    let_go(&go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(255), "a closed connection exits 255: {outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote the client closed the connection; stopped job "),
        "{outcome:?}"
    );
    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    assert_eq!(stopped.code, Some(0), "the command's job is gone: {stopped:?}");
    assert_eq!(stopped.stdout, "", "nothing is left to stop: {stopped:?}");
    assert!(
        Path::new(field(&payload, "Work")).join("new.txt").exists(),
        "the command changed the tree"
    );
    assert!(!has_ref(&payload, "refs/gate-back/k"), "a lost client gets no back commit");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_closed_stderr_pipe_stops_the_job_and_exits_255() {
    let case = Case::new("box_run_closed_stderr");
    let go = case.folder().join("go");
    let never = case.folder().join("never");
    let go_word = shown(&go);
    let never_word = shown(&never);
    let script = format!("@echo x>new.txt\r\n@\"{FAKE_TOOL}\" out started wait \"{go_word}\" err x wait \"{never_word}\"\r\n");
    let mut payload = box_payload(&case, "k", &[r".\run.cmd"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n"), ("run.cmd", &script)]);

    let running = start_with_closed(Closed::Stderr, box_command(&case, &["box", "run", &self::payload(&payload)]));
    running.wait_for_stdout("started");
    let_go(&go);

    let outcome = running.finish();

    assert_eq!(outcome.code, Some(255), "a closed connection exits 255: {outcome:?}");
    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    assert_eq!(stopped.code, Some(0), "the command's job is gone: {stopped:?}");
    assert_eq!(stopped.stdout, "", "nothing is left to stop: {stopped:?}");
    assert!(
        Path::new(field(&payload, "Work")).join("new.txt").exists(),
        "the command changed the tree"
    );
    assert!(!has_ref(&payload, "refs/gate-back/k"), "a lost client gets no back commit");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_cmd_target_runs_through_cmd_and_what_it_wrote_is_committed_back() {
    let case = Case::new("box_run_back_commit");
    let mut payload = box_payload(&case, "k", &[r".\write.cmd"]);
    let commit = snapshot(&case, &mut payload, &[("write.cmd", "@echo from-cmd\r\n@echo x>new.txt\r\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "from-cmd\n");
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    assert_eq!(
        git(&[&bare, "rev-parse", "refs/gate-back/k^"]),
        commit,
        "the back commit's parent is HEAD"
    );
    assert_eq!(git(&[&bare, "show", "refs/gate-back/k:new.txt"]), "x");
}

#[test]
fn box_run_refuses_a_batch_word_cmd_would_expand() {
    let case = Case::new("box_run_batch_refusal");
    let mut payload = box_payload(&case, "k", &[r".\write.cmd", "%PATH%"]);
    snapshot(&case, &mut payload, &[("write.cmd", "@echo x>new.txt\r\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        "gate: a .cmd or .bat command cannot take a word holding a line break or %NAME% of a set variable, which cmd.exe \
         would split or expand: %PATH%\n"
    );
    assert!(!Path::new(field(&payload, "Work")).join("new.txt").exists(), "the command did not run");
}

#[test]
fn box_run_an_unknown_program_exits_127() {
    let case = Case::new("box_run_unknown_program");
    let mut payload = box_payload(&case, "k", &["gate-suite-no-such-program"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(127), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: could not start gate-suite-no-such-program: {}\n", gate_win32::win32_message(2))
    );
}

#[test]
fn box_stop_ends_a_held_run_and_says_so() {
    let case = Case::new("box_stop_held_run");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "spawn-wait", &go_word, "wait", &go_word]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let running = start_box(&case, &payload);
    let helper = spawned_pid(&running.wait_for_stdout("\n"));

    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(stopped.stdout, format!("gate: remote stopped job {}\n", field(&payload, "Job")));
    assert_eq!(stopped.stderr, "");
    assert_eq!(gate_win32::process_start_of(helper).expect("the helper can be looked for"), None);
    let outcome = running.finish();
    assert_eq!(outcome.code, Some(124), "{outcome:?}");
}

#[test]
fn box_stop_of_an_unknown_job_says_nothing() {
    let case = Case::new("box_stop_unknown_job");
    allow_breakaway();

    let outcome = gate_box(&case, "stop", &payload(&box_payload(&case, "k", &[])), None);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "");
}

#[test]
fn box_stop_with_an_earlier_runs_job_spares_the_next_run() {
    let case = Case::new("box_stop_spares_next_run");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut earlier = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut earlier, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &earlier).code, Some(0));
    let mut next = renamed(&earlier, 1);
    next["Command"] = json!([FAKE_TOOL, "err", "started", "wait", &go_word]);
    let mut running = start_box(&case, &next);
    running.wait_for_stderr("started");

    let stopped = gate_box(&case, "stop", &payload(&earlier), None);

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(stopped.stdout, "", "the earlier run's job is gone");
    assert!(running.is_running(), "the next run goes on");
    let_go(&go);
    assert_eq!(running.finish().code, Some(0));
}

#[test]
fn box_run_refuses_a_payload_without_a_40_hex_snapshot() {
    let case = Case::new("box_run_snapshot_refused");
    let go = case.folder().join("go");
    let go_word = shown(&go);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the good payload runs");
    let work = PathBuf::from(field(&payload, "Work"));
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    let head = git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]);
    let mut held = renamed(&payload, 1);
    held["Command"] = json!([FAKE_TOOL, "err", "started", "wait", &go_word]);
    let running = start_box(&case, &held);
    running.wait_for_stderr("started");
    // Seeded while the held run holds the lock: a refusal before the lock leaves both behind.
    fs::write(work.join("untracked.txt"), "stray").expect("an untracked file can be written");
    git(&[&bare, "update-ref", "refs/gate-back/k", &head]);
    let shortened = head[..39].to_owned();
    let shouted = head.to_uppercase();
    let mut absent = payload.clone();
    let dropped = absent.as_object_mut().expect("the payload is an object").remove("Snapshot");
    assert!(dropped.is_some(), "the good payload has a Snapshot to drop");
    let mut short = payload.clone();
    short["Snapshot"] = json!(&shortened);
    let mut upper = payload.clone();
    upper["Snapshot"] = json!(&shouted);
    let mut number = payload.clone();
    number["Snapshot"] = json!(3);

    for (what, refused, quoted) in [
        ("a missing key", &absent, ""),
        ("a 39-character value", &short, shortened.as_str()),
        ("an upper-case sha", &upper, shouted.as_str()),
        ("a non-string value", &number, "3"),
    ] {
        let outcome = run_box(&case, refused);

        assert_eq!(outcome.code, Some(2), "{what}: refused before the lock, never busy: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{what}");
        assert_eq!(
            outcome.stderr,
            format!("gate: remote run payload's Snapshot '{quoted}' is not a 40-hex commit sha\n"),
            "{what}: the refusal line"
        );
        assert_eq!(git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]), head, "{what}: HEAD stands");
        assert!(work.join("untracked.txt").exists(), "{what}: the untracked file stands");
        assert!(has_ref(&payload, "refs/gate-back/k"), "{what}: the back ref stands");
    }

    let_go(&go);
    assert_eq!(running.finish().code, Some(0), "the held run ends");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_checks_out_the_payload_snapshot_not_the_ref_tip() {
    let case = Case::new("box_run_snapshot_not_ref_tip");
    let mut payload = box_payload(&case, "k", &[r".\show.cmd"]);
    let a = snapshot(&case, &mut payload, &[("show.cmd", "@type a.txt\r\n"), ("a.txt", "a\n")]);
    let b = push_snapshot(&case, &payload, &[("a.txt", "b\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    assert_ne!(a, b, "the ref moved on past the payload's commit");
    assert_eq!(git(&[&bare, "rev-parse", "refs/gate/k"]), b, "the key's tip is the later commit");
    assert_eq!(
        git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]),
        a,
        "the payload's commit is what is checked out"
    );
    assert_eq!(outcome.stdout, "a\n", "the command sees the payload's commit's file");
}

#[test]
fn box_run_says_a_snapshot_missing_after_the_fetch() {
    let case = Case::new("box_run_snapshot_missing");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the good payload runs");
    let work = PathBuf::from(field(&payload, "Work"));
    let bare = format!("--git-dir={}", field(&payload, "Bare"));
    let head = git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]);
    // Seeded before the refused run: it ends before the checkout and the back-ref clear, so both stand behind.
    fs::write(work.join("untracked.txt"), "stray").expect("an untracked file can be written");
    git(&[&bare, "update-ref", "refs/gate-back/k", &head]);
    let absent = "0".repeat(40);
    payload["Snapshot"] = json!(&absent);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert_eq!(
        outcome.stderr,
        format!("gate: remote snapshot {absent} is not in the work folder's repository after the fetch\n")
    );
    assert_eq!(git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]), head, "HEAD stands");
    assert!(work.join("untracked.txt").exists(), "the untracked file stands");
    assert!(has_ref(&payload, "refs/gate-back/k"), "the back ref stands");
}

//! The hidden `gate box`, run as the builder runs it over ssh: no `--state-dir`, every path in the base64 JSON payload,
//! all of them under the case's folder. git must be on PATH; a case about dotnet sets the gate's PATH itself.

use std::ffi::OsString;
use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_win32::{EnvironmentBlock, Job, Launch, LaunchOutput, OpenedJob, StoppableProcess};
use jiff::{SignedDuration, Timestamp};
use proc_table::Win32ProcessTable;
use regex::Regex;
use serde_json::{Value, json};

use crate::support::{
    Case, Closed, Escapee, FAKE_TOOL, GIT_REDIRECTS, Go, Outcome, Running, allow_breakaway, box_payload, build_version, copy_tool, finish, shown,
    start, start_with_closed, wait_until, wait_until_closed,
};

mod outputs;

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs|inflight <base64 JSON payload>\n";

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

/// A PATH of `folders` first, then the suite's own PATH less every folder holding a `dotnet`: the box finds the stub's
/// dotnet and no real one behind it, and git keeps everything its helpers need. A PATH of git's folder alone is not
/// enough for a run: git's local transport spawns a helper that needs the folder holding its runtime on PATH, so
/// `git fetch` fails with an access violation without it, though the steps a preflight runs do not.
fn path_for_run(folders: &[PathBuf]) -> OsString {
    let own = std::env::var_os("PATH").expect("PATH is set");
    let without_dotnet = std::env::split_paths(&own).filter(|folder| !holds_dotnet(folder));
    std::env::join_paths(folders.iter().cloned().chain(without_dotnet)).expect("the folders join into a PATH")
}

/// Whether `folder` holds a `dotnet` named with any of the four extensions a `PATHEXT` search tries, `.exe`, `.cmd`,
/// `.bat` and `.com`, so [`path_for_run`] drops it.
fn holds_dotnet(folder: &Path) -> bool {
    ["exe", "cmd", "bat", "com"]
        .iter()
        .any(|extension| folder.join(format!("dotnet.{extension}")).is_file())
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
    assert_eq!(printed[1], format!("gate: remote version {}", build_version(&case)));
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
    assert_eq!(printed[0], format!("gate: remote version {}", build_version(&case)));
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

/// The builder's `dotnet --list-sdks` that never answers is ended with its job at the bound `--test-dotnet-limit-ms`
/// sets: the preflight says why it has no SDK list and still passes, and nothing the stub started is left running. Only
/// the bound ends the call, so no load changes what the preflight says.
#[test]
fn preflight_a_dotnet_that_does_not_answer_is_an_unreadable_sdk_list() {
    let case = Case::new("preflight_dotnet_hangs");
    let folders = Folders::new(&case);
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    let go = Go(case.folder().join("dotnet-go"));
    let held = case.folder().join("dotnet-held");
    let hang = format!(
        "@echo off\r\n\"{FAKE_TOOL}\" wait \"{}\" >\"{}\"\r\n",
        go.path().display(),
        held.display()
    );
    fs::write(stub.join("dotnet.cmd"), hang).expect("the dotnet stub can be written");
    let mut command = box_command(&case, &["box", "--test-dotnet-limit-ms", "1000", "preflight", &folders.payload()]);
    command.env("PATH", path_with_git(&[stub]));

    let outcome = finish(command);

    // A stub that started holds its output file open until its job's kill ends it; one the kill came first never made it.
    if held.exists() {
        wait_until_closed(&held);
    }
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!outcome.stdout.contains("gate: remote sdks"), "{outcome:?}");
    let unreadable = "gate: remote could not read the SDK list: dotnet --list-sdks did not answer in 1 s";
    assert!(outcome.stderr.lines().any(|line| line == unreadable), "{unreadable}: {outcome:?}");
}

/// The builder's `dotnet --list-sdks` runs in the system folder, not the folder `gate box` was started in: the stub
/// writes the folder it runs in.
#[test]
fn preflight_runs_dotnet_in_the_system_folder() {
    let case = Case::new("preflight_dotnet_folder");
    let folders = Folders::new(&case);
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    let ran_in = case.folder().join("dotnet-folder");
    let body = format!("@echo off\r\n>\"{}\" echo %CD%\r\necho 8.0.404 [C:\\stub]\r\n", ran_in.display());
    fs::write(stub.join("dotnet.cmd"), body).expect("the dotnet stub can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[stub])));

    assert_eq!(passed(&outcome).last(), Some(&"gate: remote sdks 8.0.404"), "{outcome:?}");
    let ran_in = fs::read_to_string(&ran_in).expect("the stub wrote the folder it ran in");
    let system = gate_win32::system_directory().expect("the system folder can be read");
    let system = system.to_string_lossy();
    assert!(
        ran_in.trim_end().eq_ignore_ascii_case(system.trim_end_matches('\\')),
        "{system}: {ran_in:?}"
    );
}

/// A `dotnet.cmd` in the folder `gate box` was started in, which no `PATH` folder names, is not run for the SDK list:
/// the preflight says it has none, and the stub leaves no mark.
#[test]
fn preflight_does_not_run_a_dotnet_in_its_working_folder() {
    let case = Case::new("preflight_dotnet_in_working_folder");
    let folders = Folders::new(&case);
    let marker = case.folder().join("dotnet-ran");
    let decoy = format!("@echo off\r\n>\"{}\" echo ran\r\necho 997.0.100 [C:\\decoy]\r\n", marker.display());
    fs::write(case.folder().join("dotnet.cmd"), decoy).expect("the dotnet in the working folder can be written");

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[])));

    assert_eq!(passed(&outcome).last(), Some(&"gate: remote sdks none"), "{outcome:?}");
    assert!(!marker.exists(), "the dotnet.cmd in the working folder ran");
}

#[test]
fn preflight_version_line_is_the_build_version() {
    let case = Case::new("preflight_version_line_is_the_build_version");
    let folders = Folders::new(&case);
    let version = build_version(&case);

    let outcome = gate_box(&case, "preflight", &folders.payload(), None);

    assert!(
        passed(&outcome).contains(&format!("gate: remote version {version}").as_str()),
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

/// The payload's text at `key`.
fn field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().expect("the payload holds the key as text")
}

/// `payload` with the last four hex digits of its job's nonce and of its outputs nonce set to `run`, for a later run
/// in one case.
fn renamed(payload: &Value, run: u16) -> Value {
    let mut renamed = payload.clone();
    for key in ["Job", "Nonce"] {
        let nonce = field(payload, key);
        renamed[key] = json!(format!("{}{run:04x}", &nonce[..nonce.len() - 4]));
    }
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

/// `gate box run` of `payload` with `name` set to `value` in the box's own environment, as a case about a variable the
/// box inherits needs it.
fn run_box_with_env(case: &Case, payload: &Value, name: &str, value: &str) -> Outcome {
    let mut command = box_command(case, &["box", "run", &self::payload(payload)]);
    command.env(name, value);
    finish(command)
}

/// `gate box run` of `payload` with `PATH` set to `path` when given, as a case about dotnet runs it.
fn run_box_with_path(case: &Case, payload: &Value, path: Option<&OsString>) -> Outcome {
    gate_box(case, "run", &self::payload(payload), path)
}

/// `gate box run` of `payload` with `PATH` set to `path`, started, as a case that stops it mid-run needs it.
fn start_box_with_path(case: &Case, payload: &Value, path: Option<&OsString>) -> Running {
    let mut command = box_command(case, &["box", "run", &self::payload(payload)]);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    start(command)
}

/// The tool manifest a restore case's snapshot holds; its bytes are what the work folder's record hashes.
const TOOLS_MANIFEST: &str = "{\n  \"version\": 1\n}\n";
/// The manifest's path in the snapshot, at the work folder's root under `.config`.
const TOOLS_MANIFEST_PATH: &str = ".config/dotnet-tools.json";
/// The SHA-256 the record holds for [`TOOLS_MANIFEST`] at `.config\dotnet-tools.json`: its path relative to the work
/// folder, a NUL byte, its bytes, a NUL byte, as lower-case hex. Pinned, so a change to the layout fails a case.
const TOOLS_MANIFEST_HASH: &str = "d834b15f0e1d448cb8a03922e089bdf22783185fc8884f500830639c97a22083";

/// The file a stub dotnet appends its working folder to, one line per restore.
fn counter(case: &Case) -> PathBuf {
    case.folder().join("counter.txt")
}

/// The lines of the case's counter file, none when no stub ran.
fn counter_lines(case: &Case) -> Vec<String> {
    fs::read_to_string(counter(case)).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// A folder on the case's `PATH` holding a `dotnet.cmd` that handles `tool restore`: it prints `restore: out` on
/// stdout and `restore: note` on stderr, appends its working folder to the case's counter file, and exits with
/// `exit`. A case passes it through [`path_for_run`], so its `PATH` holds this dotnet and no other. The folder is also
/// where a case puts any other stub it needs on `PATH` (see [`stub_git`]).
fn stub_dotnet(case: &Case, exit: i32) -> PathBuf {
    stub_dotnet_running(case, exit, "")
}

/// [`stub_dotnet`] with the batch script `before` run first, before the stub says anything, so a case can hold the
/// restore open and has whatever that script prints as the restore's own first output.
fn stub_dotnet_running(case: &Case, exit: i32, before: &str) -> PathBuf {
    let stub = case.folder().join(format!("stub-dotnet-{exit}"));
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(
        stub.join("dotnet.cmd"),
        format!(
            "@echo off\r\n{before}echo restore: out\r\necho restore: note 1>&2\r\necho %CD%>>\"{}\"\r\nexit /b {exit}\r\n",
            shown(&counter(case))
        ),
    )
    .expect("the dotnet stub can be written");
    stub
}

/// Puts a `git.cmd` in the stub folder `stub` that parks the box on its first git step — the fetch, before the
/// restore — making `parked` and waiting for `go`, then running the real git with the same words. A case can so order
/// a stop between the work folder's lock and the restore: it waits for `parked`, stops the run, and makes `go` to let
/// the run go on to the restore's first look at the stop signal. Without it no step before the restore can be held.
fn stub_git(stub: &Path, parked: &Path, go: &Path) {
    let real = git_folder().join("git.exe");
    fs::write(
        stub.join("git.cmd"),
        format!(
            "@echo off\r\nif exist \"{go}\" goto real\r\necho parked>\"{parked}\"\r\n:wait\r\nif exist \"{go}\" goto real\r\nping -n 2 127.0.0.1 >nul\r\ngoto wait\r\n:real\r\n\"{real}\" %*\r\n",
            go = shown(go),
            parked = shown(parked),
            real = shown(&real),
        ),
    )
    .expect("the git stub can be written");
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
    assert_eq!(
        second.stderr,
        format!("{}\ngate: remote work folder k is busy\n", refused_line(&renamed(&held, 1), "busy"))
    );
    let_go(&go);
    let outcome = first.finish();
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    wait_until_closed(Path::new(field(&held, "Lock")));
}

/// The start marker the box says for the run of `payload` just before its command starts.
fn started_line(payload: &Value) -> String {
    format!("gate-remote-started: {}", field(payload, "Nonce"))
}

/// The tag the box says for the run of `payload` just before its human refusal line, refused for `reason`.
fn refused_line(payload: &Value, reason: &str) -> String {
    format!("gate-remote-refused: {} {reason}", field(payload, "Nonce"))
}

#[test]
fn the_start_marker_comes_before_the_commands_first_output() {
    let case = Case::new("box_run_start_marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "err", "first", "out", "first"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let marker = started_line(&payload);
    assert_eq!(said.iter().filter(|line| **line == marker).count(), 1, "one marker: {outcome:?}");
    let marked = said.iter().position(|line| *line == marker);
    let first = said.iter().position(|line| *line == "first");
    assert!(
        matches!((marked, first), (Some(marked), Some(first)) if marked < first),
        "the marker comes before the command's first line: {outcome:?}"
    );
    assert!(lines(&outcome.stdout).contains(&"first"), "{outcome:?}");
}

#[test]
fn a_busy_refusal_is_tagged_and_has_no_marker() {
    let case = Case::new("box_run_busy_tagged");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(field(&payload, "Lock"))
        .expect("the work folder's lock can be held");

    let outcome = run_box(&case, &payload);

    drop(held);
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let tag = refused_line(&payload, "busy");
    assert_eq!(
        lines(&outcome.stderr),
        [tag.as_str(), "gate: remote work folder k is busy"],
        "the tag comes just before the busy line, and no marker: {outcome:?}"
    );
    assert!(!marker.exists(), "the command never ran");
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
fn box_run_gives_the_command_its_run_s_compiler_server_id() {
    let case = Case::new("box_run_compiler_server_id");
    let mut payload = box_payload(&case, "k", &[r".\servers.cmd"]);
    let script = "@echo %SharedCompilationId% %MSBUILDNODEHANDSHAKESALT% %MSBUILDUSESERVER% %DOTNET_CLI_USE_MSBUILD_SERVER% \
                  %MSBUILDDISABLENODEREUSE%\r\n";
    snapshot(&case, &mut payload, &[("servers.cmd", script)]);

    // Node reuse inherited on, so the case sees the box turn it off rather than a gate above the suite's own run.
    let outcome = run_box_with_env(&case, &payload, "MSBUILDDISABLENODEREUSE", "0");

    let run = format!("gate-box-{}", field(&payload, "Nonce"));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("{run} {run} 0 0 1\n"),
        "the command's servers are named by its run, the MSBuild server and node reuse off: {outcome:?}"
    );
}

#[test]
fn box_run_turns_the_event_log_provider_off_for_the_command() {
    let case = Case::new("box_run_event_log_off");
    let mut payload = box_payload(&case, "k", &[r".\eventlog.cmd"]);
    snapshot(
        &case,
        &mut payload,
        &[("eventlog.cmd", "@echo %Logging__EventLog__LogLevel__Default%\r\n")],
    );

    // Inherited at another level, so the case sees the box set its own value rather than pass one through.
    let outcome = run_box_with_env(&case, &payload, "Logging__EventLog__LogLevel__Default", "Warning");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "None\n",
        "a .NET host the command starts logs nothing to the Event Log, which the command's token cannot write: {outcome:?}"
    );
}

#[test]
fn box_run_turns_the_event_log_provider_off_under_the_colon_spelling_too() {
    let case = Case::new("box_run_event_log_off_colon");
    let mut payload = box_payload(&case, "k", &[r".\colon.cmd"]);
    snapshot(&case, &mut payload, &[("colon.cmd", "@set \"Logging:EventLog:LogLevel:Default\"\r\n")]);

    // The colon spelling .NET reads as the same key, inherited, so one spelling left behind cannot outvote the other.
    let outcome = run_box_with_env(&case, &payload, "Logging:EventLog:LogLevel:Default", "Warning");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "Logging:EventLog:LogLevel:Default=None\n",
        "both spellings of the key say None, whichever .NET reads first: {outcome:?}"
    );
}

#[test]
fn box_run_replaces_an_inherited_compiler_server_id() {
    let case = Case::new("box_run_compiler_server_id_inherited");
    let mut payload = box_payload(&case, "k", &[r".\servers.cmd"]);
    snapshot(&case, &mut payload, &[("servers.cmd", "@echo %SharedCompilationId%\r\n")]);

    let outcome = run_box_with_env(&case, &payload, "SharedCompilationId", "other");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout,
        format!("gate-box-{}\n", field(&payload, "Nonce")),
        "the run's id replaces the box's inherited value: {outcome:?}"
    );
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
        output: LaunchOutput::Files {
            log: case.folder().join("held.log"),
            err_log: case.folder().join("held.log.err"),
        },
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
    assert_eq!(outcome.stderr, format!("{}\ne1\ne2\n", started_line(&payload)));
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

/// How many files the warning case's command writes, each of which makes the back commit's `add -A` warn once.
const WARNED_FILES: usize = 200;

#[test]
fn a_git_warning_is_never_split_by_a_heartbeat() {
    let case = Case::new("box_run_git_warning_whole");
    let mut payload = box_payload(&case, "k", &[r".\write.cmd"]);
    let write = format!("@for /L %%i in (1,1,{WARNED_FILES}) do @echo line>w%%i.txt\r\n");
    snapshot(&case, &mut payload, &[(".gitattributes", "*.txt text eol=lf\n"), ("write.cmd", &write)]);

    let outcome = finish(bare_box_command(
        &case,
        &["box", "--test-heartbeat-ms", "1", "run", &self::payload(&payload)],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let beat = Regex::new(r"^gate-remote-heartbeat: \d+$").expect("the pattern compiles");
    for line in outcome.stderr.lines().filter(|line| line.contains("gate-remote-heartbeat")) {
        assert!(beat.is_match(line), "a heartbeat is a whole line of its own: {line:?} in {outcome:?}");
    }
    let warning = Regex::new(r"^warning: in the working copy of 'w\d+\.txt', CRLF will be replaced by LF the next time Git touches it$")
        .expect("the pattern compiles");
    let warned = outcome.stderr.lines().filter(|line| warning.is_match(line)).count();
    assert_eq!(warned, WARNED_FILES, "each of git's warnings arrives whole: {outcome:?}");
}

/// A live nested gate in the command's job, which the run's end spares, still holds the command's output once the
/// command exits: the box stops reading after the drain and says so, and the run still ends. The spared gate is
/// fake-tool copied as `gate.exe`, whose `spawn-wait` child runs from the same image, since the run's end judges a gate
/// by its image name.
#[test]
fn box_run_stops_reading_output_a_leftover_process_holds_open() {
    let case = Case::new("box_run_drain");
    copy_tool(&case, "gate.exe");
    let gate = shown(&case.folder().join("gate.exe"));
    let never = case.folder().join("never");
    let never_word = shown(&never);
    let mut payload = box_payload(&case, "k", &[&gate, "spawn-wait", &never_word, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);
    // The spared child still waits for a go file no one has written, so its pid is still its own when it is opened.
    let child = StoppableProcess::open(spawned_pid(&outcome.stdout))
        .expect("the spared child is looked for")
        .expect("the spared child still runs");
    let_go(&never);
    // It may be ending on its go file already, which refuses the terminate; the wait below sees it end either way.
    let _ = child.terminate(1);
    let ended = Escapee::exit_code_once_ended(&child);

    assert_eq!(outcome.code, Some(0), "the run still ends: {outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == "gate: remote command's output is still held open by a leftover process; stopped reading"),
        "{outcome:?}"
    );
    assert!(ended.is_some(), "the spared child is cleaned up: {outcome:?}");
}

/// Runs `gate box run` of a command that starts a child in its job, a `fake-tool wait` on a go file no one writes until
/// the case ends, holding the command's output open, then exits 0 once the case lets it; `with` is set in the box's
/// environment when given. The box's outcome and the child's exit code once it has ended, `None` while it still runs.
/// The child is opened while the command still runs, so its pid is pinned.
fn run_box_leaving_a_child(case: &Case, with: Option<(&str, &str)>) -> (Outcome, Option<u32>) {
    let (go, proceed) = (case.folder().join("go"), case.folder().join("proceed"));
    let (go_word, proceed_word) = (shown(&go), shown(&proceed));
    let command = [FAKE_TOOL, "spawn-wait", &go_word, "wait", &proceed_word, "exit", "0"];
    let mut payload = box_payload(case, "k", &command);
    snapshot(case, &mut payload, &[("a.txt", "a\n")]);
    let mut box_run = box_command(case, &["box", "run", &self::payload(&payload)]);
    if let Some((name, value)) = with {
        box_run.env(name, value);
    }
    let running = start(box_run);
    let pid = spawned_pid(&running.wait_for_stdout("\n"));
    let child = StoppableProcess::open(pid)
        .expect("the child is opened")
        .expect("the child runs: it waits for a go file no one has written");

    let_go(&proceed);
    let outcome = running.finish();
    let ended = Escapee::exit_code_once_ended(&child);
    // The child is let go before any assertion, so a failing case leaves no helper waiting.
    let_go(&go);
    (outcome, ended)
}

#[test]
fn box_run_ends_what_the_command_left_in_its_job() {
    let case = Case::new("box_run_ends_leftovers");

    let (outcome, ended) = run_box_leaving_a_child(&case, None);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(ended, Some(124), "the box ended the child at the run's end: {outcome:?}");
    assert!(!outcome.stderr.contains("cannot end the run's job"), "{outcome:?}");
    assert!(
        !outcome.stderr.contains("still held open by a leftover process"),
        "the child is ended before the output drains, so it holds nothing open: {outcome:?}"
    );
}

#[test]
fn box_run_that_cannot_resolve_its_state_root_ends_the_whole_job() {
    let case = Case::new("box_run_unresolved_root");

    let (outcome, ended) = run_box_leaving_a_child(&case, Some(("LOCALAPPDATA", "")));

    assert_eq!(outcome.code, Some(0), "the command's status stands: {outcome:?}");
    assert_eq!(ended, Some(124), "the whole job was terminated: {outcome:?}");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == "gate: cannot end the run's job: the box's state root cannot be resolved; ending the whole job"),
        "{outcome:?}"
    );
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
        "gate: a .cmd or .bat command cannot take a word holding a line break or a %NAME% cmd.exe would expand: %PATH%\n"
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

/// Whether `line` is the box's line for a command it could not give a UTF-8 console, whatever the reason.
fn is_console_fallback(line: &str) -> bool {
    line.strip_prefix("gate: remote could not give the command a UTF-8 console (")
        .and_then(|rest| rest.strip_suffix("); running it on the box's default code page"))
        .is_some_and(|reason| !reason.is_empty())
}

/// The fake-tool `mark` file at `path`, empty when no run marked it.
fn marks(path: &Path) -> String {
    let text = fs::read_to_string(path);
    if matches!(&text, Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
        return String::new();
    }
    text.expect("the mark file can be read")
}

#[test]
fn box_run_gives_the_command_a_utf8_console() {
    let case = Case::new("box_run_utf8_console");
    let mut payload = box_payload(&case, "k", &["cmd", "/d", "/c", "chcp"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "Active code page: 65001\n", "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("{}\n", started_line(&payload)),
        "the marker, no fallback: {outcome:?}"
    );

    let mut bytes = renamed(&payload, 1);
    bytes["Command"] = json!([FAKE_TOOL, "bytes", "c3a9e282ac0a"]);
    let outcome = run_box(&case, &bytes);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.raw_stdout, "é€\n".as_bytes(), "non-ASCII bytes arrive intact: {outcome:?}");
    assert_eq!(outcome.stderr, format!("{}\n", started_line(&bytes)), "{outcome:?}");
}

/// The lines `fake-tool token` prints when `gate box run` starts it, in a case of its own named `name`: the run passes
/// and the box says nothing but its start marker, so the command was given the restricted token rather than the box's.
fn the_commands_token(name: &str) -> Vec<String> {
    let case = Case::new(name);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "token"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("{}\n", started_line(&payload)),
        "the marker, no line saying the command keeps the box's token: {outcome:?}"
    );
    outcome.stdout.lines().map(str::to_owned).collect()
}

/// The `dacl` line of a command whose default DACL grants `GENERIC_ALL` (0x10000000, stored as given) to this suite's
/// user and to SYSTEM alone, in that order: the restricted token's, which no token Windows gives a process has.
fn restricted_dacl_line() -> String {
    let user = gate_win32::token_facts().expect("this process's token can be read").user;
    format!("dacl {user}:0x10000000 S-1-5-18:0x10000000")
}

#[test]
fn box_run_starts_the_command_non_elevated_owning_its_objects() {
    let lines = the_commands_token("box_run_token");

    assert!(lines.iter().any(|line| line == "not-elevated"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "owner user"), "{lines:?}");
    let dacl = restricted_dacl_line();
    assert!(lines.contains(&dacl), "the restricted token's default DACL, {dacl:?}: {lines:?}");
}

#[test]
fn box_run_falls_back_to_its_own_token_when_the_restricted_start_is_refused() {
    let case = Case::new("box_run_token_refused");
    let count = case.folder().join("count");
    let count_word = shown(&count);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &count_word, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = finish(box_command(
        &case,
        &["box", "--test-box-token", "refused", "run", &self::payload(&payload)],
    ));

    assert_eq!(outcome.code, Some(0), "the refusal never fails the run: {outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    let fallback = format!(
        "gate: the command runs with the builder session's own token: {}",
        gate_win32::win32_message(5)
    );
    assert_eq!(
        outcome.stderr,
        format!("{}\n{fallback}\n", started_line(&payload)),
        "the marker, then the line saying the command keeps the box's token: {outcome:?}"
    );
    assert_eq!(marks(&count), "run\n", "the command ran once");
}

#[test]
#[ignore = "needs an elevated session: run on a builder"]
fn box_run_starts_an_elevated_session_s_command_non_elevated() {
    let lines = the_commands_token("box_run_token_elevated");

    assert!(lines.iter().any(|line| line == "not-elevated"), "{lines:?}");
    assert!(lines.iter().any(|line| line == "owner user"), "{lines:?}");
    let integrity = lines
        .iter()
        .find_map(|line| line.strip_prefix("integrity 0x"))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok());
    assert!(integrity.is_some_and(|rid| rid <= 0x2000), "medium integrity or lower: {lines:?}");
    let dacl = restricted_dacl_line();
    assert!(lines.contains(&dacl), "the restricted token's default DACL, {dacl:?}: {lines:?}");
    assert!(
        lines.iter().any(|line| line == "network absent" || line == "network deny-only"),
        "NETWORK absent or deny-only, never enabled: {lines:?}"
    );
}

#[test]
fn box_run_gives_the_command_safe_directory_for_git() {
    let case = Case::new("box_run_safe_directory");
    let mut payload = box_payload(
        &case,
        "k",
        &[
            FAKE_TOOL,
            "env",
            "GIT_CONFIG_COUNT",
            "env",
            "GIT_CONFIG_KEY_0",
            "env",
            "GIT_CONFIG_VALUE_0",
        ],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    // The box inherits no config entries, whatever the environment this suite runs in holds.
    let mut command = box_command(&case, &["box", "run", &self::payload(&payload)]);
    command.env_remove("GIT_CONFIG_COUNT");

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "1\nsafe.directory\n*\n", "{outcome:?}");
}

#[test]
fn box_run_appends_safe_directory_after_the_callers_git_config_entries() {
    let case = Case::new("box_run_safe_directory_appended");
    let mut payload = box_payload(
        &case,
        "k",
        &[
            FAKE_TOOL,
            "env",
            "GIT_CONFIG_COUNT",
            "env",
            "GIT_CONFIG_KEY_2",
            "env",
            "GIT_CONFIG_VALUE_2",
        ],
    );
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_env(&case, &payload, "GIT_CONFIG_COUNT", "2");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "3\nsafe.directory\n*\n", "the entry takes the next index: {outcome:?}");
}

/// The console's donor sits in the command's job until the wait for the command is over, while a gate the run's end
/// spares may still use its console: the run's end leaves the donor running, and the box ends it once the wait is
/// over, with status 0, where the run's end would have given it 124.
#[test]
fn box_run_end_spares_the_console_donor() {
    let case = Case::new("box_run_spares_donor");
    let proceed = case.folder().join("proceed");
    let proceed_word = shown(&proceed);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ready", "wait", &proceed_word, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let running = start_box(&case, &payload);
    running.wait_for_stdout("ready\n");
    let donor = donor_of(field(&payload, "Job"));

    let_go(&proceed);
    let outcome = running.finish();
    let ended = Escapee::exit_code_once_ended(&donor);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(ended, Some(0), "the box ended its donor itself, the run's end did not: {outcome:?}");
}

/// The console's donor of the box whose command runs in the job `job`, opened: the one `cmd.exe` in that job.
fn donor_of(job: &str) -> StoppableProcess {
    let members = OpenedJob::open_to_list(job)
        .expect("the job can be opened")
        .expect("the job exists while its command waits")
        .process_ids()
        .expect("the job's processes are listed");
    let donors: Vec<u32> = Win32ProcessTable
        .snapshot_without_command_lines()
        .into_iter()
        .filter(|process| members.contains(&process.pid) && process.name.eq_ignore_ascii_case("cmd.exe"))
        .map(|process| process.pid)
        .collect();
    assert_eq!(donors.len(), 1, "one donor in the job {members:?}: {donors:?}");
    StoppableProcess::open(*donors.first().expect("checked above"))
        .expect("the donor is looked for")
        .expect("the donor runs while the command waits")
}

#[test]
fn box_run_falls_back_before_the_start_and_runs_the_command() {
    let case = Case::new("box_run_console_fails_before");
    let count = case.folder().join("count");
    let count_word = shown(&count);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &count_word, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = finish(box_command(
        &case,
        &["box", "--test-box-console", "fail-before", "run", &self::payload(&payload)],
    ));

    assert_eq!(outcome.code, Some(0), "the fallback never fails the run: {outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let marker = started_line(&payload);
    assert!(
        matches!(said[..], [first, line] if first == marker && is_console_fallback(line)),
        "the marker, then one fallback line: {outcome:?}"
    );
    assert_eq!(marks(&count), "run\n", "the command ran once");
}

#[test]
fn box_run_of_a_command_that_leaves_the_console_runs_once() {
    let case = Case::new("box_run_leaves_console");
    let count = case.folder().join("count");
    let count_word = shown(&count);
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "free-console", "mark", &count_word]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(marks(&count), "run\n", "the command ran once");
    assert!(!outcome.stderr.lines().any(is_console_fallback), "no fallback: {outcome:?}");
}

#[test]
fn box_stop_before_the_launch_stops_the_job() {
    // The box parks with the console's donor already in the job.
    stop_before_the_launch("box_stop_before_launch", &[]);
}

#[test]
fn box_stop_before_the_launch_without_a_donor_stops_the_job() {
    stop_before_the_launch("box_stop_before_launch_no_donor", &["--test-box-console", "fail-before"]);
}

/// A run given `seams` and parked just before its launch, stopped meanwhile, then let go: it fails with the stop line
/// and no console fallback line, its command never ran, and its job holds no process.
fn stop_before_the_launch(name: &str, seams: &[&str]) {
    let case = Case::new(name);
    let park = case.folder().join("park");
    let parked = case.folder().join("park.parked");
    let count = case.folder().join("count");
    let (park_word, count_word) = (shown(&park), shown(&count));
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &count_word]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let job = field(&payload, "Job");
    let payload_word = self::payload(&payload);
    let mut args = vec!["box"];
    args.extend_from_slice(seams);
    args.extend_from_slice(&["--test-box-park-launch", &park_word, "run", &payload_word]);
    let running = start(box_command(&case, &args));
    wait_until("the box parked before the launch", || parked.exists());

    let stopped = gate_box(&case, "stop", &payload_word, None);
    let_go(&park);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop before the launch fails the run: {outcome:?}");
    let said = stopped_line_for(job);
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert!(
        !outcome.stderr.lines().any(is_console_fallback),
        "no fallback line before the stop line: {outcome:?}"
    );
    if let Some(opened) = OpenedJob::open(job).expect("the job can be looked for") {
        let left = opened.process_ids().expect("the job's processes can be listed");
        assert!(left.is_empty(), "no process of the job remains: {left:?}");
    }
    assert!(!count.exists(), "the command never ran");
    wait_until_closed(Path::new(field(&payload, "Lock")));
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
    // A terminated process leaves the job's count, which the stop waits for, before its object is signalled.
    wait_until_gone(helper);
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

/// A `gate box inflight` payload word for the builder's root `root`.
fn inflight_payload(root: &Path) -> String {
    payload(&json!({ "Root": shown(root) }))
}

#[test]
fn inflight_names_a_work_folder_whose_lock_is_held() {
    let case = Case::new("inflight_names_a_work_folder_whose_lock_is_held");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let lock = work.join("k.lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)
        .expect("the work folder's lock can be held, as a run holds it");

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote in flight 1\ngate: remote in flight k\n",
        "the held lock is one run in flight, named by its key: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

#[test]
fn inflight_counts_none_for_free_locks_or_a_missing_root() {
    let case = Case::new("inflight_counts_none_for_free_locks_or_a_missing_root");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    fs::write(work.join("k.lock"), "").expect("a free lock can be written");
    fs::write(work.join("k.index"), "").expect("a file beside the lock can be written");
    let bare = case.folder().join("bare-root");
    fs::create_dir_all(&bare).expect("a root without a work folder can be made");
    let nowhere = case.folder().join("nowhere");

    for (what, at) in [
        ("a free lock", &root),
        ("a root without a work folder", &bare),
        ("a root that is not there", &nowhere),
    ] {
        let outcome = gate_box(&case, "inflight", &inflight_payload(at), None);

        assert_eq!(outcome.code, Some(0), "{what}: {outcome:?}");
        assert_eq!(outcome.stdout, "gate: remote in flight 0\n", "{what}: {outcome:?}");
        assert_eq!(outcome.stderr, "", "{what}: {outcome:?}");
    }
}

#[test]
fn inflight_leaves_a_free_lock_takeable() {
    let case = Case::new("inflight_leaves_a_free_lock_takeable");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let lock = work.join("k.lock");
    fs::write(&lock, "").expect("a free lock can be written");

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "gate: remote in flight 0\n", "{outcome:?}");
    let taken = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&lock)
        .expect("the probe closed its handle, so a run can take the free lock");
    drop(taken);
}

/// A `work` folder the box cannot list gives no count: no count line, the reason, and status 1. A plain file where
/// the work folder goes is such a case, and one the suite can make with no right it may not have.
#[test]
fn inflight_cannot_list_a_work_folder_and_exits_1() {
    let case = Case::new("inflight_cannot_list_a_work_folder");
    let root = case.folder().join("remote");
    fs::create_dir_all(&root).expect("the root can be made");
    let work = root.join("work");
    fs::write(&work, "a file where the work folder goes").expect("a file can block the work folder");

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    assert_eq!(outcome.code, Some(1), "a work folder it cannot list is no pass: {outcome:?}");
    assert_eq!(outcome.stdout, "", "no count line is said: {outcome:?}");
    let prefix = format!("gate: remote cannot list {}: ", shown(&work));
    let reason = outcome
        .stderr
        .trim_end()
        .strip_prefix(&prefix)
        .expect("the cannot-list line names the work folder");
    assert!(!reason.is_empty(), "the line names why it cannot list it: {outcome:?}");
}

/// The lock `name` of the work folder `work`, opened with no sharing at all, as a run holds it; held until the caller
/// drops it.
fn hold_lock(work: &Path, name: &str) -> fs::File {
    let lock = work.join(name);
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(&lock)
        .expect("the work folder's lock can be held, as a run holds it")
}

/// A lock the probe cannot open at all counts as held — a folder where a lock file goes is one — since a lock it could
/// not read is never counted free; the line says why.
#[test]
fn inflight_counts_a_lock_it_cannot_read_as_held() {
    let case = Case::new("inflight_counts_a_lock_it_cannot_read_as_held");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(work.join("d.lock")).expect("a folder named as a lock can be made");

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    assert_eq!(outcome.code, Some(0), "a lock it cannot read fails no verb: {outcome:?}");
    let said = outcome
        .stdout
        .strip_prefix("gate: remote in flight 1\ngate: remote in flight d (cannot read: ")
        .expect("the count, then the lock it cannot read");
    let reason = said.trim_end().strip_suffix(')').expect("the reason is closed");
    assert!(!reason.is_empty(), "the line names why it cannot read the lock: {outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

/// The keys come out sorted, and a lock named with an upper-case `.LOCK` is one of a run's locks too.
#[test]
fn inflight_counts_several_held_keys_in_order_and_an_upper_case_lock() {
    let case = Case::new("inflight_counts_several_held_keys_in_order_and_an_upper_case_lock");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    fs::write(work.join("c.index"), "").expect("a file that is no lock can be written");
    let held = [hold_lock(&work, "b.lock"), hold_lock(&work, "A.LOCK")];

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote in flight 2\ngate: remote in flight A\ngate: remote in flight b\n",
        "the keys come out sorted, and `.LOCK` is a lock too: {outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
}

/// A key is printed with the marks `escaped()` changes escaped, so no lock's name can reorder or break the line it is
/// printed in.
#[test]
fn inflight_prints_a_key_it_escapes() {
    let case = Case::new("inflight_prints_a_key_it_escapes");
    let root = case.folder().join("remote");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let held = hold_lock(&work, "a\u{202e}b.lock");

    let outcome = gate_box(&case, "inflight", &inflight_payload(&root), None);

    drop(held);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "gate: remote in flight 1\ngate: remote in flight a\\u{202e}b\n",
        "the reordering mark is escaped: {outcome:?}"
    );
    assert!(!outcome.stdout.contains('\u{202e}'), "no mark of the key reaches the line: {outcome:?}");
}

/// A payload the probe cannot use is refused as every other box verb refuses one: the usage line's own `gate: ` line
/// and status 2, with nothing said on stdout.
#[test]
fn inflight_refuses_a_payload_without_a_usable_root() {
    let case = Case::new("inflight_refuses_a_payload_without_a_usable_root");
    for (what, given, refused) in [
        ("no Root", json!({}), "gate: box payload lacks Root\n"),
        (
            "a relative Root",
            json!({ "Root": "remote" }),
            "gate: box payload Root is not an absolute path on a drive: \"remote\"\n",
        ),
    ] {
        let outcome = gate_box(&case, "inflight", &payload(&given), None);

        assert_eq!(outcome.code, Some(2), "{what}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{what}: nothing is said on stdout");
        assert_eq!(outcome.stderr, refused, "{what}: the refusal line");
    }
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

#[test]
fn box_run_echoes_a_subfolder_with_control_characters_escaped() {
    let case = Case::new("box_run_subfolder_escaped");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "out", "ran"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    // One subfolder climbs out of the work folder and one is merely missing in the snapshot, so both refusal lines
    // that show the subfolder are checked; each holds ESC and CR/LF.
    let subs = [
        ("..\\escape\u{1b}\r\n", r"..\escape\u{1b}\r\n", "is not a folder inside the work folder"),
        ("missing\u{1b}\r\n", r"missing\u{1b}\r\n", "does not exist in the snapshot"),
    ];
    for (sub, shown, refusal) in subs {
        payload["Subfolder"] = json!(sub);

        let outcome = run_box(&case, &payload);

        assert_eq!(outcome.code, Some(2), "{sub:?}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{sub:?}");
        assert_eq!(
            outcome.stderr,
            format!("gate: remote subfolder {shown} {refusal}\n"),
            "{sub:?}: the refusal shows the subfolder with its control characters escaped"
        );
        assert!(
            !outcome.stderr.contains(['\u{1b}', '\r']),
            "{sub:?}: no raw control character of the subfolder reaches the client"
        );
    }
}

#[test]
fn box_run_echoes_a_command_word_with_control_characters_escaped() {
    let case = Case::new("box_run_command_word_escaped");
    let mut payload = box_payload(&case, "k", &[]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let work = Path::new(field(&payload, "Work"));
    // A bare word is looked for on PATH and found nowhere, so the refusal shows only the word; a relative word with a
    // separator is taken as written and read against the work folder, so the refusal shows the resolved path too. Both
    // hold ESC and CR/LF.
    let resolved = work
        .join("sub")
        .join("no-such-\u{1b}\r\n.exe")
        .to_string_lossy()
        .replace('\u{1b}', r"\u{1b}")
        .replace('\r', r"\r")
        .replace('\n', r"\n");
    let cases = [
        // A word the box cannot find is refused before the start marker; one whose launch fails, after it.
        (
            "gate-suite-no-such-\u{1b}\r\n-program".to_owned(),
            r"gate-suite-no-such-\u{1b}\r\n-program".to_owned(),
            String::new(),
            false,
        ),
        (
            "sub\\no-such-\u{1b}\r\n.exe".to_owned(),
            r"sub\no-such-\u{1b}\r\n.exe".to_owned(),
            format!(" ({resolved})"),
            true,
        ),
    ];
    for (word, shown, shown_resolved, marked) in cases {
        payload["Command"] = json!([word]);

        let outcome = run_box(&case, &payload);

        assert_eq!(outcome.code, Some(127), "{word:?}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{word:?}");
        let marker = format!("{}\n", started_line(&payload));
        let said = if marked {
            outcome.stderr.strip_prefix(&marker).expect("the marker comes first")
        } else {
            outcome.stderr.as_str()
        };
        let line = said.strip_suffix('\n').expect("the refusal is one line");
        let prefix = format!("gate: could not start {shown}{shown_resolved}: ");
        assert!(
            line.starts_with(&prefix),
            "{word:?}: the refusal shows the word and the resolved path with their control characters escaped: {line:?}"
        );
        assert!(
            !line.contains(['\u{1b}', '\r']),
            "{word:?}: no raw control character of the word reaches the client"
        );
    }
}

#[test]
fn box_run_restores_the_tools_once_for_a_manifest_and_records_its_hash() {
    let case = Case::new("box_run_tools_restore");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stdout.contains("restore: out"),
        "the restore's stdout passes through: {outcome:?}"
    );
    assert!(
        outcome.stderr.contains("restore: note"),
        "the restore's stderr passes through: {outcome:?}"
    );
    assert!(marker.exists(), "the command ran after the restore: {outcome:?}");
    let work = field(&payload, "Work");
    let said = counter_lines(&case);
    assert_eq!(said.len(), 1, "the restore ran once: {outcome:?}");
    assert!(
        said[0].eq_ignore_ascii_case(work),
        "the restore ran in the place folder {work}: {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(format!("{work}.tools")).expect("the record is written"),
        TOOLS_MANIFEST_HASH,
        "the record holds the walk's hash"
    );
}

#[test]
fn box_run_does_not_restore_again_for_an_unchanged_manifest() {
    let case = Case::new("box_run_tools_unchanged");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);
    let first = run_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(counter_lines(&case).len(), 1, "{first:?}");

    let second = run_box_with_path(&case, &renamed(&payload, 1), Some(&path_for_run(&[stub])));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(
        !second.stdout.contains("restore: out"),
        "the unchanged manifest restores nothing: {second:?}"
    );
    assert_eq!(counter_lines(&case).len(), 1, "the stub ran once, in the first run");
    assert_eq!(
        fs::read_to_string(&marker).expect("the marker is read").lines().count(),
        2,
        "the command ran in both runs"
    );
}

#[test]
fn box_run_restores_again_when_a_manifest_changes() {
    let case = Case::new("box_run_tools_changed");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);
    assert_eq!(
        run_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub)))).code,
        Some(0)
    );

    let changed = push_snapshot(&case, &payload, &[(TOOLS_MANIFEST_PATH, "{\n  \"version\": 2\n}\n")]);
    payload["Snapshot"] = json!(changed);
    let second = run_box_with_path(&case, &renamed(&payload, 1), Some(&path_for_run(&[stub])));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(second.stdout.contains("restore: out"), "the changed manifest restores again: {second:?}");
    assert_eq!(counter_lines(&case).len(), 2, "the stub ran in both runs");
}

#[test]
fn box_run_a_failed_restore_refuses_with_75_and_runs_no_command() {
    let case = Case::new("box_run_tools_failed_restore");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let passing = stub_dotnet(&case, 0);
    let first = run_box_with_path(&case, &payload, Some(&path_for_run(&[passing])));
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = format!("{}.tools", field(&payload, "Work"));
    let written = fs::read(&record).expect("the record is written");

    let changed = push_snapshot(&case, &payload, &[(TOOLS_MANIFEST_PATH, "{\n  \"version\": 2\n}\n")]);
    payload["Snapshot"] = json!(changed);
    let failing = stub_dotnet(&case, 1);
    let outcome = run_box_with_path(&case, &renamed(&payload, 1), Some(&path_for_run(&[failing])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let failed = said.iter().position(|line| *line == "gate: remote tool restore failed (status 1)");
    let refused = said
        .iter()
        .position(|line| *line == "gate: remote work folder k could not restore its dotnet tools");
    assert!(
        matches!((failed, refused), (Some(failed), Some(refused)) if failed < refused),
        "the status line comes before the refusal: {outcome:?}"
    );
    assert_eq!(counter_lines(&case).len(), 2, "the failing stub ran: {outcome:?}");
    assert_eq!(
        fs::read_to_string(&marker).expect("the marker is read").lines().count(),
        1,
        "only the first run's command ran"
    );
    assert_eq!(fs::read(&record).expect("the record is read"), written, "the record stands as it was");
    assert!(!has_ref(&payload, "refs/gate-back/k"), "the refused run made no back commit");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_failed_restore_reports_the_stub_s_status() {
    let case = Case::new("box_run_tools_failed_status");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 7);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(
        lines(&outcome.stderr).contains(&"gate: remote tool restore failed (status 7)"),
        "the status the restore failed with is the stub's own: {outcome:?}"
    );
    assert!(!marker.exists(), "the command never ran");
    assert!(!Path::new(&format!("{}.tools", field(&payload, "Work"))).exists(), "no record is written");
}

/// The shape of the line the box says once a restore it ran has ended: its elapsed seconds, one decimal.
const RESTORE_TOOK: &str = r"^gate: dotnet tool restore took \d+\.\d s$";

/// The positions of the lines of `stderr` that say how long a restore took.
fn took_positions(stderr: &str) -> Vec<usize> {
    let took = Regex::new(RESTORE_TOOK).expect("the pattern compiles");
    lines(stderr)
        .iter()
        .enumerate()
        .filter(|(_, line)| took.is_match(line))
        .map(|(position, _)| position)
        .collect()
}

#[test]
fn box_run_says_how_long_a_passing_restore_took() {
    let case = Case::new("box_run_tools_took_passing");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(took_positions(&outcome.stderr).len(), 1, "one elapsed line: {outcome:?}");
}

#[test]
fn box_run_says_how_long_a_failing_restore_took_before_its_failure_lines() {
    let case = Case::new("box_run_tools_took_failing");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 7);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let took = took_positions(&outcome.stderr);
    let failed = said.iter().position(|line| *line == "gate: remote tool restore failed (status 7)");
    let tag = refused_line(&payload, "tools");
    let tagged = said.iter().position(|line| *line == tag);
    let refused = said
        .iter()
        .position(|line| *line == "gate: remote work folder k could not restore its dotnet tools");
    assert!(
        matches!(
            (took.as_slice(), failed, tagged, refused),
            ([took], Some(failed), Some(tagged), Some(refused)) if *took < failed && failed < tagged && tagged < refused
        ),
        "one elapsed line, then the status line, the tag and the tools line: {outcome:?}"
    );
}

#[test]
fn box_run_a_work_folder_with_no_manifest_runs_no_restore() {
    let case = Case::new("box_run_tools_no_manifest");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !counter(&case).exists(),
        "a work folder with no manifest never looks for dotnet: {outcome:?}"
    );
    assert!(
        !outcome.stderr.contains("gate: dotnet tool restore took"),
        "a run that restores nothing says no elapsed line: {outcome:?}"
    );
    assert!(marker.exists(), "the command still ran: {outcome:?}");
    assert!(!Path::new(&format!("{}.tools", field(&payload, "Work"))).exists(), "no record is written");
}

#[test]
fn box_run_finds_a_root_dotnet_tools_json_with_no_config_folder() {
    let case = Case::new("box_run_tools_root_manifest");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("dotnet-tools.json", TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(counter_lines(&case).len(), 1, "{outcome:?}");
    assert!(outcome.stdout.contains("restore: out"), "{outcome:?}");
}

#[test]
fn box_run_prefers_config_dotnet_tools_json_over_the_folders_other_manifest() {
    let case = Case::new("box_run_tools_config_preferred");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(
        &case,
        &mut payload,
        &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST), ("dotnet-tools.json", "{\"loser\": 1}\n")],
    );
    let stub = stub_dotnet(&case, 0);
    let first = run_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(counter_lines(&case).len(), 1, "{first:?}");

    let changed = push_snapshot(&case, &payload, &[("dotnet-tools.json", "{\"loser\": 2}\n")]);
    payload["Snapshot"] = json!(changed);
    let second = run_box_with_path(&case, &renamed(&payload, 1), Some(&path_for_run(&[stub])));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(
        !second.stdout.contains("restore: out"),
        "the folder's other manifest is not hashed: {second:?}"
    );
    assert_eq!(counter_lines(&case).len(), 1, "the stub ran once");
}

#[test]
fn box_run_restores_from_a_manifest_above_the_place_folder() {
    let case = Case::new("box_run_tools_walk");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    payload["Subfolder"] = json!(r"src\app");
    snapshot(
        &case,
        &mut payload,
        &[("src/.config/dotnet-tools.json", TOOLS_MANIFEST), ("src/app/a.txt", "a\n")],
    );
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = counter_lines(&case);
    assert_eq!(said.len(), 1, "{outcome:?}");
    let place = Path::new(field(&payload, "Work")).join("src").join("app");
    assert!(
        said[0].eq_ignore_ascii_case(&place.to_string_lossy()),
        "the restore ran in the place folder {place:?}: {outcome:?}"
    );
}

#[test]
fn box_run_of_a_place_with_no_manifest_above_it_restores_nothing() {
    let case = Case::new("box_run_tools_walk_empty_place");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("src/.config/dotnet-tools.json", TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        !counter(&case).exists(),
        "the walk starts at the work folder, above the manifest: {outcome:?}"
    );
}

#[test]
fn box_run_restores_again_when_a_manifest_moves() {
    let case = Case::new("box_run_tools_manifest_moved");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    payload["Subfolder"] = json!("src");
    snapshot(
        &case,
        &mut payload,
        &[(".config/dotnet-tools.json", TOOLS_MANIFEST), ("src/a.txt", "a\n")],
    );
    let stub = stub_dotnet(&case, 0);
    let first = run_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(counter_lines(&case).len(), 1, "{first:?}");

    // The same bytes at another path: the manifest's path is part of the hash.
    let changed = push_snapshot(&case, &payload, &[("src/dotnet-tools.json", TOOLS_MANIFEST)]);
    payload["Snapshot"] = json!(changed);
    let second = run_box_with_path(&case, &renamed(&payload, 1), Some(&path_for_run(&[stub])));

    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(second.stdout.contains("restore: out"), "a moved manifest restores again: {second:?}");
    assert_eq!(counter_lines(&case).len(), 2, "the stub ran in both runs");
}

#[test]
fn box_run_never_reads_a_manifest_above_the_work_folder() {
    let case = Case::new("box_run_tools_above_work");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let beside = Path::new(field(&payload, "Work"))
        .parent()
        .expect("the work folder has a parent")
        .join("dotnet-tools.json");
    fs::write(&beside, TOOLS_MANIFEST).expect("the manifest beside the work folder can be written");
    let stub = stub_dotnet(&case, 0);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!counter(&case).exists(), "the walk stops at the work folder: {outcome:?}");
}

#[test]
fn box_run_without_dotnet_refuses_the_run_with_75() {
    let case = Case::new("box_run_tools_no_dotnet");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    // The win32 error `ERROR_FILE_NOT_FOUND`, as `locate` reports a word it does not find.
    let cannot_start = format!("gate: remote could not start the dotnet tool restore: {}", gate_win32::win32_message(2));
    assert_eq!(
        lines(&outcome.stderr),
        [
            cannot_start.as_str(),
            "gate: remote tool restore failed (status 1)",
            refused_line(&payload, "tools").as_str(),
            "gate: remote work folder k could not restore its dotnet tools",
        ],
        "{outcome:?}"
    );
    assert!(!marker.exists(), "the command never ran");
}

#[test]
fn a_tools_refusal_is_tagged_and_has_no_marker() {
    let case = Case::new("box_run_tools_tagged");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 7);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let tag = refused_line(&payload, "tools");
    let tagged = said.iter().position(|line| *line == tag);
    let refused = said
        .iter()
        .position(|line| *line == "gate: remote work folder k could not restore its dotnet tools");
    assert!(
        matches!((tagged, refused), (Some(tagged), Some(refused)) if tagged + 1 == refused),
        "the tag comes just before the tools line: {outcome:?}"
    );
    assert_eq!(said.iter().filter(|line| **line == tag).count(), 1, "one tag: {outcome:?}");
    assert!(
        !said.iter().any(|line| line.starts_with("gate-remote-started: ")),
        "no marker: {outcome:?}"
    );
    assert!(!marker.exists(), "the command never ran");
}

/// The line the box says for a stop that came before the command of the job `name` started.
fn stopped_line_for(name: &str) -> String {
    format!("gate: the gate failed while running the command: the job {name} was stopped before the command started")
}

/// The script a stub dotnet runs to hold the restore: a helper that waits for `never` is started and announced
/// (`spawned <pid>`), then the stub waits for `never` too, so the restore runs until something ends it.
fn held_restore(never: &Path) -> String {
    let never_word = shown(never);
    format!("@\"{FAKE_TOOL}\" spawn-wait \"{never_word}\"\r\n@\"{FAKE_TOOL}\" wait \"{never_word}\"\r\n")
}

#[test]
fn box_run_a_stop_during_the_restore_ends_it_and_runs_no_command() {
    let case = Case::new("box_run_tools_stop_during_restore");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet_running(&case, 0, &held_restore(&case.folder().join("never")));
    let running = start_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));
    let helper = spawned_pid(&running.wait_for_stdout("\n"));

    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    // The stop opens the restore's job before it sets the run's signal, so the box, seeing the signal, cannot close the
    // job before the stop finds it: the stop ends the restore itself and says so.
    assert_eq!(
        stopped.stdout,
        format!("gate: remote stopped job {}-tools\n", field(&payload, "Job")),
        "{stopped:?}"
    );
    assert_eq!(stopped.stderr, "", "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop during the restore fails the run: {outcome:?}");
    // The line names the job that was stopped: the restore's own, named after the run's.
    let said = stopped_line_for(&format!("{}-tools", field(&payload, "Job")));
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert!(!marker.exists(), "the command never ran");
    assert!(!Path::new(&format!("{}.tools", field(&payload, "Work"))).exists(), "no record is written");
    wait_until_gone(helper);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_killed_box_takes_its_restore_with_it() {
    let case = Case::new("box_run_tools_killed_box");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet_running(&case, 0, &held_restore(&case.folder().join("never")));
    let running = start_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));
    let helper = spawned_pid(&running.wait_for_stdout("\n"));
    let restore_job = format!("{}-tools", field(&payload, "Job"));
    // Listed through a handle closed again at once, so the box's is once more the job's only one.
    let restore = OpenedJob::open(&restore_job)
        .expect("the restore's job can be looked for")
        .expect("the restore runs in its job")
        .process_ids()
        .expect("the restore's processes can be listed");
    assert!(restore.contains(&helper), "the helper is in the restore's job: {restore:?}");

    // As a lost connection's cut kills the box before any stop reaches it.
    let killed = running.kill();

    // The box held the restore's job's last handle, so its death ended every process in that job.
    for pid in restore {
        wait_until_gone(pid);
    }
    assert!(!marker.exists(), "the command never ran: {killed:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_ends_a_process_the_restore_left_running() {
    let case = Case::new("box_run_tools_restore_leftover");
    let marker = case.folder().join("marker");
    let spawned = case.folder().join("spawned.txt");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    // The stub starts a helper that waits for a file no one makes, its standard output a file of the case's and its
    // standard error NUL, not the restore's pipes, and exits 0 without waiting for it.
    let script = format!(
        "@\"{FAKE_TOOL}\" spawn-wait \"{}\" >\"{}\" 2>nul\r\n",
        shown(&case.folder().join("never")),
        shown(&spawned)
    );
    let stub = stub_dotnet_running(&case, 0, &script);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(marker.exists(), "the command ran after the restore: {outcome:?}");
    let helper = spawned_pid(&fs::read_to_string(&spawned).expect("the helper's pid is written"));
    wait_until_gone(helper);
}

#[test]
fn box_run_never_truncates_a_file_hard_linked_as_the_record() {
    let case = Case::new("box_run_tools_record_hard_link");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);
    let record = format!("{}.tools", field(&payload, "Work"));
    let other = case.folder().join("other.txt");
    fs::write(&other, "keep these bytes\n").expect("the other file can be written");
    fs::hard_link(&other, &record).expect("the record can be made a second name of the other file");

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(1), "a record that cannot be written fails the box: {outcome:?}");
    let refused = format!("gate: remote could not write {record}: ");
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(&refused)),
        "{refused:?} in {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(&other).expect("the other file is read"),
        "keep these bytes\n",
        "the file the record names too keeps its bytes"
    );
    assert!(!marker.exists(), "the command never ran");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_stop_before_the_restore_runs_no_restore() {
    let case = Case::new("box_run_tools_stop_before_restore");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);
    let parked = case.folder().join("parked");
    let go = case.folder().join("go");
    stub_git(&stub, &parked, &go);
    let running = start_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));
    wait_until("the box parked in its first git step, before the restore", || parked.exists());

    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop before the restore fails the run: {outcome:?}");
    let said = stopped_line_for(field(&payload, "Job"));
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert!(!counter(&case).exists(), "the restore never ran: {outcome:?}");
    assert!(!marker.exists(), "the command never ran");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_record_that_is_a_folder_fails_with_status_1() {
    let case = Case::new("box_run_tools_record_folder");
    let marker = case.folder().join("marker");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&case, &mut payload, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let stub = stub_dotnet(&case, 0);
    let record = format!("{}.tools", field(&payload, "Work"));
    fs::create_dir_all(&record).expect("a folder can be put where the record goes");

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(std::slice::from_ref(&stub))));

    assert_eq!(outcome.code, Some(1), "a record that cannot be written fails the box: {outcome:?}");
    let refused = format!("gate: remote could not write {record}: ");
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(&refused)),
        "{refused:?} in {outcome:?}"
    );
    assert!(outcome.stdout.contains("restore: out"), "the restore ran before the write: {outcome:?}");
    assert!(!marker.exists(), "the command never ran");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

/// A command the box's ceiling cases run that never ends on its own within any case's time.
const ENDLESS: &[&str] = &[FAKE_TOOL, "err", "started", "sleep", "600000"];

/// `gate box run` of `payload` with `seams` before the verb, to its end.
fn run_box_with_seams(case: &Case, payload: &Value, seams: &[&str]) -> Outcome {
    let word = self::payload(payload);
    let mut args = vec!["box"];
    args.extend_from_slice(seams);
    args.extend(["run", word.as_str()]);
    finish(box_command(case, &args))
}

/// The lines of `outcome`'s stderr that start with `prefix`.
fn lines_starting<'a>(outcome: &'a Outcome, prefix: &str) -> Vec<&'a str> {
    outcome.stderr.lines().filter(|line| line.starts_with(prefix)).collect()
}

/// The seconds of the one `gate-remote-adjusted:` line in `outcome`'s stderr, which names the nonce of `payload` and
/// has three decimals.
fn adjusted_seconds(outcome: &Outcome, payload: &Value) -> f64 {
    let adjusted = Regex::new(&format!(r"^gate-remote-adjusted: {} (\d+\.\d{{3}})$", field(payload, "Nonce"))).expect("the pattern compiles");
    let lines = lines_starting(outcome, "gate-remote-adjusted:");
    assert_eq!(lines.len(), 1, "one adjusted line: {outcome:?}");
    assert!(adjusted.is_match(lines[0]), "this run's adjusted line, of three decimals: {outcome:?}");
    let captures = adjusted.captures(lines[0]).expect("it matched");
    captures[1].parse().expect("the seconds are a number")
}

#[test]
fn box_run_past_its_load_adjusted_ceiling_is_killed_with_the_ruled_line_and_124() {
    let case = Case::new("box_run_ceiling_kill");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "mark", "new.txt", "err", "started", "sleep", "600000"]);
    payload["Timeout"] = json!(1);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    // The whole machine free, so the clock runs with wall time however loaded the machine running the case is.
    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]);

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
    assert!(
        Path::new(field(&payload, "Work")).join("new.txt").exists(),
        "the killed command changed the tree"
    );
    assert!(
        !has_ref(&payload, "refs/gate-back/k"),
        "a killed command's tree gets no back commit: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_box_kill_of_a_payload_with_no_host_names_no_builder() {
    let case = Case::new("box_run_ceiling_no_host");
    let mut payload = box_payload(&case, "k", ENDLESS);
    payload["Timeout"] = json!(1);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let kill =
        Regex::new(r"^gate: TIMED OUT after 1 s of load-adjusted time \(wall \d+ s, machine free 100% on average\); killed .+ and its children$")
            .expect("the pattern compiles");
    assert!(
        outcome.stderr.lines().any(|line| kill.is_match(line)),
        "the kill line without a host: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("TIMED OUT on"), "{outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_loaded_box_lets_a_command_run_past_its_wall_ceiling_while_under_its_adjusted_one() {
    let case = Case::new("box_run_ceiling_loaded");
    // 3.5 s of wall time past a 3 s ceiling; at 5% free the clock counts a twentieth of it.
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "sleep", "3500", "out", "done", "exit", "0"]);
    payload["Timeout"] = json!(3);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "5"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "done\n", "{outcome:?}");
    assert!(lines_starting(&outcome, "gate-remote-killed:").is_empty(), "{outcome:?}");
    assert!(adjusted_seconds(&outcome, &payload) < 3.0, "under the ceiling: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_box_whose_sampler_fails_kills_only_at_the_backstop_and_says_no_adjusted_time() {
    let case = Case::new("box_run_ceiling_sampler_failed");
    let mut payload = box_payload(&case, "k", ENDLESS);
    payload["Timeout"] = json!(1);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-sampler-fail", "true"]);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    assert_eq!(
        lines_starting(&outcome, "gate: sampler failed:"),
        ["gate: sampler failed: --test-box-sampler-fail makes every sample fail (test seam only); watching by wall time only"],
        "said once: {outcome:?}"
    );
    assert!(
        lines_starting(&outcome, "gate: TIMED OUT").is_empty(),
        "no wall-time ceiling: {outcome:?}"
    );
    let backstop = Regex::new(concat!(
        r"^gate: BACKSTOP on builder: (\d+) s of wall time \(5 x 1 s ceiling, machine load unknown: the sampler failed\); ",
        r"killed .+ and its children$",
    ))
    .expect("the pattern compiles");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let at = said.iter().position(|line| backstop.is_match(line));
    assert!(at.is_some(), "the backstop line: {outcome:?}");
    let at = at.expect("it is there");
    let wall: u64 = backstop.captures(said[at]).expect("it matched")[1].parse().expect("whole seconds");
    assert!(wall >= 5, "five ceilings of wall time: {outcome:?}");
    assert_eq!(
        said.get(at + 1).copied(),
        Some(format!("gate-remote-killed: {} backstop", field(&payload, "Nonce")).as_str()),
        "{outcome:?}"
    );
    assert!(lines_starting(&outcome, "gate-remote-adjusted:").is_empty(), "{outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_payload_with_no_timeout_gives_the_command_no_ceiling_on_the_box() {
    let case = Case::new("box_run_ceiling_none");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "sleep", "1500", "out", "done", "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("{}\n", started_line(&payload)),
        "no kill and no adjusted line: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn a_box_run_that_ends_by_itself_says_its_load_adjusted_seconds() {
    let case = Case::new("box_run_ceiling_adjusted");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "err", "e1", "exit", "3"]);
    payload["Timeout"] = json!(600);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(3), "the command's own status: {outcome:?}");
    assert!(adjusted_seconds(&outcome, &payload) < 600.0, "{outcome:?}");
    assert!(lines_starting(&outcome, "gate-remote-killed:").is_empty(), "{outcome:?}");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let output = said.iter().position(|line| *line == "e1");
    let adjusted = said.iter().position(|line| line.starts_with("gate-remote-adjusted:"));
    assert!(
        output.is_some() && output < adjusted,
        "the adjusted line comes after the command's drained output: {outcome:?}"
    );
}

#[test]
fn a_box_kill_that_leaves_a_process_says_a_note_then_still_the_kill_line_and_tag_and_124() {
    let case = Case::new("box_run_ceiling_kill_leaves");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "err", "drained", "sleep", "600000"]);
    payload["Timeout"] = json!(1);
    payload["Host"] = json!("builder");
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let seams = [
        "--test-box-sample-ms",
        "20",
        "--test-box-free-percent",
        "100",
        "--test-box-kill-leaves",
        "1",
    ];
    let outcome = run_box_with_seams(&case, &payload, &seams);

    assert_eq!(outcome.code, Some(124), "a kill whatever its ending left: {outcome:?}");
    let note = Regex::new(r"^gate: note: the box's ceiling kill left 1 process\(es\) running in job \S+$").expect("the pattern compiles");
    let kill = Regex::new(r"^gate: TIMED OUT on builder after 1 s of load-adjusted time ").expect("the pattern compiles");
    let said: Vec<&str> = outcome.stderr.lines().collect();
    let drained = said.iter().position(|line| *line == "drained");
    let noted = said.iter().position(|line| note.is_match(line));
    let killed = said.iter().position(|line| kill.is_match(line));
    assert!(
        drained.is_some() && noted.is_some() && killed.is_some(),
        "the output, the note and the kill line: {outcome:?}"
    );
    assert!(
        drained < noted && noted < killed,
        "the drained output, then the note, then the kill line: {outcome:?}"
    );
    let at = killed.expect("it is there");
    assert_eq!(
        said.get(at + 1).copied(),
        Some(format!("gate-remote-killed: {} timed-out", field(&payload, "Nonce")).as_str()),
        "the kill tag right after the kill line: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

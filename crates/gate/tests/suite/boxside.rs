//! The hidden `gate box`, run as the builder runs it over ssh: no `--state-dir`, every path in the base64 JSON payload,
//! all of them under the case's folder. git must be on PATH; a case about dotnet sets the gate's PATH itself.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use regex::Regex;
use serde_json::{Value, json};

use crate::support::{Case, Outcome, finish};

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs <base64 JSON payload>\n";
/// The git variables a caller's environment may hold that point git at another repository or config; no case's gate or
/// git gets them.
const GIT_REDIRECTS: [&str; 4] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CONFIG_PARAMETERS"];

/// The payload word for `json`, as the remote client encodes it.
fn payload(json: &Value) -> String {
    STANDARD.encode(json.to_string())
}

/// The gate with `args` as they are, without the git variables the suite may have inherited.
fn box_command(case: &Case, args: &[&str]) -> Command {
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
        let outcome = finish(box_command(&case, args));

        assert_eq!(outcome.code, Some(2), "{args:?}");
        assert_eq!(outcome.stdout, "", "{args:?}");
        assert_eq!(outcome.stderr, BOX_USAGE, "{args:?}: the box usage alone");
    }

    let not_base64 = gate_box(&case, "preflight", "not-base64!", None);
    assert_eq!(not_base64.code, Some(2));
    assert_eq!(not_base64.stdout, "");
    assert!(not_base64.stderr.starts_with("gate: box payload is not base64 JSON: "), "{not_base64:?}");
    assert_eq!(lines(&not_base64.stderr).len(), 1, "{not_base64:?}");

    let not_an_object = gate_box(&case, "preflight", &payload(&json!(["Bare", "Work"])), None);
    assert_eq!(not_an_object.code, Some(2));
    assert_eq!(not_an_object.stdout, "");
    assert_eq!(not_an_object.stderr, "gate: box payload is not base64 JSON: the JSON is not an object\n");

    let lacking = gate_box(&case, "preflight", &payload(&json!({"Bare": folders.bare})), None);
    assert_eq!(lacking.code, Some(2));
    assert_eq!(lacking.stdout, "");
    assert_eq!(lacking.stderr, "gate: box payload lacks Work\n");
    assert!(!Path::new(&folders.bare).exists(), "nothing is made for a payload the box refuses");

    assert!(!case.run(&["--help"]).stdout.contains("box"), "gate box is hidden from the usage");
}

#[test]
fn box_run_stop_and_outputs_are_not_built_yet() {
    let case = Case::new("box_run_stop_and_outputs_are_not_built_yet");
    for verb in ["run", "stop", "outputs"] {
        let outcome = gate_box(&case, verb, &payload(&json!({"Key": "k"})), None);

        assert_eq!(outcome.code, Some(70), "{verb}");
        assert_eq!(outcome.stdout, "", "{verb}");
        assert_eq!(outcome.stderr, format!("gate: box {verb} is not built yet\n"));
    }
}

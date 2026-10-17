//! The hidden `gate box`, run as the builder runs it over ssh: no `--state-dir`, every path in the base64 JSON payload,
//! all of them under the case's folder. git must be on PATH; a case about dotnet sets the gate's PATH itself.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gate_win32::{EnvironmentBlock, Job, Launch, LaunchOutput, OpenedJob, StoppableProcess};
use jiff::{SignedDuration, Timestamp};
use proc_table::Win32ProcessTable;
use regex::Regex;
use serde_json::{Value, json};

use crate::support::{
    Case, Closed, Escapee, FAKE_TOOL, GIT_REDIRECTS, Go, Outcome, Running, allow_breakaway, box_payload, build_version, copy_tool, finish, junction,
    shown, start_with_closed, start_with_stdin, wait_until, wait_until_closed,
};

mod inputs;
mod outputs;
mod warm;

const BOX_USAGE: &str = "usage: gate box preflight|run|stop|boot|outputs|inflight|inputs --stdin (the base64 JSON request as stdin's first line)\n";

/// The request line for `json`, as the remote client encodes it: base64 of the JSON, without its newline.
fn payload(json: &Value) -> String {
    STANDARD.encode(json.to_string())
}

/// The heartbeat period every box call of the suite passes unless it says otherwise: long enough that no beat lands in
/// a case's output however slow a loaded machine makes the run.
const QUIET_HEARTBEAT_MS: &str = "2147483647";

/// A `gate box … --stdin` not yet started and the request it reads as stdin's first line, which each start writes. It
/// derefs to its command, so a case sets the box's environment as on any command.
struct BoxCall {
    command: Command,
    request: String,
}

impl BoxCall {
    /// Starts the box with its request line written and its stdin left open, for the case to write more or to drop.
    fn start_with_stdin(self) -> (Running, ChildStdin) {
        let (running, mut stdin) = start_with_stdin(self.command);
        send_request(&mut stdin, &self.request);
        (running, stdin)
    }

    /// Starts the box with its request line written and its stdin then closed.
    fn start(self) -> Running {
        let (running, stdin) = self.start_with_stdin();
        drop(stdin);
        running
    }

    /// Runs the box to its end, its request line written and its stdin then closed.
    fn finish(self) -> Outcome {
        self.start().finish()
    }

    /// Starts the box as [`start_with_closed`] does, its request line written and its stdin then closed.
    fn start_with_closed(self, closed: Closed) -> Running {
        let (running, mut stdin) = start_with_closed(closed, self.command);
        send_request(&mut stdin, &self.request);
        drop(stdin);
        running
    }
}

impl Deref for BoxCall {
    type Target = Command;

    fn deref(&self) -> &Command {
        &self.command
    }
}

impl DerefMut for BoxCall {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

/// Writes `request` and its newline to the box's stdin. A write that fails because the box has already gone is left to
/// the case's assertions on what the box said, since a box that never read its request says so.
fn send_request(stdin: &mut ChildStdin, request: &str) {
    drop(stdin.write_all(format!("{request}\n").as_bytes()));
}

/// `gate box` with `args` after `box` (the seams, then the verb) and `--stdin`, its heartbeat period
/// [`QUIET_HEARTBEAT_MS`], without the git variables the suite may have inherited; `request` is its stdin's first line.
fn box_call(case: &Case, args: &[&str], request: &str) -> BoxCall {
    let mut words = vec!["box", "--test-heartbeat-ms", QUIET_HEARTBEAT_MS];
    words.extend_from_slice(args.strip_prefix(&["box"][..]).unwrap_or(args));
    bare_box_call(case, &words, request)
}

/// [`box_call`] with `args` as they are, no seam added.
fn bare_box_call(case: &Case, args: &[&str], request: &str) -> BoxCall {
    let mut words = args.to_vec();
    words.push("--stdin");
    BoxCall {
        command: bare_box_command(case, &words),
        request: request.to_owned(),
    }
}

/// The gate with `args` as they are, no seam added, without the git variables the suite may have inherited.
fn bare_box_command(case: &Case, args: &[&str]) -> Command {
    let mut command = case.gate_without_state_dir(args);
    for name in GIT_REDIRECTS {
        command.env_remove(name);
    }
    command
}

/// `gate box <verb> --stdin` of the request line `request`, with `PATH` set to `path` when given, to its end.
fn gate_box(case: &Case, verb: &str, request: &str, path: Option<&OsString>) -> Outcome {
    let mut call = box_call(case, &["box", verb], request);
    if let Some(path) = path {
        call.env("PATH", path);
    }
    call.finish()
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
    crate::support::git_home_for(&mut command);
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
    assert_eq!(printed.len(), 5, "{outcome:?}");
    assert_eq!(printed[0], format!("gate: remote created {}", folders.bare));
    assert_eq!(printed[1], format!("gate: remote version {}", build_version(&case)));
    assert_eq!(printed[2], "gate: remote warm none", "a folder made now is not warm: {outcome:?}");
    assert!(printed[3].starts_with("gate: remote boot "), "{outcome:?}");
    assert!(printed[4].starts_with("gate: remote sdks "), "{outcome:?}");
    assert!(Path::new(&folders.work).join(".git").is_dir());
    assert_eq!(git(&["-C", &folders.work, "config", "--get", "remote.origin.url"]), folders.bare);
    assert_eq!(git(&["-C", &folders.work, "config", "--local", "--get", "core.autocrlf"]), "false");
    assert_eq!(git(&["-C", &folders.work, "config", "--local", "--get", "core.eol"]), "lf");
    assert_eq!(folders.alternates(), folders.written_alternates());
    assert!(!case.local_app_data().exists(), "the box reads and writes nothing under LOCALAPPDATA");
    assert_eq!(case.folder_entries(), ["remote"], "the box writes only where the payload says");
}

#[test]
fn preflight_rewrites_alternates_with_other_content_and_an_empty_one() {
    let case = Case::new("preflight_rewrites_alternates_with_other_content");
    let folders = Folders::new(&case);
    passed(&gate_box(&case, "preflight", &folders.payload(), None));

    fs::write(folders.alternates_path(), "C:/other/objects\n").expect("the alternates file can be written");
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    assert_eq!(
        folders.alternates(),
        folders.written_alternates(),
        "other content names the tree's own bare again"
    );

    fs::write(folders.alternates_path(), "").expect("the alternates file can be emptied");
    passed(&gate_box(&case, "preflight", &folders.payload(), None));
    assert_eq!(folders.alternates(), folders.written_alternates(), "an empty file is written again");
}

#[test]
fn preflight_a_failing_git_step_exits_with_its_status() {
    let case = Case::new("preflight_a_failing_git_step");
    let folders = Folders::new(&case);
    // An empty `.git` folder is no repository, so its first git step fails; a `.git` that is not a folder is skipped.
    fs::create_dir_all(Path::new(&folders.work).join(".git")).expect("an empty .git folder can be made");
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
    let mut command = box_call(&case, &["box", "--test-dotnet-limit-ms", "1000", "preflight"], &folders.payload());
    command.env("PATH", path_with_git(&[stub]));

    let outcome = command.finish();

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

/// A folder `stub` under the case folder holding each `(name, body)` of `tools` as `<name>.cmd`.
fn tool_stubs(case: &Case, tools: &[(&str, &str)]) -> PathBuf {
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    for (name, body) in tools {
        fs::write(stub.join(format!("{name}.cmd")), body).expect("a tool stub can be written");
    }
    stub
}

/// The preflight payload of `folders` with `Needs` set to `needs`, as the remote client sends a manifest's tools.
fn needing(folders: &Folders, needs: &Value) -> String {
    payload(&json!({
        "Key": "k",
        "Bare": folders.bare,
        "Work": folders.work,
        "Lock": format!("{}.lock", folders.work),
        "Job": "unused",
        "Needs": needs,
    }))
}

/// `gate box preflight` of a payload needing `needs`, its `PATH` the stubs of `tools` then git's folder, so no dotnet is
/// found and the sdks line says `none`.
fn probed(case: &Case, needs: &Value, tools: &[(&str, &str)]) -> Outcome {
    let folders = Folders::new(case);
    let stub = tool_stubs(case, tools);
    let mut command = box_call(
        case,
        &["box", "--test-probe-limit-ms", UNREACHED_PROBE_LIMIT_MS, "preflight"],
        &needing(&folders, needs),
    );
    command.env("PATH", path_with_git(&[stub]));
    command.finish()
}

/// The probe bound every probe case passes but the one about the bound: long enough that no probe of a case reaches it
/// however slow a loaded machine makes the run, so only that one case depends on a deadline.
const UNREACHED_PROBE_LIMIT_MS: &str = "120000";

/// The stdout lines of a passed preflight after its sdks line.
fn after_sdks(outcome: &Outcome) -> Vec<&str> {
    let printed = passed(outcome);
    let sdks = printed.iter().position(|line| *line == "gate: remote sdks none");
    assert!(sdks.is_some(), "the preflight says its sdks line: {outcome:?}");
    printed[sdks.map_or(printed.len(), |sdks| sdks + 1)..].to_vec()
}

#[test]
fn preflight_probes_a_need_after_the_sdks_line() {
    let case = Case::new("preflight_probes_a_need_after_the_sdks_line");

    let outcome = probed(&case, &json!({"node": "24"}), &[("node", "@echo v24.19.0\r\n")]);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool node 24.19.0"], "{outcome:?}");
}

#[test]
fn preflight_says_one_line_per_need_in_the_payload_s_order() {
    let case = Case::new("preflight_one_line_per_need");
    let plugin = "@echo off\r\nif \"%1\"==\"nextest\" echo cargo-nextest 0.9.100 (abc 2026-01-01)\r\n";

    let outcome = probed(
        &case,
        &json!({"rust": "1.89", "node": "", "cargo-nextest": "0.9.100", "pwsh": ""}),
        &[
            ("rustc", "@echo rustc 1.99.0 (b940084d7 2026-09-28)\r\n"),
            ("node", "@echo v24.19.0\r\n"),
            ("cargo-nextest", plugin),
            ("pwsh", "@echo PowerShell 7.6.6\r\n"),
        ],
    );

    assert_eq!(
        after_sdks(&outcome),
        [
            "gate: remote tool rust 1.99.0",
            "gate: remote tool node 24.19.0",
            "gate: remote tool cargo-nextest 0.9.100",
            "gate: remote tool pwsh 7.6.6",
        ],
        "{outcome:?}"
    );
}

#[test]
fn preflight_says_a_tool_it_does_not_find_is_missing() {
    let case = Case::new("preflight_tool_missing");

    let outcome = probed(&case, &json!({"pnpm": "10"}), &[]);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool pnpm missing"], "{outcome:?}");
}

#[test]
fn preflight_a_tool_with_no_readable_version_is_present() {
    let case = Case::new("preflight_tool_present");

    let outcome = probed(
        &case,
        &json!({"ffmpeg": ""}),
        &[(
            "ffmpeg",
            "@echo off\r\necho ffmpeg version N-117456-gabc Copyright (c) 2000-2026\r\necho built with gcc 14.2.0\r\n",
        )],
    );

    assert_eq!(after_sdks(&outcome), ["gate: remote tool ffmpeg present"], "{outcome:?}");
}

/// The version is read from the first line alone: ffmpeg's second line names the compiler's version, not its own.
#[test]
fn preflight_reads_a_tool_s_version_from_its_first_line() {
    let case = Case::new("preflight_tool_first_line");
    let ffmpeg = "@echo off\r\necho ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026\r\necho built with gcc 14.2.0\r\n";

    let outcome = probed(&case, &json!({"ffmpeg": "9"}), &[("ffmpeg", ffmpeg)]);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool ffmpeg 9.0.2"], "{outcome:?}");
}

/// The tool probes run while dotnet lists the SDKs: the dotnet stub answers only once the node stub has run, which a
/// preflight that probed after the SDK read would never let it do. Neither stub's wait ends on a clock but fake-tool's
/// own wait limit, which only the serial order reaches.
#[test]
fn preflight_probes_the_tools_while_dotnet_lists_the_sdks() {
    let case = Case::new("preflight_probes_beside_dotnet");
    let folders = Folders::new(&case);
    let go = case.folder().join("node-ran");
    let dotnet = format!("@echo off\r\n\"{FAKE_TOOL}\" wait \"{}\"\r\necho 8.0.404 [C:\\stub]\r\n", go.display());
    let node = format!("@echo off\r\ntype nul >\"{}\"\r\necho v24.19.0\r\n", go.display());
    let stub = tool_stubs(&case, &[("dotnet", &dotnet), ("node", &node)]);
    let mut command = box_call(
        &case,
        &[
            "box",
            "--test-dotnet-limit-ms",
            UNREACHED_PROBE_LIMIT_MS,
            "--test-probe-limit-ms",
            UNREACHED_PROBE_LIMIT_MS,
            "preflight",
        ],
        &needing(&folders, &json!({"node": ""})),
    );
    command.env("PATH", path_with_git(&[stub]));

    let outcome = command.finish();

    let printed = passed(&outcome);
    assert_eq!(
        printed[printed.len().saturating_sub(2)..],
        ["gate: remote sdks 8.0.404", "gate: remote tool node 24.19.0"],
        "{outcome:?}"
    );
}

/// A `cargo` on `PATH` that marks `marker` when it runs and answers as cargo-nextest would through it.
fn cargo_decoy(marker: &Path) -> String {
    format!(
        "@echo off\r\n>\"{}\" echo ran\r\necho cargo-nextest 0.9.1 (decoy 2026-01-01)\r\n",
        marker.display()
    )
}

/// cargo-nextest is probed by running the plugin itself, never through cargo, which would read a `.cargo\config.toml`
/// above its working folder and run the alias one defines: a cargo on `PATH` is never run.
#[test]
fn preflight_probes_cargo_nextest_as_the_plugin_and_never_through_cargo() {
    let lacking = Case::new("preflight_cargo_without_nextest");
    let marker = lacking.folder().join("cargo-ran");

    let outcome = probed(&lacking, &json!({"cargo-nextest": ""}), &[("cargo", &cargo_decoy(&marker))]);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool cargo-nextest missing"], "{outcome:?}");
    assert!(!marker.exists(), "cargo ran for the probe");

    let having = Case::new("preflight_cargo_with_nextest");
    let marker = having.folder().join("cargo-ran");
    let plugin = "@echo off\r\nif \"%1\"==\"nextest\" if \"%2\"==\"--version\" echo cargo-nextest 0.9.148 (cd1d6d546 2026-10-07)\r\n";

    let outcome = probed(
        &having,
        &json!({"cargo-nextest": "0.9.100"}),
        &[("cargo", &cargo_decoy(&marker)), ("cargo-nextest", plugin)],
    );

    assert_eq!(after_sdks(&outcome), ["gate: remote tool cargo-nextest 0.9.148"], "{outcome:?}");
    assert!(!marker.exists(), "cargo ran for the probe");
}

/// A tool that never answers is ended with its job at the bound `--test-probe-limit-ms` sets, and said skipped; the
/// preflight still passes. Only the bound ends the probe, so no load changes what the preflight says.
#[test]
fn preflight_a_tool_that_does_not_answer_is_skipped_with_a_line() {
    let case = Case::new("preflight_tool_hangs");
    let folders = Folders::new(&case);
    let go = Go(case.folder().join("node-go"));
    let held = case.folder().join("node-held");
    let hang = format!(
        "@echo off\r\n\"{FAKE_TOOL}\" wait \"{}\" >\"{}\"\r\n",
        go.path().display(),
        held.display()
    );
    let stub = tool_stubs(&case, &[("node", &hang)]);
    let needs = json!({"node": "24"});
    let mut command = box_call(&case, &["box", "--test-probe-limit-ms", "1000", "preflight"], &needing(&folders, &needs));
    command.env("PATH", path_with_git(&[stub]));

    let outcome = command.finish();

    // A stub that started holds its output file open until its job's kill ends it; one the kill came first never made it.
    if held.exists() {
        wait_until_closed(&held);
    }
    assert_eq!(after_sdks(&outcome), ["gate: remote tool node skipped: no answer in 1 s"], "{outcome:?}");
}

/// A tool runs in the system folder, not the folder `gate box` was started in: the stub writes the folder it runs in,
/// and a `pnpm.cmd` in the working folder, which no `PATH` folder names, is not run.
#[test]
fn preflight_runs_a_tool_in_the_system_folder_and_not_the_working_folder() {
    let case = Case::new("preflight_tool_folder");
    let ran_in = case.folder().join("node-folder");
    let node = format!("@echo off\r\n>\"{}\" echo %CD%\r\necho v24.19.0\r\n", ran_in.display());
    let marker = case.folder().join("pnpm-ran");
    let decoy = format!("@echo off\r\n>\"{}\" echo ran\r\necho 10.0.0\r\n", marker.display());
    fs::write(case.folder().join("pnpm.cmd"), decoy).expect("the pnpm in the working folder can be written");

    let outcome = probed(&case, &json!({"node": "", "pnpm": ""}), &[("node", &node)]);

    assert_eq!(
        after_sdks(&outcome),
        ["gate: remote tool node 24.19.0", "gate: remote tool pnpm missing"],
        "{outcome:?}"
    );
    let ran_in = fs::read_to_string(&ran_in).expect("the stub wrote the folder it ran in");
    let system = gate_win32::system_directory().expect("the system folder can be read");
    let system = system.to_string_lossy();
    assert!(
        ran_in.trim_end().eq_ignore_ascii_case(system.trim_end_matches('\\')),
        "{system}: {ran_in:?}"
    );
    assert!(!marker.exists(), "the pnpm.cmd in the working folder ran");
}

#[test]
fn preflight_a_need_that_is_not_an_object_of_texts_is_said_and_the_preflight_passes() {
    for (name, needs) in [("list", json!(["node"])), ("number", json!({"node": 24})), ("text", json!("node"))] {
        let case = Case::new(&format!("preflight_bad_needs_{name}"));

        let outcome = probed(&case, &needs, &[("node", "@echo v24.19.0\r\n")]);

        assert_eq!(outcome.code, Some(0), "{name}: {outcome:?}");
        assert_eq!(
            lines(&outcome.stdout).last(),
            Some(&"gate: remote sdks none"),
            "{name}: nothing is probed: {outcome:?}"
        );
        let said = "gate: box payload Needs is not an object of texts; probing nothing";
        assert!(outcome.stderr.lines().any(|line| line == said), "{name}: {outcome:?}");
    }
}

#[test]
fn preflight_a_tool_name_it_does_not_know_is_skipped() {
    let case = Case::new("preflight_unknown_tool");
    let marker = case.folder().join("nodejs-ran");
    let nodejs = format!("@echo off\r\n>\"{}\" echo ran\r\necho v24.19.0\r\n", marker.display());

    let outcome = probed(&case, &json!({"nodejs": "24"}), &[("nodejs", &nodejs)]);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool nodejs skipped: unknown tool"], "{outcome:?}");
    assert!(!marker.exists(), "a program the payload names is never run");
}

/// Puts a copy of fake-tool named `Godot_console.exe` in `folder`, answering `--version` with the line `line`, so the
/// folder stands in for a Godot install.
fn fake_godot(folder: &Path, line: &str) {
    fs::create_dir_all(folder).expect("the fake godot's folder can be made");
    fs::copy(FAKE_TOOL, folder.join("Godot_console.exe")).expect("the fake godot can be copied");
    fs::write(folder.join("version.txt"), line).expect("the line the fake godot answers with can be written");
}

/// The folder a case's `GATE_GODOT_ROOT` names, made and holding no install: one goes in a subfolder of it, put there
/// with [`fake_godot`].
fn godot_root(case: &Case) -> PathBuf {
    let root = case.folder().join("godot");
    fs::create_dir_all(&root).expect("the godot root can be made");
    root
}

/// A `gate box preflight` needing `needs`, its `PATH` the stub folders `stubs` then git's folder, so no dotnet is
/// found and the sdks line says `none`, and its `GATE_GODOT_ROOT` the folder `root`, taken out of the box's
/// environment when there is none.
fn probed_with_root(case: &Case, needs: &Value, stubs: &[PathBuf], root: Option<&Path>) -> Outcome {
    let folders = Folders::new(case);
    let mut command = box_call(
        case,
        &["box", "--test-probe-limit-ms", UNREACHED_PROBE_LIMIT_MS, "preflight"],
        &needing(&folders, needs),
    );
    command.env("PATH", path_with_git(stubs));
    if let Some(root) = root {
        command.env("GATE_GODOT_ROOT", root);
    } else {
        command.env_remove("GATE_GODOT_ROOT");
    }
    command.finish()
}

/// Every Godot the root names is probed, and the line carries every version found, by number and without repeats.
#[test]
fn preflight_says_every_godot_the_root_names() {
    let case = Case::new("preflight_godot_root");
    let root = godot_root(&case);
    fake_godot(&root.join("4.7.2"), "4.7.2.stable.mono.official.x\n");
    fake_godot(&root.join("4.3"), "4.3.stable.mono.official.y\n");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(&root));

    assert_eq!(after_sdks(&outcome), ["gate: remote tool godot 4.3.0,4.7.2"], "{outcome:?}");
}

/// A root that names no Godot install is missing, as any other tool the box does not find.
#[test]
fn preflight_says_a_root_holding_no_godot_is_missing() {
    let case = Case::new("preflight_godot_root_empty");
    let root = godot_root(&case);

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(&root));

    assert_eq!(after_sdks(&outcome), ["gate: remote tool godot missing"], "{outcome:?}");
}

/// With no `GATE_GODOT_ROOT` the probe runs the `Godot_console.exe` the box's `PATH` holds.
#[test]
fn preflight_says_the_godot_on_the_path_without_a_root() {
    let case = Case::new("preflight_godot_on_path");
    let stub = case.folder().join("stub");
    fake_godot(&stub, "4.7.2.stable.mono.official.x\n");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[stub], None);

    assert_eq!(after_sdks(&outcome), ["gate: remote tool godot 4.7.2"], "{outcome:?}");
}

/// An install that answers with no version is skipped with the line it wrote, not said as one the box has.
#[test]
fn preflight_says_a_godot_answering_with_no_version_is_skipped() {
    let case = Case::new("preflight_godot_no_version");
    let root = godot_root(&case);
    fake_godot(&root.join("4.7.2"), "hello\n");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(&root));

    assert_eq!(
        after_sdks(&outcome),
        ["gate: remote tool godot skipped: Godot_console.exe --version gave no version: hello"],
        "{outcome:?}"
    );
}

/// A root that is no absolute path on a drive is refused with its path, so a relative one is never listed in one
/// folder and run from another.
#[test]
fn preflight_skips_a_godot_root_that_is_no_drive_path() {
    let case = Case::new("preflight_godot_root_no_drive");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(Path::new("godot")));

    assert_eq!(
        after_sdks(&outcome),
        ["gate: remote tool godot skipped: GATE_GODOT_ROOT godot is not an absolute path on a drive"],
        "{outcome:?}"
    );
}

/// A root naming no folder the box can read is skipped with the folder and why it cannot be read.
#[test]
fn preflight_skips_a_godot_root_it_cannot_read() {
    let case = Case::new("preflight_godot_root_unreadable");
    let root = case.folder().join("no-such-root");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(&root));

    let unreadable = fs::read_dir(&root).expect_err("the root is no folder");
    let said = format!(
        "gate: remote tool godot skipped: cannot read GATE_GODOT_ROOT {}: {unreadable}",
        root.display()
    );
    assert_eq!(after_sdks(&outcome), [said.as_str()], "{outcome:?}");
}

/// An install inside the run's own folders is refused, as a program of any other probe found inside a worktree is,
/// and never runs.
#[test]
fn preflight_refuses_a_godot_inside_the_run_s_folders() {
    let case = Case::new("preflight_godot_inside_the_run");
    let folders = Folders::new(&case);
    let root = Path::new(&folders.work).parent().expect("the work folder has a parent").to_path_buf();
    let install = root.join("inst");
    fake_godot(&install, "9.9.9.stable.mono.official.x\n");

    let outcome = probed_with_root(&case, &json!({"godot": "9.9.9"}), &[], Some(&root));

    let program = install.join("Godot_console.exe");
    let said = format!(
        "gate: remote tool godot skipped: cannot run {}: {} lies inside the worktree",
        program.display(),
        program.display()
    );
    assert_eq!(after_sdks(&outcome), [said.as_str()], "{outcome:?}");
    assert!(!install.join("ran").exists(), "the refused install ran");
}

/// A subfolder of the root that is a junction is not followed: the install behind it is neither probed nor run.
#[test]
fn preflight_skips_a_godot_behind_a_junction() {
    let case = Case::new("preflight_godot_behind_a_junction");
    let root = godot_root(&case);
    let behind = case.folder().join("behind");
    fake_godot(&behind, "9.9.9.stable.mono.official.x\n");
    junction(&root.join("linked"), &behind);
    fake_godot(&root.join("4.7.2"), "4.7.2.stable.mono.official.x\n");

    let outcome = probed_with_root(&case, &json!({"godot": "4.7.2"}), &[], Some(&root));

    assert_eq!(after_sdks(&outcome), ["gate: remote tool godot 4.7.2"], "{outcome:?}");
    assert!(!behind.join("ran").exists(), "the install behind the junction ran");

    // A root whose only install is behind a junction is skipped with its reason, not said missing.
    let only = Case::new("preflight_godot_only_a_junction");
    let root = godot_root(&only);
    let behind = only.folder().join("behind");
    fake_godot(&behind, "9.9.9.stable.mono.official.x\n");
    junction(&root.join("linked"), &behind);

    let outcome = probed_with_root(&only, &json!({"godot": "4.7.2"}), &[], Some(&root));

    let linked = root.join("linked");
    let said = format!(
        "gate: remote tool godot skipped: cannot run {}: {} is a link or a junction",
        linked.join("Godot_console.exe").display(),
        linked.display()
    );
    assert_eq!(after_sdks(&outcome), [said.as_str()], "{outcome:?}");
    assert!(!behind.join("ran").exists(), "the install behind the junction ran");
}

#[test]
fn preflight_without_needs_says_nothing_after_the_sdks_line() {
    for (name, needs) in [("empty", json!({})), ("null", Value::Null)] {
        let case = Case::new(&format!("preflight_no_needs_{name}"));

        let outcome = probed(&case, &needs, &[("node", "@echo v24.19.0\r\n")]);

        assert_eq!(after_sdks(&outcome), Vec::<&str>::new(), "{name}: {outcome:?}");
    }
    let case = Case::new("preflight_no_needs_absent");
    let folders = Folders::new(&case);
    let stub = tool_stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = gate_box(&case, "preflight", &folders.payload(), Some(&path_with_git(&[stub])));

    assert_eq!(after_sdks(&outcome), Vec::<&str>::new(), "absent: {outcome:?}");
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
    let shapes: [&[&str]; 4] = [
        &["box"],
        &["box", "launch", "--stdin"],
        &["box", "preflight"],
        &["box", "preflight", "e30="],
    ];
    for args in shapes {
        let outcome = finish(bare_box_command(&case, args));

        assert_eq!(outcome.code, Some(2), "{args:?}");
        assert_eq!(outcome.stdout, "", "{args:?}");
        assert_eq!(outcome.stderr, BOX_USAGE, "{args:?}: the box usage alone");
    }

    let not_an_object = bare_box_call(&case, &["box", "preflight"], &payload(&json!(["Bare", "Work"]))).finish();
    assert_eq!(not_an_object.code, Some(2));
    assert_eq!(not_an_object.stdout, "");
    assert_eq!(not_an_object.stderr, "gate: box payload is not base64 JSON: the JSON is not an object\n");

    let lacking = bare_box_call(&case, &["box", "preflight"], &payload(&json!({"Bare": folders.bare}))).finish();
    assert_eq!(lacking.code, Some(2));
    assert_eq!(lacking.stdout, "");
    assert_eq!(lacking.stderr, "gate: box payload lacks Work\n");
    assert!(!Path::new(&folders.bare).exists(), "nothing is made for a payload the box refuses");

    assert!(!case.run(&["--help"]).stdout.contains("box"), "gate box is hidden from the usage");
}

#[test]
fn a_box_call_without_a_request_on_stdin_is_a_usage_error() {
    let case = Case::new("box_request_missing");
    let folders = Folders::new(&case);

    let (running, stdin) = start_with_stdin(bare_box_command(&case, &["box", "preflight", "--stdin"]));
    drop(stdin);
    let outcome = running.finish();

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "gate: the box request is missing on standard input\n");
    assert!(!Path::new(&folders.bare).exists(), "nothing is made without a request");
}

/// The most bytes the box reads of its request line: 16 MiB.
const REQUEST_LIMIT: usize = 16 * 1024 * 1024;

#[test]
fn a_box_request_over_the_cap_is_a_usage_error() {
    let case = Case::new("box_request_over_cap");

    let (running, mut stdin) = start_with_stdin(bare_box_command(&case, &["box", "preflight", "--stdin"]));
    // The box stops reading at the cap and exits, so the write is a thread's: the case waits on the box, not the write,
    // which fails once the box has gone or ends in the pipe and the box's own buffer; either way the box said its line.
    let writer = thread::spawn(move || drop(stdin.write_all(&vec![b'A'; REQUEST_LIMIT + 1])));
    let outcome = running.finish();
    writer.join().expect("the writer thread ends");

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        "gate: the box request on standard input reaches 16 MiB without a newline\n"
    );
}

#[test]
fn a_box_request_that_is_not_base64_json_is_a_usage_error() {
    let case = Case::new("box_request_not_base64");

    for request in ["not-base64!", ""] {
        let outcome = bare_box_call(&case, &["box", "preflight"], request).finish();

        assert_eq!(outcome.code, Some(2), "{request:?}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{request:?}: {outcome:?}");
        assert!(
            outcome.stderr.starts_with("gate: box payload is not base64 JSON: "),
            "{request:?}: {outcome:?}"
        );
        assert_eq!(lines(&outcome.stderr).len(), 1, "{request:?}: {outcome:?}");
    }
}

#[test]
fn every_box_verb_reads_its_request_from_stdin() {
    let case = Case::new("box_request_every_verb");
    let folders = Folders::new(&case);
    let request = folders.payload();

    let preflight = box_call(&case, &["box", "preflight"], &request).finish();
    assert_eq!(preflight.code, Some(0), "{preflight:?}");
    assert!(Path::new(&folders.bare).exists(), "the preflight made the bare repository: {preflight:?}");

    let run = payload(&box_payload(&case, "k", &[]));
    let stop = box_call(&case, &["box", "stop"], &run).finish();
    assert_eq!(stop.code, Some(0), "no job to stop is no failure: {stop:?}");
    assert_eq!((stop.stdout.as_str(), stop.stderr.as_str()), ("", ""), "{stop:?}");

    let boot = box_call(&case, &["box", "boot"], &payload(&json!({}))).finish();
    assert_eq!(boot.code, Some(0), "{boot:?}");
    assert!(boot.stdout.starts_with("gate: remote boot "), "{boot:?}");

    let outputs = box_call(&case, &["box", "outputs"], &run).finish();
    assert_eq!(outputs.code, Some(2), "the request reached the verb, which wants a Name: {outputs:?}");
    assert_eq!(outputs.stderr, "gate: box payload lacks Name\n");

    let root = case.folder().join("remote");
    fs::create_dir_all(root.join("work")).expect("the work folder can be made");
    let inflight = box_call(&case, &["box", "inflight"], &inflight_payload(&root)).finish();
    assert_eq!(inflight.code, Some(0), "{inflight:?}");
    assert_eq!(inflight.stdout, "gate: remote in flight 0\n", "{inflight:?}");
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
    push_repo_snapshot(case, "source", field(payload, "Bare"), field(payload, "Key"), files)
}

/// Commits `files` in the case's source repository `source`, made when missing, and pushes the commit to
/// `refs/gate/<key>` in the bare repository `bare`, as the client sends a snapshot of one repo; the commit.
fn push_repo_snapshot(case: &Case, source: &str, bare: &str, key: &str, files: &[(&str, &str)]) -> String {
    let source = case.folder().join(source);
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
    let target = format!("HEAD:refs/gate/{key}");
    git(&["-C", &source_text, "push", "-q", "--force", bare, &target]);
    git(&["-C", &source_text, "rev-parse", "HEAD"])
}

/// The work-folder key of the leaf `app`, as the client makes it, which a pair payload's `Key` must be.
const PAIR_KEY: &str = "app-1a2b3c4d";

/// The payload of a `gate box run` of `command` for the sibling pair of the main checkout `app` and `siblings`, laid
/// out as [`box_payload`] lays out a run of one repo, under the key [`PAIR_KEY`].
fn pair_payload(case: &Case, siblings: &[&str], command: &[&str]) -> Value {
    let mut payload = box_payload(case, PAIR_KEY, command);
    payload["Pair"] = json!({"Leaf": "app", "Siblings": siblings});
    payload
}

/// The bare repository of the sibling `name` of `payload`, beside the main repo's, as the box derives it.
fn sibling_bare(payload: &Value, name: &str) -> String {
    let bare = Path::new(field(payload, "Bare")).with_file_name(format!("{name}.git"));
    bare.to_str().expect("the case folder is Unicode").to_owned()
}

/// The tree of the repo `name`, the leaf or a sibling, in the work folder of the pair payload `payload`.
fn pair_tree(payload: &Value, name: &str) -> String {
    let tree = Path::new(field(payload, "Work")).join(name);
    tree.to_str().expect("the case folder is Unicode").to_owned()
}

/// Readies the pair payload's bare repositories with a preflight, then commits `main` in the case's source repository
/// and each sibling's files in a source repository of its own, pushes each commit to `refs/gate/<key>` in its repo's
/// bare repository, and records them as the payload's `Snapshot` and `SiblingSnapshots`; the commits, the main first.
fn pair_snapshot(case: &Case, payload: &mut Value, main: &[(&str, &str)], siblings: &[(&str, &[(&str, &str)])]) -> Vec<String> {
    allow_breakaway();
    passed(&gate_box(case, "preflight", &self::payload(payload), None));
    let mut commits = vec![push_snapshot(case, payload, main)];
    let mut snapshots = serde_json::Map::new();
    for (name, files) in siblings {
        let commit = push_repo_snapshot(case, &format!("source-{name}"), &sibling_bare(payload, name), PAIR_KEY, files);
        snapshots.insert((*name).to_owned(), json!(commit));
        commits.push(commit);
    }
    payload["Snapshot"] = json!(commits[0]);
    payload["SiblingSnapshots"] = Value::Object(snapshots);
    commits
}

/// Whether the bare repository of `payload` has the ref `name`.
fn has_ref(payload: &Value, name: &str) -> bool {
    has_ref_in(field(payload, "Bare"), name)
}

/// Whether the bare repository `bare` has the ref `name`.
fn has_ref_in(bare: &str, name: &str) -> bool {
    let mut command = Command::new("git");
    for variable in GIT_REDIRECTS {
        command.env_remove(variable);
    }
    crate::support::git_home_for(&mut command);
    let bare = format!("--git-dir={bare}");
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
    let mut command = box_call(case, &["box", "run"], &self::payload(payload));
    command.env(name, value);
    command.finish()
}

/// `gate box run` of `payload` with `PATH` set to `path` when given, as a case about dotnet runs it.
fn run_box_with_path(case: &Case, payload: &Value, path: Option<&OsString>) -> Outcome {
    gate_box(case, "run", &self::payload(payload), path)
}

/// `gate box run` of `payload` with `PATH` set to `path`, started, as a case that stops it mid-run needs it.
fn start_box_with_path(case: &Case, payload: &Value, path: Option<&OsString>) -> Running {
    let mut command = box_call(case, &["box", "run"], &self::payload(payload));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.start()
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
    box_call(case, &["box", "run"], &self::payload(payload)).start()
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
fn box_run_gives_the_command_the_payloads_env() {
    let case = Case::new("box_run_env");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "env", "GATE362_PROBE", "env", "NuGetAudit"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    payload["Env"] = json!({"GATE362_PROBE": "x", "NuGetAudit": "false"});

    // Inherited under the same name, so the case sees the payload's pair replace the box's own value.
    let outcome = run_box_with_env(&case, &payload, "GATE362_PROBE", "inherited");

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "x\nfalse\n",
        "the command reads the payload's Env, not the box's own: {outcome:?}"
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
fn box_run_refuses_a_malformed_env() {
    let case = Case::new("box_run_env_refused");
    let many = Value::Object((0..65).map(|number| (format!("A{number}"), json!("v"))).collect());
    let hundred_thousand = Value::Object((0..100_000).map(|number| (format!("A{number:06}"), json!("v"))).collect());
    // A name of 250 characters refused as the gate's own: its first 200 are shown, the control character among them
    // escaped.
    let long_name = format!("GATE_{}\u{1b}{}", "x".repeat(194), "y".repeat(50));
    let long_line = format!(
        "gate: box payload Env sets GATE_{}\\u{{1b}}…: GATE_ names are the gate's",
        "x".repeat(194)
    );
    let long_env = Value::Object(std::iter::once((long_name, json!("c"))).collect());
    let refusing = [
        (json!("A=b"), "gate: box payload Env is not an object of texts"),
        (json!({"A": 1}), "gate: box payload Env is not an object of texts"),
        (
            json!({"NuGetAudit": "false", "NUGETAUDIT": "true"}),
            "gate: box payload Env names NUGETAUDIT twice",
        ),
        (
            json!({"ZZ_\u{c4}_X": "a", "ZZ_\u{e4}_X": "b"}),
            "gate: box payload Env names ZZ_\u{e4}_X twice",
        ),
        (
            json!({"MSBUILDUSESERVER": "1"}),
            "gate: box payload Env sets MSBUILDUSESERVER, which the gate sets itself",
        ),
        (
            json!({"PATH": "C:\\x"}),
            "gate: box payload Env sets PATH, a machine path the builder keeps",
        ),
        (
            json!({"GATE_OTHER": "1"}),
            "gate: box payload Env sets GATE_OTHER: GATE_ names are the gate's",
        ),
        (json!({"A": "b\u{0}c"}), "gate: box payload Env gives A a value holding NUL"),
        (json!({"A=B": "c"}), "gate: box payload Env has a malformed name 'A=B'"),
        (json!({"": "c"}), "gate: box payload Env has a malformed name ''"),
        (json!({"A=\u{1b}": "c"}), "gate: box payload Env has a malformed name 'A=\\u{1b}'"),
        (long_env, long_line.as_str()),
        (many, "gate: box payload Env is over its cap of 64 names or 32 KiB"),
        (hundred_thousand, "gate: box payload Env is over its cap of 64 names or 32 KiB"),
    ];

    // One preflight lays the work folder out, so each refusal is shown to leave it as it was.
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "env", "A"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let laid = work_entries(&payload);
    assert_eq!(laid, [".git"], "the preflight lays nothing else out");

    for (env, line) in refusing {
        let mut refused = payload.clone();
        refused["Env"] = env;

        let outcome = run_box(&case, &refused);

        assert_eq!(outcome.code, Some(2), "{line}: {outcome:?}");
        assert_eq!(outcome.stdout, "");
        assert_eq!(outcome.stderr, format!("{line}\n"), "{outcome:?}");
        assert_eq!(work_entries(&payload), laid, "nothing is checked out or run");
    }
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

    let running = bare_box_call(&case, &["box", "--test-heartbeat-ms", "30", "run"], &self::payload(&payload)).start();
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

    let outcome = bare_box_call(&case, &["box", "--test-heartbeat-ms", "1", "run"], &self::payload(&payload)).finish();

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
    let mut box_run = box_call(case, &["box", "run"], &self::payload(&payload));
    if let Some((name, value)) = with {
        box_run.env(name, value);
    }
    let running = box_run.start();
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

    let running = box_call(&case, &["box", "run"], &self::payload(&payload)).start_with_closed(Closed::Stdout);
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

    let running = box_call(&case, &["box", "run"], &self::payload(&payload)).start_with_closed(Closed::Stderr);
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

/// A bare name holding a dot that is not a `PATHEXT` extension, the shape a versioned Godot exe's derived name has, is
/// found on the box's `PATH` as `<name>.exe`.
#[test]
fn box_run_finds_a_dotted_bare_name_with_exe_appended() {
    let case = Case::new("box_run_dotted_bare_name");
    copy_tool(&case, "fake.tool.v1.exe");
    let mut payload = box_payload(&case, "k", &["fake.tool.v1", "out", "ran", "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[case.folder()])));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "ran\n", "{outcome:?}");
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

    let outcome = box_call(&case, &["box", "--test-box-token", "refused", "run"], &self::payload(&payload)).finish();

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
    let mut command = box_call(&case, &["box", "run"], &self::payload(&payload));
    command.env_remove("GIT_CONFIG_COUNT");

    let outcome = command.finish();

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

    let outcome = box_call(&case, &["box", "--test-box-console", "fail-before", "run"], &self::payload(&payload)).finish();

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
    args.extend_from_slice(&["--test-box-park-launch", &park_word, "run"]);
    let running = box_call(&case, &args, &payload_word).start();
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

/// A `gate box inflight --stdin` request line for the builder's root `root`.
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
fn preflight_with_a_pair_creates_each_bare_and_nothing_in_the_work_folder() {
    let case = Case::new("preflight_with_a_pair_creates_each_bare");
    let payload = pair_payload(&case, &["lib", "tools"], &[]);
    let work = PathBuf::from(field(&payload, "Work"));
    let folder = work.parent().expect("the work folder has a parent");
    fs::create_dir_all(folder).expect("the work folder's parent can be made");
    // Held as a run of the key holds it: the preflight takes no lock, so it must change nothing inside the work folder.
    let _lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(field(&payload, "Lock"))
        .expect("the lock can be taken");

    let outcome = gate_box(&case, "preflight", &self::payload(&payload), None);

    let printed = passed(&outcome);
    let bares = [
        field(&payload, "Bare").to_owned(),
        sibling_bare(&payload, "lib"),
        sibling_bare(&payload, "tools"),
    ];
    let created: Vec<String> = printed.iter().take(3).map(|line| (*line).to_owned()).collect();
    assert_eq!(
        created,
        bares.iter().map(|bare| format!("gate: remote created {bare}")).collect::<Vec<_>>(),
        "{outcome:?}"
    );
    assert_eq!(
        printed.len(),
        7,
        "the created lines, then the version, warm, boot and sdks lines: {outcome:?}"
    );
    assert_eq!(printed[4], "gate: remote warm none", "{outcome:?}");
    for bare in &bares {
        assert_eq!(
            git(&[&format!("--git-dir={bare}"), "rev-parse", "--is-bare-repository"]),
            "true",
            "{bare}"
        );
    }
    assert!(!work.exists(), "nothing is made at or in the work folder: {outcome:?}");
    let beside: Vec<String> = fs::read_dir(folder)
        .expect("the work folder's parent can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(beside, [format!("{PAIR_KEY}.lock")], "only the held lock is beside it");
}

#[test]
fn box_run_with_a_pair_checks_out_each_repo_in_its_own_folder() {
    let case = Case::new("box_run_pair_checks_out_each_repo");
    let mut payload = pair_payload(&case, &["lib", "tools"], &[r".\show.cmd"]);
    let show = "@type a.txt\r\n@type ..\\lib\\b.txt\r\n@type ..\\tools\\c.txt\r\n";
    let commits = pair_snapshot(
        &case,
        &mut payload,
        &[("show.cmd", show), ("a.txt", "main\n")],
        &[("lib", &[("b.txt", "lib\n")]), ("tools", &[("c.txt", "tools\n")])],
    );

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        outcome.stdout, "main\nlib\ntools\n",
        "the command runs in the main tree and finds each sibling beside it"
    );
    for (name, commit) in ["app", "lib", "tools"].iter().zip(&commits) {
        assert_eq!(
            &git(&["-C", &pair_tree(&payload, name), "rev-parse", "HEAD"]),
            commit,
            "{name} is at its own snapshot"
        );
    }
    let alternates = Path::new(&pair_tree(&payload, "lib")).join(r".git\objects\info\alternates");
    assert_eq!(
        fs::read(alternates).expect("the sibling's alternates file can be read"),
        format!("{}/objects\n", sibling_bare(&payload, "lib").replace('\\', "/")).into_bytes(),
        "a sibling's tree borrows its own bare repository's objects"
    );
    let mut entries = work_entries(&payload);
    entries.sort();
    assert_eq!(
        entries,
        ["app", "lib", "tools"],
        "the work folder holds the trees and no repository of its own"
    );
}

#[test]
fn box_run_with_a_pair_names_the_repo_in_a_failed_step() {
    let case = Case::new("box_run_pair_names_the_repo");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "out", "ran"]);
    allow_breakaway();
    passed(&gate_box(&case, "preflight", &self::payload(&payload), None));
    let main = push_snapshot(&case, &payload, &[("a.txt", "a\n")]);
    payload["Snapshot"] = json!(main);
    payload["SiblingSnapshots"] = json!({"lib": main});
    let fetch = Regex::new(r"(?m)^gate: remote fetch lib failed \(status (-?\d+)\)$").expect("the pattern compiles");

    // Nothing pushed to the sibling's bare repository yet: its fetch fails.
    let outcome = run_box(&case, &payload);

    let captures = fetch.captures(&outcome.stderr).expect("the fetch's line names the sibling");
    let status: i32 = captures[1].parse().expect("the status is a number");
    assert_ne!(status, 0, "{outcome:?}");
    assert_eq!(outcome.code, Some(status), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran");
    assert_eq!(
        git(&["-C", &pair_tree(&payload, "app"), "rev-parse", "HEAD"]),
        main,
        "the main repo, first, was checked out"
    );

    // The sibling's ref pushed, but the payload names a commit only the main repo has.
    push_repo_snapshot(&case, "source-lib", &sibling_bare(&payload, "lib"), PAIR_KEY, &[("b.txt", "b\n")]);
    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "the command never ran");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line == format!("gate: remote snapshot {main} is not in the repository of lib after the fetch")),
        "the missing snapshot's line names the sibling: {outcome:?}"
    );
}

#[test]
fn box_run_git_in_a_tree_ignores_a_core_worktree_pointing_elsewhere() {
    let case = Case::new("box_run_ignores_core_worktree");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    let canary = case.folder().join("canary");
    fs::create_dir_all(&canary).expect("the canary folder can be made");
    fs::write(canary.join("untracked.txt"), "keep").expect("the canary's file can be written");
    let canary_text = canary.to_str().expect("the case folder is Unicode");
    git(&["-C", field(&payload, "Work"), "config", "core.worktree", canary_text]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        canary.join("untracked.txt").exists(),
        "the clean never reaches the folder core.worktree names"
    );
    assert!(!canary.join("a.txt").exists(), "nor does the checkout");
    assert!(
        Path::new(field(&payload, "Work")).join("a.txt").is_file(),
        "the snapshot is checked out in the tree"
    );
}

#[test]
fn box_run_with_a_pair_writes_a_back_commit_per_changed_repo() {
    let case = Case::new("box_run_pair_back_commit_per_repo");
    let mut payload = pair_payload(&case, &["lib", "tools"], &[r".\change.cmd"]);
    let change = "@echo changed> a.txt\r\n@echo changed> ..\\lib\\b.txt\r\n";
    let commits = pair_snapshot(
        &case,
        &mut payload,
        &[("change.cmd", change), ("a.txt", "main\n")],
        &[("lib", &[("b.txt", "lib\n")]), ("tools", &[("c.txt", "tools\n")])],
    );

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let back = format!("refs/gate-back/{PAIR_KEY}");
    let main_bare = field(&payload, "Bare").to_owned();
    for (bare, commit, file) in [(main_bare, &commits[0], "a.txt"), (sibling_bare(&payload, "lib"), &commits[1], "b.txt")] {
        let git_dir = format!("--git-dir={bare}");
        assert!(has_ref_in(&bare, &back), "{bare} has its own back commit: {outcome:?}");
        assert_eq!(&git(&[&git_dir, "rev-parse", &format!("{back}^")]), commit, "{bare}: on its own snapshot");
        assert_eq!(
            git(&[&git_dir, "show", &format!("{back}:{file}")]),
            "changed",
            "{bare}: with its own change"
        );
    }
    assert!(
        !has_ref_in(&sibling_bare(&payload, "tools"), &back),
        "an unchanged repo gets no back commit"
    );
}

/// A canary folder beside the case folder, so outside every folder the box lays out, holding only `keep.txt`; made
/// afresh, an earlier run's removed first.
fn canary(case: &Case, name: &str) -> PathBuf {
    let mut folder = case.folder().into_os_string();
    folder.push(format!("-{name}"));
    let folder = PathBuf::from(folder);
    if folder.exists() {
        fs::remove_dir_all(&folder).expect("an earlier run's canary can be removed");
    }
    fs::create_dir_all(&folder).expect("the canary folder can be made");
    fs::write(folder.join("keep.txt"), "keep").expect("the canary's file can be written");
    folder
}

/// The names in `folder`.
fn entries_of(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .expect("the folder can be listed")
        .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Whether nothing, not even a link, has the name `path`.
fn is_gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err()
}

/// Whether `path` is a folder itself, not a link or a junction.
fn is_real_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The text of the work folder's layout record, `{Work}.layout`, empty when there is none.
fn layout_record(payload: &Value) -> String {
    fs::read_to_string(format!("{}.layout", field(payload, "Work"))).unwrap_or_default()
}

/// The pair payload of `app` and `lib` whose command, `.\change.cmd`, changes the main tree's `a.txt`, its snapshots
/// pushed; the commits, the main first.
fn changing_pair(case: &Case) -> (Value, Vec<String>) {
    let mut payload = pair_payload(case, &["lib"], &[r".\change.cmd"]);
    let commits = pair_snapshot(
        case,
        &mut payload,
        &[("change.cmd", "@echo changed> a.txt\r\n"), ("a.txt", "main\n")],
        &[("lib", &[("b.txt", "lib\n")])],
    );
    (payload, commits)
}

#[test]
fn preflight_skips_a_junction_at_the_objects_info_folder_and_the_run_repairs_it() {
    let case = Case::new("preflight_skips_info_junction");
    let mut json = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let commit = snapshot(&case, &mut json, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &json).code, Some(0), "the first run passes");
    let work = PathBuf::from(field(&json, "Work"));
    let info = work.join(r".git\objects\info");
    fs::remove_dir_all(&info).expect("the info folder can be removed");
    let target = canary(&case, "canary");
    junction(&info, &target);

    passed(&gate_box(&case, "preflight", &payload(&json), None));
    assert_eq!(entries_of(&target), ["keep.txt"], "the preflight leaves the junction's target alone");

    let outcome = run_box(&case, &renamed(&json, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(entries_of(&target), ["keep.txt"], "the run leaves the junction's target alone");
    assert!(is_real_folder(&info), "the info folder is a folder again");
    assert_eq!(git(&["-C", &shown(&work), "rev-parse", "HEAD"]), commit, "at the snapshot");
    passed(&gate_box(&case, "preflight", &payload(&json), None));
}

/// A key folder of one repo, its first run passed, whose `Work\.git` `link` makes lead to a canary repository: a
/// preflight passes and leaves the canary's config and alternates file byte-identical, and the next run checks out the
/// snapshot in a `.git` folder of the tree's own.
fn preflight_leaves_the_repository_a_linked_git_leads_to_alone(name: &str, link: fn(&Path, &Path)) {
    let case = Case::new(name);
    let mut json = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let commit = snapshot(&case, &mut json, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &json).code, Some(0), "the first run passes");
    let target = canary(&case, "canary-repo");
    git(&["init", "-q", &shown(&target)]);
    let canary_git = target.join(".git");
    let looked = || {
        let config = fs::read(canary_git.join("config")).expect("the canary's config can be read");
        (config, fs::read(canary_git.join(r"objects\info\alternates")).ok())
    };
    let before = looked();
    let work_git = Path::new(field(&json, "Work")).join(".git");
    fs::remove_dir_all(&work_git).expect("the work folder's .git can be removed");
    link(&work_git, &canary_git);

    passed(&gate_box(&case, "preflight", &payload(&json), None));
    assert_eq!(looked(), before, "the preflight leaves the canary's config, remotes and alternates alone");

    let outcome = run_box(&case, &renamed(&json, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(looked(), before, "the run leaves the canary's config, remotes and alternates alone");
    assert!(is_real_folder(&work_git), "the work folder's .git is a folder again");
    assert_eq!(git(&["-C", field(&json, "Work"), "rev-parse", "HEAD"]), commit, "at the snapshot");
}

#[test]
fn preflight_of_one_repo_leaves_the_repository_a_junction_at_its_git_leads_to_alone() {
    preflight_leaves_the_repository_a_linked_git_leads_to_alone("preflight_git_junction_canary", junction);
}

#[test]
fn preflight_of_one_repo_leaves_the_repository_a_gitdir_file_at_its_git_names_alone() {
    preflight_leaves_the_repository_a_linked_git_leads_to_alone("preflight_gitdir_file_canary", |link, target| {
        fs::write(link, format!("gitdir: {}\n", shown(target))).expect("the gitdir file can be written");
    });
}

#[test]
fn preflight_of_one_repo_leaves_the_config_a_link_at_its_git_config_leads_to_alone() {
    let case = Case::new("preflight_git_config_link_canary");
    let mut json = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let commit = snapshot(&case, &mut json, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &json).code, Some(0), "the first run passes");
    let target = canary(&case, "canary-repo");
    git(&["init", "-q", &shown(&target)]);
    let canary_config = target.join(r".git\config");
    let before = fs::read(&canary_config).expect("the canary's config can be read");
    let config = Path::new(field(&json, "Work")).join(r".git\config");
    fs::remove_file(&config).expect("the work folder's config can be removed");
    std::os::windows::fs::symlink_file(&canary_config, &config).expect("this machine lets a test make a symbolic link");

    passed(&gate_box(&case, "preflight", &payload(&json), None));
    assert_eq!(
        fs::read(&canary_config).expect("the canary's config can be read"),
        before,
        "the preflight leaves the canary's config alone"
    );

    let outcome = run_box(&case, &renamed(&json, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        fs::read(&canary_config).expect("the canary's config can be read"),
        before,
        "the run leaves the canary's config alone"
    );
    assert!(
        fs::symlink_metadata(&config).is_ok_and(|metadata| metadata.is_file()),
        "the work folder's config is a file of its own again"
    );
    assert_eq!(git(&["-C", field(&json, "Work"), "rev-parse", "HEAD"]), commit, "at the snapshot");
}

#[test]
fn box_run_of_one_repo_replaces_a_dangling_junction_at_its_objects_folder() {
    let case = Case::new("box_run_dangling_objects_junction");
    let mut json = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let commit = snapshot(&case, &mut json, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &json).code, Some(0), "the first run passes");
    let work = PathBuf::from(field(&json, "Work"));
    let objects = work.join(r".git\objects");
    fs::remove_dir_all(&objects).expect("the objects folder can be removed");
    let target = canary(&case, "canary");
    junction(&objects, &target);
    fs::remove_dir_all(&target).expect("the junction's target can be removed");

    passed(&gate_box(&case, "preflight", &payload(&json), None));
    assert!(is_gone(&target), "the preflight makes nothing at the junction's target");
    assert!(
        !is_gone(&objects) && !is_real_folder(&objects),
        "the preflight leaves the junction in place"
    );

    let outcome = run_box(&case, &renamed(&json, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(is_real_folder(&objects), "the objects folder is a folder again");
    assert!(is_gone(&target), "the run makes nothing at the junction's target");
    assert_eq!(git(&["-C", &shown(&work), "rev-parse", "HEAD"]), commit, "at the snapshot");
    let again = run_box(&case, &renamed(&json, 2));
    assert_eq!(again.code, Some(0), "a second run passes: {again:?}");
}

#[test]
fn box_run_with_a_pair_names_the_repo_after_its_failing_tree_step() {
    let case = Case::new("box_run_pair_names_failing_tree_step");
    let (json, _) = changing_pair(&case);
    assert_eq!(run_box(&case, &json).code, Some(0), "the first run lays out the pair");
    let lib_git = Path::new(&pair_tree(&json, "lib")).join(".git");
    fs::remove_dir_all(&lib_git).expect("lib's .git can be removed");
    fs::create_dir(&lib_git).expect("an empty .git folder can be made");

    let outcome = run_box(&case, &renamed(&json, 1));

    assert_ne!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote git remote add failed for lib (status "),
        "{outcome:?}"
    );
}

#[test]
fn preflight_with_a_pair_names_the_repo_whose_bare_repository_it_cannot_create() {
    let case = Case::new("preflight_pair_names_failing_bare");
    let json = pair_payload(&case, &["lib"], &[FAKE_TOOL, "exit", "0"]);
    let stub = case.folder().join("stub-git");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(
        stub.join("git.cmd"),
        format!(
            concat!(
                "@echo off\r\n",
                "echo %*| findstr /c:\"init --bare\" >nul || goto real\r\n",
                "echo %*| findstr /i /c:\"lib.git\" >nul || goto real\r\n",
                "exit /b 3\r\n:real\r\n\"{}\" %*\r\n",
            ),
            shown(&git_folder().join("git.exe"))
        ),
    )
    .expect("the git stub can be written");

    let outcome = gate_box(&case, "preflight", &payload(&json), Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(3), "{outcome:?}");
    assert_eq!(outcome.stderr.trim_end(), "gate: remote git init --bare failed for lib (status 3)");
}

#[test]
fn box_run_of_one_repo_replaces_a_junction_at_its_git_folder_and_leaves_its_target_alone() {
    let case = Case::new("box_run_replaces_work_git_junction");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    let commit = snapshot(&case, &mut payload, &[("a.txt", "a\n")]);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run passes");
    let target = canary(&case, "canary-repo");
    let target_text = shown(&target);
    git(&["init", "-q", &target_text]);
    let looked = || {
        let config = fs::read(target.join(r".git\config")).expect("the canary's config can be read");
        let head = fs::read(target.join(r".git\HEAD")).expect("the canary's HEAD can be read");
        (config, head, git(&["-C", &target_text, "for-each-ref"]))
    };
    let before = looked();
    let work_git = Path::new(field(&payload, "Work")).join(".git");
    fs::remove_dir_all(&work_git).expect("the work folder's .git can be removed");
    junction(&work_git, &target.join(".git"));

    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(looked(), before, "the canary repository's config, HEAD and refs are untouched");
    assert!(is_real_folder(&work_git), "the work folder's .git is a folder again");
    assert_eq!(git(&["-C", field(&payload, "Work"), "rev-parse", "HEAD"]), commit, "at the snapshot");
}

#[test]
fn box_run_clearing_a_layout_removes_a_junction_without_following_it() {
    let case = Case::new("box_run_clearing_removes_a_junction");
    let (payload, _) = changing_pair(&case);
    let (first, second) = (canary(&case, "canary"), canary(&case, "canary2"));
    let work = PathBuf::from(field(&payload, "Work"));
    fs::create_dir_all(work.join(r"old2\node_modules")).expect("the stale folders can be made");
    junction(&work.join("old"), &first);
    junction(&work.join(r"old2\node_modules\x"), &second);

    // No record is a run of one repo's layout, so the pair's first run clears the work folder.
    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    for canary in [&first, &second] {
        assert_eq!(entries_of(canary), ["keep.txt"], "{} keeps its file: {outcome:?}", canary.display());
    }
    assert!(is_gone(&work.join("old")), "the junction is gone");
    assert!(is_gone(&work.join("old2")), "the folder holding a junction is gone");
    assert_eq!(entries_of(&work), ["app", "lib"], "only the pair's trees");
    assert!(
        layout_record(&payload).eq_ignore_ascii_case("pair app lib"),
        "the record names the pair: {:?}",
        layout_record(&payload)
    );
}

#[test]
fn box_run_that_cannot_clear_a_held_file_refuses_before_the_command() {
    let case = Case::new("box_run_cannot_clear_a_held_file");
    let marker = case.folder().join("marker");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "mark", &shown(&marker)]);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);
    let old = PathBuf::from(field(&payload, "Work")).join("old");
    fs::create_dir_all(&old).expect("the stale folder can be made");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(old.join("held.txt"))
        .expect("the stale file can be held");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let tag = format!("gate-remote-refused: {} layout", field(&payload, "Nonce"));
    let refused = said.iter().position(|line| *line == tag).expect("the box says the layout tag");
    let line = format!("gate: not routed: the builder could not clear {PAIR_KEY}'s work folder: ");
    let why = said
        .get(refused + 1)
        .and_then(|next| next.strip_prefix(line.as_str()))
        .expect("the owner's line follows the tag");
    assert!(why.contains(&shown(&old)), "the line names what could not be removed: {why}");
    assert!(!outcome.stderr.contains("gate-remote-started:"), "refused before the start marker");
    assert!(!marker.exists(), "the command never ran");
    assert_eq!(layout_record(&payload), "changing", "the record says the clear did not finish");

    drop(held);
    let again = run_box(&case, &renamed(&payload, 1));

    assert_eq!(again.code, Some(0), "a clear that did not finish clears again: {again:?}");
    assert!(is_gone(&old), "the stale folder is gone");
    assert!(marker.exists(), "the command ran");
}

#[test]
fn box_run_replaces_a_junction_where_a_tree_or_its_git_folder_should_be() {
    let case = Case::new("box_run_replaces_a_junction_tree");
    let (payload, commits) = changing_pair(&case);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run lays out the pair");
    let lib = PathBuf::from(pair_tree(&payload, "lib"));
    let lib_git = lib.join(".git");
    let first = canary(&case, "canary");
    fs::remove_dir_all(&lib).expect("the sibling's tree can be removed");
    junction(&lib, &first);

    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(entries_of(&first), ["keep.txt"], "the canary is untouched");
    assert!(is_real_folder(&lib), "the tree is a folder again");
    assert_eq!(git(&["-C", &shown(&lib), "rev-parse", "HEAD"]), commits[1], "at its snapshot");

    let second = canary(&case, "canary2");
    fs::remove_dir_all(&lib_git).expect("the sibling's .git can be removed");
    junction(&lib_git, &second);

    let outcome = run_box(&case, &renamed(&payload, 2));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(entries_of(&second), ["keep.txt"], "the canary is untouched");
    assert!(is_real_folder(&lib_git), "the .git is a folder again");
    assert_eq!(git(&["-C", &shown(&lib), "rev-parse", "HEAD"]), commits[1], "at its snapshot");
}

#[test]
fn box_run_pair_then_single_leaves_no_gitlink_and_no_leaf_folder() {
    let case = Case::new("box_run_pair_then_single");
    let (pair, commits) = changing_pair(&case);
    assert_eq!(run_box(&case, &pair).code, Some(0), "the pair run passes");
    let mut single = renamed(&pair, 1);
    let object = single.as_object_mut().expect("the payload is an object");
    object.remove("Pair");
    object.remove("SiblingSnapshots");
    passed(&gate_box(&case, "preflight", &self::payload(&single), None));

    let outcome = run_box(&case, &single);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let mut entries = work_entries(&single);
    entries.sort();
    assert_eq!(entries, [".git", "a.txt", "change.cmd"], "the main repo's tree alone, in the work folder");
    let bare = format!("--git-dir={}", field(&single, "Bare"));
    let back = format!("refs/gate-back/{PAIR_KEY}");
    assert_eq!(
        git(&[&bare, "rev-parse", &format!("{back}^")]),
        commits[0],
        "the back commit is on the snapshot"
    );
    let listed = git(&[&bare, "ls-tree", "-r", &back]);
    assert!(!listed.lines().any(|line| line.starts_with("160000")), "no gitlink: {listed}");
    assert!(layout_record(&single).eq_ignore_ascii_case("single"), "{:?}", layout_record(&single));
}

#[test]
fn box_run_single_then_pair_leaves_no_work_git_folder() {
    let case = Case::new("box_run_single_then_pair");
    let mut single = box_payload(&case, PAIR_KEY, &[r".\change.cmd"]);
    snapshot(&case, &mut single, &[("change.cmd", "@echo changed> a.txt\r\n"), ("a.txt", "main\n")]);
    assert_eq!(run_box(&case, &single).code, Some(0), "the run of one repo passes");
    let mut pair = renamed(&single, 1);
    pair["Pair"] = json!({"Leaf": "app", "Siblings": ["lib"]});
    let commits = pair_snapshot(&case, &mut pair, &[("a.txt", "main again\n")], &[("lib", &[("b.txt", "lib\n")])]);

    let outcome = run_box(&case, &pair);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let mut entries = work_entries(&pair);
    entries.sort();
    assert_eq!(entries, ["app", "lib"], "no .git and no file of the old tree in the work folder");
    assert_eq!(git(&["-C", &pair_tree(&pair, "app"), "rev-parse", "HEAD"]), commits[0]);
}

/// The pair payload of `app`, `lib` and `tools` whose command, `.\change.cmd`, changes `a.txt` in the main tree and
/// `b.txt` in `lib`'s, leaving `tools` unchanged, its snapshots pushed.
fn changing_two_of_three(case: &Case) -> Value {
    changing_two_of_three_after(case, "")
}

/// [`changing_two_of_three`], its command running the cmd lines `first` before it changes anything.
fn changing_two_of_three_after(case: &Case, first: &str) -> Value {
    let mut payload = pair_payload(case, &["lib", "tools"], &[r".\change.cmd"]);
    let change = format!("{first}@echo changed> a.txt\r\n@echo changed> ..\\lib\\b.txt\r\n");
    let change = change.as_str();
    pair_snapshot(
        case,
        &mut payload,
        &[("change.cmd", change), ("a.txt", "main\n")],
        &[("lib", &[("b.txt", "lib\n")]), ("tools", &[("c.txt", "tools\n")])],
    );
    payload
}

#[test]
fn box_run_clears_a_stale_sibling_back_ref_when_the_fetch_fails() {
    let case = Case::new("box_run_clears_stale_sibling_back_ref");
    let (payload, commits) = changing_pair(&case);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run passes");
    let back = format!("refs/gate-back/{PAIR_KEY}");
    let lib_bare = sibling_bare(&payload, "lib");
    let lib_git = format!("--git-dir={lib_bare}");
    git(&[&lib_git, "update-ref", &back, &commits[1]]);
    git(&[&lib_git, "update-ref", "-d", &format!("refs/gate/{PAIR_KEY}")]);
    assert!(has_ref(&payload, &back), "the first run left the main repo's back commit");

    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_ne!(outcome.code, Some(0), "the sibling's fetch fails: {outcome:?}");
    assert!(outcome.stderr.contains("gate: remote fetch lib failed"), "{outcome:?}");
    assert!(!has_ref_in(&lib_bare, &back), "the sibling's stale back ref is gone");
    assert!(!has_ref(&payload, &back), "and the main repo's");
}

#[test]
fn box_run_that_fails_a_sibling_s_push_leaves_no_back_ref_in_either_bare() {
    let case = Case::new("box_run_sibling_push_fails");
    // The command first prints a line that looks like the box's own back commits line.
    let payload = changing_two_of_three_after(&case, &format!("@>&2 echo {}\r\n", forged_back_commits()));
    let stub = case.folder().join("stub-git");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(
        stub.join("git.cmd"),
        format!(
            concat!(
                "@echo off\r\n",
                "echo %*| findstr /c:\" push \" >nul || goto real\r\n",
                "echo %*| findstr /i /c:\"lib.git\" >nul || goto real\r\n",
                "exit /b 1\r\n:real\r\n\"{}\" %*\r\n",
            ),
            shown(&git_folder().join("git.exe"))
        ),
    )
    .expect("the git stub can be written");

    let outcome = run_box_with_path(&case, &payload, Some(&path_for_run(&[stub])));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome.stderr.contains("gate: remote sync-back failed: push lib failed (status 1)"),
        "{outcome:?}"
    );
    let back = format!("refs/gate-back/{PAIR_KEY}");
    assert!(!has_ref(&payload, &back), "the main repo's pushed back ref is deleted");
    assert!(!has_ref_in(&sibling_bare(&payload, "lib"), &back), "the sibling has none");
    let said = lines_starting(&outcome, BACK_COMMITS);
    assert_eq!(
        said.first().copied(),
        Some(forged_back_commits().as_str()),
        "the command's line came: {outcome:?}"
    );
    assert_eq!(
        said.last().copied(),
        Some(BACK_COMMITS_FAILED),
        "the box's failed line is the last: {outcome:?}"
    );
    let lines: Vec<&str> = outcome.stderr.lines().collect();
    let failed_at = lines.iter().rposition(|line| *line == BACK_COMMITS_FAILED);
    let push_at = lines
        .iter()
        .position(|line| *line == "gate: remote sync-back failed: push lib failed (status 1)");
    assert!(failed_at < push_at, "the box says failed before its failure line: {outcome:?}");
}

/// What every back commits line starts with.
const BACK_COMMITS: &str = "gate: remote back commits: ";
/// The box's back commits line for a pair run whose back commits failed or were never made.
const BACK_COMMITS_FAILED: &str = "gate: remote back commits: failed";

/// A back commits line a command prints to pass for the box's, naming the pair `app`, `lib` and `tools`.
fn forged_back_commits() -> String {
    format!("{BACK_COMMITS}app {}, lib {}, tools none", "a".repeat(40), "b".repeat(40))
}

#[test]
fn box_pair_run_that_ends_before_its_command_says_its_back_commits_failed() {
    let case = Case::new("box_pair_run_subfolder_missing");
    let mut payload = changing_two_of_three(&case);
    payload["Subfolder"] = json!("nope");

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("gate: remote subfolder nope does not exist in the snapshot\n{BACK_COMMITS_FAILED}\n"),
        "{outcome:?}"
    );
}

#[test]
fn box_pair_run_whose_command_the_ceiling_kills_says_its_back_commits_failed() {
    let case = Case::new("box_pair_run_ceiling_kill");
    let forged = forged_back_commits();
    let mut payload = pair_payload(&case, &["lib", "tools"], &[FAKE_TOOL, "err", &forged, "sleep", "600000"]);
    payload["Timeout"] = json!(1);
    payload["Host"] = json!("builder");
    pair_snapshot(
        &case,
        &mut payload,
        &[("a.txt", "main\n")],
        &[("lib", &[("b.txt", "lib\n")]), ("tools", &[("c.txt", "tools\n")])],
    );

    // The whole machine free, so the clock runs with wall time however loaded the machine running the case is.
    let outcome = run_box_with_seams(&case, &payload, &["--test-box-sample-ms", "20", "--test-box-free-percent", "100"]);

    assert_eq!(outcome.code, Some(124), "{outcome:?}");
    let said = lines_starting(&outcome, BACK_COMMITS);
    assert_eq!(said.first().copied(), Some(forged.as_str()), "the command's line came: {outcome:?}");
    assert_eq!(
        said.last().copied(),
        Some(BACK_COMMITS_FAILED),
        "the box's failed line is the last: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_with_a_pair_names_each_repo_s_back_commit() {
    let case = Case::new("box_run_pair_names_back_commits");
    let payload = changing_two_of_three(&case);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let back = format!("refs/gate-back/{PAIR_KEY}");
    let main = git(&[&format!("--git-dir={}", field(&payload, "Bare")), "rev-parse", &back]);
    let lib = git(&[&format!("--git-dir={}", sibling_bare(&payload, "lib")), "rev-parse", &back]);
    let line = format!("gate: remote back commits: app {main}, lib {lib}, tools none");
    assert_eq!(
        lines(&outcome.stderr).iter().filter(|said| **said == line).count(),
        1,
        "one line names each repo's back commit: {outcome:?}"
    );
}

#[test]
fn box_run_with_the_same_siblings_in_another_order_keeps_the_work_folder() {
    let case = Case::new("box_run_same_siblings_other_order");
    let mut payload = pair_payload(&case, &["lib", "tools"], &[FAKE_TOOL, "exit", "0"]);
    pair_snapshot(
        &case,
        &mut payload,
        &[("a.txt", "main\n")],
        &[
            ("lib", &[("b.txt", "lib\n"), (".gitignore", "out/\n")]),
            ("tools", &[("c.txt", "tools\n")]),
        ],
    );
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run lays out the pair");
    let marker = Path::new(&pair_tree(&payload, "lib")).join(r"out\marker.txt");
    fs::create_dir_all(marker.parent().expect("the marker has a folder")).expect("the build output folder can be made");
    fs::write(&marker, "built").expect("the marker can be written");
    let mut reordered = renamed(&payload, 1);
    reordered["Pair"] = json!({"Leaf": "app", "Siblings": ["tools", "lib"]});

    let outcome = run_box(&case, &reordered);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(marker.exists(), "the same siblings in another order clear nothing");
}

#[test]
fn preflight_of_one_repo_leaves_a_work_folder_laid_out_for_a_pair_alone() {
    let case = Case::new("preflight_one_repo_on_a_pair_folder");
    let (pair, _) = changing_pair(&case);
    assert_eq!(run_box(&case, &pair).code, Some(0), "the pair run lays out the work folder");
    let mut single = renamed(&pair, 1);
    let object = single.as_object_mut().expect("the payload is an object");
    object.remove("Pair");
    object.remove("SiblingSnapshots");
    let work = PathBuf::from(field(&single, "Work"));

    for record in ["pair app lib", "changing"] {
        fs::write(format!("{}.layout", shown(&work)), record).expect("the layout record can be written");
        let outcome = gate_box(&case, "preflight", &self::payload(&single), None);

        passed(&outcome);
        assert_eq!(
            entries_of(&work),
            ["app", "lib"],
            "a record saying {record:?}: the preflight, holding no lock, makes nothing in the work folder: {outcome:?}"
        );
    }
}

#[test]
fn box_run_with_a_pair_never_touches_a_planted_work_git_folder() {
    let case = Case::new("box_run_pair_planted_work_git");
    let (payload, commits) = changing_pair(&case);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run lays out the pair");
    let work = PathBuf::from(field(&payload, "Work"));
    let work_text = shown(&work);
    git(&["init", "-q", &work_text]);
    fs::write(work.join(r".git\canary.txt"), "keep").expect("the canary file can be written");
    let looked = || {
        let config = fs::read(work.join(r".git\config")).expect("the planted config can be read");
        let head = fs::read(work.join(r".git\HEAD")).expect("the planted HEAD can be read");
        (config, head, git(&["-C", &work_text, "for-each-ref"]))
    };
    let before = looked();

    let outcome = run_box(&case, &renamed(&payload, 1));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(fs::read_to_string(work.join(r".git\canary.txt")).expect("the canary file stands"), "keep");
    assert_eq!(looked(), before, "the planted repository's config, HEAD and refs are untouched");
    assert!(
        !work.join(r".git\index").exists(),
        "no git call staged anything in the planted repository"
    );
    assert_eq!(
        git(&["-C", &pair_tree(&payload, "app"), "rev-parse", "HEAD"]),
        commits[0],
        "the main tree is at its snapshot"
    );
}

#[test]
fn box_run_with_a_pair_refuses_a_subfolder_into_a_sibling() {
    let case = Case::new("box_run_pair_subfolder_into_sibling");
    let marker = case.folder().join("marker");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "mark", &shown(&marker)]);
    payload["Subfolder"] = json!(r"..\lib");
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains(r"gate: remote subfolder ..\lib is not a folder inside the work folder"),
        "{outcome:?}"
    );
    assert!(!marker.exists(), "the command never ran");
}

#[test]
fn box_run_of_one_repo_on_a_fresh_key_folder_writes_no_layout_record() {
    let case = Case::new("box_run_one_repo_no_layout_record");
    let mut payload = box_payload(&case, "k", &[FAKE_TOOL, "exit", "0"]);
    snapshot(&case, &mut payload, &[("a.txt", "a\n")]);

    let outcome = run_box(&case, &payload);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let record = format!("{}.layout", field(&payload, "Work"));
    assert!(is_gone(Path::new(&record)), "no record: a run of one repo's layout needs none");
}

/// Why the run of `payload` was refused `layout`: what follows the owner's line, which follows the refusal tag; the
/// outcome refused before the start marker with 75.
fn layout_why(outcome: &Outcome, payload: &Value) -> String {
    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(!outcome.stderr.contains("gate-remote-started:"), "refused before the start marker");
    let said = lines(&outcome.stderr);
    let tag = format!("gate-remote-refused: {} layout", field(payload, "Nonce"));
    let refused = said.iter().position(|line| *line == tag).expect("the box says the layout tag");
    let line = format!("gate: not routed: the builder could not clear {PAIR_KEY}'s work folder: ");
    said.get(refused + 1)
        .and_then(|next| next.strip_prefix(line.as_str()))
        .expect("the owner's line follows the tag")
        .to_owned()
}

#[test]
fn box_run_with_a_folder_for_its_layout_record_refuses_with_layout() {
    let case = Case::new("box_run_layout_record_is_a_folder");
    let marker = case.folder().join("marker");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "mark", &shown(&marker)]);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);
    let record = PathBuf::from(format!("{}.layout", field(&payload, "Work")));
    fs::create_dir_all(&record).expect("a folder can take the record's name");

    let outcome = run_box(&case, &payload);

    let why = layout_why(&outcome, &payload);
    assert!(why.contains(&shown(&record)), "the line names the record it could not write: {why}");
    assert!(!marker.exists(), "the command never ran");
    assert!(is_real_folder(&record), "the folder at the record's name stands");
}

#[test]
fn box_run_that_cannot_write_a_tree_s_alternates_refuses_with_layout() {
    let case = Case::new("box_run_cannot_write_alternates");
    let marker = case.folder().join("marker");
    let mut payload = pair_payload(&case, &["lib"], &[FAKE_TOOL, "mark", &shown(&marker)]);
    pair_snapshot(&case, &mut payload, &[("a.txt", "a\n")], &[("lib", &[("b.txt", "b\n")])]);
    assert_eq!(run_box(&case, &payload).code, Some(0), "the first run lays out the pair");
    fs::remove_file(&marker).expect("the first run's marker can be removed");
    let alternates = Path::new(&pair_tree(&payload, "lib")).join(r".git\objects\info\alternates");
    fs::remove_file(&alternates).expect("the alternates file can be removed");
    fs::create_dir(&alternates).expect("a folder can take the alternates file's name");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(alternates.join("held.txt"))
        .expect("a file in it can be held");
    let again = renamed(&payload, 1);

    let outcome = run_box(&case, &again);

    let why = layout_why(&outcome, &again);
    assert!(why.contains(&shown(&alternates)), "the line names the alternates file: {why}");
    assert!(!marker.exists(), "the command never ran");
    drop(held);
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

/// The lockfile a pnpm case's snapshot holds, and its path at the work folder's root.
const PNPM_LOCKFILE: &str = "lockfileVersion: '9.0'\n";
const PNPM_LOCKFILE_PATH: &str = "pnpm-lock.yaml";
/// The ignore file of a pnpm case's snapshot, ignoring `node_modules` with the folder pattern most repos use.
const NODE_MODULES_IGNORED: (&str, &str) = (".gitignore", "node_modules/\n");

/// A folder holding a `pnpm.cmd` that stands for pnpm, both the box's install and the command: each call appends
/// `{working folder}|{arguments}|CI={%CI%}` to the case's counter file; an `install` then runs the batch script
/// `before`, makes `node_modules` in its working folder, prints `install: out` and exits with `exit`, and any other
/// call exits 0. A case passes it through [`path_for_pnpm`], so its `PATH` holds this pnpm and no other.
fn stub_pnpm(case: &Case, exit: i32, before: &str) -> PathBuf {
    let stub = case.folder().join(format!("stub-pnpm-{exit}"));
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    fs::write(
        stub.join("pnpm.cmd"),
        format!(
            concat!(
                "@echo off\r\n>>\"{counter}\" echo %CD%^|%*^|CI=%CI%\r\nif not \"%1\"==\"install\" exit /b 0\r\n",
                "{before}if not exist node_modules mkdir node_modules\r\necho install: out\r\nexit /b {exit}\r\n"
            ),
            counter = shown(&counter(case)),
            before = before,
            exit = exit,
        ),
    )
    .expect("the pnpm stub can be written");
    stub
}

/// Whether `folder` holds a `pnpm` named with any of the extensions a `PATHEXT` search tries.
fn holds_pnpm(folder: &Path) -> bool {
    ["exe", "cmd", "bat", "com"]
        .iter()
        .any(|extension| folder.join(format!("pnpm.{extension}")).is_file())
}

/// [`path_for_run`] less every folder holding a `pnpm` too, so the box finds the stub's pnpm or none.
fn path_for_pnpm(folders: &[PathBuf]) -> OsString {
    let own = path_for_run(&[]);
    let without_pnpm = std::env::split_paths(&own).filter(|folder| !holds_pnpm(folder));
    std::env::join_paths(folders.iter().cloned().chain(without_pnpm)).expect("the folders join into a PATH")
}

/// `gate box run` of `payload` with `PATH` set to `path` and no `CI` variable, so only the box's install sets one.
fn pnpm_box(case: &Case, payload: &Value, path: &OsString) -> BoxCall {
    let mut call = box_call(case, &["box", "run"], &self::payload(payload));
    call.env("PATH", path);
    call.env_remove("CI");
    call
}

/// The calls the case's stub pnpm saw, each `{working folder}|{arguments}|CI={value}`, the folder in upper case.
fn pnpm_calls(case: &Case) -> Vec<String> {
    counter_lines(case)
        .iter()
        .map(|line| match line.split_once('|') {
            Some((folder, rest)) => format!("{}|{rest}", folder.to_uppercase()),
            None => line.clone(),
        })
        .collect()
}

/// The call of the box's install the stub pnpm sees in `folder`.
fn install_call(folder: &str) -> String {
    format!("{}|install --frozen-lockfile|CI=true", folder.to_uppercase())
}

/// The call of the command `words` the stub pnpm sees in `folder`, run without a `CI` variable.
fn command_call(folder: &str, words: &str) -> String {
    format!("{}|{words}|CI=", folder.to_uppercase())
}

/// How many of the stub pnpm's calls were installs.
fn installs(case: &Case) -> usize {
    pnpm_calls(case)
        .iter()
        .filter(|call| call.contains("|install --frozen-lockfile|"))
        .count()
}

/// Whether `outcome` said the install's elapsed line, ending in `tail` after the seconds.
fn said_install_took(outcome: &Outcome, tail: &str) -> bool {
    let shape = Regex::new(&format!(r"^gate: pnpm install took \d+\.\d s{}$", regex::escape(tail))).expect("the pattern compiles");
    outcome.stderr.lines().any(|line| shape.is_match(line))
}

/// The packages record of `payload`'s work folder.
fn packages_record(payload: &Value) -> String {
    format!("{}.packages", field(payload, "Work"))
}

#[test]
fn box_run_installs_pnpm_packages_before_a_pnpm_command() {
    let case = Case::new("box_run_pnpm_install");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(
        &case,
        &mut payload,
        &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE), ("package.json", "{}\n")],
    );
    let stub = stub_pnpm(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case),
        [install_call(work), command_call(work, "run x")],
        "the install in the work root with CI=true, then the command: {outcome:?}"
    );
    assert!(
        outcome.stdout.contains("install: out"),
        "the install's output passes through: {outcome:?}"
    );
    assert!(said_install_took(&outcome, ""), "the elapsed line, no folder for the root: {outcome:?}");
    let record = fs::read_to_string(packages_record(&payload)).expect("the record is written");
    let shape = Regex::new(r"^\.\t[0-9a-f]{64}\n$").expect("the pattern compiles");
    assert!(shape.is_match(&record), "the root's line: {record:?}");
    assert!(Path::new(work).join("node_modules").is_dir(), "the install's folder stays");
}

#[test]
fn box_run_skips_the_pnpm_install_while_the_lockfile_and_node_modules_are_unchanged() {
    let case = Case::new("box_run_pnpm_unchanged");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    let first = pnpm_box(&case, &payload, &path).finish();
    assert_eq!(first.code, Some(0), "{first:?}");

    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "{second:?}");
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case),
        [install_call(work), command_call(work, "run x"), command_call(work, "run x")],
        "one install, the command in both runs: {second:?}"
    );
    assert!(!second.stderr.contains("gate: pnpm install took"), "{second:?}");
}

#[test]
fn box_run_installs_again_when_the_lockfile_changes() {
    let case = Case::new("box_run_pnpm_lockfile_changed");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    assert_eq!(pnpm_box(&case, &payload, &path).finish().code, Some(0));
    let record = fs::read_to_string(packages_record(&payload)).expect("the record is written");

    let changed = push_snapshot(&case, &payload, &[(PNPM_LOCKFILE_PATH, "lockfileVersion: '9.1'\n")]);
    payload["Snapshot"] = json!(changed);
    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(installs(&case), 2, "the changed lockfile installs again: {second:?}");
    assert!(said_install_took(&second, ""), "{second:?}");
    assert_ne!(
        fs::read_to_string(packages_record(&payload)).expect("the record is read"),
        record,
        "the record holds the new hash"
    );
}

#[test]
fn box_run_installs_again_when_node_modules_is_gone() {
    let case = Case::new("box_run_pnpm_node_modules_gone");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    assert_eq!(pnpm_box(&case, &payload, &path).finish().code, Some(0));
    fs::remove_dir_all(Path::new(field(&payload, "Work")).join("node_modules")).expect("node_modules can be removed");

    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(installs(&case), 2, "a missing node_modules installs again: {second:?}");
}

#[test]
fn box_run_installs_in_the_dir_folder_and_names_it() {
    let case = Case::new("box_run_pnpm_dir");
    let mut payload = box_payload(&case, "k", &["pnpm", "--dir", "sub", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, ("sub/pnpm-lock.yaml", PNPM_LOCKFILE)]);
    let stub = stub_pnpm(&case, 0, "");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case),
        [install_call(&format!(r"{work}\sub")), command_call(work, "--dir sub run x")],
        "the install in the folder `--dir` names: {outcome:?}"
    );
    assert!(said_install_took(&outcome, " in sub"), "the line names the folder: {outcome:?}");
    let record = fs::read_to_string(packages_record(&payload)).expect("the record is written");
    let shape = Regex::new(r"^sub\t[0-9a-f]{64}\n$").expect("the pattern compiles");
    assert!(shape.is_match(&record), "the folder's line: {record:?}");
}

#[test]
fn box_run_does_not_install_without_a_lockfile_or_for_another_program() {
    let case = Case::new("box_run_pnpm_no_lockfile");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, ("package.json", "{}\n")]);
    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        pnpm_calls(&case),
        [command_call(field(&payload, "Work"), "run x")],
        "no install: {outcome:?}"
    );
    assert!(!outcome.stderr.contains("pnpm install"), "nothing is said: {outcome:?}");
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record");

    let other = Case::new("box_run_pnpm_other_program");
    let marker = other.folder().join("marker");
    let mut payload = box_payload(&other, "k", &[FAKE_TOOL, "mark", &shown(&marker)]);
    snapshot(&other, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let outcome = pnpm_box(&other, &payload, &path_for_pnpm(&[stub_pnpm(&other, 0, "")])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(marker.exists(), "the command ran: {outcome:?}");
    assert!(pnpm_calls(&other).is_empty(), "another program installs nothing: {outcome:?}");
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record");
}

#[test]
fn box_run_refuses_tools_when_the_pnpm_install_fails() {
    let case = Case::new("box_run_pnpm_install_fails");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    assert_eq!(
        pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish().code,
        Some(0)
    );
    let record = fs::read(packages_record(&payload)).expect("the record is written");

    let changed = push_snapshot(&case, &payload, &[(PNPM_LOCKFILE_PATH, "lockfileVersion: '9.1'\n")]);
    payload["Snapshot"] = json!(changed);
    let second = renamed(&payload, 1);
    let outcome = pnpm_box(&case, &second, &path_for_pnpm(&[stub_pnpm(&case, 3, "")])).finish();

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let tag = refused_line(&second, "tools");
    let failed = said.iter().position(|line| *line == "gate: remote package restore failed (status 3)");
    let tagged = said.iter().position(|line| *line == tag);
    let refused = said
        .iter()
        .position(|line| *line == "gate: remote work folder k could not restore its pnpm packages");
    assert!(
        matches!((failed, tagged, refused), (Some(failed), Some(tagged), Some(refused)) if failed < tagged && tagged + 1 == refused),
        "the status line, then the tag just before the packages line: {outcome:?}"
    );
    assert!(said_install_took(&outcome, ""), "the elapsed line of a failed install: {outcome:?}");
    assert!(
        !said.iter().any(|line| line.starts_with("gate-remote-started: ")),
        "the command never started: {outcome:?}"
    );
    let work = field(&payload, "Work");
    assert_eq!(
        pnpm_calls(&case).last(),
        Some(&install_call(work)),
        "the failed install was the last call: {outcome:?}"
    );
    assert_eq!(installs(&case), 2, "{outcome:?}");
    assert_eq!(
        fs::read(packages_record(&payload)).expect("the record is read"),
        record,
        "the record stands"
    );
    assert!(!has_ref(&payload, "refs/gate-back/k"), "the refused run made no back commit");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_refuses_tools_when_pnpm_cannot_start() {
    let case = Case::new("box_run_pnpm_cannot_start");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[])).finish();

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    // The win32 error `ERROR_FILE_NOT_FOUND`, as `locate` reports a word it does not find.
    let cannot_start = format!("gate: remote could not start the pnpm install: {}", gate_win32::win32_message(2));
    assert_eq!(
        lines(&outcome.stderr),
        [
            cannot_start.as_str(),
            "gate: remote package restore failed (status 1)",
            refused_line(&payload, "tools").as_str(),
            "gate: remote work folder k could not restore its pnpm packages",
        ],
        "{outcome:?}"
    );
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record");
    assert!(!has_ref(&payload, "refs/gate-back/k"), "the refused run made no back commit");
}

#[test]
fn box_run_skips_the_install_when_node_modules_is_not_ignored() {
    let case = Case::new("box_run_pnpm_not_ignored");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[(PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish();

    assert_eq!(outcome.code, Some(0), "the command still runs: {outcome:?}");
    assert!(
        lines(&outcome.stderr).contains(&"gate: remote pnpm install skipped: node_modules is not ignored in ."),
        "{outcome:?}"
    );
    assert_eq!(
        pnpm_calls(&case),
        [command_call(field(&payload, "Work"), "run x")],
        "no install: {outcome:?}"
    );
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record");
}

#[test]
fn box_run_stops_a_pnpm_install_and_names_its_job() {
    let case = Case::new("box_run_pnpm_stop_during_install");
    let mut payload = box_payload(&case, "k", &["pnpm", "run", "x"]);
    snapshot(&case, &mut payload, &[NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)]);
    let stub = stub_pnpm(&case, 0, &held_restore(&case.folder().join("never")));
    let running = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).start();
    let helper = spawned_pid(&running.wait_for_stdout("\n"));

    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    let outcome = running.finish();

    let job = format!("{}-packages", field(&payload, "Job"));
    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(stopped.stdout, format!("gate: remote stopped job {job}\n"), "{stopped:?}");
    assert_eq!(stopped.stderr, "", "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop during the install fails the run: {outcome:?}");
    let said = stopped_line_for(&job);
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert_eq!(
        pnpm_calls(&case),
        [install_call(field(&payload, "Work"))],
        "the command never ran: {outcome:?}"
    );
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record is written");
    wait_until_gone(helper);
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

/// A pnpm case's payload of `pnpm run x` with `files` snapshotted beside the ignore file and the lockfile.
fn pnpm_payload(case: &Case, files: &[(&str, &str)]) -> Value {
    let mut payload = box_payload(case, "k", &["pnpm", "run", "x"]);
    let mut all = vec![NODE_MODULES_IGNORED, (PNPM_LOCKFILE_PATH, PNPM_LOCKFILE)];
    all.extend_from_slice(files);
    snapshot(case, &mut payload, &all);
    payload
}

#[test]
fn box_run_installs_again_when_npmrc_changes() {
    let case = Case::new("box_run_pnpm_npmrc_changed");
    let mut payload = pnpm_payload(&case, &[(".npmrc", "a=1\n")]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    assert_eq!(pnpm_box(&case, &payload, &path).finish().code, Some(0));

    let changed = push_snapshot(&case, &payload, &[(".npmrc", "a=2\n")]);
    payload["Snapshot"] = json!(changed);
    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(installs(&case), 2, "a changed .npmrc installs again: {second:?}");
}

#[test]
fn box_run_runs_the_pnpm_install_with_the_restricted_token() {
    let case = Case::new("box_run_pnpm_token");
    let payload = pnpm_payload(&case, &[]);
    let token = case.folder().join("token.txt");
    let stub = stub_pnpm(&case, 0, &format!("\"{FAKE_TOOL}\" token >\"{}\"\r\n", shown(&token)));

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = fs::read_to_string(&token).expect("the install wrote its token's facts");
    let lines: Vec<&str> = said.lines().collect();
    assert!(lines.contains(&"not-elevated"), "{lines:?}");
    assert!(lines.contains(&"owner user"), "{lines:?}");
    assert!(lines.contains(&restricted_dacl_line().as_str()), "the restricted token's DACL: {lines:?}");
}

#[test]
fn box_run_a_killed_box_takes_its_pnpm_install_with_it() {
    let case = Case::new("box_run_pnpm_killed_box");
    let payload = pnpm_payload(&case, &[]);
    let stub = stub_pnpm(&case, 0, &held_restore(&case.folder().join("never")));
    let running = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).start();
    let helper = spawned_pid(&running.wait_for_stdout("\n"));
    let install_job = format!("{}-packages", field(&payload, "Job"));
    // Listed through a handle closed again at once, so the box's is once more the job's only one.
    let install = OpenedJob::open(&install_job)
        .expect("the install's job can be looked for")
        .expect("the install runs in its job")
        .process_ids()
        .expect("the install's processes can be listed");
    assert!(install.contains(&helper), "the helper is in the install's job: {install:?}");

    let killed = running.kill();

    for pid in install {
        wait_until_gone(pid);
    }
    assert_eq!(
        pnpm_calls(&case),
        [install_call(field(&payload, "Work"))],
        "the command never ran: {killed:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_packages_record_that_is_a_folder_fails_with_status_1() {
    let case = Case::new("box_run_pnpm_record_folder");
    let payload = pnpm_payload(&case, &[]);
    let record = packages_record(&payload);
    fs::create_dir_all(&record).expect("a folder can be put where the record goes");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish();

    assert_eq!(outcome.code, Some(1), "a record that cannot be written fails the box: {outcome:?}");
    let refused = format!("gate: remote could not write {record}: ");
    assert!(
        outcome.stderr.lines().any(|line| line.starts_with(&refused)),
        "{refused:?} in {outcome:?}"
    );
    assert_eq!(
        pnpm_calls(&case),
        [install_call(field(&payload, "Work"))],
        "the install ran before the write, the command never: {outcome:?}"
    );
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_an_unreadable_packages_record_installs_and_says_so() {
    let case = Case::new("box_run_pnpm_record_unreadable");
    let payload = pnpm_payload(&case, &[]);
    let record = packages_record(&payload);
    // Not UTF-8, so the record opens but cannot be read as text.
    fs::write(&record, [0xff_u8, 0xfe, b'\n']).expect("the record can be written");

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = format!("gate: remote could not read {record}: ");
    assert!(
        outcome
            .stderr
            .lines()
            .any(|line| line.starts_with(&said) && line.ends_with("; installing the packages")),
        "{said:?} in {outcome:?}"
    );
    assert_eq!(installs(&case), 1, "{outcome:?}");
    let text = fs::read_to_string(&record).expect("the record is rewritten");
    assert!(
        Regex::new(r"^\.\t[0-9a-f]{64}\n$").expect("the pattern compiles").is_match(&text),
        "{text:?}"
    );
}

#[test]
fn box_run_an_unreadable_package_file_fails_with_status_1() {
    let case = Case::new("box_run_pnpm_package_file_unreadable");
    // A folder where `package.json` is expected: a file in it makes git check the folder out.
    let payload = pnpm_payload(&case, &[("package.json/x.txt", "x\n")]);

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub_pnpm(&case, 0, "")])).finish();

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let said = format!(
        "gate: remote could not read the package file {}: ",
        Path::new(field(&payload, "Work")).join("package.json").display()
    );
    assert!(outcome.stderr.lines().any(|line| line.starts_with(&said)), "{said:?} in {outcome:?}");
    assert!(pnpm_calls(&case).is_empty(), "no install, no command: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_stop_set_before_the_pnpm_install_runs_none() {
    let case = Case::new("box_run_pnpm_stop_before_install");
    let payload = pnpm_payload(&case, &[]);
    let stub = stub_pnpm(&case, 0, "");
    let parked = case.folder().join("parked");
    let go = case.folder().join("go");
    stub_git(&stub, &parked, &go);
    let running = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).start();
    wait_until("the box parked in its first git step, before the install", || parked.exists());

    let stopped = gate_box(&case, "stop", &self::payload(&payload), None);
    let_go(&go);
    let outcome = running.finish();

    assert_eq!(stopped.code, Some(0), "{stopped:?}");
    assert_eq!(outcome.code, Some(1), "a stop before the install fails the run: {outcome:?}");
    let said = stopped_line_for(field(&payload, "Job"));
    assert!(outcome.stderr.lines().any(|line| line == said), "{said:?} in {outcome:?}");
    assert!(pnpm_calls(&case).is_empty(), "no install, no command: {outcome:?}");
    wait_until_closed(Path::new(field(&payload, "Lock")));
}

#[test]
fn box_run_a_node_modules_junction_installs_again() {
    let case = Case::new("box_run_pnpm_node_modules_junction");
    let payload = pnpm_payload(&case, &[]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    assert_eq!(pnpm_box(&case, &payload, &path).finish().code, Some(0));
    let node_modules = Path::new(field(&payload, "Work")).join("node_modules");
    fs::remove_dir_all(&node_modules).expect("node_modules can be removed");
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the junction's target can be made");
    junction(&node_modules, &elsewhere);

    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(installs(&case), 2, "a junction is no node_modules: {second:?}");
}

#[test]
fn box_run_skips_the_install_when_the_lockfile_folder_is_reached_through_a_junction() {
    let case = Case::new("box_run_pnpm_linked_folder");
    let mut payload = box_payload(&case, "k", &["pnpm", "--dir", "linked", "run", "x"]);
    snapshot(&case, &mut payload, &[(".gitignore", "node_modules/\nlinked\n")]);
    let path = path_for_pnpm(&[stub_pnpm(&case, 0, "")]);
    // The first run lays the work folder out; the junction made after it is ignored, so no checkout removes it.
    assert_eq!(pnpm_box(&case, &payload, &path).finish().code, Some(0));
    let elsewhere = case.folder().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("the junction's target can be made");
    fs::write(elsewhere.join("pnpm-lock.yaml"), PNPM_LOCKFILE).expect("the lockfile can be written");
    junction(&Path::new(field(&payload, "Work")).join("linked"), &elsewhere);

    let second = pnpm_box(&case, &renamed(&payload, 1), &path).finish();

    assert_eq!(second.code, Some(0), "the command still runs: {second:?}");
    assert!(
        lines(&second.stderr).contains(&"gate: remote pnpm install skipped: linked is reached through a link"),
        "{second:?}"
    );
    assert_eq!(installs(&case), 0, "{second:?}");
    assert!(!Path::new(&packages_record(&payload)).exists(), "no record");
}

#[test]
fn box_run_restores_dotnet_tools_before_pnpm_packages() {
    let case = Case::new("box_run_pnpm_after_tools");
    let payload = pnpm_payload(&case, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let path = path_for_pnpm(&[stub_dotnet(&case, 0), stub_pnpm(&case, 0, "")]);

    let outcome = pnpm_box(&case, &payload, &path).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = lines(&outcome.stderr);
    let tools = said.iter().position(|line| line.starts_with("gate: dotnet tool restore took "));
    let packages = said.iter().position(|line| line.starts_with("gate: pnpm install took "));
    assert!(
        matches!((tools, packages), (Some(tools), Some(packages)) if tools < packages),
        "the tool restore, then the install: {outcome:?}"
    );
    let (out_tools, out_packages) = (outcome.stdout.find("restore: out"), outcome.stdout.find("install: out"));
    assert!(
        matches!((out_tools, out_packages), (Some(tools), Some(packages)) if tools < packages),
        "{outcome:?}"
    );
}

#[test]
fn box_run_a_tools_refusal_runs_no_pnpm_install() {
    let case = Case::new("box_run_pnpm_tools_refused");
    let payload = pnpm_payload(&case, &[(TOOLS_MANIFEST_PATH, TOOLS_MANIFEST)]);
    let path = path_for_pnpm(&[stub_dotnet(&case, 1), stub_pnpm(&case, 0, "")]);

    let outcome = pnpm_box(&case, &payload, &path).finish();

    assert_eq!(outcome.code, Some(75), "{outcome:?}");
    assert!(
        lines(&outcome.stderr).contains(&"gate: remote work folder k could not restore its dotnet tools"),
        "{outcome:?}"
    );
    assert_eq!(installs(&case), 0, "{outcome:?}");
    assert!(!outcome.stderr.contains("pnpm"), "nothing of pnpm is said: {outcome:?}");
}

#[test]
fn box_run_the_pnpm_install_gets_safe_directory() {
    let case = Case::new("box_run_pnpm_safe_directory");
    let payload = pnpm_payload(&case, &[]);
    let variables = case.folder().join("variables.txt");
    let stub = stub_pnpm(&case, 0, &format!("set GIT_CONFIG >\"{}\"\r\n", shown(&variables)));

    let outcome = pnpm_box(&case, &payload, &path_for_pnpm(&[stub])).finish();

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let said = fs::read_to_string(&variables).expect("the install wrote its git variables");
    let index = said
        .lines()
        .find_map(|line| line.strip_prefix("GIT_CONFIG_KEY_")?.strip_suffix("=safe.directory"))
        .expect("the install's variables hold a safe.directory entry");
    assert!(
        said.lines().any(|line| line == format!("GIT_CONFIG_VALUE_{index}=*")),
        "safe.directory is `*`: {said:?}"
    );
}

/// A command the box's ceiling cases run that never ends on its own within any case's time.
const ENDLESS: &[&str] = &[FAKE_TOOL, "err", "started", "sleep", "600000"];

/// `gate box run` of `payload` with `seams` before the verb, to its end.
fn run_box_with_seams(case: &Case, payload: &Value, seams: &[&str]) -> Outcome {
    let mut args = vec!["box"];
    args.extend_from_slice(seams);
    args.push("run");
    box_call(case, &args, &self::payload(payload)).finish()
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

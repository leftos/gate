//! The bridge to remote.ps1: a routed run whose client is a pwsh on `remote-bridge.ps1`, beside the gate under test,
//! which dot-sources the stand-in remote.ps1 a case writes and names with `--test-remote-script`. The stand-in's
//! `Invoke-RemoteCommand` records what it was given and answers as the case asks; a case orders on the marker files it
//! writes and waits for, never on a clock.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::{fs, process};

use jiff::Timestamp;
use serde_json::{Value, json};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::install::checked_in;
use crate::routing::{
    BUSY_LINE, CONTROL_C_EXIT, HOST, assert_fell_back, assert_refused, heavy_words_with_limit, names, remote_claim, repo, routed_case,
};
use crate::support::{Case, GATE, Outcome, Running, assert_in_order, events, finish, shown, start, wait_for_file, wait_until};

/// What the gate says once on the first Ctrl+C of a call.
const STOPPING: &str = "gate: stopping the remote command; Ctrl+C again to kill it here";
/// The command every case routes: fake-tool, as `tool.exe` in the case's worktree, printing a line.
const COMMAND: [&str; 3] = [r".\tool.exe", "out", "ran here"];

/// The functions the stand-in remote.ps1 defines before its `Invoke-RemoteCommand`: the host check the bridge script
/// calls, as remote.ps1 has it for a plain host name, and those a case's body calls.
const PRELUDE: &str = r#"
function Test-RemoteHostAlias([string]$Value) {
    return $Value -cmatch '^[A-Za-z0-9._-]+$'
}

function Write-Record([string]$Path, $Value) {
    $text = ConvertTo-Json -InputObject $Value -Depth 5
    [System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
}

function Write-Call([string]$Path, $Options, $Command) {
    Write-Record $Path ([ordered]@{
        args    = @([Environment]::GetCommandLineArgs())
        options = [ordered]@{
            Keys           = @($Options.Keys | Sort-Object)
            HostAlias      = $Options.HostAlias
            Root           = $Options.Root
            Repo           = $Options.Repo
            Key            = $Options.Key
            Subfolder      = $Options.Subfolder
            Outputs        = @($Options.Outputs)
            Log            = $Options.Log
            TimeoutSeconds = $Options.TimeoutSeconds
            StallSeconds   = $Options.StallSeconds
            Tail           = $Options.Tail
            Clock          = $Options.Clock.GetType().FullName
        }
        command = @($Command)
        git     = @(Get-ChildItem env: | Where-Object Name -like 'GIT_*' | ForEach-Object Name | Sort-Object)
    })
}

function Write-Marker([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText("$Path.part", $Text)
    [System.IO.File]::Move("$Path.part", $Path)
}

function Wait-Marker([string]$Path) {
    while (-not (Test-Path -LiteralPath $Path)) { Start-Sleep -Milliseconds 20 }
}

function Add-LogLine([string]$Path, [string]$Line) {
    $stream = [System.IO.FileStream]::new($Path, 'Append', 'Write', 'ReadWrite, Delete')
    try {
        $bytes = [System.Text.UTF8Encoding]::new($false).GetBytes("$Line`n")
        $stream.Write($bytes, 0, $bytes.Length)
    } finally {
        $stream.Dispose()
    }
}

function Send-CtrlC {
    if (-not ('GateSuite.Console' -as [type])) {
        Add-Type -Namespace GateSuite -Name Console -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroup);
'@
    }
    if (-not [GateSuite.Console]::GenerateConsoleCtrlEvent(0, 0)) {
        throw "GenerateConsoleCtrlEvent failed: $([System.Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }
}
"#;

/// Puts the checked-in bridge script beside the gate under test, where the gate looks for it, once: copied to a name
/// of this process's own and moved into place, so no gate reads it half written.
fn install_shim() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let target = shim();
        let staged = target.with_file_name(format!("remote-bridge.ps1.{}.part", process::id()));
        fs::copy(checked_in("remote-bridge.ps1"), &staged).expect("the bridge script can be copied beside the gate");
        fs::rename(&staged, &target).expect("the bridge script can be moved into place");
    });
}

/// The bridge script's path beside the gate under test.
fn shim() -> PathBuf {
    Path::new(GATE).with_file_name("remote-bridge.ps1")
}

/// Writes the stand-in remote.ps1 `name` in the case folder, whose `Invoke-RemoteCommand` runs `body`, with the bridge
/// script installed; its path, as `--test-remote-script` takes it.
fn stand_in(case: &Case, name: &str, body: &str) -> String {
    stand_in_with(case, name, "", body)
}

/// [`stand_in`], with `top` run when the bridge script dot-sources it, before the script says the run started.
fn stand_in_with(case: &Case, name: &str, top: &str, body: &str) -> String {
    install_shim();
    let path = case.folder().join(name);
    let text = format!("{PRELUDE}\n{top}\nfunction Invoke-RemoteCommand {{\n    param($Options, $Command)\n{body}\n}}\n");
    fs::write(&path, text).expect("the stand-in remote.ps1 can be written");
    shown(&path)
}

/// `path` as a single-quoted PowerShell string.
fn quoted(path: &Path) -> String {
    format!("'{}'", shown(path).replace('\'', "''"))
}

/// The line that ends a stand-in's body: the attempt's status, kill flag and reason, as `Invoke-RemoteCommand` returns them.
fn answer(status: i64, killed: bool, reason: Option<&str>) -> String {
    let reason = reason.map_or_else(|| "$null".to_owned(), |reason| format!("'{reason}'"));
    format!("    return [pscustomobject]@{{ Status = {status}; Killed = ${killed}; Reason = {reason} }}")
}

/// The words of a heavy run logging to `log` whose remote attempt goes through the bridge to the remote.ps1 `script`,
/// `extra` before `--` and `command` after it.
fn bridge_words<'a>(log: &'a str, script: &'a str, extra: &[&'a str], command: &[&'a str]) -> Vec<&'a str> {
    let mut before = vec!["--test-remote-script", script];
    before.extend(extra);
    heavy_words_with_limit(log, &before, command)
}

/// The JSON a stand-in recorded at `path`.
fn recorded(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("the stand-in recorded its call")).expect("the record is JSON")
}

/// The `client could not start` line of a routed run whose bridge could not run for `why`.
fn client_line(why: &str) -> String {
    format!("gate: remote {HOST} client could not start: {why}; running here")
}

/// Asserts a run that fell back with reason `client` and ran the command here: status 0, `line` said and the command's
/// output.
fn assert_client_fallback(outcome: &Outcome, line: &str) {
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(line), "{line:?} is said: {outcome:?}");
    assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
}

/// Writes the go files when dropped, so a stand-in a failing case leaves waiting ends. A case declares it after its
/// [`Case`] and before its running gate.
struct LetGo(Vec<PathBuf>);

impl Drop for LetGo {
    fn drop(&mut self) {
        for path in &self.0 {
            // The folder may already be gone; the gate's job ends a stand-in that cannot see the file.
            let _ = fs::write(path, "");
        }
    }
}

/// The pwsh a stand-in runs in, from the pid it wrote to `started`, with its start, so a later process given the same
/// pid is not taken for it.
fn started_pwsh(started: &Path) -> (u32, Timestamp) {
    wait_for_file(started);
    let pid: u32 = fs::read_to_string(started)
        .expect("the started marker can be read")
        .trim()
        .parse()
        .expect("the started marker holds a pid");
    let at = gate_win32::process_start_of(pid)
        .expect("the stand-in's pwsh can be queried")
        .expect("the stand-in's pwsh runs while it waits for its go file");
    (pid, at)
}

/// Whether the process `pid` that started `at` has ended.
fn gone((pid, at): (u32, Timestamp)) -> bool {
    !matches!(gate_win32::process_start_of(pid), Ok(Some(start)) if start == at)
}

/// Returns once the process `pid` that started `at` has ended.
fn wait_gone(pwsh: (u32, Timestamp)) {
    wait_until(&format!("pwsh {} ending", pwsh.0), || gone(pwsh));
}

/// Starts the gate with `args` in a console of its own with no window, which the pwsh it starts shares, so a Ctrl+C a
/// stand-in sends to its console reaches the gate and pwsh and nothing else. The suite first lets the processes it
/// starts see Ctrl+C, which they would otherwise inherit ignoring when the suite itself was started so.
fn start_in_own_console(case: &Case, args: &[&str]) -> Running {
    static ALLOWED: OnceLock<()> = OnceLock::new();
    ALLOWED.get_or_init(|| {
        // SAFETY: a null handler with FALSE only clears this process's ignore-Ctrl+C attribute, which its children
        // inherit; no pointer is passed.
        let set = unsafe { SetConsoleCtrlHandler(None, 0) };
        assert_ne!(set, 0, "the suite's children can see Ctrl+C: {}", io::Error::last_os_error());
    });
    let mut command = case.gate(args);
    command.creation_flags(CREATE_NO_WINDOW);
    start(command)
}

#[test]
fn the_bridge_starts_pwsh_with_the_shim_and_the_host_as_one_hostalias_argument() {
    let case = routed_case("bridge-smoke");
    let record = case.folder().join("record.json");
    let body = format!("    Write-Call {} $Options $Command\n{}", quoted(&record), answer(0, false, None));
    let script = stand_in(&case, "remote.ps1", &body);
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &[], &COMMAND)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&record);
    let expected = [
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-File".to_owned(),
        shown(&shim()),
        "-RemoteScript".to_owned(),
        script.clone(),
        "-HostAlias".to_owned(),
        HOST.to_owned(),
    ];
    let args = call["args"].as_array().expect("the record has pwsh's arguments");
    assert_eq!(args[1..], expected.map(Value::from), "{call}");
    let mut options = call["options"].clone();
    let root = options["Root"].take();
    assert!(root.as_str().is_some_and(|root| !root.is_empty()), "{call}");
    let keys = [
        "Clock",
        "HostAlias",
        "Key",
        "Log",
        "Outputs",
        "Repo",
        "Root",
        "StallSeconds",
        "Subfolder",
        "Tail",
        "TimeoutSeconds",
    ];
    let expected_options = json!({
        "Keys": keys,
        "HostAlias": HOST,
        "Root": null,
        "Repo": repo(&case),
        "Key": events(&case)[0]["key"],
        "Subfolder": "",
        "Outputs": ["**/bin/**", "**/.godot/mono/temp/bin/**"],
        "Log": shown(&case.folder().join("out.log")),
        "TimeoutSeconds": 3600,
        "StallSeconds": 120,
        "Tail": 20,
        "Clock": "System.Diagnostics.Stopwatch",
    });
    assert_eq!(options, expected_options, "{call}");
}

#[test]
fn the_command_s_status_and_killed_flag_come_back_through_the_result_line() {
    let case = routed_case("bridge-status");
    let kill_line = "    Add-LogLine $Options.Log 'gate: STALLED: no output and no CPU for 120 s'\n";
    let failing = stand_in(&case, "failing.ps1", &answer(3, false, None));
    let killed = stand_in(&case, "killed.ps1", &format!("{kill_line}{}", answer(124, true, None)));
    let own = stand_in(&case, "own.ps1", &format!("{kill_line}{}", answer(124, false, None)));
    let first = finish(case.gate(&bridge_words("failing.log", &failing, &[], &COMMAND)));
    let second = finish(case.gate(&bridge_words("killed.log", &killed, &[], &COMMAND)));
    let third = finish(case.gate(&bridge_words("own.log", &own, &[], &COMMAND)));

    assert_eq!(
        (first.code, second.code, third.code),
        (Some(3), Some(124), Some(124)),
        "{first:?} {second:?} {third:?}"
    );
    let verdicts: Vec<Value> = events(&case)
        .into_iter()
        .filter(|event| event["event"] == "release")
        .map(|event| event["verdict"].clone())
        .collect();
    assert_eq!(verdicts, [json!("failed"), json!("stalled"), json!("failed")]);
    assert!(!first.stdout.contains("ran here"), "the command never ran here: {first:?}");
}

#[test]
fn each_refusal_reason_falls_back_with_its_reason() {
    let case = routed_case("bridge-reasons");
    let reasons = ["busy", "unreachable", "parity", "tools", "rebooted"];
    for reason in reasons {
        let script = stand_in(&case, &format!("{reason}.ps1"), &answer(75, false, Some(reason)));
        let log = format!("{reason}.log");
        let outcome = finish(case.gate(&bridge_words(&log, &script, &[], &COMMAND)));
        assert_eq!(outcome.code, Some(0), "{outcome:?}");
        let line = format!("gate: remote {HOST} {reason}; running here");
        assert!(outcome.stderr.contains(&line), "{outcome:?}");
        assert!(outcome.stdout.contains("ran here"), "the command ran here: {outcome:?}");
    }
    let events = events(&case);
    for (index, reason) in reasons.iter().enumerate() {
        assert_refused(&events[index * 5..index * 5 + 5], reason);
    }
}

#[test]
fn a_75_with_no_or_an_unknown_reason_is_the_command_s_own_status() {
    let case = routed_case("bridge-75");
    let none = stand_in(&case, "none.ps1", &answer(75, false, None));
    let unknown = stand_in(&case, "unknown.ps1", &answer(75, false, Some("tired")));
    let first = finish(case.gate(&bridge_words("none.log", &none, &[], &COMMAND)));
    let second = finish(case.gate(&bridge_words("unknown.log", &unknown, &[], &COMMAND)));

    assert_eq!((first.code, second.code), (Some(75), Some(75)), "{first:?} {second:?}");
    for outcome in [&first, &second] {
        assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
        assert!(!outcome.stderr.contains("running here"), "no fallback: {outcome:?}");
    }
    assert_eq!(names(&events(&case)), ["queue", "take", "release", "queue", "take", "release"]);
}

#[test]
fn a_refusal_keeps_the_attempt_s_log_lines_once() {
    let case = routed_case("bridge-refusal-lines");
    let body = format!("    Add-LogLine $Options.Log 'attempt line'\n{}", answer(75, false, Some("busy")));
    let script = stand_in(&case, "remote.ps1", &body);
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &[], &COMMAND)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(outcome.stderr.contains(BUSY_LINE), "{outcome:?}");
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert_eq!(log.matches("attempt line").count(), 1, "the attempt's line stands once: {log}");
    assert_in_order(&log, &["ran here", "attempt line", BUSY_LINE]);
    assert_refused(&events(&case), "busy");
}

#[test]
fn the_passed_line_names_the_host_and_the_outputs_and_words_reach_the_shim() {
    let case = routed_case("bridge-passed");
    let record = case.folder().join("record.json");
    let body = format!("    Write-Call {} $Options $Command\n{}", quoted(&record), answer(0, false, None));
    let script = stand_in(&case, "remote.ps1", &body);
    let words = ["argv", r#"say "hi""#, "--", "ünïcødé 名前", r"trailing\"];
    let mut command = vec![r".\tool.exe"];
    command.extend(words);
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &[], &command)));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_in_order(
        &outcome.stdout,
        &["gate: passed in ", &format!(" s on {HOST} (wall clock, ceiling 3600 s).")],
    );
    let call = recorded(&record);
    let sent = call["command"].as_array().expect("the record has the command");
    assert!(sent[0].as_str().is_some_and(|word| word.ends_with("tool.exe")), "{call}");
    assert_eq!(sent[1..], words.map(Value::from), "{call}");
    assert_eq!(call["options"]["Outputs"], json!(["**/bin/**", "**/.godot/mono/temp/bin/**"]), "{call}");
}

#[test]
fn a_missing_remote_script_falls_back_client_and_names_the_path() {
    let case = routed_case("bridge-no-script");
    install_shim();
    let script = shown(&case.folder().join("no-remote.ps1"));
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &[], &COMMAND)));

    assert_client_fallback(&outcome, &client_line(&format!("cannot find remote.ps1 at {script}")));
    assert_fell_back(&case, &events(&case));
}

#[test]
fn a_missing_pwsh_falls_back_client() {
    let case = routed_case("bridge-no-pwsh");
    let script = stand_in(&case, "remote.ps1", &answer(0, false, None));
    let pwsh = shown(&case.folder().join("no-pwsh.exe"));
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &["--test-bridge-pwsh", &pwsh], &COMMAND)));

    assert_client_fallback(&outcome, &client_line(&format!("cannot find pwsh at {pwsh}")));
    assert_fell_back(&case, &events(&case));
}

#[test]
fn a_shim_that_throws_before_the_run_falls_back_client_with_its_message() {
    let case = routed_case("bridge-throws");
    let top = "Add-LogLine $env:GATE_SUITE_LOG 'loading line'\nthrow 'the stand-in cannot load'";
    let script = stand_in_with(&case, "remote.ps1", top, &answer(0, false, None));
    let log = case.folder().join("out.log");
    let mut command = case.gate(&bridge_words("out.log", &script, &[], &COMMAND));
    command.env("GATE_SUITE_LOG", &log);
    let outcome = finish(command);

    assert_client_fallback(&outcome, &client_line("the stand-in cannot load"));
    assert_refused(&events(&case), "client");
    let text = fs::read_to_string(&log).expect("the log was written");
    assert_eq!(text.matches("loading line").count(), 1, "the attempt's line stands once: {text}");
}

#[test]
fn a_shim_that_ends_after_the_run_started_without_a_result_is_status_1_and_runs_nothing_here() {
    let case = routed_case("bridge-no-result");
    let script = stand_in(&case, "remote.ps1", "    [Environment]::Exit(9)");
    let outcome = finish(case.gate(&bridge_words("out.log", &script, &[], &COMMAND)));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let line = "gate: the bridge ended (status 9) after the remote run started; it did not report how the run ended";
    assert_eq!(outcome.stderr.matches(line).count(), 1, "{outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    assert!(!outcome.stderr.contains("running here"), "no fallback: {outcome:?}");
    assert_eq!(names(&events(&case)), ["queue", "take", "release"]);
    let log = fs::read_to_string(case.folder().join("out.log")).expect("the log was written");
    assert!(log.contains(line), "the line reaches the log: {log}");
}

#[test]
fn the_shim_s_environment_has_no_git_location_variables() {
    let case = routed_case("bridge-environment");
    let record = case.folder().join("record.json");
    let body = format!("    Write-Call {} $Options $Command\n{}", quoted(&record), answer(0, false, None));
    let script = stand_in(&case, "remote.ps1", &body);
    let empty = case.folder().join("empty.gitconfig");
    fs::write(&empty, "").expect("an empty git config can be written");
    let mut command = case.gate(&bridge_words("out.log", &script, &[], &COMMAND));
    // As a hook run at the worktree's root exports them, with config the hook's git was given and one it keeps.
    command.env("GIT_DIR", ".git").env("GIT_WORK_TREE", ".").env("GIT_CONFIG_GLOBAL", &empty);
    command
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.quotepath")
        .env("GIT_CONFIG_VALUE_0", "false");
    command.env("GIT_TERMINAL_PROMPT", "0");
    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let call = recorded(&record);
    let git: Vec<String> = call["git"]
        .as_array()
        .expect("the record has the git variables")
        .iter()
        .map(|name| name.as_str().expect("a name is text").to_ascii_uppercase())
        .collect();
    let stripped = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
    ];
    for name in stripped {
        assert!(!git.iter().any(|seen| seen == name), "{name} reached the shim: {git:?}");
    }
    assert!(
        git.iter().any(|seen| seen == "GIT_TERMINAL_PROMPT"),
        "the rest of the environment reaches it: {git:?}"
    );
}

/// The markers of a Ctrl+C case: the stand-in's started, go, stopping, release and stopped files, and one the case
/// writes only to end a stand-in the gate failed to end.
struct Markers {
    started: PathBuf,
    go: PathBuf,
    stopping: PathBuf,
    release: PathBuf,
    stopped: PathBuf,
    never: PathBuf,
}

impl Markers {
    fn of(case: &Case) -> Markers {
        let folder = case.folder();
        Markers {
            started: folder.join("started"),
            go: folder.join("go"),
            stopping: folder.join("stopping"),
            release: folder.join("release"),
            stopped: folder.join("stopped"),
            never: folder.join("never"),
        }
    }

    /// A stand-in that says its pid, waits for go, sends its console a Ctrl+C and waits for a file that never comes;
    /// its break path runs `on_break`.
    fn body(&self, on_break: &str) -> String {
        format!(
            "    Write-Marker {started} \"$PID\"\n    Wait-Marker {go}\n    try {{\n        Send-CtrlC\n        Wait-Marker {never}\n    \
             }} finally {{\n{on_break}\n    }}\n{answer}",
            started = quoted(&self.started),
            go = quoted(&self.go),
            never = quoted(&self.never),
            answer = answer(0, false, None),
        )
    }
}

/// Asserts the end of a routed run a Ctrl+C ended during the call: the interrupt's status, the stopping line `stopping`
/// times, and the remote slot released with the remote tag.
fn assert_interrupted(case: &Case, outcome: &Outcome, stopping: usize) {
    assert_eq!(outcome.code, Some(CONTROL_C_EXIT), "{outcome:?}");
    assert_eq!(outcome.stderr.matches(STOPPING).count(), stopping, "the stopping line: {outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    assert_eq!(names(&events(case)), ["queue", "take", "release"]);
    assert!(!remote_claim(case).exists(), "the remote claim is gone after the release");
}

#[test]
fn a_ctrl_c_before_the_run_starts_ends_the_bridge_at_once() {
    let case = routed_case("bridge-ctrl-c-early");
    let markers = Markers::of(&case);
    // At dot-source time, before the bridge script says the run started.
    let top = format!(
        "Write-Marker {started} \"$PID\"\nWait-Marker {go}\nSend-CtrlC\nWait-Marker {never}",
        started = quoted(&markers.started),
        go = quoted(&markers.go),
        never = quoted(&markers.never),
    );
    let script = stand_in_with(&case, "remote.ps1", &top, &answer(0, false, None));
    let _let_go = LetGo(vec![markers.go.clone(), markers.never.clone()]);
    let running = start_in_own_console(&case, &bridge_words("out.log", &script, &[], &COMMAND));
    let pwsh = started_pwsh(&markers.started);
    fs::write(&markers.go, "").expect("the go file can be made");
    let outcome = running.finish();

    assert_interrupted(&case, &outcome, 0);
    wait_gone(pwsh);
}

#[test]
fn a_ctrl_c_runs_the_shim_s_stop_path_and_waits_for_it() {
    let case = routed_case("bridge-ctrl-c");
    let markers = Markers::of(&case);
    let on_break = format!(
        "        Write-Marker {stopping} 'stopping'\n        Wait-Marker {release}\n        Write-Marker {stopped} 'stopped'",
        stopping = quoted(&markers.stopping),
        release = quoted(&markers.release),
        stopped = quoted(&markers.stopped),
    );
    let script = stand_in(&case, "remote.ps1", &markers.body(&on_break));
    let _let_go = LetGo(vec![markers.go.clone(), markers.release.clone(), markers.never.clone()]);
    let mut running = start_in_own_console(&case, &bridge_words("out.log", &script, &[], &COMMAND));
    let pwsh = started_pwsh(&markers.started);
    fs::write(&markers.go, "").expect("the go file can be made");
    wait_for_file(&markers.stopping);

    assert!(running.is_running(), "the gate waits while the stop path runs");
    fs::write(&markers.release, "").expect("the release file can be made");
    let outcome = running.finish();

    assert!(markers.stopped.exists(), "the gate waited for the stop path: {outcome:?}");
    assert_interrupted(&case, &outcome, 1);
    wait_gone(pwsh);
}

#[test]
fn a_second_ctrl_c_kills_the_shim_at_once() {
    let case = routed_case("bridge-ctrl-c-again");
    let markers = Markers::of(&case);
    let on_break = format!(
        "        Write-Marker {stopping} 'stopping'\n        Send-CtrlC\n        Wait-Marker {never}\n        Write-Marker {stopped} 'stopped'",
        stopping = quoted(&markers.stopping),
        never = quoted(&markers.never),
        stopped = quoted(&markers.stopped),
    );
    let script = stand_in(&case, "remote.ps1", &markers.body(&on_break));
    let _let_go = LetGo(vec![markers.go.clone(), markers.never.clone()]);
    let mut running = start_in_own_console(&case, &bridge_words("out.log", &script, &[], &COMMAND));
    let pwsh = started_pwsh(&markers.started);
    fs::write(&markers.go, "").expect("the go file can be made");
    wait_for_file(&markers.stopping);
    wait_until("pwsh or the gate ending on the second Ctrl+C", || gone(pwsh) || !running.is_running());
    let killed = gone(pwsh);
    // A second press that missed leaves pwsh and the gate running, so the wait above fails at the suite's hang guard
    // (600 s) and the go files end pwsh. A gate that ended while pwsh still runs lets it end here, and the case fails on
    // the assertion below.
    fs::write(&markers.never, "").expect("the never file can be made");
    let outcome = running.finish();

    assert!(killed, "the second Ctrl+C ended pwsh before the gate did: {outcome:?}");
    assert!(!markers.stopped.exists(), "the stop path was cut short: {outcome:?}");
    assert_interrupted(&case, &outcome, 1);
    wait_gone(pwsh);
}

#[test]
fn gate_stop_tree_reaches_the_shim() {
    let case = routed_case("bridge-stop-tree");
    let markers = Markers::of(&case);
    let body = format!(
        "    Write-Marker {} \"$PID\"\n    Wait-Marker {}\n{}",
        quoted(&markers.started),
        quoted(&markers.go),
        answer(0, false, None)
    );
    let script = stand_in(&case, "remote.ps1", &body);
    let _let_go = LetGo(vec![markers.go.clone()]);
    let running = case.start(&bridge_words("out.log", &script, &[], &COMMAND));
    let pwsh = started_pwsh(&markers.started);
    let gate_pid = running.id();

    let stopped = case.run(&["stop-tree", &gate_pid.to_string()]);
    let outcome = running.finish();

    assert_eq!((stopped.code, stopped.stderr.as_str()), (Some(0), ""), "{stopped:?}");
    assert!(stopped.stdout.starts_with("gate: stopped 1 gate job(s) and "), "{stopped:?}");
    // stop-tree ends pwsh's job, then the gate; a gate that ends first ends its run with status 1, never here.
    assert!(matches!(outcome.code, Some(124 | 1)), "the gate ends stopped: {outcome:?}");
    assert!(!outcome.stdout.contains("ran here"), "the command never ran here: {outcome:?}");
    let takes = names(&events(&case)).into_iter().filter(|name| *name == "take").count();
    assert_eq!(takes, 1, "no take after the remote one: {:?}", events(&case));
    wait_gone(pwsh);
}

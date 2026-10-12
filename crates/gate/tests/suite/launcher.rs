//! The launcher's hand-over: `tools/gate.ps1` hands every word to `%LOCALAPPDATA%\gate\bin\gate.exe` once
//! `%LOCALAPPDATA%\gate\cutover.flag` exists, prints nothing of its own and exits with gate.exe's whole status; with the
//! flag unset it hands the words to the canonical gate instead. Each case runs the checked-in launcher with a
//! LOCALAPPDATA, a state root and a canonical gate of its own, and compares what it did with the same run made directly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{checked_in, copy_gate, unix};
use crate::support::{Case, Outcome, USAGE, finish, shown};

/// The words of today's form whose slot gate.exe refuses before anything runs.
pub(crate) fn refused_words() -> Vec<&'static str> {
    vec!["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "auto", "--", "cmd"]
}

/// The words of a heavy run that prints its request, the hidden switch before the `--` and the command after it, with a
/// colon switch and a command word holding a space.
fn request_words() -> Vec<&'static str> {
    vec![
        "-Log",
        "out.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "-NoMarkers:$false",
        "--test-print-request",
        "--",
        "a word with spaces",
        "cmd",
        "/c",
        "echo",
        "hi",
    ]
}

/// The words of a plain heavy run in today's form, the command after `--`.
fn compat_run_words() -> Vec<&'static str> {
    vec![
        "-Log",
        "out.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "--",
        "cmd",
        "/c",
        "echo",
        "hi",
    ]
}

/// The launcher under test, the checked-in `tools/gate.ps1`.
fn launcher() -> PathBuf {
    checked_in("gate.ps1")
}

/// The case folder the canonical gate is looked for in: a stub at the path the launcher builds,
/// `CLAUDE_CONFIG_DIR\tools\gate\gate.ps1`, which writes its words, one per line, to `words` and exits 9. A launcher a
/// flag does not switch therefore says which side it took and runs nothing that matters.
pub(crate) fn stub_claude(case: &Case, words: &Path) -> PathBuf {
    let folder = case.folder().join("claude");
    let script = folder.join("tools").join("gate").join("gate.ps1");
    fs::create_dir_all(script.parent().expect("the stub's folder")).expect("the stub's folder can be made");
    let text = format!("[System.IO.File]::WriteAllText('{}', ($args -join [char]10))\nexit 9\n", shown(words));
    fs::write(&script, text).expect("the stub canonical gate can be written");
    folder
}

/// The file the case's stub canonical gate writes its words to, under the case folder and never the state root.
pub(crate) fn stub_words(case: &Case) -> PathBuf {
    case.folder().join("stub-words.txt")
}

/// Sets the cutover flag `%LOCALAPPDATA%\gate\cutover.flag` to `content`, which is a flag whether or not it reads as
/// off: the launcher never reads it.
fn set_flag(case: &Case, content: &str) {
    let folder = case.local_app_data().join("gate");
    fs::create_dir_all(&folder).expect("the flag's folder can be made");
    fs::write(folder.join("cutover.flag"), content).expect("the cutover flag can be written");
}

/// Installs a copy of the built gate as the case's `%LOCALAPPDATA%\gate\bin\gate.exe`, never through the install
/// script, which writes the registry, and returns its path.
pub(crate) fn install_gate(case: &Case) -> PathBuf {
    fs::create_dir_all(case.local_app_data().join("gate").join("bin")).expect("the install folder can be made");
    copy_gate(case, r"appdata\gate\bin\gate.exe")
}

/// The launcher run as a caller runs it, `pwsh -NoProfile -File <tools/gate.ps1> <words>`, with the case's LOCALAPPDATA
/// and working folder, its state root through `GATE_STATE_DIR` and `config` as `CLAUDE_CONFIG_DIR`.
pub(crate) fn run_launcher(case: &Case, config: &Path, words: &[&str]) -> Outcome {
    let mut command = Command::new("pwsh");
    let all: Vec<String> = ["-NoProfile", "-File"]
        .into_iter()
        .map(str::to_owned)
        .chain([shown(&launcher())])
        .chain(words.iter().map(|word| (*word).to_owned()))
        .collect();
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    case.prepare_without_state_dir(&mut command, &refs);
    command.env("GATE_STATE_DIR", case.root());
    command.env("CLAUDE_CONFIG_DIR", config);
    finish(command)
}

/// The installed gate.exe run directly with `words`, its environment the launcher's child's: the case's LOCALAPPDATA
/// and working folder, its state root through `GATE_STATE_DIR` and `config` as `CLAUDE_CONFIG_DIR`. A case compares the
/// hand-over with this, so nothing it asserts is a literal that could drift from gate.exe's own.
pub(crate) fn run_direct(exe: &Path, case: &Case, config: &Path, words: &[&str]) -> Outcome {
    let mut command = Command::new(exe);
    case.prepare_without_state_dir(&mut command, words);
    command.env("GATE_STATE_DIR", case.root());
    command.env("CLAUDE_CONFIG_DIR", config);
    finish(command)
}

/// The launcher as a caller's own session calls it, `& '<tools/gate.ps1>' <words>`, with the case's LOCALAPPDATA and
/// working folder, its state root through `GATE_STATE_DIR` and `config` as `CLAUDE_CONFIG_DIR`. The call's own words
/// carry no `--`, so the launcher is the side that puts one back.
fn run_launcher_in_session(case: &Case, config: &Path, words: &[&str]) -> Outcome {
    let line = format!("& '{}' {}; exit $LASTEXITCODE", shown(&launcher()), words.join(" "));
    let mut command = Command::new("pwsh");
    case.prepare_without_state_dir(&mut command, &["-NoProfile", "-Command", &line]);
    command.env("GATE_STATE_DIR", case.root());
    command.env("CLAUDE_CONFIG_DIR", config);
    finish(command)
}

/// `text` as the process that carried it wrote it: Windows line ends read as `\n`, and a trailing line end dropped, so
/// a launcher's output compares with a direct run's whatever the PowerShell host did to the child's bytes.
pub(crate) fn lines(text: &str) -> String {
    unix(text).trim_end_matches('\n').to_owned()
}

#[test]
fn flag_set_hands_over_the_whole_status_and_output() {
    let case = Case::new("flag_set_hands_over_the_whole_status_and_output");
    let exe = install_gate(&case);
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);

    let handed = run_launcher(&case, &config, &refused_words());
    let direct = run_direct(&exe, &case, &config, &refused_words());

    assert_eq!(handed.code, Some(2), "{handed:?}");
    assert_eq!(handed.stdout, "", "the launcher printed a line of its own: {handed:?}");
    assert_eq!(
        lines(&handed.stderr),
        lines(&direct.stderr),
        "not the status and output gate.exe gives directly"
    );
    assert_eq!(
        lines(&handed.stderr),
        format!("gate: -Slot must be heavy, light, critical or none, got 'auto'\n{USAGE}").trim_end_matches('\n')
    );
    assert!(!words.exists(), "the canonical gate ran: the launcher did not hand over");
}

#[test]
fn flag_set_hand_over_keeps_the_words_whole() {
    let case = Case::new("flag_set_hand_over_keeps_the_words_whole");
    let exe = install_gate(&case);
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);

    let handed = run_launcher(&case, &config, &request_words());
    let direct = run_direct(&exe, &case, &config, &request_words());

    assert_eq!(handed.request(), direct.request(), "the hand-over asked for another run");
    assert_eq!(lines(&handed.stdout), lines(&direct.stdout));
    assert_eq!(
        handed.request()["command"],
        serde_json::json!(["a word with spaces", "cmd", "/c", "echo", "hi"]),
        "a command word was split or lost"
    );
    assert_eq!(handed.request()["noMarkers"], false, "the colon switch was read as a bare -NoMarkers");
    assert!(!words.exists(), "the canonical gate ran: the launcher did not hand over");
}

/// The whole status, not a byte of it: the launcher hands gate.exe's `exit $LASTEXITCODE` on as it is, so a status above
/// 255 reaches the caller as it does through gate.exe directly. The run's timeout is far above what it needs, as the
/// suite's other runs give theirs, and the run takes its light slot under the case's own LOCALAPPDATA, so no other
/// session's gate is involved.
#[test]
fn the_hand_over_keeps_a_status_above_255() {
    let case = Case::new("the_hand_over_keeps_a_status_above_255");
    let exe = install_gate(&case);
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);
    let call = [
        "-Log",
        "s.log",
        "-TimeoutSeconds",
        "3600",
        "-Slot",
        "light",
        "--",
        "cmd",
        "/c",
        "exit",
        "300",
    ];

    let handed = run_launcher(&case, &config, &call);
    let direct = run_direct(&exe, &case, &config, &call);

    assert_eq!(handed.code, Some(300), "{handed:?}");
    assert_eq!(handed.code, direct.code, "{handed:?} against {direct:?}");
    assert!(!words.exists(), "the canonical gate ran: the launcher did not hand over");
}

/// The in-session form, whose caller leaves the `--` out: the launcher puts one back before the first
/// word that is not one of its options, so gate.exe is given the words a `-File` caller's would carry. The hidden
/// `--test-print-request` switch cannot be used to see them: the launcher does not know it, reads it as an unreadable
/// option and then hands the words over as they came, with no `--` put back, which gate.exe answers with its usage
/// text. The case is decided against gate.exe's own two runs, one with the `--` and one without it.
#[test]
fn in_session_words_get_their_dash_dash_back_before_the_hand_over() {
    let case = Case::new("in_session_words_get_their_dash_dash_back_before_the_hand_over");
    let exe = install_gate(&case);
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);
    let in_session = ["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "auto", "pwsh"];
    let mut restored_words = in_session.to_vec();
    restored_words.insert(6, "--");

    let handed = run_launcher_in_session(&case, &config, &in_session);
    let restored = run_direct(&exe, &case, &config, &restored_words);
    let bare = run_direct(&exe, &case, &config, &in_session);

    let restored_said = (restored.code, lines(&restored.stderr));
    let bare_said = (bare.code, lines(&bare.stderr));
    assert_ne!(
        restored_said, bare_said,
        "a -- put back cannot be told from one lost: {restored:?} {bare:?}"
    );
    assert_eq!(
        (handed.code, lines(&handed.stderr)),
        restored_said,
        "the launcher did not put the -- back: {handed:?} against {restored:?}"
    );
    assert_eq!(handed.stdout, "", "the launcher printed a line of its own: {handed:?}");
    assert!(!words.exists(), "the canonical gate ran: the launcher did not hand over");
}

#[test]
fn flag_set_stop_tree_goes_to_gate_exe() {
    let case = Case::new("flag_set_stop_tree_goes_to_gate_exe");
    let exe = install_gate(&case);
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);
    let call = ["-StopTree", "abc"];

    let handed = run_launcher(&case, &config, &call);
    let direct = run_direct(&exe, &case, &config, &call);

    assert_eq!(handed.code, direct.code, "{handed:?} against {direct:?}");
    assert_eq!(lines(&handed.stdout), lines(&direct.stdout));
    assert_eq!(lines(&handed.stderr), lines(&direct.stderr));
    assert!(
        !unix(&handed.stderr).contains("usage: pwsh tools/gate.ps1"),
        "the PowerShell fallback answered its own -StopTree: {handed:?}"
    );
    assert!(!words.exists(), "the canonical gate ran: the launcher did not hand over");
}

#[test]
fn flag_set_with_the_exe_missing_runs_nothing() {
    let case = Case::new("flag_set_with_the_exe_missing_runs_nothing");
    set_flag(&case, "");
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);
    let missing = case.local_app_data().join("gate").join("bin").join("gate.exe");

    let handed = run_launcher(&case, &config, &refused_words());

    assert_eq!(handed.code, Some(127), "{handed:?}");
    assert_eq!(handed.stdout, "", "the launcher printed a line of its own: {handed:?}");
    assert_eq!(
        lines(&handed.stderr),
        format!(
            "gate: the cutover flag is set but {} is missing; reinstall with tools/install.ps1",
            shown(&missing)
        )
    );
    assert!(!words.exists(), "the canonical gate ran: the launcher did not refuse");
    assert!(!case.folder().join("out.log").exists(), "the launcher's log was written: something ran");
    assert!(
        case.root_entries().is_empty(),
        "something ran under the state root: {:?}",
        case.root_entries()
    );
    let mut entries = case.folder_entries();
    entries.sort();
    assert_eq!(
        entries,
        ["appdata", "claude"],
        "something ran: the case folder holds more than the flag and the stub"
    );
}

#[test]
fn without_the_flag_the_canonical_gate_runs() {
    let case = Case::new("without_the_flag_the_canonical_gate_runs");
    let _installed = install_gate(&case);
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);

    let stop_tree = ["-StopTree", "abc"];
    let handed = run_launcher(&case, &config, &stop_tree);
    assert_eq!(handed.code, Some(9), "{handed:?}");
    assert_eq!(handed.stdout, "", "the launcher printed a line of its own: {handed:?}");
    assert_eq!(handed.stderr, "");
    assert_eq!(
        fs::read_to_string(&words).expect("the stub wrote its words"),
        stop_tree.join("\n"),
        "the canonical gate got other words"
    );

    let run = compat_run_words();
    fs::remove_file(&words).expect("the words file can go");
    let handed = run_launcher(&case, &config, &run);
    assert_eq!(handed.code, Some(9), "{handed:?}");
    assert_eq!(handed.stdout, "", "the launcher printed a line of its own: {handed:?}");
    assert_eq!(handed.stderr, "");
    assert_eq!(
        fs::read_to_string(&words).expect("the stub wrote its words"),
        run.join("\n"),
        "the separator or a word did not reach the canonical gate"
    );
}

#[test]
fn only_the_local_app_data_flag_switches() {
    let case = Case::new("only_the_local_app_data_flag_switches");
    let exe = install_gate(&case);
    let words = stub_words(&case);
    let config = stub_claude(&case, &words);

    // A flag whose content reads as off switches anyway: the launcher never reads it.
    set_flag(&case, "0");
    let handed = run_launcher(&case, &config, &refused_words());
    let direct = run_direct(&exe, &case, &config, &refused_words());
    assert_eq!(handed.code, Some(2), "{handed:?}");
    assert_eq!(lines(&handed.stderr), lines(&direct.stderr), "a flag reading as off did not hand over");
    assert_eq!(handed.stdout, "");
    assert!(!words.exists(), "the canonical gate ran with the flag set");

    // A flag anywhere but %LOCALAPPDATA%\gate\cutover.flag leaves the launcher on the PowerShell side.
    fs::remove_file(case.local_app_data().join("gate").join("cutover.flag")).expect("the flag can go");
    fs::write(case.root().join("cutover.flag"), "0").expect("the state root's flag can be written");
    let handed = run_launcher(&case, &config, &compat_run_words());
    assert_eq!(handed.code, Some(9), "a flag in the state root switched the launcher: {handed:?}");
    assert_eq!(
        fs::read_to_string(&words).expect("the stub wrote its words"),
        compat_run_words().join("\n")
    );

    fs::create_dir_all(case.local_app_data().join("gate").join("v1")).expect("the v1 folder can be made");
    fs::write(case.local_app_data().join("gate").join("v1").join("cutover.flag"), "0").expect("the v1 flag can be written");
    fs::remove_file(&words).expect("the words file can go");
    let handed = run_launcher(&case, &config, &compat_run_words());
    assert_eq!(handed.code, Some(9), "a flag in the default state root switched the launcher: {handed:?}");
    assert_eq!(
        fs::read_to_string(&words).expect("the stub wrote its words"),
        compat_run_words().join("\n")
    );
}

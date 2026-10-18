//! The launcher: `tools/gate.ps1` hands every word to gate.exe (`GATE_BOX_EXE` when set, else the first of
//! `<Program Files root>\gate\bin\gate.exe` and `%LOCALAPPDATA%\gate\bin\gate.exe`), prints nothing of its own and exits
//! with gate.exe's whole status; with none found it runs the command directly. Each case runs the checked-in launcher
//! with a LOCALAPPDATA, a Program Files root (`GATE_LAUNCHER_PROGRAM_FILES`, since Windows sets `ProgramFiles` afresh in
//! every process) and a state root of its own, and compares what it did with the same run made directly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{checked_in, copy_gate, unix};
use crate::support::{Case, FAKE_TOOL, Outcome, USAGE, finish, shown};

/// The line the launcher writes on stderr before it runs a command with no gate.exe found.
const NO_GATE: &str = "gate: no gate.exe found; running the command without slots or watchdog";

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

/// The launcher under test, the checked-in `tools/gate.ps1`.
fn launcher() -> PathBuf {
    checked_in("gate.ps1")
}

/// Installs a copy of the built gate as the case's `%LOCALAPPDATA%\gate\bin\gate.exe`, never through the install
/// script, which writes the registry, and returns its path.
pub(crate) fn install_gate(case: &Case) -> PathBuf {
    fs::create_dir_all(case.local_app_data().join("gate").join("bin")).expect("the install folder can be made");
    copy_gate(case, r"appdata\gate\bin\gate.exe")
}

/// The case's own Program Files root, made empty, so the machine's own `C:\Program Files\gate\bin\gate.exe` is never
/// found.
fn program_files(case: &Case) -> PathBuf {
    let folder = case.folder().join("programfiles");
    fs::create_dir_all(&folder).expect("the case's Program Files root can be made");
    folder
}

/// Puts a copy of fake-tool at `<root>\gate\bin\gate.exe`: a gate.exe the launcher must not pick, which answers any
/// gate words with `fake-tool: unknown step '<first word>'`, so a case can tell it from the gate it expects.
fn place_decoy(root: &Path) {
    let bin = root.join("gate").join("bin");
    fs::create_dir_all(&bin).expect("the decoy's folder can be made");
    fs::copy(FAKE_TOOL, bin.join("gate.exe")).expect("fake-tool can be copied as a decoy gate.exe");
}

/// Readies `command` with the case's LOCALAPPDATA and working folder, its own empty Program Files root as
/// `GATE_LAUNCHER_PROGRAM_FILES`, no `GATE_BOX_EXE` and its state root through `GATE_STATE_DIR`, then sets `extra` over
/// them.
fn prepare(command: &mut Command, case: &Case, words: &[&str], extra: &[(&str, &str)]) {
    case.prepare_without_state_dir(command, words);
    command.env_remove("GATE_BOX_EXE");
    command.env("GATE_LAUNCHER_PROGRAM_FILES", program_files(case));
    command.env("GATE_STATE_DIR", case.root());
    for (name, value) in extra {
        command.env(name, value);
    }
}

/// The launcher run as a caller runs it, `pwsh -NoProfile -File <tools/gate.ps1> <words>`, its environment as
/// [`prepare`] readies it.
pub(crate) fn run_launcher(case: &Case, words: &[&str]) -> Outcome {
    run_launcher_with(case, words, &[])
}

/// As [`run_launcher`], with `extra` variables set on the launcher over the case's own.
fn run_launcher_with(case: &Case, words: &[&str], extra: &[(&str, &str)]) -> Outcome {
    let mut command = Command::new("pwsh");
    let all: Vec<String> = ["-NoProfile", "-File"]
        .into_iter()
        .map(str::to_owned)
        .chain([shown(&launcher())])
        .chain(words.iter().map(|word| (*word).to_owned()))
        .collect();
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    prepare(&mut command, case, &refs, extra);
    finish(command)
}

/// The gate.exe at `exe` run directly with `words`, its environment the launcher's child's as [`prepare`] readies it. A
/// case compares the hand-over with this, so nothing it asserts is a literal that could drift from gate.exe's own.
pub(crate) fn run_direct(exe: &Path, case: &Case, words: &[&str]) -> Outcome {
    let mut command = Command::new(exe);
    prepare(&mut command, case, words, &[]);
    finish(command)
}

/// The PowerShell `line` run in a session of its own, `pwsh -NoProfile -Command <line>`, its environment as [`prepare`]
/// readies it.
fn run_session(case: &Case, line: &str) -> Outcome {
    let mut command = Command::new("pwsh");
    prepare(&mut command, case, &["-NoProfile", "-Command", line], &[]);
    finish(command)
}

/// The launcher as a caller's own session calls it, `& '<tools/gate.ps1>' <words>`, its environment as [`prepare`]
/// readies it. The call's own words carry no `--`, so the launcher is the side that puts one back.
fn run_launcher_in_session(case: &Case, words: &[&str]) -> Outcome {
    run_session(case, &format!("& '{}' {}; exit $LASTEXITCODE", shown(&launcher()), words.join(" ")))
}

/// `text` as the process that carried it wrote it: Windows line ends read as `\n`, and a trailing line end dropped, so
/// a launcher's output compares with a direct run's whatever the PowerShell host did to the child's bytes.
pub(crate) fn lines(text: &str) -> String {
    unix(text).trim_end_matches('\n').to_owned()
}

/// Asserts the launcher handed `refused_words` to `exe`: the status and output are gate.exe's own, the slot refusal.
fn assert_handed_to(exe: &Path, case: &Case, handed: &Outcome) {
    let direct = run_direct(exe, case, &refused_words());
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
}

/// The words of a direct run in the PowerShell form: the options, then `--` and fake-tool with `steps`.
fn compat_direct<'a>(steps: &[&'a str]) -> Vec<&'a str> {
    let mut words = vec!["-Log", "x.log", "-TimeoutSeconds", "5", "-Slot", "heavy", "--", FAKE_TOOL];
    words.extend_from_slice(steps);
    words
}

#[test]
fn without_a_flag_the_local_app_data_gate_runs() {
    let case = Case::new("without_a_flag_the_local_app_data_gate_runs");
    let exe = install_gate(&case);

    let handed = run_launcher(&case, &refused_words());

    assert_handed_to(&exe, &case, &handed);
}

#[test]
fn the_box_exe_runs_first_when_set() {
    let case = Case::new("the_box_exe_runs_first_when_set");
    let exe = copy_gate(&case, "box-gate.exe");
    place_decoy(&case.local_app_data());
    place_decoy(&program_files(&case));

    let handed = run_launcher_with(&case, &refused_words(), &[("GATE_BOX_EXE", &shown(&exe))]);

    assert_handed_to(&exe, &case, &handed);
}

#[test]
fn the_program_files_gate_runs_before_the_local_app_data_one() {
    let case = Case::new("the_program_files_gate_runs_before_the_local_app_data_one");
    let own = case.folder().join("pf");
    fs::create_dir_all(own.join("gate").join("bin")).expect("the Program Files install folder can be made");
    let exe = copy_gate(&case, r"pf\gate\bin\gate.exe");
    place_decoy(&case.local_app_data());
    // An empty GATE_BOX_EXE is skipped as an unset one is.
    let root = shown(&own);
    let extra = [("GATE_LAUNCHER_PROGRAM_FILES", root.as_str()), ("GATE_BOX_EXE", "")];

    let handed = run_launcher_with(&case, &refused_words(), &extra);

    assert_handed_to(&exe, &case, &handed);
}

/// A `GATE_BOX_EXE` that is relative, even one naming a gate in the working folder, or that names no file runs nothing:
/// the installed gate is not tried in its place.
#[test]
fn a_relative_or_missing_box_exe_runs_nothing() {
    let case = Case::new("a_relative_or_missing_box_exe_runs_nothing");
    install_gate(&case);
    copy_gate(&case, "box-gate.exe");
    let missing = shown(&case.folder().join("missing").join("gate.exe"));
    let call = compat_direct(&["out", "ran"]);

    for value in ["box-gate.exe", missing.as_str()] {
        let ran = run_launcher_with(&case, &call, &[("GATE_BOX_EXE", value)]);
        assert_eq!(ran.code, Some(127), "{value}: {ran:?}");
        assert_eq!(ran.stdout, "", "{value}: something ran: {ran:?}");
        assert_eq!(
            lines(&ran.stderr),
            format!("gate: GATE_BOX_EXE names {value}, which is not a gate.exe here"),
            "{value}: {ran:?}"
        );
    }
}

#[test]
fn the_hand_over_keeps_the_words_whole() {
    let case = Case::new("the_hand_over_keeps_the_words_whole");
    let exe = install_gate(&case);

    let handed = run_launcher(&case, &request_words());
    let direct = run_direct(&exe, &case, &request_words());

    assert_eq!(handed.request(), direct.request(), "the hand-over asked for another run");
    assert_eq!(lines(&handed.stdout), lines(&direct.stdout));
    assert_eq!(
        handed.request()["command"],
        serde_json::json!(["a word with spaces", "cmd", "/c", "echo", "hi"]),
        "a command word was split or lost"
    );
    assert_eq!(handed.request()["noMarkers"], false, "the colon switch was read as a bare -NoMarkers");
}

/// The whole status, not a byte of it: the launcher hands gate.exe's `exit $LASTEXITCODE` on as it is, so a status above
/// 255 reaches the caller as it does through gate.exe directly. The run's timeout is far above what it needs, as the
/// suite's other runs give theirs, and the run takes its light slot under the case's own LOCALAPPDATA, so no other
/// session's gate is involved.
#[test]
fn the_hand_over_keeps_a_status_above_255() {
    let case = Case::new("the_hand_over_keeps_a_status_above_255");
    let exe = install_gate(&case);
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

    let handed = run_launcher(&case, &call);
    let direct = run_direct(&exe, &case, &call);

    assert_eq!(handed.code, Some(300), "{handed:?}");
    assert_eq!(handed.code, direct.code, "{handed:?} against {direct:?}");
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
    let in_session = ["-Log", "out.log", "-TimeoutSeconds", "5", "-Slot", "auto", "pwsh"];
    let mut restored_words = in_session.to_vec();
    restored_words.insert(6, "--");

    let handed = run_launcher_in_session(&case, &in_session);
    let restored = run_direct(&exe, &case, &restored_words);
    let bare = run_direct(&exe, &case, &in_session);

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
}

#[test]
fn stop_tree_goes_to_gate_exe() {
    let case = Case::new("stop_tree_goes_to_gate_exe");
    let exe = install_gate(&case);
    let call = ["-StopTree", "abc"];

    let handed = run_launcher(&case, &call);
    let direct = run_direct(&exe, &case, &call);

    assert_eq!(handed.code, direct.code, "{handed:?} against {direct:?}");
    assert_eq!(lines(&handed.stdout), lines(&direct.stdout));
    assert_eq!(lines(&handed.stderr), lines(&direct.stderr));
    assert!(
        !unix(&handed.stderr).contains("usage: pwsh tools/gate.ps1"),
        "the launcher answered its own -StopTree: {handed:?}"
    );
}

#[test]
fn the_run_form_is_handed_over_as_it_came() {
    let case = Case::new("the_run_form_is_handed_over_as_it_came");
    let exe = install_gate(&case);
    let call = [
        "run",
        "--log",
        "out.log",
        "--timeout",
        "5",
        "--slot",
        "heavy",
        "--test-print-request",
        "--",
        "cmd",
        "/c",
        "echo",
        "hi",
    ];

    let handed = run_launcher(&case, &call);
    let direct = run_direct(&exe, &case, &call);

    assert_eq!(handed.request(), direct.request(), "the hand-over asked for another run");
    assert_eq!(handed.request()["command"], serde_json::json!(["cmd", "/c", "echo", "hi"]));
}

/// A `run` call from a session that ate its `--`: the launcher puts it back before the first command word, reading the
/// CLI form's options and gate.exe's hidden switch as options, and joins the `-p:` word the session split.
#[test]
fn an_in_session_run_form_gets_its_dash_dash_back() {
    let case = Case::new("an_in_session_run_form_gets_its_dash_dash_back");
    let exe = install_gate(&case);
    let options = ["run", "--log", "out.log", "--timeout", "5", "--slot", "heavy", "--test-print-request"];
    let command = ["cmd", "/c", "echo", "-p:X=Y"];
    let in_session: Vec<&str> = options.iter().chain(&command).copied().collect();
    let restored: Vec<&str> = options.iter().chain(&["--"]).chain(&command).copied().collect();

    let handed = run_launcher_in_session(&case, &in_session);
    let direct = run_direct(&exe, &case, &restored);

    assert_eq!(handed.request(), direct.request(), "the launcher did not put the -- back: {handed:?}");
    assert_eq!(
        handed.request()["command"],
        serde_json::json!(command),
        "a command word was split or lost"
    );
}

#[test]
fn the_stop_tree_subcommand_is_handed_over_as_it_came() {
    let case = Case::new("the_stop_tree_subcommand_is_handed_over_as_it_came");
    let exe = install_gate(&case);
    let call = ["stop-tree", "abc"];

    let handed = run_launcher(&case, &call);
    let direct = run_direct(&exe, &case, &call);

    assert_eq!(
        (handed.code, lines(&handed.stdout), lines(&handed.stderr)),
        (direct.code, lines(&direct.stdout), lines(&direct.stderr)),
        "not what gate.exe says to the same words"
    );
}

#[test]
fn with_no_gate_the_command_runs_directly_with_its_status() {
    let case = Case::new("with_no_gate_the_command_runs_directly_with_its_status");
    let failing = [FAKE_TOOL, "out", "hi", "err", "oops", "exit", "3"];
    let passing = [FAKE_TOOL, "out", "hi"];
    let compat = ["-Log", "x.log", "-TimeoutSeconds", "5", "-Slot", "heavy", "--"];
    let compat_colon = [
        "-Log:x.log",
        "-TimeoutSeconds",
        "5",
        "-Slot:heavy",
        "-NoMarkers",
        "-Alone",
        "-Local",
        "--",
    ];
    let hook = ["run", "--log", "x.log", "--timeout", "5", "--slot", "heavy", "--"];
    let hook_equals = [
        "RUN",
        "--log=x.log",
        "--timeout=5",
        "--slot",
        "heavy",
        "--no-markers",
        "--alone",
        "--local",
        "--",
    ];
    let runs: [(&[&str], &[&str], i32, &str); 4] = [
        (&compat, &failing, 3, "oops"),
        (&compat_colon, &passing, 0, ""),
        (&hook, &failing, 3, "oops"),
        (&hook_equals, &passing, 0, ""),
    ];

    for (options, command, code, said) in runs {
        let call: Vec<&str> = options.iter().chain(command).copied().collect();
        let ran = run_launcher(&case, &call);
        assert_eq!(ran.code, Some(code), "{call:?}: {ran:?}");
        assert_eq!(lines(&ran.stdout), "hi", "{call:?}: {ran:?}");
        let stderr = if said.is_empty() {
            NO_GATE.to_owned()
        } else {
            format!("{NO_GATE}\n{said}")
        };
        assert_eq!(lines(&ran.stderr), stderr, "{call:?}: {ran:?}");
    }
    assert!(!case.folder().join("x.log").exists(), "the launcher wrote a log");
}

#[test]
fn with_no_gate_a_powershell_command_exits_0_or_1() {
    let case = Case::new("with_no_gate_a_powershell_command_exits_0_or_1");
    let missing = shown(&case.folder().join("missing"));
    let options = ["-Log", "x.log", "-TimeoutSeconds", "5", "-Slot", "heavy", "--"];

    let passing: Vec<&str> = options.iter().chain(&["Write-Output", "hi"]).copied().collect();
    let ran = run_launcher(&case, &passing);
    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(lines(&ran.stdout), "hi", "{ran:?}");
    assert_eq!(lines(&ran.stderr), NO_GATE, "{ran:?}");

    let failing: Vec<&str> = options.iter().chain(&["Get-Item", missing.as_str()]).copied().collect();
    let ran = run_launcher(&case, &failing);
    assert_eq!(ran.code, Some(1), "{ran:?}");
    assert_eq!(ran.stdout, "", "{ran:?}");
    assert!(
        lines(&ran.stderr).starts_with(&format!("{NO_GATE}\ngate: ")),
        "the failure was not said: {ran:?}"
    );
}

/// `pwsh -File` gives the launcher the argv words as they came, so a `-p:X=Y` a caller's session split into `-p:` and
/// `X=Y` after the `--` it ate reaches the launcher as two words; the command is given them as one.
#[test]
fn with_no_gate_a_split_colon_word_is_rejoined() {
    let case = Case::new("with_no_gate_a_split_colon_word_is_rejoined");
    let call = ["-Log", "x.log", "-TimeoutSeconds", "5", "-Slot", "heavy", FAKE_TOOL, "argv", "-p:", "X=Y"];

    let ran = run_launcher(&case, &call);

    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(lines(&ran.stdout), "-p:X=Y", "{ran:?}");
    assert_eq!(lines(&ran.stderr), NO_GATE, "{ran:?}");
}

#[test]
fn with_no_gate_the_cd_folder_is_the_command_s_working_folder() {
    let case = Case::new("with_no_gate_the_cd_folder_is_the_command_s_working_folder");
    let work = case.folder().join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let folder = shown(&work);
    let equals = format!("--cd={folder}");
    let calls: [Vec<&str>; 3] = [
        vec![
            "-Log",
            "x.log",
            "-TimeoutSeconds",
            "5",
            "-Slot",
            "heavy",
            "-Cd",
            &folder,
            "--",
            FAKE_TOOL,
            "cwd",
        ],
        vec![
            "run",
            "--cd",
            &folder,
            "--log",
            "x.log",
            "--timeout",
            "5",
            "--slot",
            "heavy",
            "--",
            FAKE_TOOL,
            "cwd",
        ],
        vec![
            "run",
            &equals,
            "--log",
            "x.log",
            "--timeout",
            "5",
            "--slot",
            "heavy",
            "--",
            FAKE_TOOL,
            "cwd",
        ],
    ];

    for call in calls {
        let ran = run_launcher(&case, &call);
        assert_eq!(ran.code, Some(0), "{call:?}: {ran:?}");
        assert_eq!(lines(&ran.stdout), format!("cwd {folder}"), "{call:?}: {ran:?}");
        assert_eq!(lines(&ran.stderr), NO_GATE, "{call:?}: {ran:?}");
    }
}

/// Option names are read ignoring case, as PowerShell reads them: with no `--`, an option the launcher did not know
/// would be taken for the command.
#[test]
fn with_no_gate_lower_case_option_names_are_read() {
    let case = Case::new("with_no_gate_lower_case_option_names_are_read");
    let work = case.folder().join("work");
    fs::create_dir_all(&work).expect("the work folder can be made");
    let folder = shown(&work);
    let call = [
        "-log",
        "x.log",
        "-timeoutseconds",
        "5",
        "-slot",
        "heavy",
        "-nomarkers",
        "-cd",
        &folder,
        FAKE_TOOL,
        "cwd",
    ];

    let ran = run_launcher(&case, &call);

    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(lines(&ran.stdout), format!("cwd {folder}"), "{ran:?}");
    assert_eq!(lines(&ran.stderr), NO_GATE, "{ran:?}");
}

#[test]
fn with_no_gate_env_reaches_the_command() {
    let case = Case::new("with_no_gate_env_reaches_the_command");
    let steps = [FAKE_TOOL, "env", "LAUNCHER_CASE_A", "env", "LAUNCHER_CASE_B"];
    let compat = [
        "-Log",
        "x.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "-Env",
        "LAUNCHER_CASE_A=1",
        "-Env:LAUNCHER_CASE_B=2",
        "--",
    ];
    let cli = [
        "run",
        "--log",
        "x.log",
        "--timeout",
        "5",
        "--slot",
        "heavy",
        "--env",
        "LAUNCHER_CASE_A=1",
        "--env=LAUNCHER_CASE_B=2",
        "--",
    ];

    for options in [&compat[..], &cli[..]] {
        let call: Vec<&str> = options.iter().chain(&steps).copied().collect();
        let ran = run_launcher(&case, &call);
        assert_eq!(ran.code, Some(0), "{call:?}: {ran:?}");
        assert_eq!(lines(&ran.stdout), "1\n2", "{call:?}: {ran:?}");
        assert_eq!(lines(&ran.stderr), NO_GATE, "{call:?}: {ran:?}");
    }

    let refused = [
        "-Log",
        "x.log",
        "-TimeoutSeconds",
        "5",
        "-Slot",
        "heavy",
        "-Env",
        "gate_x=1",
        "--",
        FAKE_TOOL,
        "out",
        "ran",
    ];
    let ran = run_launcher(&case, &refused);
    assert_eq!(ran.code, Some(2), "{ran:?}");
    assert_eq!(ran.stdout, "", "the command ran: {ran:?}");
    assert_eq!(lines(&ran.stderr), "gate: -Env cannot set GATE_ names: gate_x", "{ran:?}");
}

/// An in-session caller's own process holds the variables the launcher sets: each is put back after the command, a
/// variable that was not set before going away again.
#[test]
fn with_no_gate_an_in_session_call_restores_its_variables() {
    let case = Case::new("with_no_gate_an_in_session_call_restores_its_variables");
    let line = format!(
        "$env:LAUNCHER_CASE_A = 'before'; $env:LAUNCHER_CASE_B = $null; \
         & '{}' -Log x.log -TimeoutSeconds 5 -Slot heavy -Env LAUNCHER_CASE_A=during -Env LAUNCHER_CASE_B=new -- \
         {FAKE_TOOL} env LAUNCHER_CASE_A env LAUNCHER_CASE_B; $code = $LASTEXITCODE; \
         \"after $env:LAUNCHER_CASE_A/$env:LAUNCHER_CASE_B\"; exit $code",
        shown(&launcher())
    );

    let ran = run_session(&case, &line);

    assert_eq!(ran.code, Some(0), "{ran:?}");
    assert_eq!(lines(&ran.stdout), "during\nnew\nafter before/", "{ran:?}");
    assert_eq!(lines(&ran.stderr), NO_GATE, "{ran:?}");
}

/// A session call whose `--` PowerShell's parser ate, from a caller that turned native errors into throws: the command
/// is found without the `--` and its own status is the launcher's.
#[test]
fn with_no_gate_an_in_session_call_with_the_dash_dash_eaten_runs() {
    let case = Case::new("with_no_gate_an_in_session_call_with_the_dash_dash_eaten_runs");
    let line = format!(
        "$PSNativeCommandUseErrorActionPreference = $true; \
         & '{}' -Log x.log -TimeoutSeconds 5 -Slot heavy {FAKE_TOOL} out hi exit 3; exit $LASTEXITCODE",
        shown(&launcher())
    );

    let ran = run_session(&case, &line);

    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert_eq!(lines(&ran.stdout), "hi", "{ran:?}");
    assert_eq!(lines(&ran.stderr), NO_GATE, "{ran:?}");
}

#[test]
fn with_no_gate_stop_tree_says_it_cannot_and_fails() {
    let case = Case::new("with_no_gate_stop_tree_says_it_cannot_and_fails");
    let calls: [&[&str]; 3] = [&["-StopTree", "4242"], &["-StopTree:4242"], &["stop-tree", "4242"]];

    for call in calls {
        let ran = run_launcher(&case, call);
        assert_eq!(ran.code, Some(1), "{call:?}: {ran:?}");
        assert_eq!(ran.stdout, "", "{call:?}: {ran:?}");
        assert_eq!(
            lines(&ran.stderr),
            "gate: no gate.exe found; cannot stop the tree of 4242",
            "{call:?}: {ran:?}"
        );
    }
}

//! The install script: the exe it copies, the aside copy a running gate is left in, the user PATH it writes, and what
//! it prints. Every case's install reads and writes a registry key of its own and its own LOCALAPPDATA, so none
//! touches the real user environment or the real install folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::support::{Case, FAKE_TOOL, Outcome, finish, shown, start, wait_for_journal_line, wait_until_closed};

/// `tools/install.ps1`, the script under test: a checked-in file a case runs.
fn script() -> PathBuf {
    checked_in("install.ps1")
}

/// The checked-in file `name` of the repo's `tools` folder: the suite's one way to name a checked-in file, and the
/// only caller of the checkout route, so a case that needs another file comes here rather than to the route itself.
pub(crate) fn checked_in(name: &str) -> PathBuf {
    crate::checkout::repo_file(&format!("tools/{name}"))
}

/// The user environment registry key a case's install reads and writes, removed when the guard goes, so no case
/// touches the real `HKCU:\Environment`.
struct EnvironmentKey(String);

impl EnvironmentKey {
    /// The key named for `case` and this test process, under the suite's own registry folder, so two suite runs at
    /// once (two worktrees) never share a case's key.
    fn new(case: &str) -> EnvironmentKey {
        EnvironmentKey(format!(r"HKCU:\Software\gate-suite\{case}-{}", std::process::id()))
    }
}

impl Drop for EnvironmentKey {
    /// Takes the case's key, and the folder that holds it once no case needs it, away.
    fn drop(&mut self) {
        let parent = self.0.rsplit_once('\\').map(|(parent, _)| parent).unwrap_or_default();
        let remove = format!(
            "Remove-Item -LiteralPath '{}' -Recurse -Force -ErrorAction SilentlyContinue; \
             Remove-Item -LiteralPath '{}' -Force -ErrorAction SilentlyContinue; exit 0",
            self.0, parent
        );
        let _ = Command::new("pwsh")
            .args(["-NoProfile", "-Command", &remove])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `text` with Windows line ends turned into `\n`, so a case asserts the lines a PowerShell script said.
fn unix(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// A pwsh running `script` as its command, the way the suite readies a program.
fn pwsh_command(script: &str) -> Command {
    let mut command = Command::new("pwsh");
    command.args(["-NoProfile", "-Command", script]);
    command
}

/// The install script with `args`, the case's registry key and its own LOCALAPPDATA.
fn install_command(case: &Case, key: &EnvironmentKey, args: &[&str]) -> Command {
    let words: Vec<String> = ["-NoProfile", "-File", &shown(&script()), "-EnvironmentKey", &key.0]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut command = Command::new("pwsh");
    case.prepare_without_state_dir(&mut command, &words);
    command
}

/// Runs the install script for `case` and returns what it did.
fn install(case: &Case, key: &EnvironmentKey, args: &[&str]) -> Outcome {
    finish(install_command(case, key, args))
}

/// What a child of `case` sees as its LOCALAPPDATA when the suite asks for `value`: `None` for the empty value
/// Windows will not hold as a variable, which a case proves before relying on it.
fn seen_local_app_data(case: &Case, value: &str) -> Option<String> {
    let mut command = Command::new("pwsh");
    command.args(["-NoProfile", "-Command", "$env:LOCALAPPDATA"]);
    case.prepare_without_state_dir(&mut command, &[]);
    command.env("LOCALAPPDATA", value);
    let outcome = finish(command);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let seen = unix(&outcome.stdout).trim_end_matches('\n').to_owned();
    if seen.is_empty() { None } else { Some(seen) }
}

/// The install folder a case's own LOCALAPPDATA holds.
fn install_dir(case: &Case) -> PathBuf {
    case.local_app_data().join("gate").join("bin")
}

/// The installed gate.
fn installed_gate(case: &Case) -> PathBuf {
    install_dir(case).join("gate.exe")
}

/// A copy of the built gate in the case folder, under `name`.
fn copy_gate(case: &Case, name: &str) -> PathBuf {
    let copy = case.folder().join(name);
    fs::copy(env!("CARGO_BIN_EXE_gate"), &copy).expect("the gate can be copied");
    copy
}

/// A copy of the built gate with a byte appended, so its SHA-256 differs from the installed gate's while it still
/// runs and prints its version.
fn copy_gate_with_a_byte(case: &Case, name: &str) -> PathBuf {
    let copy = case.folder().join(name);
    let mut bytes = fs::read(env!("CARGO_BIN_EXE_gate")).expect("the built gate can be read");
    bytes.push(b'\0');
    fs::write(&copy, bytes).expect("the copy can be written");
    copy
}

/// What the gate at `exe` prints after `gate `.
fn version_of(exe: &Path) -> String {
    let mut command = Command::new(exe);
    command.arg("--version");
    let outcome = finish(command);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    outcome
        .stdout
        .trim_end()
        .strip_prefix("gate ")
        .expect("a gate prints its version after `gate `")
        .to_owned()
}

/// Asserts `left` and `right` hold the same bytes, the same thing the script's SHA-256 comparison decides.
fn assert_same_bytes(left: &Path, right: &Path) {
    assert_eq!(
        fs::read(left).expect("the installed gate can be read"),
        fs::read(right).expect("the source gate can be read"),
        "{} and {} hold different bytes",
        shown(left),
        shown(right)
    );
}

/// The aside copies in a case's install folder, in name order.
fn aside_copies(case: &Case) -> Vec<PathBuf> {
    let mut copies: Vec<PathBuf> = fs::read_dir(install_dir(case))
        .expect("the install folder can be listed")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let aside = Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("old"));
            name.starts_with("gate.exe.") && aside
        })
        .collect();
    copies.sort();
    copies
}

/// The `Path` value of `key`: its kind and its raw, unexpanded text, or `None` when the key or the value is absent.
fn path_value(key: &EnvironmentKey) -> Option<(String, String)> {
    let check = format!(
        "$item = Get-Item -LiteralPath '{}' -ErrorAction SilentlyContinue; \
         if ($null -ne $item) {{ \
           $value = $item.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); \
           if ($null -ne $value) {{ $item.GetValueKind('Path').ToString(); $value }} \
         }}; exit 0",
        key.0
    );
    let outcome = finish(pwsh_command(&check));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let text = unix(&outcome.stdout);
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return None;
    }
    let (kind, value) = text
        .split_once('\n')
        .expect("the check prints the kind and the value, each on its own line");
    Some((kind.to_owned(), value.to_owned()))
}

/// Presets the `Path` value of `key` to `value` as an expand-string, making the key when it is missing.
fn preset_path(key: &EnvironmentKey, value: &str) {
    let set = format!(
        "$key = '{}'; if (-not (Test-Path -LiteralPath $key)) {{ New-Item -Path $key -Force | Out-Null }}; \
         Set-ItemProperty -LiteralPath $key -Name 'Path' -Value '{}' -Type ExpandString; exit 0",
        key.0, value
    );
    let outcome = finish(pwsh_command(&set));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// Takes the `Path` value of `key` away, so a later install has to add the folder again.
fn delete_path_value(key: &EnvironmentKey) {
    let remove = format!(
        "Remove-ItemProperty -LiteralPath '{}' -Name 'Path' -ErrorAction SilentlyContinue; exit 0",
        key.0
    );
    let outcome = finish(pwsh_command(&remove));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The words of a run a case holds: its ceiling is far above the suite's hang guard, so the gate does not end it
/// while the case is still asserting.
fn held_run(go: &str) -> [&str; 13] {
    [
        "run",
        "--slot",
        "light",
        "--timeout",
        "900",
        "--log",
        "out.log",
        "--",
        FAKE_TOOL,
        "wait",
        go,
        "exit",
        "0",
    ]
}

#[test]
fn install_copies_the_exe_and_adds_the_path() {
    let case = Case::new("install_copies_the_exe_and_adds_the_path");
    let key = EnvironmentKey::new("install_copies_the_exe_and_adds_the_path");
    let exe = copy_gate(&case, "gate-a.exe");
    let exe_word = shown(&exe);

    let outcome = install(&case, &key, &["-Exe", &exe_word]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let dest = installed_gate(&case);
    let dir = shown(&install_dir(&case));
    let version = version_of(&exe);
    assert!(
        unix(&outcome.stdout).contains(&format!("install: installed gate {version} at {}", shown(&dest))),
        "{outcome:?}"
    );
    assert!(
        unix(&outcome.stdout).contains(&format!("install: added {dir} to your user PATH; open a new terminal to use gate")),
        "{outcome:?}"
    );
    assert_same_bytes(&dest, &exe);
    assert_eq!(
        path_value(&key),
        Some(("ExpandString".to_owned(), dir)),
        "the folder is added once, as an expand-string"
    );
}

#[test]
fn install_puts_the_bridge_script_beside_the_exe() {
    let case = Case::new("install_puts_the_bridge_script_beside_the_exe");
    let key = EnvironmentKey::new("install_puts_the_bridge_script_beside_the_exe");
    let exe = copy_gate(&case, "gate-a.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        fs::read(install_dir(&case).join("remote-bridge.ps1")).expect("the bridge script is installed"),
        fs::read(checked_in("remote-bridge.ps1")).expect("the checked-in bridge script can be read"),
        "the installed bridge script is the checked-in one"
    );
}

#[test]
fn a_second_install_of_the_same_exe_changes_nothing() {
    let case = Case::new("a_second_install_of_the_same_exe_changes_nothing");
    let key = EnvironmentKey::new("a_second_install_of_the_same_exe_changes_nothing");
    let exe = copy_gate(&case, "gate-a.exe");
    let exe_word = shown(&exe);
    let dest = installed_gate(&case);
    let dir = shown(&install_dir(&case));

    let first = install(&case, &key, &["-Exe", &exe_word]);
    assert_eq!(first.code, Some(0), "{first:?}");
    // The entry is taken away again, so the same-bytes path still runs the PATH step and adds it back.
    delete_path_value(&key);
    let second = install(&case, &key, &["-Exe", &exe_word]);

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(
        unix(&second.stdout),
        format!(
            "install: gate {} already installed at {}\ninstall: added {dir} to your user PATH; open a new terminal to use gate\n",
            version_of(&exe),
            shown(&dest)
        ),
        "the same bytes are not copied again, the PATH step still runs: {second:?}"
    );
    assert_same_bytes(&dest, &exe);
    assert!(aside_copies(&case).is_empty(), "no aside copy was made: {:?}", aside_copies(&case));
    assert_eq!(path_value(&key), Some(("ExpandString".to_owned(), dir)));
}

#[test]
fn install_over_a_running_gate_moves_it_aside() {
    let case = Case::new("install_over_a_running_gate_moves_it_aside");
    let key = EnvironmentKey::new("install_over_a_running_gate_moves_it_aside");
    let first = copy_gate(&case, "gate-a.exe");
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let second_word = shown(&second);
    let dest = installed_gate(&case);
    let version = version_of(&first);

    let first_install = install(&case, &key, &["-Exe", &shown(&first)]);
    assert_eq!(first_install.code, Some(0), "{first_install:?}");

    let go = case.folder().join("go");
    let go_word = shown(&go);
    let running = start({
        let mut command = Command::new(&dest);
        case.prepare(&mut command, &held_run(&go_word));
        command
    });
    wait_for_journal_line(&case.root(), r#""event":"take""#);

    let replaced = install(&case, &key, &["-Exe", &second_word]);
    let again = install(&case, &key, &["-Exe", &second_word]);
    let aside_while_held = aside_copies(&case);

    // The held run is let go and seen to end before any assertion, so a failing one leaves no command waiting.
    fs::write(&go, "").expect("the go file can be written");
    let outcome = running.finish();

    assert_eq!(replaced.code, Some(0), "{replaced:?}");
    assert_eq!(
        unix(&replaced.stdout),
        format!(
            "install: replaced gate {version}\ninstall: installed gate {version} at {}\n",
            shown(&dest)
        ),
        "the replaced version is the running gate's, read from the exe moved aside: {replaced:?}"
    );
    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(again.stderr, "", "an aside copy a running gate still holds is left silently: {again:?}");
    assert_eq!(outcome.code, Some(0), "the gate that was moved aside ran to its end: {outcome:?}");
    assert_same_bytes(&dest, &second);
    assert_eq!(
        aside_while_held.len(),
        1,
        "the running gate was moved aside, not broken: {aside_while_held:?}"
    );
    wait_until_closed(&aside_while_held[0]);

    let last = install(&case, &key, &["-Exe", &second_word]);

    assert_eq!(last.code, Some(0), "{last:?}");
    assert!(
        aside_copies(&case).is_empty(),
        "a later install deletes the aside copy no process holds: {:?}",
        aside_copies(&case)
    );
}

#[test]
fn an_existing_path_value_keeps_its_kind_and_entries() {
    let case = Case::new("an_existing_path_value_keeps_its_kind_and_entries");
    let key = EnvironmentKey::new("an_existing_path_value_keeps_its_kind_and_entries");
    let exe = copy_gate(&case, "gate-a.exe");
    let dir = shown(&install_dir(&case));
    preset_path(&key, r"%USERPROFILE%\bin;C:\x");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let (kind, value) = path_value(&key).expect("the key has a Path value");
    assert_eq!(kind, "ExpandString", "the value keeps its kind: {outcome:?}");
    assert_eq!(
        value,
        format!(r"%USERPROFILE%\bin;C:\x;{dir}"),
        "the entries stay unexpanded and the folder is appended: {outcome:?}"
    );
}

#[test]
fn a_path_entry_that_already_holds_the_folder_adds_nothing() {
    let case = Case::new("a_path_entry_that_already_holds_the_folder_adds_nothing");
    let key = EnvironmentKey::new("a_path_entry_that_already_holds_the_folder_adds_nothing");
    let exe = copy_gate(&case, "gate-a.exe");
    let preset = r"%LOCALAPPDATA%\gate\bin\";
    preset_path(&key, preset);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        unix(&outcome.stdout).contains("install: installed gate"),
        "the exe is installed: {outcome:?}"
    );
    assert!(
        !unix(&outcome.stdout).contains("install: added"),
        "the entry is already there expanded, with its trailing backslash trimmed: {outcome:?}"
    );
    assert_eq!(
        path_value(&key),
        Some(("ExpandString".to_owned(), preset.to_owned())),
        "the value is left as it was: {outcome:?}"
    );
}

#[test]
fn install_without_localappdata_fails() {
    let case = Case::new("install_without_localappdata_fails");
    let key = EnvironmentKey::new("install_without_localappdata_fails");
    let exe = copy_gate(&case, "gate-a.exe");
    assert_eq!(
        seen_local_app_data(&case, ""),
        None,
        "an empty LOCALAPPDATA reaches a child as no variable, which the script reads as unset"
    );
    let mut command = install_command(&case, &key, &["-Exe", &shown(&exe)]);
    command.env("LOCALAPPDATA", "");

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(unix(&outcome.stderr), "install: LOCALAPPDATA is not set\n", "{outcome:?}");
}

#[test]
fn install_whatif_changes_nothing() {
    let case = Case::new("install_whatif_changes_nothing");
    let key = EnvironmentKey::new("install_whatif_changes_nothing");
    let exe = copy_gate(&case, "gate-a.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!installed_gate(&case).exists(), "nothing was installed: {outcome:?}");
    assert_eq!(path_value(&key), None, "no Path value was written: {outcome:?}");
}

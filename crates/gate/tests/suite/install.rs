//! The install script: the exe it copies, the aside copy a running gate is left in, the user PATH it writes, and what
//! it prints. Every case's install reads and writes a registry key of its own and its own LOCALAPPDATA, so none
//! touches the real user environment or the real install folder.

use std::fs;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::support::{
    Case, DenyEntry, FAKE_TOOL, Outcome, finish, git, git_init, junction, names, shown, start, version_of, wait_for_journal_line, wait_until_closed,
};

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
pub(crate) fn unix(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// A pwsh running `script` as its command, the way the suite readies a program.
fn pwsh_command(script: &str) -> Command {
    let mut command = Command::new("pwsh");
    command.args(["-NoProfile", "-Command", script]);
    command
}

/// The install script with `args`, the desktop helper at `helper` (`-HelperExe`), the case's registry key and its own
/// LOCALAPPDATA.
fn install_command(case: &Case, key: &EnvironmentKey, helper: &Path, args: &[&str]) -> Command {
    let helper_word = shown(helper);
    let words: Vec<&str> = ["-HelperExe", &helper_word].into_iter().chain(args.iter().copied()).collect();
    bare_install_command(case, key, &words)
}

/// The install script with only `args`, the case's registry key and its own LOCALAPPDATA: no `-HelperExe` unless
/// `args` name one.
fn bare_install_command(case: &Case, key: &EnvironmentKey, args: &[&str]) -> Command {
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

/// Runs the install script for `case` with the case's default stand-in helper and returns what it did.
fn install(case: &Case, key: &EnvironmentKey, args: &[&str]) -> Outcome {
    install_with_helper(case, key, &default_helper(case), args)
}

/// Runs the install script for `case` with the desktop helper at `helper` and returns what it did.
fn install_with_helper(case: &Case, key: &EnvironmentKey, helper: &Path, args: &[&str]) -> Outcome {
    finish(install_command(case, key, helper, args))
}

/// A stand-in desktop helper in the case folder under `name`, holding `bytes`: the install only copies the helper, so
/// any small file without the seams' text stands in for it.
fn stand_in_helper(case: &Case, name: &str, bytes: &[u8]) -> PathBuf {
    let helper = case.folder().join(name);
    fs::write(&helper, bytes).expect("the stand-in helper can be written");
    helper
}

/// The stand-in helper a case installs when it does not care which: the same bytes on every call.
fn default_helper(case: &Case) -> PathBuf {
    stand_in_helper(case, "helper-default.exe", b"a stand-in desktop helper")
}

/// The installed desktop helper.
fn installed_helper(case: &Case) -> PathBuf {
    install_dir(case).join("gate-desktop-helper.exe")
}

/// What a success line adds after the installed gate.exe: where its desktop helper is.
fn with_helper(case: &Case) -> String {
    format!(" with its desktop helper at {}", shown(&installed_helper(case)))
}

/// Opens `path` for reading and lets others only read it, so while the handle is held the file can be hashed but not
/// moved aside, renamed or deleted.
fn hold_against_moves(path: &Path) -> fs::File {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .expect("the file can be held")
}

/// Opens `path` and lets no one else open it, so while the handle is held the file cannot even be read.
fn hold_against_reads(path: &Path) -> fs::File {
    fs::OpenOptions::new().read(true).share_mode(0).open(path).expect("the file can be held")
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
pub(crate) fn copy_gate(case: &Case, name: &str) -> PathBuf {
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

/// The gate.exe aside copies in a case's install folder, in name order.
fn aside_copies(case: &Case) -> Vec<PathBuf> {
    asides_of(case, "gate.exe.")
}

/// The desktop helper's aside copies in a case's install folder, in name order.
fn helper_aside_copies(case: &Case) -> Vec<PathBuf> {
    asides_of(case, "gate-desktop-helper.exe.")
}

/// The aside copies in a case's install folder whose names start with `prefix`, in name order.
fn asides_of(case: &Case, prefix: &str) -> Vec<PathBuf> {
    let mut copies: Vec<PathBuf> = fs::read_dir(install_dir(case))
        .expect("the install folder can be listed")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let aside = Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("old"));
            name.starts_with(prefix) && aside
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
fn install_says_the_version_the_installed_exe_prints() {
    let case = Case::new("install_says_the_version_the_installed_exe_prints");
    let key = EnvironmentKey::new("install_says_the_version_the_installed_exe_prints");
    let exe = copy_gate(&case, "gate-a.exe");
    let version = version_of(&exe);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    // `-Exe` skips the release build the suite does not run, so this pins the text path install.ps1 carries: what
    // follows `gate ` is the whole version, the package version and the build's own id, which `Get-GateVersion` keeps
    // (a suite build has no `GATE_BUILD_ID`, so its id is `dev`).
    assert!(
        version.starts_with(&format!("{}+", env!("CARGO_PKG_VERSION"))),
        "the installed gate names the build it was built from: {version}"
    );
    assert!(
        unix(&outcome.stdout).contains(&format!("install: installed gate {version} at {}", shown(&installed_gate(&case)))),
        "{outcome:?}"
    );
}

#[test]
fn an_install_removes_a_leftover_bridge_shim() {
    let case = Case::new("an_install_removes_a_leftover_bridge_shim");
    let key = EnvironmentKey::new("an_install_removes_a_leftover_bridge_shim");
    let exe = copy_gate(&case, "gate-a.exe");
    let shim = install_dir(&case).join("remote-bridge.ps1");
    fs::create_dir_all(install_dir(&case)).expect("the install folder can be made");
    fs::write(&shim, "# the bridge shim an earlier install left\n").expect("the leftover shim can be written");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(!shim.exists(), "the leftover shim is gone: {outcome:?}");
    assert!(!outcome.stderr.contains("remote-bridge.ps1"), "a removed shim says nothing: {outcome:?}");
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
            "install: gate {} already installed at {}{}\ninstall: added {dir} to your user PATH; open a new terminal to use gate\n",
            version_of(&exe),
            shown(&dest),
            with_helper(&case)
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
            "install: replaced gate {version}\ninstall: installed gate {version} at {}{}\n",
            shown(&dest),
            with_helper(&case)
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

/// An aside copy an earlier install left, named as the script names one.
const ASIDE: &str = "gate.exe.20200101000000.4.old";

/// Installs a copy of the built gate for `case` and leaves an aside copy no process holds beside it, as an earlier
/// install would: the installed copy and the aside copy's path.
fn installed_with_an_aside(case: &Case, key: &EnvironmentKey) -> (PathBuf, PathBuf) {
    let exe = copy_gate(case, "gate-a.exe");
    let first = install(case, key, &["-Exe", &shown(&exe)]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let aside = install_dir(case).join(ASIDE);
    fs::write(&aside, b"an old gate no process runs").expect("the aside copy can be written");
    (exe, aside)
}

#[test]
fn an_install_while_a_push_holds_the_lock_changes_nothing() {
    let case = Case::new("an_install_while_a_push_holds_the_lock_changes_nothing");
    let key = EnvironmentKey::new("an_install_while_a_push_holds_the_lock_changes_nothing");
    let (exe, aside) = installed_with_an_aside(&case, &key);
    // The entry is taken away, so an install that went on would add it back.
    delete_path_value(&key);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(install_dir(&case).join("push.lock"))
        .expect("the lock can be held");
    let other = copy_gate_with_a_byte(&case, "gate-b.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&other)]);

    drop(lock);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        "install: another gate push is installing; nothing was changed\n",
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert!(aside.exists(), "the aside copy is left: {outcome:?}");
    assert_eq!(
        names(&install_dir(&case)),
        ["gate-desktop-helper.exe", "gate.exe", ASIDE, "push.lock"],
        "nothing was added or deleted"
    );
    assert_eq!(path_value(&key), None, "the PATH step did not run: {outcome:?}");
}

/// A `push.lock` that is a second name of another file is refused as a link is, and nothing is changed.
#[test]
fn an_install_refuses_a_push_lock_that_is_a_hard_link() {
    let case = Case::new("an_install_refuses_a_push_lock_that_is_a_hard_link");
    let key = EnvironmentKey::new("an_install_refuses_a_push_lock_that_is_a_hard_link");
    let (exe, aside) = installed_with_an_aside(&case, &key);
    // The entry is taken away, so an install that went on would add it back.
    delete_path_value(&key);
    let other = case.folder().join("other.txt");
    fs::write(&other, b"another file").expect("the other file is written");
    let lock = install_dir(&case).join("push.lock");
    fs::remove_file(&lock).expect("the first install's push.lock is removed");
    fs::hard_link(&other, &lock).expect("push.lock is made a second name of the other file");
    let newer = copy_gate_with_a_byte(&case, "gate-b.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&newer)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("install: {} is a link; nothing was changed\n", shown(&lock)),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert!(aside.exists(), "the aside copy is left: {outcome:?}");
    assert_eq!(
        names(&install_dir(&case)),
        ["gate-desktop-helper.exe", "gate.exe", ASIDE, "push.lock"],
        "nothing was added or deleted"
    );
    assert_eq!(path_value(&key), None, "the PATH step did not run: {outcome:?}");
    assert_eq!(
        fs::read(&other).expect("the other file is read"),
        b"another file",
        "the other file is untouched"
    );
}

#[test]
fn an_install_keeps_the_aside_copy_when_it_installs_nothing() {
    let case = Case::new("an_install_keeps_the_aside_copy_when_it_installs_nothing");
    let key = EnvironmentKey::new("an_install_keeps_the_aside_copy_when_it_installs_nothing");
    let (exe, aside) = installed_with_an_aside(&case, &key);
    let not_a_gate = case.folder().join("not-a-gate.exe");
    fs::write(&not_a_gate, b"no program").expect("the file can be written");

    let outcome = install(&case, &key, &["-Exe", &shown(&not_a_gate)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with("install: could not read the version of "),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert!(aside.exists(), "an install that installed nothing keeps the aside copy: {outcome:?}");
}

/// The icacls rights that deny this user running any file put in a folder, existing or new, so an exe there cannot
/// start while it can still be read, moved and deleted.
const NO_RUNNING_IN: &str = "(OI)(IO)(X)";

#[test]
fn an_install_whose_copy_does_not_run_puts_the_previous_gate_back() {
    let case = Case::new("an_install_whose_copy_does_not_run_puts_the_previous_gate_back");
    let key = EnvironmentKey::new("an_install_whose_copy_does_not_run_puts_the_previous_gate_back");
    let (exe, aside) = installed_with_an_aside(&case, &key);
    // The new exe answers its version where it is built; the copy the install puts in the folder cannot start.
    let denied = DenyEntry::new(&install_dir(&case), NO_RUNNING_IN);
    let other = copy_gate_with_a_byte(&case, "gate-b.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&other)]);

    drop(denied);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        "install: the installed gate.exe does not run; the previous gate.exe is back in place\n",
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert_eq!(aside_copies(&case), [aside], "the aside copies are kept, the previous gate back in place");
}

#[test]
fn install_whatif_of_the_installed_exe_says_already_installed() {
    let case = Case::new("install_whatif_of_the_installed_exe_says_already_installed");
    let key = EnvironmentKey::new("install_whatif_of_the_installed_exe_says_already_installed");
    let (exe, aside) = installed_with_an_aside(&case, &key);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!(
            "install: gate {} already installed at {}{}\nWhat if: Performing the operation \"Delete\" on target \"{}\".\n",
            version_of(&exe),
            shown(&installed_gate(&case)),
            with_helper(&case),
            shown(&aside)
        ),
        "what a real run says and the aside delete it takes, and no install step: {outcome:?}"
    );
    assert!(aside.exists(), "nothing was deleted: {outcome:?}");
}

#[test]
fn install_whatif_lists_the_aside_deletes_it_would_take() {
    let case = Case::new("install_whatif_lists_the_aside_deletes_it_would_take");
    let key = EnvironmentKey::new("install_whatif_lists_the_aside_deletes_it_would_take");
    let (exe, aside) = installed_with_an_aside(&case, &key);
    let other = copy_gate_with_a_byte(&case, "gate-b.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&other), "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        outcome.stdout.contains(&shown(&aside)),
        "the aside copy a verified install would delete is listed: {outcome:?}"
    );
    assert!(aside.exists(), "nothing was deleted: {outcome:?}");
    assert_same_bytes(&installed_gate(&case), &exe);
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
    let mut command = install_command(&case, &key, &default_helper(&case), &["-Exe", &shown(&exe)]);
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
    assert!(!install_dir(&case).exists(), "no folder and no push.lock was made: {outcome:?}");
    assert_eq!(path_value(&key), None, "no Path value was written: {outcome:?}");
}

#[test]
fn an_install_puts_the_desktop_helper_beside_gate() {
    let case = Case::new("an_install_puts_the_desktop_helper_beside_gate");
    let key = EnvironmentKey::new("an_install_puts_the_desktop_helper_beside_gate");
    let exe = copy_gate(&case, "gate-a.exe");
    let helper = stand_in_helper(&case, "helper-a.exe", b"helper one");

    let outcome = install_with_helper(&case, &key, &helper, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert!(
        unix(&outcome.stdout).contains(&format!(
            "install: installed gate {} at {}{}\n",
            version_of(&exe),
            shown(&installed_gate(&case)),
            with_helper(&case)
        )),
        "the success line names both files: {outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert_same_bytes(&installed_helper(&case), &helper);
}

#[test]
fn an_identical_helper_is_kept() {
    let case = Case::new("an_identical_helper_is_kept");
    let key = EnvironmentKey::new("an_identical_helper_is_kept");
    let first = copy_gate(&case, "gate-a.exe");
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let helper = stand_in_helper(&case, "helper-a.exe", b"helper one");
    let installed = install_with_helper(&case, &key, &helper, &["-Exe", &shown(&first)]);
    assert_eq!(installed.code, Some(0), "{installed:?}");
    // A held handle that lets others only read: a helper the install moved aside or replaced would fail the install.
    let held = hold_against_moves(&installed_helper(&case));

    let outcome = install_with_helper(&case, &key, &helper, &["-Exe", &shown(&second)]);

    drop(held);
    assert_eq!(outcome.code, Some(0), "the identical helper is left as it is: {outcome:?}");
    assert_same_bytes(&installed_gate(&case), &second);
    assert_same_bytes(&installed_helper(&case), &helper);
    assert!(
        helper_aside_copies(&case).is_empty(),
        "no helper aside copy was made: {:?}",
        helper_aside_copies(&case)
    );
}

#[test]
fn a_helper_built_with_its_seams_is_refused_and_nothing_is_installed() {
    let case = Case::new("a_helper_built_with_its_seams_is_refused_and_nothing_is_installed");
    let key = EnvironmentKey::new("a_helper_built_with_its_seams_is_refused_and_nothing_is_installed");
    let exe = copy_gate(&case, "gate-a.exe");
    let helper = stand_in_helper(&case, "helper-seams.exe", b"\0\0GATE_DESKTOP_TEST_FAULT\0\0");

    let outcome = install_with_helper(&case, &key, &helper, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!(
            "install: the desktop helper at '{}' was built with its test seams; build it without features\n",
            shown(&helper)
        ),
        "{outcome:?}"
    );
    assert!(!install_dir(&case).exists(), "nothing was installed: {outcome:?}");
    assert_eq!(path_value(&key), None, "the PATH step did not run: {outcome:?}");
}

#[test]
fn a_helper_that_cannot_be_placed_leaves_the_previous_pair() {
    let case = Case::new("a_helper_that_cannot_be_placed_leaves_the_previous_pair");
    let key = EnvironmentKey::new("a_helper_that_cannot_be_placed_leaves_the_previous_pair");
    let first = copy_gate(&case, "gate-a.exe");
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let old_helper = stand_in_helper(&case, "helper-a.exe", b"helper one");
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");
    let installed = install_with_helper(&case, &key, &old_helper, &["-Exe", &shown(&first)]);
    assert_eq!(installed.code, Some(0), "{installed:?}");
    let held = hold_against_moves(&installed_helper(&case));

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&second)]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with(&format!("install: could not install {}: ", shown(&installed_helper(&case)))),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &first);
    assert_same_bytes(&installed_helper(&case), &old_helper);
    assert!(aside_copies(&case).is_empty(), "gate.exe was not touched: {:?}", aside_copies(&case));
    assert!(
        helper_aside_copies(&case).is_empty(),
        "no helper aside copy is left: {:?}",
        helper_aside_copies(&case)
    );
}

#[test]
fn a_gate_that_does_not_run_puts_the_previous_helper_back() {
    let case = Case::new("a_gate_that_does_not_run_puts_the_previous_helper_back");
    let key = EnvironmentKey::new("a_gate_that_does_not_run_puts_the_previous_helper_back");
    let first = copy_gate(&case, "gate-a.exe");
    let old_helper = stand_in_helper(&case, "helper-a.exe", b"helper one");
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");
    let installed = install_with_helper(&case, &key, &old_helper, &["-Exe", &shown(&first)]);
    assert_eq!(installed.code, Some(0), "{installed:?}");
    // The new exe answers its version where it is built; the copy the install puts in the folder cannot start.
    let denied = DenyEntry::new(&install_dir(&case), NO_RUNNING_IN);
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&second)]);

    drop(denied);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        "install: the installed gate.exe does not run; the previous gate.exe is back in place\n",
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &first);
    assert_same_bytes(&installed_helper(&case), &old_helper);
    assert!(
        helper_aside_copies(&case).is_empty(),
        "the previous helper was moved back, not left aside: {:?}",
        helper_aside_copies(&case)
    );
}

#[test]
fn what_if_lists_the_helper() {
    let case = Case::new("what_if_lists_the_helper");
    let key = EnvironmentKey::new("what_if_lists_the_helper");
    let exe = copy_gate(&case, "gate-a.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let stdout = unix(&outcome.stdout);
    for target in [installed_helper(&case), installed_gate(&case)] {
        assert!(
            stdout.contains(&format!(
                "What if: Performing the operation \"Install\" on target \"{}\".\n",
                shown(&target)
            )),
            "the install of {} is listed: {outcome:?}",
            shown(&target)
        );
    }
    assert!(!install_dir(&case).exists(), "nothing was installed: {outcome:?}");
}

#[test]
fn the_install_build_names_no_features() {
    let mut command = Command::new("pwsh");
    command.args(["-NoProfile", "-File", &shown(&script()), "-PrintBuild"]);

    let outcome = finish(command);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let printed = unix(&outcome.stdout);
    for banned in ["--features", "--all-features", "test-seams"] {
        assert!(!printed.contains(banned), "an installed build names no features: {outcome:?}");
    }
    assert_eq!(
        printed, "build --release -p gate -p gate-desktop\n",
        "the release build makes the desktop helper with gate.exe and nothing else: {outcome:?}"
    );
}

/// Installs gate-a.exe with a helper holding `b"helper one"` for `case`: the installed pair's sources, gate first.
fn installed_pair_a(case: &Case, key: &EnvironmentKey) -> (PathBuf, PathBuf) {
    let gate = copy_gate(case, "gate-a.exe");
    let helper = stand_in_helper(case, "helper-a.exe", b"helper one");
    let installed = install_with_helper(case, key, &helper, &["-Exe", &shown(&gate)]);
    assert_eq!(installed.code, Some(0), "{installed:?}");
    (gate, helper)
}

#[test]
fn a_gate_that_cannot_be_moved_aside_after_the_helper_was_placed_leaves_the_previous_pair() {
    let name = "a_gate_that_cannot_be_moved_aside_after_the_helper_was_placed_leaves_the_previous_pair";
    let case = Case::new(name);
    let key = EnvironmentKey::new(name);
    let (first, old_helper) = installed_pair_a(&case, &key);
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");
    let held = hold_against_moves(&installed_gate(&case));

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&second)]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with(&format!("install: could not install {}: ", shown(&installed_gate(&case)))),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &first);
    assert_same_bytes(&installed_helper(&case), &old_helper);
    assert!(
        helper_aside_copies(&case).is_empty(),
        "the previous helper was moved back, not left aside: {:?}",
        helper_aside_copies(&case)
    );
}

#[test]
fn a_first_install_of_the_helper_that_fails_removes_it() {
    let case = Case::new("a_first_install_of_the_helper_that_fails_removes_it");
    let key = EnvironmentKey::new("a_first_install_of_the_helper_that_fails_removes_it");
    let (first, _) = installed_pair_a(&case, &key);
    // A folder that holds gate.exe and no helper, as one an install before the helper left.
    fs::remove_file(installed_helper(&case)).expect("the installed helper can be removed");
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");
    let held = hold_against_moves(&installed_gate(&case));

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&second)]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_same_bytes(&installed_gate(&case), &first);
    assert!(
        !installed_helper(&case).exists(),
        "the helper this install placed is removed: {outcome:?}"
    );
    assert!(helper_aside_copies(&case).is_empty(), "{:?}", helper_aside_copies(&case));
}

#[test]
fn exe_without_helper_exe_is_refused() {
    let case = Case::new("exe_without_helper_exe_is_refused");
    let key = EnvironmentKey::new("exe_without_helper_exe_is_refused");
    let exe = shown(&copy_gate(&case, "gate-a.exe"));
    let helper = shown(&default_helper(&case));

    for args in [["-Exe", exe.as_str()], ["-HelperExe", helper.as_str()]] {
        let outcome = finish(bare_install_command(&case, &key, &args));

        assert_eq!(outcome.code, Some(1), "{args:?}: {outcome:?}");
        assert_eq!(outcome.stdout, "", "{args:?}: {outcome:?}");
        assert_eq!(
            unix(&outcome.stderr),
            "install: -Exe and -HelperExe are given together or not at all\n",
            "{args:?}: {outcome:?}"
        );
    }
    assert!(!install_dir(&case).exists(), "nothing was installed");
}

#[test]
fn a_missing_helper_exe_is_refused() {
    let case = Case::new("a_missing_helper_exe_is_refused");
    let key = EnvironmentKey::new("a_missing_helper_exe_is_refused");
    let exe = copy_gate(&case, "gate-a.exe");
    let missing = case.folder().join("no-helper.exe");

    let outcome = install_with_helper(&case, &key, &missing, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("install: no desktop helper at '{}'\n", shown(&missing)),
        "{outcome:?}"
    );
    assert!(!install_dir(&case).exists(), "nothing was installed: {outcome:?}");
}

#[test]
fn an_unreadable_helper_is_refused() {
    let case = Case::new("an_unreadable_helper_is_refused");
    let key = EnvironmentKey::new("an_unreadable_helper_is_refused");
    let exe = copy_gate(&case, "gate-a.exe");
    let helper = stand_in_helper(&case, "helper-a.exe", b"helper one");
    let held = hold_against_reads(&helper);

    let outcome = install_with_helper(&case, &key, &helper, &["-Exe", &shown(&exe)]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with(&format!("install: could not read the desktop helper at '{}': ", shown(&helper))),
        "{outcome:?}"
    );
    assert!(!install_dir(&case).exists(), "nothing was installed: {outcome:?}");
}

#[test]
fn an_installed_helper_that_cannot_be_read_changes_nothing() {
    let case = Case::new("an_installed_helper_that_cannot_be_read_changes_nothing");
    let key = EnvironmentKey::new("an_installed_helper_that_cannot_be_read_changes_nothing");
    let (first, old_helper) = installed_pair_a(&case, &key);
    let second = copy_gate_with_a_byte(&case, "gate-b.exe");
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");
    let held = hold_against_reads(&installed_helper(&case));

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&second)]);

    drop(held);
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with(&format!(
            "install: could not read the desktop helper at '{}': ",
            shown(&installed_helper(&case))
        )),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &first);
    assert_same_bytes(&installed_helper(&case), &old_helper);
    assert!(aside_copies(&case).is_empty(), "{:?}", aside_copies(&case));
}

#[test]
fn a_new_helper_beside_the_same_gate_replaces_only_the_helper() {
    let case = Case::new("a_new_helper_beside_the_same_gate_replaces_only_the_helper");
    let key = EnvironmentKey::new("a_new_helper_beside_the_same_gate_replaces_only_the_helper");
    let (first, _) = installed_pair_a(&case, &key);
    let new_helper = stand_in_helper(&case, "helper-b.exe", b"helper two");

    let outcome = install_with_helper(&case, &key, &new_helper, &["-Exe", &shown(&first)]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stdout),
        format!(
            "install: gate {} already installed at {}; installed its desktop helper at {}\n",
            version_of(&first),
            shown(&installed_gate(&case)),
            shown(&installed_helper(&case))
        ),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &first);
    assert_same_bytes(&installed_helper(&case), &new_helper);
    assert!(aside_copies(&case).is_empty(), "gate.exe was not moved aside: {:?}", aside_copies(&case));
}

/// Makes the case folder the scratch repository a `-Source` case records, holding one commit of `file` with `text`.
fn commit_a_file(case: &Case, file: &str, text: &str) {
    git_init(case);
    commit_changed_file(case, file, text);
}

/// Writes `file` with `text` and commits it in the case folder's scratch repository, so the tree is clean again.
fn commit_changed_file(case: &Case, file: &str, text: &str) {
    fs::write(case.folder().join(file), text).expect("the tracked file can be written");
    git(case, &["add", "--", file]);
    git(case, &["commit", "--quiet", "-m", "a commit of the source tree"]);
}

/// The `source.json` a case's install folder holds, parsed, and the text it was written as.
fn source_json(case: &Case) -> (serde_json::Value, String) {
    let text = fs::read_to_string(install_dir(case).join("source.json")).expect("the source record can be read");
    let record = serde_json::from_str(&text).expect("the source record is one JSON object");
    (record, text)
}

#[test]
fn an_exe_install_with_source_records_the_commit_and_build_id() {
    let case = Case::new("an_exe_install_with_source_records_the_commit_and_build_id");
    let key = EnvironmentKey::new("an_exe_install_with_source_records_the_commit_and_build_id");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let head = git(&case, &["rev-parse", "HEAD"]);
    let short = git(&case, &["rev-parse", "--short=12", "HEAD"]);
    let exe = copy_gate(&case, "gate-a.exe");
    let source = shown(&case.folder());

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let (record, text) = source_json(&case);
    assert_eq!(record["repoRoot"].as_str(), Some(source.as_str()), "{text}");
    assert_eq!(record["commit"].as_str(), Some(head.as_str()), "{text}");
    assert_eq!(record["buildId"].as_str(), Some(short.as_str()), "{text}");
    assert_eq!(record["dirty"].as_bool(), Some(false), "{text}");
    let keys: Vec<&str> = record
        .as_object()
        .expect("the record is one JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["repoRoot", "commit", "buildId", "dirty"], "the four keys, in this order: {text}");
    assert!(text.starts_with(r#"{"repoRoot":"#), "the record opens with the repo folder: {text}");
    assert!(text.ends_with("}\n"), "the record ends with one LF after its object: {text}");
    assert!(!text.starts_with('\u{feff}'), "the record is written without a byte order mark: {text}");
    assert!(!text.contains('\r'), "the record is one line, ended with LF and no CR: {text}");
}

#[test]
fn a_dirty_source_tree_is_recorded_as_dirty() {
    let case = Case::new("a_dirty_source_tree_is_recorded_as_dirty");
    let key = EnvironmentKey::new("a_dirty_source_tree_is_recorded_as_dirty");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let short = git(&case, &["rev-parse", "--short=12", "HEAD"]);
    // A tracked file changed after the commit: the build is of no commit alone, and its id and the record say so.
    fs::write(case.folder().join("notes.txt"), "changed after the commit\n").expect("the tracked file can be changed");
    let exe = copy_gate(&case, "gate-a.exe");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let (record, text) = source_json(&case);
    let dirty = format!("{short}-dirty.");
    let id = record["buildId"].as_str().expect("the record names a build id");
    assert!(id.starts_with(&dirty), "a changed tree names a build of its own: {text}");
    assert_eq!(
        id.len(),
        dirty.len() + 8,
        "the build id, `-dirty.` and 8 hex of the changed bytes' hash: {text}"
    );
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'),
        "the id is lowercase hex, a `-` and a `.`: {text}"
    );
    assert_eq!(record["dirty"].as_bool(), Some(true), "the record says the tree was changed: {text}");
}

#[test]
fn an_exe_install_without_a_source_removes_the_record() {
    let case = Case::new("an_exe_install_without_a_source_removes_the_record");
    let key = EnvironmentKey::new("an_exe_install_without_a_source_removes_the_record");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    assert!(record.exists(), "the source is recorded: {first:?}");

    let second = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(second.code, Some(0), "{second:?}");
    assert_eq!(second.stderr, "", "{second:?}");
    assert!(!record.exists(), "a prebuilt exe names no source, so no record is left: {second:?}");
}

#[test]
fn install_whatif_leaves_the_source_record_as_it_was() {
    let case = Case::new("install_whatif_leaves_the_source_record_as_it_was");
    let key = EnvironmentKey::new("install_whatif_leaves_the_source_record_as_it_was");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    let before = fs::read(&record).expect("the record can be read");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(fs::read(&record).expect("the record can still be read"), before, "nothing was rewritten");
    assert!(
        unix(&outcome.stdout).contains(&format!(
            "What if: Performing the operation \"Delete\" on target \"{}\".\n",
            shown(&record)
        )),
        "the delete a real run would take is listed: {outcome:?}"
    );
}

#[test]
fn source_without_exe_is_refused() {
    let case = Case::new("source_without_exe_is_refused");
    let key = EnvironmentKey::new("source_without_exe_is_refused");
    commit_a_file(&case, "notes.txt", "the tracked file\n");

    let outcome = finish(bare_install_command(&case, &key, &["-Source", &shown(&case.folder())]));

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(unix(&outcome.stderr), "install: -Source is given only with -Exe\n", "{outcome:?}");
    assert!(!install_dir(&case).exists(), "nothing was changed: {outcome:?}");
}

/// A folder where `source.json` goes, holding a file, so neither a write of the record nor a delete of it can take it:
/// the record's name holds a folder its rename cannot replace and its file delete cannot remove.
fn a_folder_at_the_record(case: &Case) -> PathBuf {
    let record = install_dir(case).join("source.json");
    fs::create_dir_all(&record).expect("the folder at the record's name can be made");
    fs::write(record.join("a file"), "left here\n").expect("the file in it can be written");
    record
}

#[test]
fn source_that_is_not_a_git_work_tree_is_refused() {
    let case = Case::new("source_that_is_not_a_git_work_tree_is_refused");
    let key = EnvironmentKey::new("source_that_is_not_a_git_work_tree_is_refused");
    let exe = copy_gate(&case, "gate-a.exe");
    let missing = case.folder().join("no-such-folder");
    // A folder naming a git folder of its own that is no repository: git refuses to read a commit there, so the install
    // would have no commit and no build id to record.
    let broken = case.folder().join("not-a-tree");
    fs::create_dir_all(&broken).expect("the folder can be made");
    fs::write(broken.join(".git"), "gitdir: nowhere\n").expect("the git marker can be written");
    // A plain folder that is in a work tree without being its root: its commit is the tree's, not its own.
    let inner = case.folder().join("inner");
    fs::create_dir_all(&inner).expect("the inner folder can be made");

    for source in [&missing, &broken, &inner] {
        let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(source)]);

        assert_eq!(outcome.code, Some(1), "{outcome:?}");
        assert_eq!(outcome.stdout, "", "{outcome:?}");
        assert_eq!(
            unix(&outcome.stderr),
            format!("install: -Source {} is not a git work tree\n", shown(source)),
            "{outcome:?}"
        );
        assert!(!install_dir(&case).exists(), "nothing was changed: {outcome:?}");
    }
}

/// The install script with `args` and the desktop helper at `helper`, run as a pwsh command under a shell that turns a
/// native command's non-zero exit into a terminating error, as a caller's profile may: `git diff --quiet` exits 1 to say
/// the tree differs, and the installer must read that status rather than be stopped by it.
fn install_command_under_native_errors(case: &Case, key: &EnvironmentKey, helper: &Path, args: &[&str]) -> Command {
    let mut line = format!(
        "$PSNativeCommandUseErrorActionPreference = $true; & '{}' -EnvironmentKey '{}' -HelperExe '{}'",
        shown(&script()),
        key.0,
        shown(helper)
    );
    for arg in args {
        // A parameter's name is a bare word, never a quoted one: only the value after it is quoted, so a path stays one
        // word and the name still binds.
        line.push(' ');
        if arg.starts_with('-') {
            line.push_str(arg);
        } else {
            line.push('\'');
            line.push_str(arg);
            line.push('\'');
        }
    }
    let mut command = Command::new("pwsh");
    case.prepare_without_state_dir(&mut command, &["-NoProfile", "-Command", &line]);
    command
}

#[test]
fn a_dirty_tree_read_under_native_command_errors_still_records_its_source() {
    let case = Case::new("a_dirty_tree_read_under_native_command_errors_still_records_its_source");
    let key = EnvironmentKey::new("a_dirty_tree_read_under_native_command_errors_still_records_its_source");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let head = git(&case, &["rev-parse", "HEAD"]);
    fs::write(case.folder().join("notes.txt"), "changed after the commit\n").expect("the tracked file can be changed");
    let exe = copy_gate(&case, "gate-a.exe");

    let outcome = finish(install_command_under_native_errors(
        &case,
        &key,
        &default_helper(&case),
        &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())],
    ));

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    let (record, text) = source_json(&case);
    assert_eq!(record["commit"].as_str(), Some(head.as_str()), "{text}");
    assert_eq!(record["dirty"].as_bool(), Some(true), "a changed tree is still recorded as dirty: {text}");
}

#[test]
fn source_through_a_junction_and_a_trailing_separator_is_accepted() {
    let case = Case::new("source_through_a_junction_and_a_trailing_separator_is_accepted");
    let key = EnvironmentKey::new("source_through_a_junction_and_a_trailing_separator_is_accepted");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let head = git(&case, &["rev-parse", "HEAD"]);
    let exe = copy_gate(&case, "gate-a.exe");
    let link = case.folder().join("link");
    junction(&link, &case.folder());
    let trailing = format!("{}\\", shown(&case.folder()));

    for source in [shown(&link), trailing] {
        let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);

        assert_eq!(outcome.code, Some(0), "{source}: {outcome:?}");
        assert_eq!(outcome.stderr, "", "{source}: {outcome:?}");
        let (record, text) = source_json(&case);
        let written = record["repoRoot"].as_str().expect("the record names its folder");
        assert_eq!(written.trim_end_matches('\\'), source.trim_end_matches('\\'), "{source}: {text}");
        assert_eq!(record["commit"].as_str(), Some(head.as_str()), "{source}: {text}");
    }
}

#[test]
fn source_in_a_repository_with_no_commits_is_refused() {
    let case = Case::new("source_in_a_repository_with_no_commits_is_refused");
    let key = EnvironmentKey::new("source_in_a_repository_with_no_commits_is_refused");
    let exe = copy_gate(&case, "gate-a.exe");
    let empty = case.folder().join("empty");
    fs::create_dir_all(&empty).expect("the folder can be made");
    git(&case, &["-C", &shown(&empty), "init", "--quiet"]);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&empty)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("install: -Source {} has no commits yet\n", shown(&empty)),
        "{outcome:?}"
    );
    assert!(!install_dir(&case).exists(), "nothing was changed: {outcome:?}");
}

#[test]
fn install_whatif_of_a_gate_that_does_not_answer_lists_the_record_delete() {
    let case = Case::new("install_whatif_of_a_gate_that_does_not_answer_lists_the_record_delete");
    let key = EnvironmentKey::new("install_whatif_of_a_gate_that_does_not_answer_lists_the_record_delete");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let source = shown(&case.folder());
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    let before = fs::read(&record).expect("the record can be read");
    // The installed gate cannot start, so the record a real run would leave is no record at all.
    let denied = DenyEntry::new(&install_dir(&case), NO_RUNNING_IN);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source, "-WhatIf"]);

    drop(denied);
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert!(
        unix(&outcome.stdout).contains(&format!(
            "What if: Performing the operation \"Delete\" on target \"{}\".\n",
            shown(&record)
        )),
        "the delete a real run would take is listed, not a write: {outcome:?}"
    );
    assert_eq!(
        fs::read(&record).expect("the record can still be read"),
        before,
        "nothing was removed: {outcome:?}"
    );
}

#[test]
fn install_whatif_of_an_exe_that_does_not_answer_lists_the_record_write() {
    let case = Case::new("install_whatif_of_an_exe_that_does_not_answer_lists_the_record_write");
    let key = EnvironmentKey::new("install_whatif_of_an_exe_that_does_not_answer_lists_the_record_write");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let source = shown(&case.folder());
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    let before = fs::read(&record).expect("the record can be read");
    // An -Exe whose version cannot be read stops a real run before the record step, so the only record action a
    // preview can name is the one a real run that gets there takes: a write.
    let not_a_gate = stand_in_helper(&case, "not-a-gate.exe", b"not a gate");

    let outcome = install(&case, &key, &["-Exe", &shown(&not_a_gate), "-Source", &source, "-WhatIf"]);

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let stdout = unix(&outcome.stdout);
    assert!(
        stdout.contains(&format!(
            "What if: Performing the operation \"Write\" on target \"{}\".\n",
            shown(&record)
        )),
        "the write a real run would take is listed: {outcome:?}"
    );
    assert!(
        !stdout.contains(&format!(
            "What if: Performing the operation \"Delete\" on target \"{}\".\n",
            shown(&record)
        )),
        "no delete of the record is listed: {outcome:?}"
    );
    assert_eq!(
        fs::read(&record).expect("the record can still be read"),
        before,
        "nothing was rewritten: {outcome:?}"
    );
}

#[test]
fn a_refused_source_leaves_the_record_as_it_was() {
    let case = Case::new("a_refused_source_leaves_the_record_as_it_was");
    let key = EnvironmentKey::new("a_refused_source_leaves_the_record_as_it_was");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    let before = fs::read(&record).expect("the record can be read");
    let inner = case.folder().join("inner");
    fs::create_dir_all(&inner).expect("the inner folder can be made");

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&inner)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(
        unix(&outcome.stderr),
        format!("install: -Source {} is not a git work tree\n", shown(&inner)),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read(&record).expect("the record can still be read"),
        before,
        "the refused install removed nothing: {outcome:?}"
    );
}

#[test]
fn an_install_whose_gate_does_not_answer_removes_the_record() {
    let case = Case::new("an_install_whose_gate_does_not_answer_removes_the_record");
    let key = EnvironmentKey::new("an_install_whose_gate_does_not_answer_removes_the_record");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let source = shown(&case.folder());
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);
    assert_eq!(first.code, Some(0), "{first:?}");
    let record = install_dir(&case).join("source.json");
    assert!(record.exists(), "the source is recorded: {first:?}");
    // Nothing put in the install folder can start, though it can still be read, so the installed gate answers nothing
    // while its bytes are the same ones it was installed with.
    let denied = DenyEntry::new(&install_dir(&case), NO_RUNNING_IN);

    let second = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);

    drop(denied);
    assert_eq!(second.code, Some(0), "the install keeps its own outcome: {second:?}");
    assert!(!record.exists(), "a gate that does not answer keeps no record: {second:?}");
    assert_eq!(
        unix(&second.stderr),
        format!("install: the installed gate does not answer --version; removed {}\n", shown(&record)),
        "{second:?}"
    );
}

#[test]
fn a_source_record_that_cannot_be_written_fails_the_install() {
    let case = Case::new("a_source_record_that_cannot_be_written_fails_the_install");
    let key = EnvironmentKey::new("a_source_record_that_cannot_be_written_fails_the_install");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let exe = copy_gate(&case, "gate-a.exe");
    let record = a_folder_at_the_record(&case);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &shown(&case.folder())]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(unix(&outcome.stderr).lines().count(), 1, "the failure is one line: {outcome:?}");
    assert!(
        unix(&outcome.stderr).starts_with(&format!("install: could not write {}: ", shown(&record))),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert_eq!(
        path_value(&key),
        Some(("ExpandString".to_owned(), shown(&install_dir(&case)))),
        "the gate is installed and on the PATH before the record is tried: {outcome:?}"
    );
}

#[test]
fn a_source_record_that_cannot_be_removed_fails_the_install() {
    let case = Case::new("a_source_record_that_cannot_be_removed_fails_the_install");
    let key = EnvironmentKey::new("a_source_record_that_cannot_be_removed_fails_the_install");
    let exe = copy_gate(&case, "gate-a.exe");
    let record = a_folder_at_the_record(&case);

    let outcome = install(&case, &key, &["-Exe", &shown(&exe)]);

    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        unix(&outcome.stdout).contains("install: installed gate "),
        "the gate was installed first: {outcome:?}"
    );
    assert!(
        unix(&outcome.stderr).starts_with(&format!("install: could not remove {}: ", shown(&record))),
        "{outcome:?}"
    );
    assert_same_bytes(&installed_gate(&case), &exe);
    assert_eq!(
        path_value(&key),
        Some(("ExpandString".to_owned(), shown(&install_dir(&case)))),
        "the gate is on the PATH before the record is tried: {outcome:?}"
    );
}

#[test]
fn a_second_install_with_a_source_records_the_new_commit() {
    let case = Case::new("a_second_install_with_a_source_records_the_new_commit");
    let key = EnvironmentKey::new("a_second_install_with_a_source_records_the_new_commit");
    commit_a_file(&case, "notes.txt", "the tracked file\n");
    let first_commit = git(&case, &["rev-parse", "HEAD"]);
    let exe = copy_gate(&case, "gate-a.exe");
    let source = shown(&case.folder());
    let first = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);
    assert_eq!(first.code, Some(0), "{first:?}");
    assert_eq!(
        source_json(&case).0["commit"].as_str(),
        Some(first_commit.as_str()),
        "the first install records the first commit"
    );

    commit_changed_file(&case, "notes.txt", "the tracked file, committed again\n");
    let second_commit = git(&case, &["rev-parse", "HEAD"]);
    let again = install(&case, &key, &["-Exe", &shown(&exe), "-Source", &source]);

    assert_eq!(again.code, Some(0), "{again:?}");
    assert_eq!(again.stderr, "", "{again:?}");
    assert_ne!(first_commit, second_commit, "the scratch repository has a second commit");
    let (record, text) = source_json(&case);
    assert_eq!(
        record["commit"].as_str(),
        Some(second_commit.as_str()),
        "the same files installed again record the commit they would now be built from: {text}"
    );
}

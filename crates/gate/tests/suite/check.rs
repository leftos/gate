//! `gate check`: what this machine lacks for the repo it is run in. Each case's repo is the `app` folder of its case
//! folder, a git worktree of its own; its tools are `.cmd` stubs in the case folder's `stub` folder, outside the repo,
//! on a `PATH` of that folder and git's alone, so no real tool of the machine answers.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use gate_router::warm::warmup_hash;
use gate_router::work_folder_key;
use gate_state::v1::{self, WarmRecord, WarmState};
use jiff::Timestamp;

use crate::remote_client::{fake_unreal_install, open_to_another_account};
use crate::support::{
    ANSWERING, Case, FAKE_SSH, FAKE_TOOL, GATE, GATE_VARIABLE, Go, NetfxKey, Outcome, PREFLIGHT_SEAMS_VARIABLE, STEPS_VARIABLE, build_version, calls,
    finish, git, ssh_steps, wait_until_closed, write_remote_json,
};

/// The repo's folder in the case folder.
const REPO: &str = "app";

/// A case whose folder holds the repo `app`, a git worktree with no commit.
fn repo_case(name: &str) -> Case {
    let case = Case::new(name);
    fs::create_dir_all(case.folder().join(REPO)).expect("the repo folder can be made");
    git(&case, &["-C", REPO, "init", "--quiet"]);
    case
}

/// The repo's worktree root, as git spells it.
fn root(case: &Case) -> String {
    git(case, &["-C", REPO, "rev-parse", "--show-toplevel"])
}

/// Writes `text` to the repo's file `relative`, its folders made.
fn write(case: &Case, relative: &str, text: &str) {
    let path = case.folder().join(REPO).join(relative);
    fs::create_dir_all(path.parent().expect("a repo file has a folder")).expect("the file's folder can be made");
    fs::write(&path, text).expect("the repo file can be written");
}

/// Writes the `bytes` to the repo's file `relative`, its folders made.
fn write_bytes(case: &Case, relative: &str, bytes: &[u8]) {
    let path = case.folder().join(REPO).join(relative);
    fs::create_dir_all(path.parent().expect("a repo file has a folder")).expect("the file's folder can be made");
    fs::write(&path, bytes).expect("the repo file can be written");
}

/// Writes the case's tool stubs, each `<name>.cmd` with its body, to the case folder's `stub` folder; its path.
fn stubs(case: &Case, tools: &[(&str, &str)]) -> PathBuf {
    let stub = case.folder().join("stub");
    fs::create_dir_all(&stub).expect("the stub folder can be made");
    for (name, body) in tools {
        fs::write(stub.join(format!("{name}.cmd")), body).expect("a tool stub can be written");
    }
    stub
}

/// The folder on the suite's PATH that holds git.exe.
fn git_folder() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .find(|folder| folder.join("git.exe").is_file())
        .expect("git.exe is on PATH; the check cases need it")
}

/// A PATH of `folders`, then git's folder.
fn path_with_git(folders: &[PathBuf]) -> OsString {
    std::env::join_paths(folders.iter().cloned().chain([git_folder()])).expect("the folders join into a PATH")
}

/// `gate check` with `extra` in the repo, its `PATH` the `folders` then git's folder and no `GATE_GODOT_ROOT`, so the
/// godot probe reads that `PATH` alone, nor `GATE_UNREAL_ROOT`, so this machine's own engines are never read.
fn check(case: &Case, folders: &[PathBuf], extra: &[&str]) -> Outcome {
    let args: Vec<&str> = ["check"].iter().chain(extra).copied().collect();
    let mut command = case.gate_in(REPO, &args);
    command.env("PATH", path_with_git(folders));
    command.env_remove("GATE_GODOT_ROOT");
    command.env_remove("GATE_UNREAL_ROOT");
    finish(command)
}

/// A stand-in for this PC's .NET Framework SDK list that lists a 4.8 SDK in a folder of the case's, so a check of an
/// Unreal-pinned repo never reads this machine's own list.
fn netfx_present(case: &Case) -> NetfxKey {
    NetfxKey::new().with_sdk("4.8", &case.folder().join("netfx").join("4.8"))
}

/// The line a check of an Unreal-pinned repo says of the SDK [`netfx_present`] lists, `at` this PC or a builder.
fn netfx_ok(at: &str) -> String {
    format!("check: {at}: netfx-sdk 4.8 ok (needs 4.6.0 or later)")
}

/// `gate check` of a repo whose manifest is `manifest`, with `GATE_UNREAL_ROOT` the folder `root` holding one fake
/// engine per version of `versions`, each in the folder named for it, its `PATH` git's folder alone, and the 4.8 SDK
/// of [`netfx_present`].
fn check_unreal(case: &Case, manifest: &str, root: &Path, versions: &[&str]) -> Outcome {
    write(case, ".gate/toolchain.toml", manifest);
    fs::create_dir_all(root).expect("the unreal root can be made");
    for version in versions {
        fake_unreal_install(&root.join(version), version);
    }
    let key = netfx_present(case);
    let mut command = case.gate_in(REPO, &["check", "--test-netfx-key", key.path()]);
    command.env("PATH", path_with_git(&[]));
    command.env_remove("GATE_GODOT_ROOT");
    command.env("GATE_UNREAL_ROOT", root);
    finish(command)
}

/// A pinned engine the root lists is ok, judged exactly on the parts the pin gives.
#[test]
fn check_a_pinned_unreal_the_root_lists_is_ok() {
    let case = repo_case("check_unreal_ok");
    let root = case.folder().join("unreal");

    let outcome = check_unreal(&case, "[tools]\nunreal = \"5.8\"\n", &root, &["5.8.0"]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: unreal 5.8.0 ok (needs 5.8 exactly)",
            &netfx_ok("here"),
            "check: here: nothing missing"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A pinned engine the root holds only at another version is missing, though that version is later.
#[test]
fn check_a_pinned_unreal_at_another_version_is_missing() {
    let case = repo_case("check_unreal_other_version");
    let root = case.folder().join("unreal");

    let outcome = check_unreal(&case, "[tools]\nunreal = \"5.8\"\n", &root, &["5.9.0"]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: unreal missing (needs 5.8 exactly: 5.9.0)",
            &netfx_ok("here"),
            "check: here: 1 missing, 0 too old"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// With no `GATE_UNREAL_ROOT` on this PC the engine is not checked here, nor the .NET Framework SDK its builds need,
/// which counts as neither missing nor skipped.
#[test]
fn check_an_unreal_pin_without_a_root_says_not_checked_here() {
    let case = repo_case("check_unreal_no_root");
    write(&case, ".gate/toolchain.toml", "[tools]\nunreal = \"5.8\"\n");
    let key = netfx_present(&case);

    let outcome = check(&case, &[], &["--test-netfx-key", key.path()]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: unreal not checked here: GATE_UNREAL_ROOT is not set",
            "check: here: netfx-sdk not checked here: GATE_UNREAL_ROOT is not set, so the Unreal builds that need it are not checked here",
            "check: here: nothing missing"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// With no `GATE_UNREAL_ROOT` the .NET Framework SDK those Unreal builds need is not checked here either, its line
/// saying why, and counts as neither missing nor skipped.
#[test]
fn check_a_missing_netfx_sdk_beside_an_unchecked_unreal_is_not_checked() {
    let case = repo_case("check_unreal_no_root_netfx");
    write(&case, ".gate/toolchain.toml", "[tools]\nunreal = \"5.8\"\n");
    let key = NetfxKey::new();

    let outcome = check(&case, &[], &["--test-netfx-key", key.path()]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: unreal not checked here: GATE_UNREAL_ROOT is not set",
            "check: here: netfx-sdk not checked here: GATE_UNREAL_ROOT is not set, so the Unreal builds that need it are not checked here",
            "check: here: nothing missing"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A manifest pinning the .NET Framework SDK alone is checked for it, and a missing one is missing with the hint
/// naming the Developer Pack.
#[test]
fn check_a_missing_netfx_sdk_pinned_alone_is_missing_with_its_hint() {
    let case = repo_case("check_netfx_pinned_alone");
    write(&case, ".gate/toolchain.toml", "[tools]\nnetfx-sdk = \"4.6\"\n");
    let key = NetfxKey::new();

    let outcome = check(&case, &[], &["--test-netfx-key", key.path()]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: netfx-sdk missing (needs 4.6.0 or later)",
            "install: here: netfx-sdk: install the .NET Framework 4.8 Developer Pack (https://dotnet.microsoft.com/download/dotnet-framework/net48)",
            "check: here: 1 missing, 0 too old"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A root inside the repo is refused as a Godot root inside it is, though it holds an engine.
#[test]
fn check_an_unreal_root_inside_the_repo_is_refused() {
    let case = repo_case("check_unreal_root_inside");
    let root = case.folder().join(REPO).join("unreal");

    let outcome = check_unreal(&case, "[tools]\nunreal = \"\"\n", &root, &["5.8.0"]);

    let said = format!(
        "check: here: unreal skipped: GATE_UNREAL_ROOT {} overlaps {}, which a run writes",
        root.display(),
        // The repo root as the check names it, in git's spelling, as its `app at` line does.
        case.folder().join(REPO).display().to_string().replace('\\', "/")
    );
    assert_eq!(
        lines(&outcome.stdout)[1..],
        [said.as_str(), &netfx_ok("here"), "check: here: nothing missing (1 skipped)"],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// Every stub tool answering at a version past the minimums the cases declare.
const EVERY_TOOL: [(&str, &str); 6] = [
    ("rustc", "@echo rustc 1.99.0 (b940084d7 2026-09-28)\r\n"),
    (
        "cargo-nextest",
        "@echo off\r\nif \"%1\"==\"nextest\" echo cargo-nextest 0.9.148 (cd1d6d546 2026-10-07)\r\n",
    ),
    ("node", "@echo v24.19.0\r\n"),
    ("pnpm", "@echo 10.0.0\r\n"),
    ("pwsh", "@echo PowerShell 7.6.6\r\n"),
    ("ffmpeg", "@echo ffmpeg version 9.0.2-full_build-www.gyan.dev Copyright (c) 2000-2026\r\n"),
];

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

#[test]
fn check_with_every_tool_present_exits_0() {
    let case = repo_case("check_every_tool");
    write(
        &case,
        ".gate/toolchain.toml",
        "[tools]\nrust = \"1.89\"\ncargo-nextest = \"0.9.100\"\nnode = \"24\"\npnpm = \"\"\npwsh = \"7.4\"\nffmpeg = \"\"\n",
    );
    let stub = stubs(&case, &EVERY_TOOL);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: rust 1.99.0 ok (needs 1.89.0 or later)",
            "check: here: cargo-nextest 0.9.148 ok (needs 0.9.100 or later)",
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)",
            "check: here: pnpm 10.0.0 ok",
            "check: here: pwsh 7.6.6 ok (needs 7.4.0 or later)",
            "check: here: ffmpeg 9.0.2 ok",
            "check: here: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_with_a_missing_tool_exits_1_and_says_how_to_install_it() {
    let case = repo_case("check_missing_tool");
    write(
        &case,
        ".gate/toolchain.toml",
        "[tools]\nnode = \"24\"\ncargo-nextest = \"\"\ngodot = \"4.5\"\n",
    );
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)",
            "check: here: cargo-nextest missing (needs it present)",
            "check: here: godot missing (needs 4.5 exactly: none)",
            "install: here: cargo-nextest: cargo install cargo-nextest --locked",
            "check: here: 2 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A root inside the repo is refused as a program inside the worktree is, whatever the manifest declares of godot,
/// and the install behind it never runs.
#[test]
fn check_a_godot_root_inside_the_repo_is_refused() {
    let case = repo_case("check_godot_root_inside");
    write(&case, ".gate/toolchain.toml", "[tools]\ngodot = \"\"\n");
    let root = case.folder().join(REPO).join("godot");
    let install = root.join("4.7.2");
    fs::create_dir_all(&install).expect("the install folder can be made");
    fs::copy(FAKE_TOOL, install.join("Godot_console.exe")).expect("the fake godot can be copied");
    fs::write(install.join("version.txt"), "4.7.2.stable.mono.official.x\n").expect("the fake's answer can be written");
    let mut command = case.gate_in(REPO, &["check"]);
    command.env("PATH", path_with_git(&[]));
    command.env("GATE_GODOT_ROOT", &root);

    let outcome = finish(command);

    let program = install.join("Godot_console.exe");
    let said = format!(
        "check: here: godot skipped: cannot run {}: {} lies inside the worktree",
        program.display(),
        program.display()
    );
    assert_eq!(
        lines(&outcome.stdout)[1..],
        [said.as_str(), "check: here: nothing missing (1 skipped)"],
        "{outcome:?}"
    );
    assert!(!install.join("ran").exists(), "the refused install ran");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A pinned godot whose probe cannot run the install is a lack, as a routed run judges it (it falls back here for the
/// same answer), never `nothing missing`: the refusal here stands in for any reason the box cannot run the install.
#[test]
fn check_a_pinned_godot_the_probe_cannot_run_is_missing() {
    let case = repo_case("check_pinned_godot_cannot_run");
    write(&case, ".gate/toolchain.toml", "[tools]\ngodot = \"4.7.2\"\n");
    let root = case.folder().join(REPO).join("godot");
    let install = root.join("4.7.2");
    fs::create_dir_all(&install).expect("the install folder can be made");
    fs::copy(FAKE_TOOL, install.join("Godot_console.exe")).expect("the fake godot can be copied");
    fs::write(install.join("version.txt"), "4.7.2.stable.mono.official.x\n").expect("the fake's answer can be written");
    let mut command = case.gate_in(REPO, &["check"]);
    command.env("PATH", path_with_git(&[]));
    command.env("GATE_GODOT_ROOT", &root);

    let outcome = finish(command);

    let said = lines(&outcome.stdout);
    assert!(
        said.iter()
            .any(|line| line.starts_with("check: here: godot") && line.contains("lies inside the worktree") && line.contains("needs 4.7.2 exactly")),
        "the godot line says why it was not probed and the exact pin: {outcome:?}"
    );
    assert_eq!(said.last().copied(), Some("check: here: 1 missing, 0 too old"), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

#[test]
fn check_below_the_minimum_says_too_old() {
    let case = repo_case("check_too_old");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"25\"\n");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: node 24.19.0 too old (needs 25.0.0 or later)",
            "install: here: node: winget install OpenJS.NodeJS",
            "check: here: 0 missing, 1 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

#[test]
fn check_reads_an_uncommitted_manifest() {
    let case = repo_case("check_uncommitted_manifest");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"\"\n");
    git(&case, &["-C", REPO, "add", "."]);
    git(&case, &["-C", REPO, "commit", "--quiet", "-m", "manifest"]);
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"99\"\n");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1],
        "check: here: node 24.19.0 too old (needs 99.0.0 or later)",
        "the manifest on disk is read, not the commit's: {outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A manifest that does not parse leaves its tools unchecked: the check could not run for them, so it exits 1 and its
/// summary says so, and the rest of the repo is still checked.
#[test]
fn check_an_unreadable_manifest_exits_1_and_says_so() {
    let case = repo_case("check_unreadable_manifest");
    write(&case, ".gate/toolchain.toml", "[tools\nnode = \"24\"\n");
    write(&case, ".gate/siblings.toml", "siblings = ['docs']\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    let root = root(&case);
    let parent = root.rsplit_once('/').map_or("", |(parent, _)| parent);
    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            format!("check: here: sibling docs missing (no git worktree at {parent}/docs)"),
            format!("install: here: sibling docs: clone it to {parent}/docs"),
            "check: here: could not read .gate/toolchain.toml; 1 missing, 0 too old".to_owned(),
        ],
        "{outcome:?}"
    );
    assert!(outcome.stderr.starts_with("check: .gate/toolchain.toml: line "), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A declarations folder in the case folder holding each `(name, text)` of `files`, named for `app` in a routing file
/// that lists it; the folder.
fn declarations(case: &Case, files: &[(&str, &str)]) -> PathBuf {
    let folder = case.folder().join("declarations");
    fs::create_dir_all(&folder).expect("the declarations folder can be made");
    for (name, text) in files {
        fs::write(folder.join(name), text).expect("a declaration file can be written");
    }
    name_declarations(case, &folder);
    folder
}

/// Writes a routing file that lists `app` and names `folder` as its declarations folder.
fn name_declarations(case: &Case, folder: &Path) {
    let routing = serde_json::json!({ "host": "builder-1", "repos": { "app": ["*"] }, "declarations": { "app": folder.to_string_lossy() } });
    write_remote_json(case, &routing.to_string());
}

/// A repo with no manifest of its own is checked against its declarations folder's, and one line says where it was read.
#[test]
fn check_says_a_file_read_from_the_declarations_folder() {
    let case = repo_case("check_declared_manifest");
    let folder = declarations(&case, &[("toolchain.toml", "[tools]\nnode = \"24\"\n")]);
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)),
            format!("check: here: .gate/toolchain.toml: read from {}", folder.join("toolchain.toml").display()),
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)".to_owned(),
            "check: here: nothing missing".to_owned(),
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A declarations folder that is not there is a problem of its own, exiting 1, and the repo's own files still apply.
#[test]
fn check_an_unusable_declarations_entry_exits_1_and_says_so() {
    let case = repo_case("check_declarations_unusable");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let missing = case.folder().join("no-such-folder");
    name_declarations(&case, &missing);
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)".to_owned(),
            format!("check: here: declarations for app: '{}' does not exist", missing.display()),
            "check: here: 1 missing, 0 too old".to_owned(),
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A declarations folder's manifest that does not parse is named by its full path, in its problem line and the summary.
#[test]
fn check_a_problem_in_a_declarations_file_names_its_path() {
    let case = repo_case("check_declared_manifest_unreadable");
    let folder = declarations(&case, &[("toolchain.toml", "[tools\nnode = \"24\"\n")]);
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    let path = folder.join("toolchain.toml").display().to_string();
    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            format!("check: here: .gate/toolchain.toml: read from {path}"),
            format!("check: here: could not read {path}; 0 missing, 0 too old"),
        ],
        "{outcome:?}"
    );
    assert!(outcome.stderr.starts_with(&format!("check: {path}: line ")), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// The worktree's own manifest wins over the declarations folder's, and the check says which copy it shadows.
#[test]
fn check_the_worktree_s_file_wins_and_says_the_copy_it_shadows() {
    let case = repo_case("check_declared_manifest_shadowed");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let folder = declarations(&case, &[("toolchain.toml", "[tools]\nnode = \"99\"\n")]);
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)),
            format!(
                "check: here: .gate/toolchain.toml: the worktree's copy wins over {}",
                folder.join("toolchain.toml").display()
            ),
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)".to_owned(),
            "check: here: nothing missing".to_owned(),
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A declarations entry that cannot be used is said, once, and fails the check even when the repo has every file of its
/// own, so no read reaches the folder.
#[test]
fn check_an_unusable_declarations_entry_is_said_when_the_repo_has_every_file() {
    let case = repo_case("check_declarations_unusable_unread");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    write(&case, ".gate/siblings.toml", "siblings = []\n");
    write(&case, ".gate/warmup.toml", "commands = [[\"tool.exe\"]]\n");
    let missing = case.folder().join("no-such-folder");
    name_declarations(&case, &missing);
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    let line = format!("check: here: declarations for app: '{}' does not exist", missing.display());
    assert_eq!(
        lines(&outcome.stdout).iter().filter(|said| **said == line).count(),
        1,
        "{line}: {outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A routing file that cannot be used is said on stderr as its problem, naming none of the repo's declarations, and the
/// check's exit code stays the one its items give.
#[test]
fn check_an_unusable_routing_file_is_said_on_stderr_and_keeps_the_exit_code() {
    let case = repo_case("check_routing_unusable");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let path = write_remote_json(&case, "{}");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);

    let outcome = check(&case, &[stub], &[]);

    let line = format!("check: remote.json: ignored {}: host is missing", path.display());
    assert!(outcome.stderr.lines().any(|said| said == line), "{line}: {outcome:?}");
    assert_eq!(
        lines(&outcome.stdout).last().copied(),
        Some("check: here: nothing missing"),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A tool of the repo's own, in a folder inside the worktree first on `PATH`, is never the one probed: the probes look
/// along `PATH` with the worktree's folders dropped, as the SDK list does.
#[test]
fn check_a_tool_inside_the_worktree_is_not_probed() {
    let case = repo_case("check_tool_inside_worktree");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\npnpm = \"\"\n");
    let marker = case.folder().join("repo-node-ran");
    write(
        &case,
        "tools/node.cmd",
        &format!("@echo off\r\n>\"{}\" echo ran\r\necho v99.0.0\r\n", marker.display()),
    );
    write(&case, "tools/pnpm.cmd", "@echo 10.0.0\r\n");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);
    let repo_tools = case.folder().join(REPO).join("tools");

    let outcome = check(&case, &[repo_tools, stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: node 24.19.0 ok (needs 24.0.0 or later)",
            "check: here: pnpm missing (needs it present)",
            "install: here: pnpm: corepack enable pnpm",
            "check: here: 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert!(!marker.exists(), "the repo's node ran: {outcome:?}");
}

#[test]
fn check_outside_a_repo_exits_1() {
    let case = Case::new("check_outside_a_repo");
    let outside = case.folder().join("outside");
    fs::create_dir_all(&outside).expect("the folder can be made");
    let mut command = case.gate_without_state_dir(&["check"]);
    // The case folder lies under the gate's own worktree; git looks no higher than it.
    command.current_dir(&outside).env("GIT_CEILING_DIRECTORIES", case.folder());

    let outcome = finish(command);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "check: not in a git repository\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

#[test]
fn check_without_a_manifest_says_it_declares_nothing_and_exits_0() {
    let case = repo_case("check_no_manifest");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: app declares no toolchain",
            "check: here: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A tool that never answers is ended at the bound `--test-probe-limit-ms` sets and said skipped; only the bound ends
/// the probe, so no load changes what the check says, and nothing here asserts how long it took.
#[test]
fn check_a_hanging_probe_is_skipped_and_exits_0() {
    let case = repo_case("check_hanging_probe");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let go = Go(case.folder().join("node-go"));
    let held = case.folder().join("node-held");
    let hang = format!(
        "@echo off\r\n\"{FAKE_TOOL}\" wait \"{}\" >\"{}\"\r\n",
        go.path().display(),
        held.display()
    );
    let stub = stubs(&case, &[("node", &hang)]);

    let outcome = check(&case, &[stub], &["--test-probe-limit-ms", "1000"]);

    // A stub that started holds its output file open until its job's kill ends it; one the kill came first never made it.
    if held.exists() {
        wait_until_closed(&held);
    }
    assert_eq!(
        lines(&outcome.stdout)[1..],
        ["check: here: node skipped: no answer in 1 s", "check: here: nothing missing (1 skipped)"],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_lists_a_missing_sibling() {
    let case = repo_case("check_missing_sibling");
    write(&case, ".gate/siblings.toml", "siblings = ['server', 'docs']\n");
    fs::create_dir_all(case.folder().join("server").join(".git")).expect("the present sibling's git can be made");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    let root = root(&case);
    let parent = root.rsplit_once('/').map_or("", |(parent, _)| parent);
    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain".to_owned(),
            format!("check: here: sibling server ok ({parent}/server)"),
            format!("check: here: sibling docs missing (no git worktree at {parent}/docs)"),
            format!("install: here: sibling docs: clone it to {parent}/docs"),
            "check: here: 1 missing, 0 too old".to_owned(),
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A routing file whose `siblings` value cannot be used says why on a line of its own, counted missing, and allows no
/// sibling.
#[test]
fn check_a_bad_siblings_value_in_the_routing_file_says_why() {
    let case = repo_case("check_bad_siblings_value");
    write(&case, ".gate/siblings.toml", "siblings = ['b']\n");
    write_remote_json(&case, r#"{"host": "builder-1", "repos": {"app": ["*"]}, "siblings": {"app": ["b", 3]}}"#);
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            r#"check: here: remote.json: siblings."app"[1] is not a non-empty string"#,
            "check: here: sibling b not allowed in remote.json",
            "check: here: 2 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A sibling named like the worktree's own folder, letter case aside, is refused as routing refuses it, and its folder
/// is not looked for.
#[test]
fn check_refuses_a_sibling_named_like_the_worktree() {
    let case = repo_case("check_sibling_like_root");
    write(&case, ".gate/siblings.toml", "siblings = ['APP']\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: sibling APP is the worktree's own folder",
            "check: here: 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// The SDK list is read with the repo's own folders dropped from `PATH`: a `dotnet.cmd` of the repo that would list the
/// pinned SDK is never run.
#[test]
fn check_lists_the_sdk_pin_missing() {
    let case = repo_case("check_sdk_pin_missing");
    write(&case, "global.json", "{\"sdk\": {\"version\": \"10.0.401\"}}\n");
    write(&case, "tools/dotnet.cmd", "@echo 10.0.401 [C:\\repo]\r\n");
    let dotnet = "@echo off\r\necho 8.0.404 [C:\\stub]\r\necho 10.0.300 [C:\\stub]\r\n";
    let stub = stubs(&case, &[("dotnet", dotnet)]);
    let repo_tools = case.folder().join(REPO).join("tools");

    let outcome = check(&case, &[repo_tools, stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: .NET SDK 10.0.401 (rollForward patch) missing (this PC has 8.0.404, 10.0.300)",
            "install: here: .NET SDK: winget install Microsoft.DotNet.SDK.10",
            "check: here: 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A stub that writes `marker` when it runs.
fn marking(marker: &Path) -> String {
    format!("@echo off\r\n>\"{}\" echo ran %*\r\n", marker.display())
}

/// Every tool missing, with the programs the install hints name standing on `PATH`: the check says the hints and runs
/// none of them, and leaves the repo as it found it.
#[test]
fn check_installs_nothing() {
    let case = repo_case("check_installs_nothing");
    write(
        &case,
        ".gate/toolchain.toml",
        "[tools]\nrust = \"1.89\"\ncargo-nextest = \"\"\nnode = \"\"\npnpm = \"\"\npwsh = \"\"\nffmpeg = \"\"\n",
    );
    let marker = |name: &str| case.folder().join(format!("{name}-ran"));
    let installers = ["winget", "cargo", "corepack", "rustup"];
    let bodies: Vec<(&str, String)> = installers.iter().map(|name| (*name, marking(&marker(name)))).collect();
    let tools: Vec<(&str, &str)> = bodies.iter().map(|(name, body)| (*name, body.as_str())).collect();
    let stub = stubs(&case, &tools);
    let status_before = git(&case, &["-C", REPO, "status", "--porcelain", "--untracked-files=all"]);

    let outcome = check(&case, &[stub], &[]);

    assert!(
        outcome
            .stdout
            .lines()
            .any(|line| line == "install: here: rust: winget install Rustlang.Rustup, then rustup default 1.89.0"),
        "{outcome:?}"
    );
    assert_eq!(lines(&outcome.stdout).last(), Some(&"check: here: 6 missing, 0 too old"), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    for name in installers {
        assert!(!marker(name).exists(), "{name} ran: {outcome:?}");
    }
    assert_eq!(
        git(&case, &["-C", REPO, "status", "--porcelain", "--untracked-files=all"]),
        status_before,
        "the repo is as it was"
    );
}

/// A `.gate` that is not a folder cannot hold a manifest: the check could not run for the declared tools, so it exits
/// 1 and its summary says the manifest could not be read.
#[test]
fn check_a_gate_that_is_not_a_folder_exits_1_and_says_so() {
    let case = repo_case("check_gate_not_a_folder");
    write(&case, ".gate", "not a folder\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        ["check: here: could not read .gate/toolchain.toml; 0 missing, 0 too old"],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "check: .gate/toolchain.toml: .gate is not a folder\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `.gate/siblings.toml` that cannot be read leaves the siblings unchecked: the check could not run for them, so it
/// exits 1 and its summary says so, and the rest of the repo is still checked.
#[test]
fn check_an_unreadable_siblings_file_exits_1_and_says_so() {
    let case = repo_case("check_unreadable_siblings");
    write(&case, ".gate/siblings.toml", "siblings = [\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: could not read .gate/siblings.toml; 0 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert!(outcome.stderr.starts_with("check: .gate/siblings.toml: line "), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `global.json` that cannot be read whole means the SDK check could not run: the pin is skipped with its reason on
/// stderr, and the check exits 1 with its summary naming the file, as an unreadable `.gate` file does.
#[test]
fn check_a_global_json_that_cannot_be_read_says_why() {
    let case = repo_case("check_global_json_over_cap");
    let pinned = "{\"sdk\": {\"version\": \"10.0.401\"}}";
    write(&case, "global.json", &format!("{pinned}{}\n", " ".repeat(64 * 1024)));
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: .NET SDK skipped: global.json has no readable sdk pin",
            "check: here: could not read global.json; 0 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "check: global.json: is larger than 64 KiB\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `global.json` that is not UTF-8 cannot be read, and says so with its own reason.
#[test]
fn check_a_global_json_that_is_not_utf8_says_so() {
    let case = repo_case("check_global_json_not_utf8");
    write_bytes(&case, "global.json", &[0xff, 0xfe, b'{', b'}']);
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: .NET SDK skipped: global.json has no readable sdk pin",
            "check: here: could not read global.json; 0 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "check: global.json: is not UTF-8\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `.gate/toolchain.toml` and a `global.json` that both cannot be read are both named by the summary, the manifest
/// first, and the check exits 1.
#[test]
fn check_both_the_manifest_and_the_global_json_are_named_when_unreadable() {
    let case = repo_case("check_manifest_and_global_json_unreadable");
    write(&case, ".gate/toolchain.toml", "[tools\nnode = \"24\"\n");
    write(&case, "global.json", "{\"sdk\": \"10.0.401\"}\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: .NET SDK skipped: global.json has no readable sdk pin",
            "check: here: could not read .gate/toolchain.toml, global.json; 0 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert!(outcome.stderr.starts_with("check: .gate/toolchain.toml: line "), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// An `sdk` present but not an object names a pin that cannot be read: the SDK is skipped, the reason is said, and
/// the check exits 1.
#[test]
fn check_an_sdk_that_is_not_an_object_skips_the_pin() {
    let case = repo_case("check_sdk_not_an_object");
    write(&case, "global.json", "{\"sdk\": \"10.0.401\"}\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout)[1..],
        [
            "check: here: app declares no toolchain",
            "check: here: .NET SDK skipped: global.json has no readable sdk pin",
            "check: here: could not read global.json; 0 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "check: global.json: its sdk is not an object\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `global.json` that names no `sdk` at all, as a Godot repo's `msbuild-sdks`-only file does, prints no SDK line.
#[test]
fn check_a_global_json_naming_no_sdk_prints_no_sdk_line() {
    let case = repo_case("check_no_sdk_key");
    write(&case, "global.json", "{\"msbuild-sdks\": {\"Godot.NET.Sdk\": \"4.5.0\"}}\n");
    let stub = stubs(&case, &[]);

    let outcome = check(&case, &[stub], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: app declares no toolchain",
            "check: here: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The routing file of a warm-up case: the repo listed, routed to `builder-1` and consenting to warm-ups.
const WARM_ROUTING: &str = r#"{"host": "builder-1", "repos": {"app": ["*"]}, "warmup": ["app"]}"#;
/// A warm-up file that reads.
const WARMUP_FILE: &str = "commands = [[\"dotnet\", \"build\"]]\n";

/// A [`repo_case`] whose routing file consents to warm-ups and whose `.gate/warmup.toml` is [`WARMUP_FILE`].
fn warm_case(name: &str) -> Case {
    let case = repo_case(name);
    write_remote_json(&case, WARM_ROUTING);
    write(&case, ".gate/warmup.toml", WARMUP_FILE);
    case
}

/// `gate check` in the repo, its state root the case's through `GATE_STATE_DIR`, since check takes no `--state-dir`.
fn check_warm(case: &Case) -> Outcome {
    let mut command = case.gate_in(REPO, &["check"]);
    command
        .env("PATH", path_with_git(&[]))
        .env_remove("GATE_GODOT_ROOT")
        .env("GATE_STATE_DIR", case.root());
    finish(command)
}

/// Writes the work folder's warm-up record in the case's state root through the gate's own writer: `state` on
/// `builder-1` for [`WARMUP_FILE`], written now by the suite's gate build.
fn write_warm_record(case: &Case, state: WarmState) {
    let at = Timestamp::now();
    let record = WarmRecord {
        host: "builder-1".to_owned(),
        state,
        at,
        hash: warmup_hash(WARMUP_FILE.as_bytes()),
        gate: build_version(case),
        pid: 1,
        started: at,
        failure: None,
        reason: None,
    };
    let path = v1::warm_record_path(&case.root(), &key(case));
    fs::create_dir_all(v1::warm_dir(&case.root())).expect("the warm-up folder can be made");
    v1::write_warm_record(&path, &record).expect("the warm-up record can be written");
}

/// The lines of a warm-up case with no manifest whose one warm-up line is `line`, and the summary `summary`.
fn warm_lines(case: &Case, line: &str, summary: &str) -> Vec<String> {
    vec![
        format!("check: here: app at {}", root(case)),
        "check: here: app declares no toolchain".to_owned(),
        line.to_owned(),
        summary.to_owned(),
    ]
}

/// A repo with no warm-up file and a routing file that lists it without consenting says nothing of warm-ups.
#[test]
fn check_without_a_warm_up_file_or_consent_says_nothing_of_warm_ups() {
    let case = repo_case("check_warm_silent");
    write_remote_json(&case, r#"{"host": "builder-1", "repos": {"app": ["*"]}}"#);

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: app declares no toolchain",
            "check: here: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// Each problem of a consented warm-up file is a stderr line of its own, and the check fails, since `gate warm` then
/// cannot prewarm a new worktree.
#[test]
fn check_a_consented_broken_warm_up_file_prints_each_problem_and_exits_1() {
    let case = warm_case("check_warm_broken");
    write(&case, ".gate/warmup.toml", "commands = [[]]\nextra = 1\n");

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stderr),
        [
            "check: .gate/warmup.toml: commands[0] is empty",
            "check: .gate/warmup.toml: extra is not a key (commands, timeout)",
        ],
        "{outcome:?}"
    );
    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: .gate/warmup.toml has problems, so gate warm cannot prewarm a new worktree",
            "check: here: 1 missing, 0 too old"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

#[test]
fn check_a_consented_warm_up_file_with_no_record_says_not_warm_and_exits_0() {
    let case = warm_case("check_warm_no_record");

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: not warm on builder-1; gate warm warms it",
            "check: here: nothing missing"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_a_warm_record_for_the_host_and_file_says_warm() {
    let case = warm_case("check_warm_warm");
    write_warm_record(&case, WarmState::Warm);

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(&case, "check: here: warm-up: warm on builder-1", "check: here: nothing missing"),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A warm-up lock another handle holds says a warm-up is running, whatever the record says; the check only probes it.
#[test]
fn check_a_held_warm_up_lock_says_the_host_is_warming() {
    let case = warm_case("check_warm_lock_held");
    write_warm_record(&case, WarmState::Warming);
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(v1::warm_lock_path(&case.root(), &key(&case)))
        .expect("the case holds the warm-up lock");

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: builder-1 is warming the work folder",
            "check: here: nothing missing"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    drop(held);
}

/// A routing file whose `warmup` is no array of repo names.
const BAD_WARMUP_ROUTING: &str = r#"{"host": "builder-1", "repos": {"app": ["*"]}, "warmup": "app"}"#;

/// A `warmup` value that cannot be used fails the check of a listed repo with a warm-up file, saying why.
#[test]
fn check_an_unusable_warmup_value_with_a_warm_up_file_says_why_and_exits_1() {
    let case = warm_case("check_warm_bad_value");
    write_remote_json(&case, BAD_WARMUP_ROUTING);

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: remote.json: warmup must be an array of repo names",
            "check: here: 1 missing, 0 too old"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A routing file whose `hooks` is no array of repo names.
const BAD_HOOKS_ROUTING: &str = r#"{"host": "builder-1", "repos": {"app": ["*"]}, "hooks": "app"}"#;

/// A `hooks` value that cannot be used fails the check once, saying why.
#[test]
fn check_says_an_unusable_hooks_value() {
    let case = repo_case("check_bad_hooks_value");
    write_remote_json(&case, BAD_HOOKS_ROUTING);

    let outcome = check(&case, &[], &[]);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: app declares no toolchain",
            "check: here: remote.json: hooks must be an array of repo names",
            "check: here: 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A `warmup` value that cannot be used says nothing for a repo with no warm-up file, so one bad value never fails
/// the check of every repo on the PC.
#[test]
fn check_an_unusable_warmup_value_without_a_warm_up_file_says_nothing() {
    let case = repo_case("check_warm_bad_value_no_file");
    write_remote_json(&case, BAD_WARMUP_ROUTING);

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        [
            format!("check: here: app at {}", root(&case)).as_str(),
            "check: here: app declares no toolchain",
            "check: here: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A broken warm-up file without consent still has each problem said, but no warm-up would run, so it does not fail.
#[test]
fn check_a_broken_warm_up_file_without_consent_prints_its_problems_and_exits_0() {
    let case = repo_case("check_warm_broken_unconsented");
    write_remote_json(&case, r#"{"host": "builder-1", "repos": {"app": ["*"]}}"#);
    write(&case, ".gate/warmup.toml", "commands = [[]]\n");

    let outcome = check_warm(&case);

    assert_eq!(lines(&outcome.stderr), ["check: .gate/warmup.toml: commands[0] is empty"], "{outcome:?}");
    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: .gate/warmup.toml is there, but remote.json does not consent to warm-ups for app",
            "check: here: nothing missing"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_consent_without_a_warm_up_file_says_so_and_exits_0() {
    let case = repo_case("check_warm_consent_no_file");
    write_remote_json(&case, WARM_ROUTING);

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: remote.json consents, but the repo has no .gate/warmup.toml",
            "check: here: nothing missing"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A repo `remote.json`'s `repos` does not list has no consent, whether `warmup` names it or cannot be used, and its
/// warm-up file never fails the check.
#[test]
fn check_a_repo_remote_json_does_not_list_has_no_consent_and_exits_0() {
    let case = warm_case("check_warm_unlisted");
    for routing in [
        r#"{"host": "builder-1", "repos": {"other": ["*"]}, "warmup": ["app"]}"#,
        r#"{"host": "builder-1", "repos": {"other": ["*"]}, "warmup": "app"}"#,
    ] {
        write_remote_json(&case, routing);

        let outcome = check_warm(&case);

        assert_eq!(
            lines(&outcome.stdout),
            warm_lines(
                &case,
                "check: here: warm-up: .gate/warmup.toml is there, but remote.json does not consent to warm-ups for app",
                "check: here: nothing missing"
            ),
            "{routing}: {outcome:?}"
        );
        assert_eq!(outcome.code, Some(0), "{routing}: {outcome:?}");
    }
}

/// A warm-up file that cannot be read whole fails the check whatever the consent, since the repo declares a warm-up it
/// cannot use; one that reads with problems only informs without consent.
#[test]
fn check_an_unreadable_warmup_file_without_consent_says_why_and_exits_1() {
    let case = repo_case("check_warm_unreadable_unconsented");
    write_remote_json(&case, r#"{"host": "builder-1", "repos": {"app": ["*"]}}"#);
    let unconsented = "check: here: warm-up: .gate/warmup.toml is there, but remote.json does not consent to warm-ups for app";
    for (name, bytes, problem) in [
        (
            "not UTF-8",
            b"commands = [[\"\xff\"]]\n".to_vec(),
            "check: .gate/warmup.toml: not valid UTF-8",
        ),
        ("not TOML", b"commands = [\n".to_vec(), "check: .gate/warmup.toml: line "),
        (
            "over 64 KiB",
            format!("{WARMUP_FILE}#{}\n", "x".repeat(64 * 1024)).into_bytes(),
            "check: .gate/warmup.toml: is larger than 64 KiB",
        ),
    ] {
        write_bytes(&case, ".gate/warmup.toml", &bytes);

        let outcome = check_warm(&case);

        let stderr = lines(&outcome.stderr);
        assert!(stderr.len() == 1 && stderr[0].starts_with(problem), "{name}: {outcome:?}");
        assert_eq!(
            lines(&outcome.stdout),
            warm_lines(&case, unconsented, "check: here: could not read .gate/warmup.toml; 0 missing, 0 too old"),
            "{name}: {outcome:?}"
        );
        assert_eq!(outcome.code, Some(1), "{name}: {outcome:?}");
    }
}

/// A consented warm-up file that cannot be read whole has problems, so `gate warm` cannot run, and the summary names
/// it.
#[test]
fn check_an_unreadable_consented_warmup_file_says_why_and_exits_1() {
    let case = warm_case("check_warm_unreadable_consented");
    write(&case, ".gate/warmup.toml", "commands = [\n");

    let outcome = check_warm(&case);

    let stderr = lines(&outcome.stderr);
    assert!(
        stderr.len() == 1 && stderr[0].starts_with("check: .gate/warmup.toml: line "),
        "{outcome:?}"
    );
    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: .gate/warmup.toml has problems, so gate warm cannot prewarm a new worktree",
            "check: here: could not read .gate/warmup.toml; 1 missing, 0 too old"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A warm-up file that parses as TOML but holds a key of the wrong type reads with problems, which only inform without
/// consent.
#[test]
fn check_a_wrong_typed_warmup_key_without_consent_only_informs() {
    let case = repo_case("check_warm_wrong_type_unconsented");
    write_remote_json(&case, r#"{"host": "builder-1", "repos": {"app": ["*"]}}"#);
    write(&case, ".gate/warmup.toml", "commands = 5\n");

    let outcome = check_warm(&case);

    assert_eq!(
        lines(&outcome.stderr),
        ["check: .gate/warmup.toml: commands is not an array (TOML integer)"],
        "{outcome:?}"
    );
    assert_eq!(
        lines(&outcome.stdout),
        warm_lines(
            &case,
            "check: here: warm-up: .gate/warmup.toml is there, but remote.json does not consent to warm-ups for app",
            "check: here: nothing missing"
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// The routing file of a builder case: `builder-1` the host a routed run goes to, `builder-2` one `gate push` reaches.
const ROUTING: &str = r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}}"#;

/// A [`repo_case`] whose routing file lists the builders `builder-1` and `builder-2`, with a folder for the stand-in's
/// files.
fn builder_case(name: &str) -> Case {
    let case = repo_case(name);
    write_remote_json(&case, ROUTING);
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    case
}

/// The builder root of a builder case, a folder of the case's.
fn builder_root(case: &Case) -> PathBuf {
    case.folder().join("builder")
}

/// The work-folder key a routed run of the case's repo uses.
fn key(case: &Case) -> String {
    work_folder_key(&root(case))
}

/// `gate check --builder <host>` in the repo over the local transport, its one call made through the stand-in of the
/// steps file `steps`, whose `gate box preflight` runs with probe and SDK bounds no case reaches; the check's `PATH`,
/// which the builder's probes look along too, the `folders` then git's folder.
fn check_builder(case: &Case, host: &str, folders: &[PathBuf], steps: &Path) -> Outcome {
    finish(builder_check(case, host, folders, steps))
}

/// The command of a [`check_builder`] call, not yet run.
fn builder_check(case: &Case, host: &str, folders: &[PathBuf], steps: &Path) -> Command {
    let transport = format!("local:{}", builder_root(case).display());
    let args = [
        "check",
        "--builder",
        host,
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        FAKE_SSH,
    ];
    let mut command = case.gate_in(REPO, &args);
    command
        .env("PATH", path_with_git(folders))
        .env(STEPS_VARIABLE, steps)
        .env(GATE_VARIABLE, GATE)
        .env(PREFLIGHT_SEAMS_VARIABLE, ANSWERING)
        .env(crate::support::ROOT_WALK_STOP_VARIABLE, case.folder());
    command
}

/// The header and the created line of a builder case's first check of `host`.
fn first_lines(case: &Case, host: &str) -> [String; 2] {
    [
        format!("check: {host}: app (work folder {})", key(case)),
        format!("check: {host}: created {}\\app.git", builder_root(case).display()),
    ]
}

#[test]
fn check_builder_lists_every_item_and_exits_0_when_nothing_is_missing() {
    let case = builder_case("check_builder_every_tool");
    write(
        &case,
        ".gate/toolchain.toml",
        "[tools]\nrust = \"1.89\"\ncargo-nextest = \"0.9.100\"\nnode = \"24\"\npnpm = \"\"\npwsh = \"7.4\"\nffmpeg = \"\"\n",
    );
    let stub = stubs(&case, &EVERY_TOOL);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    let [header, created] = first_lines(&case, "builder-1");
    assert_eq!(
        lines(&outcome.stdout),
        [
            header.as_str(),
            created.as_str(),
            "check: builder-1: rust 1.99.0 ok (needs 1.89.0 or later)",
            "check: builder-1: cargo-nextest 0.9.148 ok (needs 0.9.100 or later)",
            "check: builder-1: node 24.19.0 ok (needs 24.0.0 or later)",
            "check: builder-1: pnpm 10.0.0 ok",
            "check: builder-1: pwsh 7.6.6 ok (needs 7.4.0 or later)",
            "check: builder-1: ffmpeg 9.0.2 ok",
            "check: builder-1: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "one call reached the builder");
}

/// The stub of a git-lfs that answers as 3.8.0 does.
const GIT_LFS_STUB: (&str, &str) = ("git-lfs", "@echo git-lfs/3.8.0 (GitHub; windows amd64; go 1.24.4)\r\n");

#[test]
fn check_builder_lists_git_lfs_for_a_repo_with_an_lfs_filter() {
    let case = builder_case("check_builder_git_lfs");
    write(&case, ".gitattributes", "# assets\n*.bin filter=lfs diff=lfs merge=lfs -text\n");
    git(&case, &["-C", REPO, "add", ".gitattributes"]);
    let stub = stubs(&case, &[GIT_LFS_STUB]);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    assert!(
        lines(&outcome.stdout).contains(&"check: builder-1: git-lfs 3.8.0 ok (needs 3.1.3 or later)"),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_builder_lists_git_lfs_for_an_lfs_filter_in_a_subfolder_alone() {
    let case = builder_case("check_builder_git_lfs_deep");
    write(&case, "assets/.gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    git(&case, &["-C", REPO, "add", "assets/.gitattributes"]);
    let stub = stubs(&case, &[GIT_LFS_STUB]);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    assert!(
        lines(&outcome.stdout).contains(&"check: builder-1: git-lfs 3.8.0 ok (needs 3.1.3 or later)"),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_builder_says_nothing_of_git_lfs_without_one() {
    let case = builder_case("check_builder_no_git_lfs");
    write(&case, "docs/.gitattributes", "*.txt text\n# *.bin filter=lfs\n");
    git(&case, &["-C", REPO, "add", "docs/.gitattributes"]);
    // Untracked, so no routed snapshot's checkout reads it.
    write(&case, ".gitattributes", "*.bin filter=lfs diff=lfs merge=lfs -text\n");
    let stub = stubs(&case, &[GIT_LFS_STUB]);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    assert!(!outcome.stdout.contains("git-lfs"), "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// `gate check --builder builder-1` of a repo whose manifest pins unreal 5.8, its builder holding the engine 5.8.0
/// under a root of the case's and listing the .NET Framework SDKs of the stand-in `key`.
fn check_builder_unreal(case: &Case, key: &NetfxKey) -> Outcome {
    write(case, ".gate/toolchain.toml", "[tools]\nunreal = \"5.8\"\n");
    let root = case.folder().join("ssh").join("unreal");
    fake_unreal_install(&root.join("5.8.0"), "5.8.0");
    let steps = ssh_steps(case, "ssh", &["ok"]);
    let mut command = builder_check(case, "builder-1", &[], &steps);
    command
        .env("GATE_UNREAL_ROOT", &root)
        .env(PREFLIGHT_SEAMS_VARIABLE, format!("{ANSWERING} {}", key.seam()));
    finish(command)
}

/// A builder an Unreal-pinned repo would build on without a .NET Framework SDK is said to miss it, with the hint that
/// names the Developer Pack, though its engine serves the pin.
#[test]
fn check_builder_says_a_missing_netfx_sdk_for_an_unreal_pinned_repo() {
    let case = builder_case("check_builder_netfx_missing");
    let key = NetfxKey::new();

    let outcome = check_builder_unreal(&case, &key);

    let said = lines(&outcome.stdout);
    for line in [
        "check: builder-1: unreal 5.8.0 ok (needs 5.8 exactly)",
        "check: builder-1: netfx-sdk missing (needs 4.6.0 or later)",
        "install: builder-1: netfx-sdk: copy tools/builder/ from the gate repo to builder-1 and run install-toolchains.ps1 -User <user> -Only '.NET Framework 4.8 Developer Pack' there, elevated (docs/routing-a-repo.md, Builder setup)",
    ] {
        assert!(said.contains(&line), "{line}: {outcome:?}");
    }
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A builder listing a .NET Framework SDK of 4.6 or later serves an Unreal-pinned repo.
#[test]
fn check_builder_says_a_netfx_sdk_ok_for_an_unreal_pinned_repo() {
    let case = builder_case("check_builder_netfx_ok");
    let key = netfx_present(&case);

    let outcome = check_builder_unreal(&case, &key);

    let said = lines(&outcome.stdout);
    assert!(said.contains(&netfx_ok("builder-1").as_str()), "{outcome:?}");
    assert!(said.contains(&"check: builder-1: nothing missing"), "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_builder_says_a_missing_tool_with_its_builder_hint_and_exits_1() {
    let case = builder_case("check_builder_missing_tool");
    write(
        &case,
        ".gate/toolchain.toml",
        "[tools]\nnode = \"24\"\ncargo-nextest = \"\"\npnpm = \"10\"\nrust = \"1.89\"\npwsh = \"\"\n",
    );
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: node 24.19.0 ok (needs 24.0.0 or later)",
            "check: builder-1: cargo-nextest missing (needs it present)",
            "check: builder-1: pnpm missing (needs 10.0.0 or later)",
            "check: builder-1: rust missing (needs 1.89.0 or later)",
            "check: builder-1: pwsh missing (needs it present)",
            "install: builder-1: cargo-nextest: cargo install cargo-nextest --locked",
            "install: builder-1: pnpm: npm install -g pnpm@10.0.0",
            "install: builder-1: rust: rustup toolchain install 1.89.0 && rustup default 1.89.0",
            "install: builder-1: pwsh: copy tools/builder/ from the gate repo to builder-1 and run install-toolchains.ps1 -User <user> -Only PowerShell there, elevated (docs/routing-a-repo.md, Builder setup)",
            "check: builder-1: 4 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A builder whose box refuses its work root says it as a missing item; the rest of the report still shows, its tools
/// unanswered.
#[test]
fn check_builder_reports_an_open_work_root_as_missing() {
    let case = builder_case("check_builder_open_root");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);
    let root = builder_root(&case);
    fs::create_dir_all(&root).expect("the builder root can be made");
    open_to_another_account(&root);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    let [header, _] = first_lines(&case, "builder-1");
    let item = format!(
        "check: builder-1: the work root is not locked down: {} lets S-1-0-0 add files to it; run the installer's \
         \"Gate work root\" step (install-toolchains.ps1 -Only \"Gate work root\")",
        root.display()
    );
    assert_eq!(
        lines(&outcome.stdout),
        [
            header.as_str(),
            "check: builder-1: node skipped: the builder gave no answer",
            item.as_str(),
            "check: builder-1: 1 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.stderr, "", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "one call reached the builder");
}

#[test]
fn check_builder_refuses_a_host_remote_json_does_not_list() {
    let case = builder_case("check_builder_unlisted_host");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-3", &[], &steps);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "gate check: builder-3 is not a builder remote.json lists\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(2), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn check_builder_without_remote_json_cannot_run() {
    let case = repo_case("check_builder_no_routing_file");
    fs::create_dir_all(case.folder().join("ssh")).expect("the stand-in's folder can be made");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    let path = case.local_app_data().join("gate").join("remote.json");
    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!("check: builder-1: remote.json not found at {}\n", path.display()),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(calls(&steps), Vec::<String>::new(), "no call was made");
}

#[test]
fn check_builder_says_an_unreachable_builder_and_exits_1() {
    let case = builder_case("check_builder_unreachable");
    let steps = ssh_steps(&case, "ssh", &["fail-other", "ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "check: builder-1: unreachable: ssh: connect to host builder port 22: Connection refused\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(calls(&steps), ["preflight"], "the call is made once, with no retry");
}

#[test]
fn check_builder_says_a_different_gate_build_and_nothing_else() {
    let case = builder_case("check_builder_other_build");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \"24\"\n");
    let stub = stubs(&case, &[("node", "@echo v24.19.0\r\n")]);
    let steps = ssh_steps(&case, "ssh", &["version 0.0.0-old"]);

    let outcome = check_builder(&case, "builder-1", &[stub], &steps);

    assert_eq!(outcome.stdout, "", "no tool line is said: {outcome:?}");
    assert_eq!(
        outcome.stderr,
        format!(
            "check: builder-1: runs gate 0.0.0-old, this PC runs {}; run gate push\n",
            build_version(&case)
        ),
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A builder that does not answer within the call's bound is ended at it and said unreachable. The stand-in holds on a
/// file no one makes, so only the bound ends the call, and nothing here asserts how long it took.
#[test]
fn check_builder_says_a_builder_that_does_not_answer_in_time() {
    let case = builder_case("check_builder_no_answer");
    let never = case.folder().join("never-made");
    let steps = ssh_steps(&case, "ssh", &[&format!("hold {}", never.display())]);
    let transport = format!("local:{}", builder_root(&case).display());
    let args = [
        "check",
        "--builder",
        "builder-1",
        "--test-remote-transport",
        &transport,
        "--test-remote-ssh",
        FAKE_SSH,
        "--test-remote-timeout-ms",
        "500",
    ];
    let mut command = case.gate_in(REPO, &args);
    command
        .env("PATH", path_with_git(&[]))
        .env(STEPS_VARIABLE, &steps)
        .env(GATE_VARIABLE, GATE);

    let outcome = finish(command);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(outcome.stderr, "check: builder-1: unreachable: no answer in 500 ms\n", "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// `--builder` names a builder in any letter case, and the lines spell it as `remote.json` does.
#[test]
fn check_builder_matches_the_host_in_any_letter_case() {
    let case = builder_case("check_builder_host_case");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "BUILDER-1", &[], &steps);

    let [header, _] = first_lines(&case, "builder-1");
    assert_eq!(lines(&outcome.stdout).first(), Some(&header.as_str()), "{outcome:?}");
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

#[test]
fn check_builder_says_a_builder_with_no_gate_box() {
    let case = builder_case("check_builder_no_gate_box");
    let steps = ssh_steps(&case, "ssh", &["exit 9009"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "check: builder-1: has no gate box that takes this call (status 9009); run gate push\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A check of a macOS `builders` entry calls that builder's `gate-box` under its own root, not the transport's, and a
/// preparation that fails names that root.
#[test]
fn check_builder_of_a_macos_entry_calls_its_gate_box_and_names_its_root() {
    let case = builder_case("check_builder_macos");
    write_remote_json(
        &case,
        r#"{"host": "builder-1", "repos": {"app": ["*"]},
            "builders": [{"host": "mac-1", "os": "macos", "root": "/Users/b/gate-remote", "repos": {"app": ["*"]}}]}"#,
    );
    let steps = ssh_steps(&case, "ssh", &["exit 1"]);

    let outcome = check_builder(&case, "MAC-1", &[], &steps);

    assert_eq!(outcome.stdout, "", "{outcome:?}");
    assert_eq!(
        outcome.stderr, "check: mac-1: preparing /Users/b/gate-remote failed (status 1)\n",
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    let lines = fs::read_to_string(crate::support::beside(&steps, ".lines")).expect("the stand-in kept the call's line");
    assert_eq!(lines, "mac-1 /Users/b/gate-remote/bin/gate-box box preflight --stdin\n");
}

/// The first check readies the work folder a routed run of the repo would, and says the bare repository it created; a
/// second creates nothing and says nothing of it. The builder root holds only what a routed run's preflight makes.
#[test]
fn check_builder_says_the_folders_its_preflight_created() {
    let case = builder_case("check_builder_created");
    let steps = ssh_steps(&case, "ssh", &["ok", "ok"]);

    let first = check_builder(&case, "builder-1", &[], &steps);
    let second = check_builder(&case, "builder-1", &[], &steps);

    let [header, created] = first_lines(&case, "builder-1");
    assert_eq!(
        lines(&first.stdout),
        [
            header.as_str(),
            created.as_str(),
            "check: builder-1: app declares no toolchain",
            "check: builder-1: nothing missing",
        ],
        "{first:?}"
    );
    assert_eq!(
        lines(&second.stdout),
        [
            header.as_str(),
            "check: builder-1: app declares no toolchain",
            "check: builder-1: nothing missing",
        ],
        "{second:?}"
    );
    let entries = |folder: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry reads").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(entries(&builder_root(&case)), ["app.git", "work"]);
    assert_eq!(
        entries(&builder_root(&case).join("work")),
        [key(&case)],
        "the work folder of the run's key alone"
    );
}

#[test]
fn check_builder_skips_declared_siblings() {
    let case = builder_case("check_builder_siblings");
    write(&case, ".gate/siblings.toml", "siblings = ['docs']\n");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: app declares no toolchain",
            "check: builder-1: siblings skipped: a routed run lays them out",
            "check: builder-1: nothing missing (1 skipped)",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A builder check reads the worktree's `.gate/siblings.toml` on this PC and says each of its problems as this PC's
/// check does.
#[test]
fn check_builder_says_the_siblings_file_s_problems() {
    let case = builder_case("check_builder_siblings_problem");
    write(&case, ".gate/toolchain.toml", "[tools]\nnode = \">=24\"\n");
    write(&case, ".gate/siblings.toml", "siblings = ['']\n");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stderr),
        [
            "check: .gate/toolchain.toml: tools.node is not a version: \">=24\"",
            "check: .gate/siblings.toml: siblings[0] is empty",
        ],
        "the manifest's problems come first, as on this PC: {outcome:?}"
    );
    assert!(
        lines(&outcome.stdout).contains(&"check: builder-1: siblings skipped: a routed run lays them out"),
        "{outcome:?}"
    );
}

/// A `.gate/siblings.toml` that cannot be read is named in the builder check's summary, and fails it, as on this PC.
#[test]
fn check_builder_an_unreadable_siblings_file_exits_1_and_says_so() {
    let case = builder_case("check_builder_siblings_unreadable");
    fs::create_dir_all(Path::new(&root(&case)).join(".gate").join("siblings.toml")).expect("a folder named siblings.toml can be made");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: app declares no toolchain",
            "check: builder-1: siblings skipped: a routed run lays them out",
            "check: builder-1: could not read .gate/siblings.toml; 0 missing, 0 too old, 1 skipped",
        ],
        "{outcome:?}"
    );
    assert!(outcome.stderr.starts_with("check: .gate/siblings.toml: "), "{outcome:?}");
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// The builder's SDK list is the one its preflight says, read there with its own `dotnet`; a builder `hosts` lists is
/// checked as the one `host` names is.
#[test]
fn check_builder_says_the_sdk_the_builder_has() {
    let case = builder_case("check_builder_sdk");
    write(&case, "global.json", "{\"sdk\": {\"version\": \"10.0.401\"}}\n");
    let dotnet = "@echo off\r\necho 8.0.404 [C:\\stub]\r\necho 10.0.300 [C:\\stub]\r\n";
    let stub = stubs(&case, &[("dotnet", dotnet)]);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-2", &[stub], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-2: app declares no toolchain",
            "check: builder-2: .NET SDK 10.0.401 (rollForward patch) missing (builder-2 has 8.0.404, 10.0.300)",
            "install: builder-2: .NET SDK: dotnet-install.ps1 -Version 10.0.401",
            "check: builder-2: 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

/// A consented warm-up file of a builder whose work folder holds no `.warm` says the folder is not warm, which never
/// fails the check; with no PC record naming the builder, no record line is said.
#[test]
fn check_builder_says_a_work_folder_that_is_not_warm() {
    let case = builder_case("check_builder_not_warm");
    write_remote_json(
        &case,
        r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}, "warmup": ["app"]}"#,
    );
    write(&case, ".gate/warmup.toml", WARMUP_FILE);
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: app declares no toolchain",
            "check: builder-1: warm-up: the work folder is not warm",
            "check: builder-1: nothing missing",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A PC warm-up record that cannot be read names no builder, so a builder check says it, as it may be the builder's; a
/// routed run does not read it, so it fails nothing.
#[test]
fn check_builder_says_an_unreadable_warm_record_and_exits_0() {
    let case = builder_case("check_builder_unreadable_record");
    write_remote_json(
        &case,
        r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}, "warmup": ["app"]}"#,
    );
    write(&case, ".gate/warmup.toml", WARMUP_FILE);
    let state = case.local_app_data().join("gate").join("v1");
    let record = v1::warm_record_path(&state, &key(&case));
    fs::create_dir_all(v1::warm_dir(&state)).expect("the warm-up folder can be made");
    fs::write(&record, "x".repeat(64 * 1024 + 1)).expect("the oversized record can be written");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: app declares no toolchain".to_owned(),
            format!(
                "check: builder-1: warm-up: the warm-up record {} cannot be read: is larger than 64 KiB; \
                 gate warm overwrites it, and only a layout fallback's delete reads it",
                record.display()
            ),
            "check: builder-1: warm-up: the work folder is not warm".to_owned(),
            "check: builder-1: nothing missing".to_owned(),
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A builder case whose routing file consents to warm-ups on `builder-1`, `builder-2` among its hosts.
fn warm_builder_case(name: &str) -> Case {
    let case = builder_case(name);
    write_remote_json(
        &case,
        r#"{"host": "builder-1", "hosts": ["builder-2"], "repos": {"app": ["*"]}, "warmup": ["app"]}"#,
    );
    case
}

/// A warm-up lock that cannot be probed is said before any record is read, as a held lock outweighs the record, so a
/// builder check says it though no record names the builder; it fails nothing.
#[test]
fn check_builder_says_a_warm_lock_that_cannot_be_probed_and_exits_0() {
    let case = warm_builder_case("check_builder_lock_unprobeable");
    write(&case, ".gate/warmup.toml", WARMUP_FILE);
    let lock = v1::warm_lock_path(&case.local_app_data().join("gate").join("v1"), &key(&case));
    fs::create_dir_all(&lock).expect("a folder can stand where the lock goes");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    let said = lines(&outcome.stdout);
    let start = format!("check: builder-1: warm-up: the warm-up lock {} cannot be read: ", lock.display());
    assert_eq!(said.len(), 6, "{outcome:?}");
    assert!(said[3].starts_with(&start), "{outcome:?}");
    assert!(
        said[3].ends_with("; gate warm and a routed run waiting on the work folder read it"),
        "{outcome:?}"
    );
    assert_eq!(
        said[4..],
        [
            "check: builder-1: warm-up: the work folder is not warm",
            "check: builder-1: nothing missing"
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
}

/// A consented warm-up file that cannot be read whole fails a builder check, the summary naming it.
#[test]
fn check_builder_says_an_unreadable_warmup_file_and_exits_1() {
    let case = warm_builder_case("check_builder_unreadable_warmup");
    write(&case, ".gate/warmup.toml", "commands = [\n");
    let steps = ssh_steps(&case, "ssh", &["ok"]);

    let outcome = check_builder(&case, "builder-1", &[], &steps);

    assert_eq!(
        lines(&outcome.stdout)[2..],
        [
            "check: builder-1: app declares no toolchain",
            "check: builder-1: warm-up: .gate/warmup.toml has problems, so gate warm cannot prewarm a new worktree",
            "check: builder-1: could not read .gate/warmup.toml; 1 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
}

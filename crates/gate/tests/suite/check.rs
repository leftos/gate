//! `gate check`: what this machine lacks for the repo it is run in. Each case's repo is the `app` folder of its case
//! folder, a git worktree of its own; its tools are `.cmd` stubs in the case folder's `stub` folder, outside the repo,
//! on a `PATH` of that folder and git's alone, so no real tool of the machine answers.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use gate_router::work_folder_key;

use crate::support::{
    ANSWERING, Case, FAKE_SSH, FAKE_TOOL, GATE, GATE_VARIABLE, Go, Outcome, PREFLIGHT_SEAMS_VARIABLE, STEPS_VARIABLE, build_version, calls, finish,
    git, ssh_steps, wait_until_closed, write_remote_json,
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
/// godot probe reads that `PATH` alone.
fn check(case: &Case, folders: &[PathBuf], extra: &[&str]) -> Outcome {
    let args: Vec<&str> = ["check"].iter().chain(extra).copied().collect();
    let mut command = case.gate_in(REPO, &args);
    command.env("PATH", path_with_git(folders));
    command.env_remove("GATE_GODOT_ROOT");
    finish(command)
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
            "check: here: godot missing (needs 4.5.0 or later)",
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
        .env(PREFLIGHT_SEAMS_VARIABLE, ANSWERING);
    finish(command)
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
            "install: builder-1: pwsh: run the build-box toolchain script on builder-1",
            "check: builder-1: 4 missing, 0 too old",
        ],
        "{outcome:?}"
    );
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
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

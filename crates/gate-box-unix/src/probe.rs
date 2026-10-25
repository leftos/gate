//! The preflight's tool probes and SDK list: each program found along the login shell's `PATH` and run in the bounded
//! runner from `/`, so no project file of the ssh session's folder (a `rust-toolchain.toml`, a `global.json`) bends
//! what it says. Godot is read from the installs under the login shell's `GATE_GODOT_ROOT`, as the Windows box reads
//! its own.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gate_wire::{MAX_PART_DIGITS, Probed, probed, sdk_list};

use crate::lines::no_answer;
use crate::process::{NotRan, Ran, run_bounded, status_text, with_path};

/// The deadline every probe of one preflight shares.
pub const PROBE_LIMIT: Duration = Duration::from_secs(10);
/// How long `dotnet --list-sdks` may take.
pub const DOTNET_LIMIT: Duration = Duration::from_secs(30);
/// The most a probe or `dotnet --list-sdks` may write on its standard output.
const OUTPUT_CAP: usize = 16 * 1024 * 1024;
/// The folder every probe runs in.
const RUN_FOLDER: &str = "/";
/// The variable naming the folder of the builder's Godot installs, one subfolder per version.
const GODOT_ROOT: &str = "GATE_GODOT_ROOT";
/// The program of a Godot install, inside its version's folder.
const GODOT_PROGRAM: &str = "Godot.app/Contents/MacOS/Godot";
/// The most subfolders of the Godot root the probe reads.
const ROOT_ENTRIES: usize = 64;
/// The most characters of a Godot install's answer the no-version reason carries.
const GODOT_ANSWER_MAX: usize = 200;

/// How a tool is probed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Run the program with the arguments and read its version.
    Runs { program: &'static str, args: &'static [&'static str] },
    /// Read the installs under the Godot root ([`godot`]).
    Godot,
    /// A tool this builder never has: a run that needs it is not routed here.
    Missing,
    /// A name the box knows no probe for.
    Unknown,
}

/// The probe of the tool `name`.
fn plan(name: &str) -> Plan {
    if let Some((program, args)) = gate_wire::runs_probe(name) {
        return Plan::Runs { program, args };
    }
    match name {
        "godot" => Plan::Godot,
        "unreal" | "netfx-sdk" => Plan::Missing,
        _ => Plan::Unknown,
    }
}

/// What each tool of `names` is, in their order, probed in parallel along `path` (the box's own `PATH` when `None`)
/// under one shared [`PROBE_LIMIT`], Godot under the Godot root `godot_root` (none when `None`).
#[must_use]
pub fn probe_all(names: &[String], path: Option<&str>, godot_root: Option<&str>) -> Vec<Probed> {
    let deadline = Instant::now() + PROBE_LIMIT;
    thread::scope(|scope| {
        let probes: Vec<_> = names
            .iter()
            .map(|name| scope.spawn(move || probe(name, path, godot_root, deadline)))
            .collect();
        probes
            .into_iter()
            .map(|probe| probe.join().unwrap_or_else(|_| Probed::Skipped("the probe failed".to_owned())))
            .collect()
    })
}

/// What the tool `name` is, its program run along `path` until `deadline`, Godot read under `godot_root`.
fn probe(name: &str, path: Option<&str>, godot_root: Option<&str>, deadline: Instant) -> Probed {
    match plan(name) {
        Plan::Runs { program, args } => ran(program, args, path, deadline, |ran| {
            probed(ran.status.success(), &ran.stdout, &ran.stderr)
        }),
        Plan::Godot => godot(godot_root, path, deadline),
        Plan::Missing => Probed::Missing,
        Plan::Unknown => Probed::Skipped("unknown tool".to_owned()),
    }
}

/// What `program` run with `args` along `path` until `deadline` found, `read` turning what it wrote into it; nothing is
/// run once the deadline has passed.
fn ran(program: &str, args: &[&str], path: Option<&str>, deadline: Instant, read: impl Fn(&Ran) -> Probed) -> Probed {
    if Instant::now() >= deadline {
        return Probed::Skipped(no_answer(PROBE_LIMIT));
    }
    let mut command = Command::new(program);
    with_path(command.args(args).current_dir(RUN_FOLDER), path);
    match run_bounded(command, OUTPUT_CAP, deadline) {
        Ok(ran) => read(&ran),
        Err(NotRan::Start(error)) if error.kind() == io::ErrorKind::NotFound => Probed::Missing,
        Err(NotRan::Start(error)) => Probed::Skipped(format!("cannot run {program}: {error}")),
        Err(NotRan::TimedOut { .. }) => Probed::Skipped(no_answer(PROBE_LIMIT)),
        Err(NotRan::OverCap) => Probed::Skipped(format!("{program} {} wrote more than 16 MiB", args.join(" "))),
        Err(NotRan::Failed(why)) => Probed::Skipped(format!("cannot run {program}: {why}")),
    }
}

/// What the godot probe finds under the Godot root `root`, each install's `--version` run along `path` until
/// `deadline`: the version of every install, `,`-joined in version order, an install whose folder is not named the
/// version it reports left out, as a run finds an install by its folder's name ([`installed_godot`]); `missing` with no
/// root or no install under it; `skipped` when the root is not an absolute path or cannot be read, or with the first
/// reason an install gave when installs are there and none of them gives its version.
fn godot(root: Option<&str>, path: Option<&str>, deadline: Instant) -> Probed {
    let Some(root) = root else {
        return Probed::Missing;
    };
    if !root.starts_with('/') {
        return Probed::Skipped(format!("{GODOT_ROOT} {root} is not an absolute path"));
    }
    let folders = match listed(Path::new(root), deadline) {
        Ok(folders) => folders,
        Err(why) => return Probed::Skipped(why),
    };
    gathered(folders.iter().filter_map(|folder| {
        godot_install(folder).map(|candidate| match candidate {
            Candidate::Install(program) => {
                let found = ran(&program.to_string_lossy(), &["--version"], path, deadline, read_godot);
                named_as(&folder.file_name().unwrap_or_default().to_string_lossy(), found)
            }
            Candidate::Skipped(why) => Probed::Skipped(why),
        })
    }))
}

/// The direct subfolders of the Godot root `root`, the first [`ROOT_ENTRIES`] read, in name order; one whose name is no
/// Unicode and an entry that cannot be read are left out, and the listing ends at `deadline`, which with nothing read
/// at all is the probe's own `no answer` reason. Else why the root cannot be read.
fn listed(root: &Path, deadline: Instant) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(root).map_err(|error| format!("cannot read {GODOT_ROOT} {}: {error}", root.display()))?;
    let mut folders = Vec::new();
    for entry in entries.take(ROOT_ENTRIES) {
        if Instant::now() >= deadline {
            if folders.is_empty() {
                return Err(no_answer(PROBE_LIMIT));
            }
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_name().to_str().is_some() {
            folders.push(entry.path());
        }
    }
    folders.sort();
    Ok(folders)
}

/// One direct subfolder of the Godot root as the godot probe reads it.
#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    /// The Godot program to run, by path.
    Install(PathBuf),
    /// A folder, or the program in it, that is a symbolic link or not what its place holds: left out for this reason.
    Skipped(String),
}

/// What `folder`, one direct subfolder of the Godot root, comes to: the [`GODOT_PROGRAM`] in it to run; why it is left
/// out when the folder, its `Godot.app`, `Contents` or `MacOS` folder or the program is a link, or one of those
/// folders is no folder or the program no plain file; nothing when the folder is no folder or holds no such program.
/// The Godot root above it may be a link: the builder's own setting.
fn godot_install(folder: &Path) -> Option<Candidate> {
    let program = folder.join(GODOT_PROGRAM);
    let skipped = |path: &Path, flaw: &str| Some(Candidate::Skipped(format!("cannot run {}: {} {flaw}", program.display(), path.display())));
    // The folder and every level down to the program, the folder first.
    let mut levels: Vec<&Path> = program.ancestors().take(Path::new(GODOT_PROGRAM).components().count() + 1).collect();
    levels.reverse();
    for (depth, level) in levels.iter().enumerate() {
        let kind = fs::symlink_metadata(level).ok()?.file_type();
        let is_program = depth + 1 == levels.len();
        if kind.is_symlink() {
            return skipped(level, "is a link");
        }
        if depth == 0 && !kind.is_dir() {
            return None;
        }
        if !is_program && !kind.is_dir() {
            return skipped(level, "is not a folder");
        }
        if is_program && !kind.is_file() {
            return skipped(level, "is not a file");
        }
    }
    Some(Candidate::Install(program))
}

/// The Godot program of the install of `version` under the Godot root `root`, as a run finds it by name:
/// `<root>/<version>/Godot.app/Contents/MacOS/Godot`, when the root is an absolute path and that program is a plain
/// file in plain folders, no level of it under the root a link ([`godot_install`]); else `None`.
pub(crate) fn installed_godot(root: &str, version: &str) -> Option<PathBuf> {
    if !root.starts_with('/') {
        return None;
    }
    match godot_install(&Path::new(root).join(version))? {
        Candidate::Install(program) => Some(program),
        Candidate::Skipped(_) => None,
    }
}

/// The version an install whose folder is named `folder` `found`, kept only when it is that name: else skipped with
/// the reason `named <folder> but reports <version>`. Any other finding as it is.
fn named_as(folder: &str, found: Probed) -> Probed {
    match found {
        Probed::Version(version) if version != folder => Probed::Skipped(format!("named {folder} but reports {version}")),
        found => found,
    }
}

/// What a probe of several installs comes to from what each install's probe `found`, in order: the version of every
/// one that gave one, `,`-joined in version order, `missing` when there is none at all, and `skipped` with the first
/// reason one gave when none gave a version.
fn gathered(found: impl Iterator<Item = Probed>) -> Probed {
    let mut versions = Vec::new();
    let mut reason: Option<String> = None;
    for found in found {
        match found {
            Probed::Version(version) => versions.push(version),
            Probed::Present | Probed::Missing => {}
            Probed::Skipped(why) => {
                reason.get_or_insert(why);
            }
        }
    }
    if versions.is_empty() {
        return reason.map_or(Probed::Missing, Probed::Skipped);
    }
    // Each part is at most `MAX_PART_DIGITS` ASCII digits ([`godot_leading`]), which a `u64` always holds.
    versions.sort_by_key(|version| version.split('.').map(|part| part.parse::<u64>().unwrap_or(u64::MAX)).collect::<Vec<_>>());
    Probed::Version(versions.join(","))
}

/// What a Godot install's `--version` run `ran` found: the version the first line of its standard output leads with
/// ([`godot_version`]), or why it gave none: the first line it wrote, cut to [`GODOT_ANSWER_MAX`] characters with `…`
/// after when it is longer.
fn read_godot(ran: &Ran) -> Probed {
    let stdout = String::from_utf8_lossy(&ran.stdout);
    godot_version(&stdout).map_or_else(
        || {
            let line = stdout.lines().next().unwrap_or("").trim();
            let mut cut: String = line.chars().take(GODOT_ANSWER_MAX).collect();
            if cut.len() < line.len() {
                cut.push('…');
            }
            Probed::Skipped(format!("Godot --version gave no version: {cut}"))
        },
        Probed::Version,
    )
}

/// The version a Godot install's standard output `stdout` gives: the leading dot-separated runs of ASCII digits of its
/// first line, trimmed, as Godot writes it (`4.7.2.stable.mono.official.abc`), each spelled with no leading zero, the
/// canonical spelling an install's folder and a run's `GodotVersion` have ([`gate_wire::is_canonical_version`]). Three
/// runs are taken, and two are given a `.0`, which Godot leaves out of a zero patch (`4.3.stable.mono.official.77dcf97d8`
/// is `4.3.0`). `None` for any other shape, an output with no line among them.
fn godot_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next().unwrap_or("").trim();
    let canonical = |run: &&str| match run.trim_start_matches('0') {
        "" => "0".to_owned(),
        digits => digits.to_owned(),
    };
    let runs: Vec<String> = godot_leading(line)?.iter().map(canonical).collect();
    match runs.as_slice() {
        [major, minor] => Some(format!("{major}.{minor}.0")),
        [major, minor, patch] => Some(format!("{major}.{minor}.{patch}")),
        _ => None,
    }
}

/// The leading dot-separated runs of ASCII digits `line` spells, with a trailing `.` dropped: `None` when it does not
/// start with a digit, one of the runs is empty, or one has more than [`MAX_PART_DIGITS`] digits.
fn godot_leading(line: &str) -> Option<Vec<&str>> {
    let end = line
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(line.len());
    let runs: Vec<&str> = line[..end].trim_end_matches('.').split('.').collect();
    let part = |run: &&str| !run.is_empty() && run.len() <= MAX_PART_DIGITS && run.bytes().all(|byte| byte.is_ascii_digit());
    runs.iter().all(part).then_some(runs)
}

/// The SDK list of `dotnet --list-sdks` run along `path` within [`DOTNET_LIMIT`] ([`sdk_list`]); `none` when no
/// dotnet is found. Else why it could not be read, for [`crate::lines::sdks_failure`].
///
/// # Errors
///
/// Why the list could not be read: dotnet ran past the limit, wrote more than 16 MiB, failed, or could not start.
pub fn sdks(path: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("dotnet");
    with_path(command.arg("--list-sdks").current_dir(RUN_FOLDER), path);
    match run_bounded(command, OUTPUT_CAP, Instant::now() + DOTNET_LIMIT) {
        Ok(ran) if ran.status.success() => Ok(sdk_list(&String::from_utf8_lossy(&ran.stdout))),
        Ok(ran) => Err(format!("dotnet --list-sdks exited with {}", status_text(ran.status))),
        Err(NotRan::Start(error)) if error.kind() == io::ErrorKind::NotFound => Ok("none".to_owned()),
        Err(NotRan::Start(error)) => Err(format!("cannot run dotnet: {error}")),
        Err(NotRan::TimedOut { .. }) => Err(format!("dotnet --list-sdks did not answer in {} s", DOTNET_LIMIT.as_secs())),
        Err(NotRan::OverCap) => Err("dotnet --list-sdks wrote more than 16 MiB".to_owned()),
        Err(NotRan::Failed(why)) => Err(format!("cannot run dotnet: {why}")),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose fake Godot root cannot be made must fail loudly")]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use gate_wire::Probed;

    use super::{GODOT_PROGRAM, Plan, godot_version, installed_godot, listed, plan, probe_all};

    #[test]
    fn each_tool_has_its_probe() {
        assert_eq!(
            plan("rust"),
            Plan::Runs {
                program: "rustc",
                args: &["--version"]
            }
        );
        assert_eq!(
            plan("cargo-nextest"),
            Plan::Runs {
                program: "cargo-nextest",
                args: &["nextest", "--version"]
            }
        );
        assert_eq!(
            plan("ffmpeg"),
            Plan::Runs {
                program: "ffmpeg",
                args: &["-version"]
            }
        );
        assert_eq!(
            plan("git-lfs"),
            Plan::Runs {
                program: "git-lfs",
                args: &["version"]
            }
        );
        assert_eq!(plan("godot"), Plan::Godot);
        for name in ["unreal", "netfx-sdk"] {
            assert_eq!(plan(name), Plan::Missing, "{name}");
        }
        assert_eq!(plan("Rust"), Plan::Unknown, "names are exact");
    }

    #[test]
    fn the_probes_answer_in_the_order_asked() {
        let names = ["godot", "bogus", "netfx-sdk"].map(str::to_owned);
        assert_eq!(
            probe_all(&names, None, None),
            [Probed::Missing, Probed::Skipped("unknown tool".to_owned()), Probed::Missing],
            "no Godot root is Godot missing"
        );
    }

    /// A folder of its own for the case `name`, removed when dropped.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let folder = std::env::temp_dir().join(format!("gate-box-unix-godot-{name}-{}", std::process::id()));
            // Best effort: a folder an earlier run of this process id left.
            let _ = fs::remove_dir_all(&folder);
            fs::create_dir_all(&folder).expect("the case's folder is made");
            Folder(folder)
        }

        fn root(&self) -> String {
            self.0.join("godot").display().to_string()
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Puts at `<base>/<version>/Godot.app/Contents/MacOS/Godot` the runnable `script`, standing in for Godot; the
    /// program's path.
    pub(crate) fn fake_godot(base: &Path, version: &str, script: &str) -> PathBuf {
        let program = base.join(version).join(GODOT_PROGRAM);
        fs::create_dir_all(program.parent().expect("the program has a folder")).expect("the install's folders are made");
        fs::write(&program, script).expect("the fake Godot is written");
        fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the fake Godot is made runnable");
        program
    }

    /// A script standing in for Godot that answers `--version` with `says`.
    fn answering(says: &str) -> String {
        format!("#!/bin/sh\necho '{says}'\n")
    }

    /// What the probe of `godot` finds under `root`.
    fn godot_probed(root: Option<&str>) -> Probed {
        probe_all(&["godot".to_owned()], None, root).remove(0)
    }

    #[test]
    fn the_godot_probe_lists_each_install_named_as_its_version_and_leaves_out_links() {
        let case = Folder::new("probe");
        let root = case.root();
        let installs = Path::new(&root);
        let outside = case.0.join("outside");
        fake_godot(installs, "4.7.2", &answering("4.7.2.stable.mono.official.abc"));
        fake_godot(installs, "4.11.0", &answering("4.11.stable.mono.official.def"));
        fake_godot(installs, "4.7.1", &answering("4.7.2.stable.mono.official.abc"));
        fake_godot(&outside, "4.9.0", &answering("4.9.0.stable.mono.official.abc"));
        symlink(outside.join("4.9.0"), installs.join("4.9.0")).expect("a link at the folder is made");
        let linked_program = fake_godot(&outside, "4.10.0", &answering("4.10.0.stable.mono.official.abc"));
        let program = installs.join("4.10.0").join(GODOT_PROGRAM);
        fs::create_dir_all(program.parent().expect("the program has a folder")).expect("the folders are made");
        symlink(&linked_program, &program).expect("a link at the program is made");
        fs::create_dir_all(installs.join("empty")).expect("a folder with no install is made");

        assert_eq!(godot_probed(Some(&root)), Probed::Version("4.7.2,4.11.0".to_owned()), "in version order");
        assert_eq!(
            installed_godot(&root, "4.7.2"),
            Some(installs.join("4.7.2").join(GODOT_PROGRAM)),
            "a run finds the install by its folder"
        );
        for absent in ["4.9.0", "4.10.0", "5.0.0"] {
            assert_eq!(installed_godot(&root, absent), None, "{absent}: a link or no install");
        }
        assert_eq!(installed_godot("godot", "4.7.2"), None, "a relative root");

        for kept in ["4.7.2", "4.11.0"] {
            fs::remove_dir_all(installs.join(kept)).expect("the install is removed");
        }
        let link = format!("cannot run {0}: {0} is a link", program.display());
        assert_eq!(godot_probed(Some(&root)), Probed::Skipped(link), "no version: the first reason, 4.10.0's");
        fs::remove_dir_all(installs.join("4.10.0")).expect("the linked install is removed");
        assert_eq!(
            godot_probed(Some(&root)),
            Probed::Skipped("named 4.7.1 but reports 4.7.2".to_owned()),
            "then 4.7.1's"
        );
    }

    #[test]
    fn a_godot_root_that_is_none_relative_empty_or_unreadable_finds_no_install() {
        let case = Folder::new("roots");
        let root = case.root();
        assert_eq!(godot_probed(None), Probed::Missing);
        assert_eq!(
            godot_probed(Some("godot")),
            Probed::Skipped("GATE_GODOT_ROOT godot is not an absolute path".to_owned())
        );
        let unread = godot_probed(Some(&root));
        assert!(
            matches!(&unread, Probed::Skipped(why) if why.starts_with(&format!("cannot read GATE_GODOT_ROOT {root}: "))),
            "{unread:?}"
        );
        fs::create_dir_all(&root).expect("the root is made");
        assert_eq!(godot_probed(Some(&root)), Probed::Missing, "a root with no install");
        fake_godot(Path::new(&root), "4.7.2", &answering("Godot Engine"));
        assert_eq!(
            godot_probed(Some(&root)),
            Probed::Skipped("Godot --version gave no version: Godot Engine".to_owned())
        );
    }

    #[test]
    fn a_link_or_a_wrong_kind_at_any_level_under_the_version_folder_leaves_the_install_out() {
        let case = Folder::new("levels");
        let root = case.root();
        let installs = Path::new(&root);
        let outside = case.0.join("outside");
        let program = installs.join("4.7.2").join(GODOT_PROGRAM);
        let skipped = |level: &Path, flaw: &str| Probed::Skipped(format!("cannot run {}: {} {flaw}", program.display(), level.display()));
        for level in ["Godot.app", "Godot.app/Contents", "Godot.app/Contents/MacOS"] {
            fake_godot(&outside, "4.7.2", &answering("4.7.2.stable.mono.official.abc"));
            let linked = installs.join("4.7.2").join(level);
            fs::create_dir_all(linked.parent().expect("the level has a folder")).expect("the folders above the link are made");
            symlink(outside.join("4.7.2").join(level), &linked).expect("a link at the level is made");
            assert_eq!(installed_godot(&root, "4.7.2"), None, "{level}");
            assert_eq!(godot_probed(Some(&root)), skipped(&linked, "is a link"), "{level}");
            fs::remove_dir_all(installs.join("4.7.2")).expect("the install is removed");
        }

        let app = installs.join("4.7.2").join("Godot.app");
        fs::create_dir_all(installs.join("4.7.2")).expect("the version folder is made");
        fs::write(&app, "").expect("a file where Godot.app goes is written");
        assert_eq!(godot_probed(Some(&root)), skipped(&app, "is not a folder"));
        fs::remove_file(&app).expect("the file is removed");
        fs::create_dir_all(&program).expect("a folder where the program goes is made");
        assert_eq!(godot_probed(Some(&root)), skipped(&program, "is not a file"));
        assert_eq!(installed_godot(&root, "4.7.2"), None);
    }

    #[test]
    fn the_godot_root_listing_considers_at_most_the_first_64_entries_read() {
        let case = Folder::new("listing");
        let root = case.0.join("godot");
        for n in 0..69 {
            fs::create_dir_all(root.join(format!("4.0.{n}"))).expect("a version folder is made");
        }
        fs::write(root.join(".DS_Store"), "").expect("a file is written");
        let considered = listed(&root, Instant::now() + Duration::from_secs(3600)).expect("the root is listed");
        assert_eq!(considered.len(), 64, "{considered:?}");
        assert!(considered.is_sorted(), "{considered:?}");
    }

    #[test]
    fn godot_s_version_is_its_first_line_s_leading_digits_in_canonical_spelling() {
        assert_eq!(godot_version("4.7.2.stable.mono.official.abc\n"), Some("4.7.2".to_owned()));
        assert_eq!(godot_version("4.3.stable.mono.official.77dcf97d8"), Some("4.3.0".to_owned()));
        assert_eq!(godot_version("04.07.02.stable"), Some("4.7.2".to_owned()));
        assert_eq!(godot_version("4.stable"), None);
        assert_eq!(godot_version("Godot 4.7.2"), None);
        assert_eq!(godot_version(""), None);
    }
}

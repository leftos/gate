//! The preflight's tool probes and SDK list: each program found along the login shell's `PATH` and run in the bounded
//! runner from `/`, so no project file of the ssh session's folder (a `rust-toolchain.toml`, a `global.json`) bends
//! what it says. Godot is read from the installs under the login shell's `GATE_GODOT_ROOT`, and Blender from those
//! under its `GATE_BLENDER_ROOT`, as the Windows box reads its own.

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
/// The variable naming the folder of the builder's Blender installs, one subfolder per version
/// ([`gate_wire::blender_folder_version`]).
const BLENDER_ROOT: &str = "GATE_BLENDER_ROOT";
/// The program of a Blender install, inside its version's folder.
const BLENDER_PROGRAM: &str = "Blender.app/Contents/MacOS/Blender";
/// The most subfolders of the Godot or Blender root the probe reads.
const ROOT_ENTRIES: usize = 64;
/// The most characters of a Godot or Blender install's answer the no-version reason carries.
const GODOT_ANSWER_MAX: usize = 200;

/// How a tool is probed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Run the program with the arguments and read its version.
    Runs { program: &'static str, args: &'static [&'static str] },
    /// Read the installs under the Godot root ([`godot`]).
    Godot,
    /// Read the installs under the Blender root ([`blender`]).
    Blender,
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
        "blender" => Plan::Blender,
        "unreal" | "netfx-sdk" => Plan::Missing,
        _ => Plan::Unknown,
    }
}

/// The roots of the builder's installs a probe reads, each `None` when the login shell gives none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Roots<'a> {
    /// The Godot root, `GATE_GODOT_ROOT`.
    pub godot: Option<&'a str>,
    /// The Blender root, `GATE_BLENDER_ROOT`.
    pub blender: Option<&'a str>,
}

/// What each tool of `names` is, in their order, probed in parallel along `path` (the box's own `PATH` when `None`)
/// under one shared [`PROBE_LIMIT`], Godot and Blender under their `roots`.
#[must_use]
pub fn probe_all(names: &[String], path: Option<&str>, roots: Roots<'_>) -> Vec<Probed> {
    let deadline = Instant::now() + PROBE_LIMIT;
    thread::scope(|scope| {
        let probes: Vec<_> = names.iter().map(|name| scope.spawn(move || probe(name, path, roots, deadline))).collect();
        probes
            .into_iter()
            .map(|probe| probe.join().unwrap_or_else(|_| Probed::Skipped("the probe failed".to_owned())))
            .collect()
    })
}

/// What the tool `name` is, its program run along `path` until `deadline`, Godot and Blender read under `roots`.
fn probe(name: &str, path: Option<&str>, roots: Roots<'_>, deadline: Instant) -> Probed {
    match plan(name) {
        Plan::Runs { program, args } => ran(program, args, path, deadline, |ran| {
            probed(ran.status.success(), &ran.stdout, &ran.stderr)
        }),
        Plan::Godot => godot(roots.godot, path, deadline),
        Plan::Blender => blender(roots.blender, path, deadline),
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
    root_probe(GODOT_ROOT, root, path, deadline, |folder, path, deadline| {
        app_install(folder, GODOT_PROGRAM).map(|candidate| match candidate {
            Candidate::Install(program) => {
                let found = ran(&program.to_string_lossy(), &["--version"], path, deadline, read_godot);
                named_as(&folder.file_name().unwrap_or_default().to_string_lossy(), found)
            }
            Candidate::Skipped(why) => Probed::Skipped(why),
        })
    })
}

/// What a probe of the installs under the root `root`, which the variable `variable` names, finds, `per_folder`
/// judging each of its listed folders ([`listed`]) along `path` until `deadline`, and [`gathered`] joining what they
/// come to: `missing` with no root, `skipped` when the root is not an absolute path or cannot be read.
fn root_probe(
    variable: &str,
    root: Option<&str>,
    path: Option<&str>,
    deadline: Instant,
    per_folder: impl Fn(&Path, Option<&str>, Instant) -> Option<Probed>,
) -> Probed {
    let Some(root) = root else {
        return Probed::Missing;
    };
    if !root.starts_with('/') {
        return Probed::Skipped(format!("{variable} {root} is not an absolute path"));
    }
    let folders = match listed(variable, Path::new(root), deadline) {
        Ok(folders) => folders,
        Err(why) => return Probed::Skipped(why),
    };
    gathered(folders.iter().filter_map(|folder| per_folder(folder, path, deadline)))
}

/// What the blender probe finds under the Blender root `root`, each install's `--version` run along `path` until
/// `deadline`: the version of every install, `,`-joined in version order ([`gathered`]), a folder whose name names no
/// version ([`gate_wire::blender_folder_version`]) not looked at, and an install whose folder names a version the one
/// it reports does not meet left out ([`blender_named_as`]), as a run finds an install by its folder's name
/// ([`installed_blender`]); `missing` with no root, as nothing is looked for along the `PATH`, or no install under it;
/// `skipped` when the root is not an absolute path or cannot be read, or with the first reason an install gave when
/// installs are there and none of them gives its version.
fn blender(root: Option<&str>, path: Option<&str>, deadline: Instant) -> Probed {
    root_probe(BLENDER_ROOT, root, path, deadline, blender_found)
}

/// What the Blender install in `folder`, one direct subfolder of the Blender root, comes to, the one judgement the
/// probe ([`blender`]) and a run ([`installed_blender`]) share: nothing when the folder's name names no version
/// ([`gate_wire::blender_folder_version`]) or it holds no install; why it is left out when [`app_install`] leaves it
/// out; else what its `--version` run along `path` until `deadline` reports, kept only when it meets the folder's
/// version ([`blender_named_as`]).
fn blender_found(folder: &Path, path: Option<&str>, deadline: Instant) -> Option<Probed> {
    let name = folder.file_name()?.to_str()?;
    let named = gate_wire::blender_folder_version(name)?;
    app_install(folder, BLENDER_PROGRAM).map(|candidate| match candidate {
        Candidate::Install(program) => {
            let found = ran(&program.to_string_lossy(), &["--version"], path, deadline, read_blender);
            blender_named_as(name, named, found)
        }
        Candidate::Skipped(why) => Probed::Skipped(why),
    })
}

/// The version a Blender install whose folder is named `folder`, for the version `named`, `found`, kept only when it
/// meets that version ([`gate_wire::version_meets`]): else skipped with the reason `named <folder> but reports
/// <version>`. Any other finding as it is.
fn blender_named_as(folder: &str, named: &str, found: Probed) -> Probed {
    match found {
        Probed::Version(version) if !gate_wire::version_meets(named, &version) => Probed::Skipped(format!("named {folder} but reports {version}")),
        found => found,
    }
}

/// What a Blender install's `--version` run `ran` found: the first version of the first line of its standard output
/// ([`gate_wire::first_version`]), as Blender writes it (`Blender 5.2.2 LTS`), or why it gave none: that line, cut as
/// [`read_godot`] cuts Godot's.
fn read_blender(ran: &Ran) -> Probed {
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let line = stdout.lines().next().unwrap_or("");
    gate_wire::first_version(line).map_or_else(
        || Probed::Skipped(format!("Blender --version gave no version: {}", answer_cut(&stdout))),
        Probed::Version,
    )
}

/// The Blender program of the install the version `version` resolves to under the Blender root `root`, exactly as the
/// probe keeps installs: of the root's folders whose name's version `version` meets
/// ([`gate_wire::blender_folder_version`], [`gate_wire::version_meets`]), taken most parts first and then in name
/// order, the first the probe keeps ([`blender_found`], its `--version` run along `path` within [`PROBE_LIMIT`]) and
/// that reports `version` itself: its `<folder>/Blender.app/Contents/MacOS/Blender`.
///
/// # Errors
///
/// Why not: the root is not an absolute path or cannot be read; the first such folder's reason when one was tried and
/// passed over, as the probe says it when it skips the folder (`named <folder> but reports <version>`, the program
/// giving no answer, a link), an install the probe keeps that reports another version than `version` itself passed
/// over as `named <folder> but reports <version>`; or, when no folder gives a reason,
/// `GATE_BLENDER_ROOT <root> holds no folder for blender <version>`.
pub(crate) fn installed_blender(root: &str, version: &str, path: Option<&str>) -> Result<PathBuf, String> {
    if !root.starts_with('/') {
        return Err(format!("{BLENDER_ROOT} {root} is not an absolute path"));
    }
    let deadline = Instant::now() + PROBE_LIMIT;
    let folders = listed(BLENDER_ROOT, Path::new(root), deadline)?;
    let mut met: Vec<(usize, &str, &PathBuf)> = folders
        .iter()
        .filter_map(|folder| {
            let name = folder.file_name()?.to_str()?;
            let named = gate_wire::blender_folder_version(name)?;
            gate_wire::version_meets(named, version).then(|| (named.split('.').count(), name, folder))
        })
        .collect();
    // A stable sort keeps the listing's name order among folders naming as many parts.
    met.sort_by_key(|(parts, _, _)| std::cmp::Reverse(*parts));
    let mut first_why = None;
    for (_, name, folder) in met {
        let why = match blender_found(folder, path, deadline) {
            Some(Probed::Version(found)) if found == version => return Ok(folder.join(BLENDER_PROGRAM)),
            Some(Probed::Version(found)) => Some(format!("named {name} but reports {found}")),
            Some(Probed::Skipped(why)) => Some(why),
            Some(Probed::Present | Probed::Missing) | None => None,
        };
        first_why = first_why.or(why);
    }
    Err(first_why.unwrap_or_else(|| format!("{BLENDER_ROOT} {root} holds no folder for blender {version}")))
}

/// The direct subfolders of the root `root`, which the variable `variable` names, the first [`ROOT_ENTRIES`] read, in
/// name order; one whose name is no Unicode and an entry that cannot be read are left out, and the listing ends at
/// `deadline`, which with nothing read at all is the probe's own `no answer` reason. Else why the root cannot be read.
fn listed(variable: &str, root: &Path, deadline: Instant) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(root).map_err(|error| format!("cannot read {variable} {}: {error}", root.display()))?;
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

/// One direct subfolder of the Godot or Blender root as its probe reads it.
#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    /// The program to run, by path.
    Install(PathBuf),
    /// A folder, or the program in it, that is a symbolic link or not what its place holds: left out for this reason.
    Skipped(String),
}

/// What `folder`, one direct subfolder of the Godot or Blender root, comes to: the program at `relative` in it to run,
/// a path relative to it such as [`GODOT_PROGRAM`]; why it is left out when the folder, a folder on the way to the
/// program or the program is a link, or one of those folders is no folder or the program no plain file; nothing when the
/// folder is no folder or holds no such program. The root above it may be a link: the builder's own setting.
fn app_install(folder: &Path, relative: &str) -> Option<Candidate> {
    let program = folder.join(relative);
    let skipped = |path: &Path, flaw: &str| Some(Candidate::Skipped(format!("cannot run {}: {} {flaw}", program.display(), path.display())));
    // The folder and every level down to the program, the folder first.
    let mut levels: Vec<&Path> = program.ancestors().take(Path::new(relative).components().count() + 1).collect();
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
/// file in plain folders, no level of it under the root a link ([`app_install`]); else `None`.
pub(crate) fn installed_godot(root: &str, version: &str) -> Option<PathBuf> {
    if !root.starts_with('/') {
        return None;
    }
    match app_install(&Path::new(root).join(version), GODOT_PROGRAM)? {
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
        || Probed::Skipped(format!("Godot --version gave no version: {}", answer_cut(&stdout))),
        Probed::Version,
    )
}

/// The first line of an install's answer `stdout`, trimmed and cut to [`GODOT_ANSWER_MAX`] characters with `…` after
/// when it is longer.
fn answer_cut(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("").trim();
    let mut cut: String = line.chars().take(GODOT_ANSWER_MAX).collect();
    if cut.len() < line.len() {
        cut.push('…');
    }
    cut
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
/// dotnet is found. Else why it could not be read, for [`gate_wire::sdks_failure`].
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

    use super::{
        BLENDER_PROGRAM, GODOT_PROGRAM, GODOT_ROOT, Plan, Roots, godot_version, installed_blender, installed_godot, listed, plan, probe_all,
    };

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
        assert_eq!(plan("blender"), Plan::Blender);
        for name in ["unreal", "netfx-sdk"] {
            assert_eq!(plan(name), Plan::Missing, "{name}");
        }
        assert_eq!(plan("Rust"), Plan::Unknown, "names are exact");
    }

    #[test]
    fn the_probes_answer_in_the_order_asked() {
        let names = ["godot", "bogus", "netfx-sdk"].map(str::to_owned);
        assert_eq!(
            probe_all(&names, None, Roots::default()),
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
        let roots = Roots { godot: root, blender: None };
        probe_all(&["godot".to_owned()], None, roots).remove(0)
    }

    /// Puts at `<base>/<folder>/Blender.app/Contents/MacOS/Blender` the runnable `script`, standing in for Blender; the
    /// program's path.
    pub(crate) fn fake_blender(base: &Path, folder: &str, script: &str) -> PathBuf {
        let program = base.join(folder).join(BLENDER_PROGRAM);
        fs::create_dir_all(program.parent().expect("the program has a folder")).expect("the install's folders are made");
        fs::write(&program, script).expect("the fake Blender is written");
        fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o700)).expect("the fake Blender is made runnable");
        program
    }

    /// What the probe of `blender` finds under `root`, with a Godot root that would answer for Godot.
    fn blender_probed(root: Option<&str>) -> Probed {
        let roots = Roots {
            godot: Some("/no/godot"),
            blender: root,
        };
        probe_all(&["blender".to_owned()], None, roots).remove(0)
    }

    #[test]
    fn the_blender_probe_lists_each_install_whose_folder_its_version_meets_and_leaves_out_links() {
        let case = Folder::new("blender-probe");
        let root = case.0.join("blender").display().to_string();
        let installs = Path::new(&root);
        let outside = case.0.join("outside");
        let says = answering("Blender 5.2.2 LTS");
        let full = fake_blender(installs, "5.2.2", &says);
        let minor = fake_blender(installs, "Blender 5.2", &says);
        fake_blender(installs, "5.3", &says);
        fake_blender(installs, "Blender", &says);
        fake_blender(&outside, "5.2.9", &says);
        symlink(outside.join("5.2.9"), installs.join("5.2.9")).expect("a link at the folder is made");

        assert_eq!(
            blender_probed(Some(&root)),
            Probed::Version("5.2.2,5.2.2".to_owned()),
            "5.2.2 and Blender 5.2 are kept; 5.3, the folder naming no version and the link are left out"
        );
        assert_eq!(installed_blender(&root, "5.2.2", None), Ok(full), "the folder naming the most parts");
        let linked = installs.join("5.2.9");
        let link = format!("cannot run {}: {} is a link", linked.join(BLENDER_PROGRAM).display(), linked.display());
        let none = format!("GATE_BLENDER_ROOT {root} holds no folder for blender 6.0.0");
        for (version, said, why) in [
            ("5.2.7", "named Blender 5.2 but reports 5.2.2", "Blender 5.2 reports another 5.2"),
            (
                "5.2.9",
                link.as_str(),
                "a link, then Blender 5.2 reporting another 5.2: the link's reason",
            ),
            ("5.3.0", "named 5.3 but reports 5.2.2", "5.3 reports 5.2.2"),
            ("6.0.0", none.as_str(), "no folder"),
        ] {
            assert_eq!(installed_blender(&root, version, None), Err(said.to_owned()), "{why}");
        }
        assert_eq!(
            installed_blender("blender", "5.2.2", None),
            Err("GATE_BLENDER_ROOT blender is not an absolute path".to_owned())
        );
        fs::remove_dir_all(installs.join("5.2.2")).expect("the full install is removed");
        assert_eq!(installed_blender(&root, "5.2.2", None), Ok(minor), "then the minor folder");

        fs::remove_dir_all(installs.join("Blender 5.2")).expect("the minor install is removed");
        assert_eq!(
            blender_probed(Some(&root)),
            Probed::Skipped(link),
            "no version: the first reason, 5.2.9's"
        );
        fs::remove_file(&linked).expect("the link is removed");
        assert_eq!(blender_probed(Some(&root)), Probed::Skipped("named 5.3 but reports 5.2.2".to_owned()));
    }

    #[test]
    fn a_run_s_blender_is_the_first_install_the_probe_keeps_past_an_empty_folder_naming_more_parts() {
        let case = Folder::new("blender-empty");
        let root = case.0.join("blender").display().to_string();
        let installs = Path::new(&root);
        let minor = fake_blender(installs, "Blender 5.2", &answering("Blender 5.2.2 LTS"));
        fs::create_dir_all(installs.join("5.2.2")).expect("an empty 5.2.2 folder is made");
        assert_eq!(installed_blender(&root, "5.2.2", None), Ok(minor));
    }

    #[test]
    fn a_run_s_blender_whose_one_matching_folder_reports_another_version_says_that_reason() {
        let case = Folder::new("blender-misreports");
        let root = case.0.join("blender").display().to_string();
        fake_blender(Path::new(&root), "5.2.2", &answering("Blender 5.2.1"));
        assert_eq!(installed_blender(&root, "5.2.2", None), Err("named 5.2.2 but reports 5.2.1".to_owned()));
    }

    #[test]
    fn a_blender_probe_with_no_root_is_missing() {
        assert_eq!(blender_probed(None), Probed::Missing, "no PATH fallback");
        let case = Folder::new("blender-roots");
        let root = case.0.join("blender").display().to_string();
        fs::create_dir_all(&root).expect("the root is made");
        assert_eq!(blender_probed(Some(&root)), Probed::Missing, "a root with no install");
        fake_blender(Path::new(&root), "5.2.2", &answering("Blender"));
        assert_eq!(
            blender_probed(Some(&root)),
            Probed::Skipped("Blender --version gave no version: Blender".to_owned())
        );
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
        let considered = listed(GODOT_ROOT, &root, Instant::now() + Duration::from_secs(3600)).expect("the root is listed");
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

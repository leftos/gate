//! A dotnet `--no-build` run here that would find no build: the worktree's nothing-back record a routed build left,
//! read as itself through any link at its name, and the projects with no `obj\project.assets.json` here; with the git
//! calls that find the worktree's root and its tracked projects.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use gate_router::{BuildSettings, DotnetTarget, PROJECT_EXTENSIONS, is_usable_host};
use gate_state::v1;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

use crate::boxside::payload::escaped;
use crate::remote::git;
use crate::run::{is_within, resolved};

/// The builder a drop line names when the nothing-back record names none that can be read and used.
const UNKNOWN_HOST: &str = "the builder";
/// The git call that lists a worktree's tracked project files, run at its root.
const PROJECT_LISTING: [&str; 6] = ["ls-files", "-z", "--", "*.csproj", "*.fsproj", "*.vbproj"];
/// The git call that gives the working folder's worktree root and its place below that root.
const WORKTREE: [&str; 3] = ["rev-parse", "--show-toplevel", "--show-prefix"];
/// The variable every git call here is given, so git says what it says in English, as [`NOT_A_REPOSITORY`] reads it.
const GIT_LOCALE: (&str, &str) = ("LC_ALL", "C");
/// The variable no git call here is given, as it would choose git's language over [`GIT_LOCALE`].
const GIT_LANGUAGE: [&str; 1] = ["LANGUAGE"];
/// The most of a nothing-back record read: a record is a few dozen bytes.
const RECORD_CAP: u64 = 4096;
/// The most of a solution or a worktree's build file read; a larger one cannot be told.
const FILE_CAP: usize = 1 << 20;
/// The files at a worktree's root whose `MSBuild` properties apply to every project in it.
const BUILD_FILES: [&str; 2] = ["Directory.Build.props", "Directory.Build.targets"];
/// The `MSBuild` properties that can move a project's `project.assets.json` out of its `obj` folder.
const MOVING_PROPERTIES: [&str; 4] = [
    "UseArtifactsOutput",
    "ArtifactsPath",
    "BaseIntermediateOutputPath",
    "MSBuildProjectExtensionsPath",
];
/// What git says of a folder in no repository.
const NOT_A_REPOSITORY: &str = "not a git repository";
/// How a git that cannot be found or started fails ([`git::Git::find`], [`git::Git::run`]).
const CANNOT_RUN_GIT: &str = "cannot run git: ";

/// A nothing-back record found: the builder it names, `None` when it names none that can be read and used as a host,
/// and the configuration, framework and runtime of the build that left it, none when it cannot be read.
pub(crate) struct NothingBack {
    pub host: Option<String>,
    pub settings: BuildSettings,
}

/// The git worktree the gate runs in: its root as git gives it, and the gate's folder below it, empty at the root.
pub(crate) struct Worktree {
    pub root: String,
    pub prefix: String,
}

/// Why the gate's git worktree could not be found.
pub(crate) enum Unfound {
    /// git cannot be found or started: the line saying so.
    NoGit(String),
    /// Anything else: the line saying what failed.
    Failed(String),
}

impl Unfound {
    /// What failed, as a line's end says it.
    pub(crate) fn text(&self) -> &str {
        match self {
            Unfound::NoGit(text) | Unfound::Failed(text) => text,
        }
    }
}

/// Why a dotnet `--no-build` run here drops the word, `None` when it keeps it: the record's reason when there is a
/// `record`, naming its builder or [`UNKNOWN_HOST`]; else the reason naming the project `without_assets` finds, which
/// is asked only when there is no record.
pub(crate) fn drop_reason(record: Option<NothingBack>, without_assets: impl FnOnce() -> Option<String>) -> Option<String> {
    match record {
        Some(record) => {
            let host = record.host.as_deref().unwrap_or(UNKNOWN_HOST);
            Some(format!("the last build ran on {host} and brought nothing back"))
        }
        None => without_assets().map(|project| format!(r"{project} has no obj\project.assets.json here")),
    }
}

/// The nothing-back record at `path`, `None` when there is none; a link or junction at its name is read as itself,
/// and at most [`RECORD_CAP`] bytes of it. A record that holds no builder of its format, or one that is no usable host,
/// names none; one not of its format sets none of the settings.
///
/// # Errors
///
/// A record that is there but cannot be opened or read, which counts as there: a record wrongly taken as there costs
/// one build here, one wrongly taken as missing a `--no-build` run that finds no build.
pub(crate) fn read_record(path: &Path) -> io::Result<Option<NothingBack>> {
    let file = match OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened?,
    };
    let mut bytes = Vec::new();
    file.take(RECORD_CAP).read_to_end(&mut bytes)?;
    // A record of another shape is still a record: only what it would say is lost.
    let Ok(record) = v1::nothing_back_record(&bytes) else {
        return Ok(Some(NothingBack {
            host: None,
            settings: BuildSettings::default(),
        }));
    };
    Ok(Some(NothingBack {
        host: Some(record.host).filter(|host| is_usable_host(host)),
        settings: BuildSettings {
            configuration: record.configuration,
            framework: record.framework,
            runtime: record.runtime,
        },
    }))
}

/// The bytes of the regular file at `path`, opened as itself: a link, a junction, a folder or a file larger than
/// [`FILE_CAP`] cannot be read.
///
/// # Errors
///
/// A file that is missing (`NotFound`), cannot be opened or read, is not a regular file or is too large.
fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(io::Error::other("it is a link, a junction or not a file"));
    }
    let mut bytes = Vec::new();
    file.take(u64::try_from(FILE_CAP + 1).unwrap_or(u64::MAX)).read_to_end(&mut bytes)?;
    if bytes.len() > FILE_CAP {
        return Err(io::Error::other("it is larger than 1 MiB"));
    }
    Ok(bytes)
}

/// Whether the worktree at `root` has a `Directory.Build.props` or `Directory.Build.targets` naming one of
/// [`MOVING_PROPERTIES`], ignoring letter case: its projects' assets files may stand anywhere.
///
/// # Errors
///
/// Such a file that is there but cannot be read ([`read_regular`]): `cannot read <path>: <error>`, the path escaped.
pub(crate) fn moves_intermediates(root: &Path) -> Result<bool, String> {
    for name in BUILD_FILES {
        let path = root.join(name);
        match read_regular(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_lowercase();
                if MOVING_PROPERTIES.iter().any(|property| text.contains(&property.to_lowercase())) {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot read {}: {error}", escaped(&path.to_string_lossy()))),
        }
    }
    Ok(false)
}

/// The first project `target` names with no `obj\project.assets.json` ([`project_without_assets`]), read against the
/// working folder, `/`-separated and escaped: the project itself, or the first its solution lists inside the worktree
/// at `root`.
///
/// # Errors
///
/// A solution that cannot be read ([`read_regular`]), or a working folder that cannot be: the line's text, escaped.
pub(crate) fn named_without_assets(target: &DotnetTarget, root: &Path) -> Result<Option<String>, String> {
    let projects = match target {
        DotnetTarget::Project(project) => vec![PathBuf::from(project)],
        DotnetTarget::Solution(solution) => {
            let bytes = read_regular(Path::new(solution)).map_err(|error| format!("cannot read {}: {error}", escaped(solution)))?;
            let folder = std::env::current_dir().map_err(|error| format!("cannot read the working directory: {error}"))?;
            let resolved_root = resolved(root);
            solution_projects(Path::new(solution), &String::from_utf8_lossy(&bytes))
                .into_iter()
                .filter(|project| is_within(&resolved(&lexical(&folder.join(project))), &resolved_root))
                .collect()
        }
    };
    Ok(projects
        .into_iter()
        .find(|project| project_without_assets(project))
        .map(|project| shown(&project.to_string_lossy())))
}

/// `path` with each `.` left out and each `..` taking the part before it away, as the system reads a full path.
fn lexical(path: &Path) -> PathBuf {
    let mut kept = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                kept.pop();
            }
            other => kept.push(other),
        }
    }
    kept
}

/// The projects the solution at `solution` lists, each joined to the folder its entries are relative to: the `.sln`'s
/// `Project(` lines, the `.slnx`'s `<Project Path=` elements, or the `.slnf`'s `solution.projects`, which are relative
/// to the solution its `solution.path` names. An entry that is no project file (a solution folder), or one with a part
/// that is not a plain name (`..`, `.`, a root, a drive or a share), is left out, as is every entry of a `.slnf` whose
/// `solution.path` has such a part.
pub(crate) fn solution_projects(solution: &Path, text: &str) -> Vec<PathBuf> {
    let folder = solution.parent().unwrap_or_else(|| Path::new(""));
    let extension = solution.extension().map(|extension| extension.to_string_lossy().to_lowercase());
    let (base, entries) = match extension.as_deref() {
        Some("slnx") => (folder.to_owned(), slnx_entries(text)),
        Some("slnf") => slnf_entries(folder, text),
        _ => (folder.to_owned(), sln_entries(text)),
    };
    entries
        .into_iter()
        .filter(|entry| is_project_file(entry) && is_plain(Path::new(entry)))
        .map(|entry| base.join(entry))
        .collect()
}

/// Whether every part of `path` is a plain name: no `..`, `.`, root, drive or share.
fn is_plain(path: &Path) -> bool {
    path.components().all(|part| matches!(part, Component::Normal(_)))
}

/// The path of each `Project("{type}") = "Name", "path", "{guid}"` line of a `.sln`.
fn sln_entries(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("Project("))
        .filter_map(|line| line.split_once('=').and_then(|(_, values)| values.split('"').nth(3)).map(str::to_owned))
        .collect()
}

/// The `Path` attribute of each `<Project ...>` element of a `.slnx`, in double or single quotes.
fn slnx_entries(text: &str) -> Vec<String> {
    text.split("<Project")
        .skip(1)
        .filter(|element| element.starts_with(char::is_whitespace))
        .filter_map(|element| attribute(element.split('>').next().unwrap_or(element), "Path"))
        .collect()
}

/// The value of the attribute `name` among an element's `attributes`, quoted with `"` or `'`.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let spelled = format!("{name}=");
    attributes.match_indices(&spelled).find_map(|(at, _)| {
        let after_space = attributes[..at].ends_with(char::is_whitespace);
        let value = &attributes[at + spelled.len()..];
        let quote = value.chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
        after_space.then(|| value[1..].split(quote).next().map(str::to_owned)).flatten()
    })
}

/// The folder a `.slnf`'s entries are relative to, the folder of the solution its `solution.path` names, and the
/// entries of its `solution.projects`; none when it is not that JSON, or when its `solution.path` has a part that is
/// not a plain name.
fn slnf_entries(folder: &Path, text: &str) -> (PathBuf, Vec<String>) {
    let Ok(filter) = serde_json::from_str::<serde_json::Value>(text) else {
        return (folder.to_owned(), Vec::new());
    };
    let solution = &filter["solution"];
    let base = match solution["path"].as_str() {
        Some(path) if !is_plain(Path::new(path)) => return (folder.to_owned(), Vec::new()),
        Some(path) => folder.join(path).parent().map_or_else(|| folder.to_owned(), Path::to_owned),
        None => folder.to_owned(),
    };
    let entries = solution["projects"]
        .as_array()
        .map(|projects| projects.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    (base, entries)
}

/// Whether `path` ends in one of [`PROJECT_EXTENSIONS`], ignoring letter case.
fn is_project_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    PROJECT_EXTENSIONS
        .iter()
        .any(|extension| lower.len() > extension.len() && lower.ends_with(extension))
}

/// The first project of a NUL-separated `git ls-files -z` listing, its paths relative to `root`, with no
/// `obj\project.assets.json` ([`project_without_assets`]), escaped: an entry with a part that is not a plain name
/// (`..`, a root or a drive) is left out.
pub(crate) fn first_without_assets(root: &Path, listing: &[u8]) -> Option<String> {
    listing
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(String::from_utf8_lossy)
        .find(|project| {
            let path = Path::new(project.as_ref());
            is_plain(path) && project_without_assets(&root.join(path))
        })
        .map(|project| escaped(&project))
}

/// Whether the project at `path`, a project file or (`--project`) its folder, is on disk and has no
/// `obj\project.assets.json` beside it: a project dotnet has not restored here. A project missing on disk has none to
/// look for.
fn project_without_assets(path: &Path) -> bool {
    let folder = if path.is_dir() {
        path
    } else if path.is_file() {
        // A project at the top of the folder it is read against has the empty folder.
        path.parent().unwrap_or_else(|| Path::new(""))
    } else {
        return false;
    };
    !folder.join("obj").join("project.assets.json").is_file()
}

/// `path` as a line shows it: each `\` written `/`, then escaped, so it cannot break or forge the line.
fn shown(path: &str) -> String {
    escaped(&path.replace('\\', "/"))
}

/// The worktree `git rev-parse --show-toplevel --show-prefix` gives for the gate's working folder, the git call limited
/// to `limit`; `None` when git says the folder is in no repository.
///
/// # Errors
///
/// A working folder that cannot be read, git that cannot be found or started ([`Unfound::NoGit`]), or git that fails
/// otherwise.
pub(crate) fn worktree(limit: Duration) -> Result<Option<Worktree>, Unfound> {
    let folder = std::env::current_dir().map_err(|error| Unfound::Failed(format!("cannot read the working directory: {error}")))?;
    let unfound = |text: String| {
        if text.starts_with(CANNOT_RUN_GIT) {
            Unfound::NoGit(text)
        } else {
            Unfound::Failed(text)
        }
    };
    let set = [(GIT_LOCALE.0, OsStr::new(GIT_LOCALE.1))];
    let output = match git::Git::find(&folder)
        .map_err(unfound)?
        .output_bytes(&in_english(&folder, &WORKTREE, limit, &set))
    {
        Ok(output) => output,
        Err(git::GitError::Failed(line)) if line.contains(NOT_A_REPOSITORY) => return Ok(None),
        Err(error) => return Err(unfound(error.into())),
    };
    let text = String::from_utf8(output).map_err(|_| Unfound::Failed("git rev-parse's output is not valid UTF-8".to_owned()))?;
    Ok(parse_worktree(&text))
}

/// The worktree `git rev-parse --show-toplevel --show-prefix` wrote: the root on its first line, the prefix on its
/// second, empty or missing at the root; `None` when the root is empty.
fn parse_worktree(text: &str) -> Option<Worktree> {
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    let root = lines.next().filter(|root| !root.is_empty())?;
    Some(Worktree {
        root: root.to_owned(),
        prefix: lines.next().unwrap_or_default().to_owned(),
    })
}

/// The tracked project files of the worktree at `root` ([`PROJECT_LISTING`]), NUL-separated, each `/`-separated and
/// relative to the root, the git call limited to `limit`.
///
/// # Errors
///
/// git that cannot be run, fails, or writes more than its reader's cap.
pub(crate) fn project_listing(root: &Path, limit: Duration) -> Result<Vec<u8>, String> {
    let set = [(GIT_LOCALE.0, OsStr::new(GIT_LOCALE.1))];
    Ok(git::Git::find(root)?.output_bytes(&in_english(root, &PROJECT_LISTING, limit, &set))?)
}

/// A call as [`git::Call::plain`] makes it, given `set` ([`GIT_LOCALE`]) and not [`GIT_LANGUAGE`], so git speaks
/// English whatever the caller's locale.
fn in_english<'a>(folder: &'a Path, args: &'a [&'a str], limit: Duration, set: &'a [(&'a str, &'a OsStr)]) -> git::Call<'a> {
    git::Call {
        set,
        unset: &GIT_LANGUAGE,
        ..git::Call::plain(folder, args, limit)
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its folders must fail loudly")]
#[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use gate_router::{BuildSettings, DotnetTarget};
    use gate_state::v1;

    use super::{
        NothingBack, drop_reason, first_without_assets, moves_intermediates, named_without_assets, parse_worktree, read_record, solution_projects,
    };

    /// A folder of this test process's own named `name`, empty.
    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-no-build-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// Makes the project file `project` under `root`, restored (with an assets file) when `restored`.
    fn project(root: &Path, project: &str, restored: bool) {
        let path = root.join(project);
        let folder = path.parent().expect("a project has a folder");
        fs::create_dir_all(folder.join("obj")).expect("the project's folders can be made");
        fs::write(&path, "<Project />").expect("the project can be written");
        if restored {
            fs::write(folder.join("obj").join("project.assets.json"), "{}").expect("an assets file can be written");
        }
    }

    /// Makes `link` a junction to the folder `target`.
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    /// A record of the builder `host` whose build set the configuration `Release`.
    fn release_record(host: &str) -> v1::NothingBackRecord {
        v1::NothingBackRecord {
            host: host.to_owned(),
            configuration: Some("Release".to_owned()),
            ..v1::NothingBackRecord::default()
        }
    }

    #[test]
    fn a_record_is_read_with_its_builder_and_settings_and_one_that_cannot_be_read_is_an_error() {
        let folder = scratch("record");
        let path = folder.join("app-1a2b3c4d.nothing-back");
        assert!(read_record(&path).expect("no record is no error").is_none(), "no record");

        v1::write_nothing_back(&path, &release_record("builder")).expect("the record can be written");
        let record = read_record(&path).expect("the record reads").expect("the record is there");
        assert_eq!(record.host.as_deref(), Some("builder"));
        assert_eq!(record.settings.configuration.as_deref(), Some("Release"));

        for (label, text) in [
            ("no host of the format", r#"{"v":1}"#),
            ("a host that is unusable", r#"{"v":1,"host":"bad host!"}"#),
        ] {
            fs::write(&path, text).expect("the record can be written");
            let record = read_record(&path).expect("the record reads").expect("the record is there");
            assert_eq!(record.host, None, "{label}");
            assert_eq!(record.settings, BuildSettings::default(), "{label}");
        }

        fs::remove_file(&path).expect("the record can be removed");
        fs::create_dir_all(&path).expect("a folder stands where the record would");
        assert!(read_record(&path).is_err(), "a record that cannot be read");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_junction_at_the_record_name_counts_as_a_record() {
        let folder = scratch("record-junction");
        let target = folder.join("elsewhere");
        fs::create_dir_all(&target).expect("the target can be made");
        let path = folder.join("app-1a2b3c4d.nothing-back");
        junction(&path, &target);

        assert!(!matches!(read_record(&path), Ok(None)), "a junction is not read as no record");
        fs::remove_dir(&path).expect("the junction can be removed");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn the_record_reason_wins_and_the_assets_are_looked_for_only_without_a_record() {
        let mut asked = false;
        let builder = Some(NothingBack {
            host: Some("builder".to_owned()),
            settings: BuildSettings::default(),
        });
        let reason = drop_reason(builder, || {
            asked = true;
            Some("app/App.csproj".to_owned())
        });
        assert_eq!(reason.as_deref(), Some("the last build ran on builder and brought nothing back"));
        assert!(!asked, "a record's reason needs no project listing");
        let unknown = NothingBack {
            host: None,
            settings: BuildSettings::default(),
        };
        assert_eq!(
            drop_reason(Some(unknown), || None).as_deref(),
            Some("the last build ran on the builder and brought nothing back")
        );
        assert_eq!(
            drop_reason(None, || Some("app/App.csproj".to_owned())).as_deref(),
            Some(r"app/App.csproj has no obj\project.assets.json here")
        );
        assert_eq!(drop_reason(None, || None), None, "nothing to drop for");
    }

    #[test]
    fn the_first_listed_project_on_disk_without_an_assets_file_is_named() {
        let root = scratch("assets");
        project(&root, "a/A.csproj", true);
        project(&root, "b/c/C.fsproj", false);
        project(&root, "Root.vbproj", false);

        assert_eq!(
            first_without_assets(&root, b"a/A.csproj\0b/c/C.fsproj\0Root.vbproj\0").as_deref(),
            Some("b/c/C.fsproj")
        );
        assert_eq!(first_without_assets(&root, b"a/A.csproj\0"), None, "every project restored");
        assert_eq!(first_without_assets(&root, b""), None, "no project");
        assert_eq!(first_without_assets(&root, b"Root.vbproj\0").as_deref(), Some("Root.vbproj"));
        assert_eq!(
            first_without_assets(&root, b"gone/Gone.csproj\0a/A.csproj\0"),
            None,
            "a project missing on disk"
        );
        assert_eq!(
            first_without_assets(&root, b"../b/c/C.fsproj\0b/../b/c/C.fsproj\0"),
            None,
            "a part that is not a name"
        );
        fs::write(root.join("obj").join("project.assets.json"), "{}").expect("an assets file can be written");
        assert_eq!(first_without_assets(&root, b"Root.vbproj\0"), None, "a project at the root");
        fs::remove_dir_all(&root).expect("the folder can be removed");
    }

    #[test]
    fn a_build_file_naming_a_property_that_moves_the_assets_skips_the_check() {
        let root = scratch("moved");
        assert_eq!(moves_intermediates(&root), Ok(false), "no build file");
        fs::write(
            root.join("Directory.Build.props"),
            "<Project><PropertyGroup><Nullable>enable</Nullable></PropertyGroup></Project>",
        )
        .expect("the props file can be written");
        assert_eq!(moves_intermediates(&root), Ok(false), "a build file moving nothing");
        for property in [
            "UseArtifactsOutput",
            "artifactspath",
            "BaseIntermediateOutputPath",
            "MSBuildProjectExtensionsPath",
        ] {
            fs::write(
                root.join("Directory.Build.targets"),
                format!("<Project><PropertyGroup><{property}>x</{property}>"),
            )
            .expect("the targets file can be written");
            assert_eq!(moves_intermediates(&root), Ok(true), "{property}");
        }
        fs::remove_dir_all(&root).expect("the folder can be removed");
    }

    #[test]
    fn a_build_file_that_is_a_junction_or_too_large_cannot_be_told() {
        let root = scratch("moved-unread");
        let target = root.join("elsewhere");
        fs::create_dir_all(&target).expect("the target can be made");
        junction(&root.join("Directory.Build.props"), &target);
        let linked = moves_intermediates(&root).expect_err("a junction is not read");
        assert!(linked.starts_with("cannot read "), "{linked}");
        fs::remove_dir(root.join("Directory.Build.props")).expect("the junction can be removed");

        fs::write(root.join("Directory.Build.targets"), " ".repeat((1 << 20) + 1)).expect("a large file can be written");
        let large = moves_intermediates(&root).expect_err("a file over 1 MiB is not read");
        assert!(large.ends_with("it is larger than 1 MiB"), "{large}");
        fs::write(root.join("Directory.Build.targets"), " ".repeat(1 << 20)).expect("a file of 1 MiB can be written");
        assert_eq!(moves_intermediates(&root), Ok(false), "a file of 1 MiB is read");
        fs::remove_dir_all(&root).expect("the folder can be removed");
    }

    #[test]
    fn a_solution_lists_its_project_files_relative_to_its_folder() {
        let sln = "Project(\"{FAE04EC0}\") = \"App\", \"app\\App.csproj\", \"{1}\"\r\nEndProject\r\n\
                   Project(\"{2150E333}\") = \"docs\", \"docs\", \"{2}\"\r\nEndProject\r\n";
        assert_eq!(
            solution_projects(Path::new("src/All.sln"), sln),
            [Path::new("src").join(r"app\App.csproj")]
        );
        let slnx = "<Solution>\n  <Folder Name=\"/x/\">\n    <Project Path=\"lib/Lib.fsproj\" />\n  </Folder>\n  \
                    <Project Path=\"app/App.csproj\" Type=\"C#\" />\n  <Properties Name=\"Path=x.csproj\" />\n  \
                    <Project Type='C#' Path='one/One.csproj' />\n  <Project DisplayPath=\"no/No.csproj\" />\n</Solution>\n";
        assert_eq!(
            solution_projects(Path::new("All.slnx"), slnx),
            [
                Path::new("").join("lib/Lib.fsproj"),
                Path::new("").join("app/App.csproj"),
                Path::new("").join("one/One.csproj")
            ]
        );
        let slnf = r#"{"solution": {"path": "src\\All.sln", "projects": ["app\\App.csproj", "docs"]}}"#;
        assert_eq!(
            solution_projects(Path::new("filters/Some.slnf"), slnf),
            [Path::new("filters").join(r"src").join(r"app\App.csproj")]
        );
        assert_eq!(solution_projects(Path::new("Bad.slnf"), "not JSON"), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_solution_entry_that_is_not_plain_names_is_skipped() {
        let entry = |path: &str| format!("Project(\"{{FAE04EC0}}\") = \"P\", \"{path}\", \"{{1}}\"\r\nEndProject\r\n");
        for path in [
            r"..\..\x.csproj",
            r"app\..\..\x.csproj",
            r".\x.csproj",
            r"C:\x.csproj",
            r"C:x.csproj",
            r"\x.csproj",
            r"\\server\share\x.csproj",
        ] {
            assert_eq!(solution_projects(Path::new("All.sln"), &entry(path)), Vec::<PathBuf>::new(), "{path}");
        }
        for path in [r"..\All.sln", r"C:\All.sln", r"\\server\share\All.sln"] {
            let slnf = serde_json::json!({ "solution": { "path": path, "projects": ["app\\App.csproj"] } }).to_string();
            assert_eq!(solution_projects(Path::new("Some.slnf"), &slnf), Vec::<PathBuf>::new(), "{path}");
        }
    }

    #[test]
    fn a_named_project_or_solution_is_checked_alone_and_only_inside_the_worktree() {
        let root = scratch("named");
        project(&root, "app/App.csproj", false);
        project(&root, "lib/Lib.csproj", true);
        let sln = |entry: &str| format!("Project(\"{{FAE04EC0}}\") = \"P\", \"{entry}\", \"{{1}}\"\r\nEndProject\r\n");
        fs::write(root.join("Lib.sln"), sln(r"lib\Lib.csproj")).expect("the solution can be written");
        fs::write(root.join("App.sln"), sln(r"app\App.csproj")).expect("the solution can be written");
        let inner = root.join("inner");
        fs::create_dir_all(&inner).expect("the inner worktree can be made");
        let named = |target: DotnetTarget, root: &Path| named_without_assets(&target, root).expect("the target can be checked");
        let at = |name: &str| shown(&root.join(name));

        assert_eq!(named(DotnetTarget::Project(at("lib/Lib.csproj")), &root), None);
        assert_eq!(named(DotnetTarget::Project(at("app")), &root), Some(slashed_at(&root, "app")));
        assert_eq!(
            named(DotnetTarget::Project(at("app/App.csproj")), &root),
            Some(slashed_at(&root, "app/App.csproj"))
        );
        assert_eq!(
            named(DotnetTarget::Project(at("gone/Gone.csproj")), &root),
            None,
            "a project missing on disk"
        );
        assert_eq!(named(DotnetTarget::Solution(at("Lib.sln")), &root), None);
        assert_eq!(
            named(DotnetTarget::Solution(at("App.sln")), &root),
            Some(slashed_at(&root, "app/App.csproj"))
        );
        assert_eq!(
            named(DotnetTarget::Solution(at("App.sln")), &inner),
            None,
            "a listed project outside the worktree is skipped"
        );
        assert!(
            named_without_assets(&DotnetTarget::Solution(at("Gone.sln")), &root).is_err(),
            "a solution that cannot be read"
        );
        fs::write(root.join("Large.sln"), " ".repeat((1 << 20) + 1)).expect("a large solution can be written");
        let large = named_without_assets(&DotnetTarget::Solution(at("Large.sln")), &root).expect_err("a solution over 1 MiB");
        assert!(large.ends_with("it is larger than 1 MiB"), "{large}");
        fs::remove_dir_all(&root).expect("the folder can be removed");
    }

    #[test]
    fn a_solution_named_through_another_spelling_of_the_worktree_is_checked() {
        let root = scratch("spelled");
        project(&root, "app/App.csproj", false);
        let sln = "Project(\"{FAE04EC0}\") = \"App\", \"app\\App.csproj\", \"{1}\"\r\nEndProject\r\n";
        fs::write(root.join("App.sln"), sln).expect("the solution can be written");
        let spelled = scratch("spelled-link");
        fs::remove_dir(&spelled).expect("the link's place can be cleared");
        junction(&spelled, &root);

        let found = named_without_assets(&DotnetTarget::Solution(shown(&spelled.join("App.sln"))), &root);

        fs::remove_dir(&spelled).expect("the junction can be removed");
        fs::remove_dir_all(&root).expect("the folder can be removed");
        assert_eq!(
            found,
            Ok(Some(slashed_at(&spelled, "app/App.csproj"))),
            "the worktree as git names it, the solution named through a link to it"
        );
    }

    #[test]
    fn a_path_a_line_names_has_its_control_characters_escaped() {
        let error = named_without_assets(&DotnetTarget::Solution("x\u{1b}[31m\u{7}.sln".to_owned()), Path::new("C:\\"))
            .expect_err("no such solution can be read");
        assert!(error.starts_with(r"cannot read x\u{1b}[31m\u{7}.sln: "), "{error}");
        assert!(!error.contains(['\u{1b}', '\u{7}']), "{error:?}");
    }

    #[test]
    fn the_worktree_is_the_root_line_and_the_prefix_line() {
        let parsed = |text: &str| parse_worktree(text).map(|worktree| (worktree.root, worktree.prefix));
        assert_eq!(parsed("C:/w\nsub/dir/\n"), Some(("C:/w".to_owned(), "sub/dir/".to_owned())));
        assert_eq!(parsed("C:/w\r\n\r\n"), Some(("C:/w".to_owned(), String::new())), "at the root");
        assert_eq!(parsed("C:/w"), Some(("C:/w".to_owned(), String::new())), "no prefix line");
        assert_eq!(parsed("\n"), None, "no root");
    }

    fn shown(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// `name` under `root`, `/`-separated as a drop line names it.
    fn slashed_at(root: &Path, name: &str) -> String {
        shown(&root.join(name)).replace('\\', "/")
    }
}

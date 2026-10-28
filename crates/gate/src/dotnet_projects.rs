//! Whether a `dotnet test` would run a Windows-only test project, which a macOS builder drops without a word: the
//! project or solution the command names, its folder argument or the gate's folder, read as text from the project
//! files with no `MSBuild` evaluation.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use gate_router::{CrossTarget, DotnetTarget, PROJECT_EXTENSIONS, dotnet_target, escaped, names_program};

use crate::no_build::{below_root, project_files, read_regular, sln_entries, slnx_entries};

/// The program a dotnet command runs.
const DOTNET: &str = "dotnet";
/// The word after which a command's words go to the program it runs.
const SEPARATOR: &str = "--";
/// The line a `.sln` starts with, after any blank lines and byte-order mark.
const SLN_HEADER: &str = "Microsoft Visual Studio Solution File";
/// The start of a `.slnx`'s root element, lower-cased.
const SLNX_ROOT: &str = "<solution";
/// The file extensions of a solution file, without the dot, compared ignoring letter case.
const SOLUTION_EXTENSIONS: [&str; 3] = ["sln", "slnx", "slnf"];
/// The texts any of which, ignoring letter case, makes a solution's member a test project.
const TEST_MARKERS: [&str; 7] = [
    "Microsoft.NET.Test.Sdk",
    "xunit",
    "MSTest",
    "NUnit",
    "TUnit",
    "IsTestProject",
    "IsTestingPlatformApplication",
];
/// The elements whose `;`-separated values are a project's target frameworks.
const FRAMEWORK_ELEMENTS: [&str; 2] = ["TargetFramework", "TargetFrameworks"];
/// The elements that turn a project's tests on or off.
const TEST_SWITCH_ELEMENTS: [&str; 2] = ["IsTestProject", "IsTestingPlatformApplication"];
/// The text a Windows target framework holds, lower-cased.
const WINDOWS_FRAMEWORK: &str = "-windows";
/// The start of a property's value in a target framework, which only `MSBuild` can tell.
const PROPERTY_START: &str = "$(";
/// A condition that holds when the build does not run on Windows, lower-cased and without white space, in either quote.
const OFF_WINDOWS_CONDITIONS: [&str; 2] = ["!$([msbuild]::isosplatform('windows'))", "!$([msbuild]::isosplatform(\"windows\"))"];
/// The value of a test switch that turns a project's tests off.
const SWITCHED_OFF: &str = "false";

/// What a `dotnet test` runs: one project, judged itself, or a solution, judged by its test projects.
enum Tested {
    Project(PathBuf),
    Solution(PathBuf),
}

/// Whether a test project the `dotnet test` command `words` runs is Windows-only ([`windows_only`]), read from the gate's
/// folder `prefix` (`/`-separated and `/`-ended, empty at the root) below the worktree root `root`: the project or
/// solution the words name ([`dotnet_target`]), else their folder argument ([`folder_argument`]), else the gate's
/// folder, each read against the gate's folder. A folder is judged by its one project file, else its one solution
/// file; a solution by each of its members ([`members`]) that is a test project ([`is_test_project`]).
///
/// # Errors
///
/// Why the projects cannot be told: a folder holding no project or solution file, or more than one of a kind, a word
/// that names a file that is neither a project nor a solution, a file that cannot be read ([`read_regular`]), a
/// solution that cannot be listed ([`members`]) or a target framework set from a property. The text is escaped.
pub(crate) fn windows_only_tests(root: &Path, prefix: &str, words: &[String]) -> Result<bool, String> {
    match tested(&below_root(root, prefix), words)? {
        Tested::Project(project) => windows_only(&read_text(&project)?).map_err(|why| cannot(&project, &why)),
        Tested::Solution(solution) => solution_windows_only(root, &solution),
    }
}

/// The one project file a cross build's `target` builds, read against the gate's folder `prefix` (`/`-separated and
/// `/`-ended, empty at the root) below the worktree root `root`: a project file is itself; a folder is its one project
/// file, else its one solution file ([`in_folder`]); `None` is the gate's folder, the only case it is read. A named
/// target is never replaced by the gate's folder. The path is the project a caller would build or restore; the cross
/// build's fact reads only whether there is one.
///
/// # Errors
///
/// [`CrossTarget::Solution`] for a solution file or a folder whose pick is one; [`CrossTarget::NotOneProject`], with
/// why, escaped, for a path that is not there or is neither, or a folder holding no project or solution file, more
/// than one of a kind, or that cannot be read. Never [`CrossTarget::OneProject`].
pub(crate) fn one_project(root: &Path, prefix: &str, target: Option<&str>) -> Result<PathBuf, CrossTarget> {
    let folder = below_root(root, prefix);
    let Some(named) = target else {
        return one_in_folder(&folder);
    };
    let path = folder.join(named);
    match fs::metadata(&path) {
        Ok(found) if found.is_dir() => one_in_folder(&path),
        Ok(_) if is_project_file(named) => Ok(path),
        Ok(_) if is_solution_file(&path) => Err(CrossTarget::Solution),
        Ok(_) => Err(CrossTarget::NotOneProject(format!("{} is no project file", shown(&path)))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(CrossTarget::NotOneProject(format!("{} does not exist", shown(&path)))),
        Err(error) => Err(CrossTarget::NotOneProject(cannot(&path, &error.to_string()))),
    }
}

/// The one project file in `folder` when it holds no solution file, else [`CrossTarget::Solution`] for its one solution
/// file, else why not ([`in_folder`]). A folder holding a solution beside its one project is no one project: `MSBuild`
/// builds the solution when their names match and refuses the pair (MSB1011) when they do not.
fn one_in_folder(folder: &Path) -> Result<PathBuf, CrossTarget> {
    match in_folder(folder) {
        Ok(Tested::Project(project)) => match solution_files(folder) {
            Ok(solutions) if solutions.is_empty() => Ok(project),
            Ok(_) => Err(CrossTarget::NotOneProject(format!(
                "{} holds a project and a solution file",
                shown(folder)
            ))),
            Err(error) => Err(CrossTarget::NotOneProject(cannot(folder, &error.to_string()))),
        },
        Ok(Tested::Solution(_)) => Err(CrossTarget::Solution),
        Err(why) => Err(CrossTarget::NotOneProject(why)),
    }
}

/// Whether `path` ends in a solution extension ([`SOLUTION_EXTENSIONS`]), ignoring letter case.
fn is_solution_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| SOLUTION_EXTENSIONS.iter().any(|known| extension.eq_ignore_ascii_case(known)))
}

/// What the words run, read against `folder`.
fn tested(folder: &Path, words: &[String]) -> Result<Tested, String> {
    match dotnet_target(words) {
        Some(DotnetTarget::Solution(solution)) => Ok(Tested::Solution(folder.join(solution))),
        Some(DotnetTarget::Project(project)) if project.is_empty() => Err("the command names an empty project".to_owned()),
        Some(DotnetTarget::Project(project)) => {
            let path = folder.join(project);
            if path.is_dir() { in_folder(&path) } else { Ok(Tested::Project(path)) }
        }
        None => in_folder(&folder_argument(folder, words).unwrap_or_else(|| folder.to_owned())),
    }
}

/// The first of the dotnet command's words after its verb ([`after_verb`]) not starting with `-` or `/` that names,
/// read against `folder`, a folder holding a project or a solution file ([`holds_projects`]); every other word, a file
/// or a folder holding neither or a word naming nothing there, is an option's value, and skipped. A project or a
/// solution file the words name is [`dotnet_target`]'s.
fn folder_argument(folder: &Path, words: &[String]) -> Option<PathBuf> {
    after_verb(words)
        .iter()
        .filter(|word| !word.is_empty() && !word.starts_with(['-', '/']))
        .map(|word| folder.join(word))
        .find(|path| path.is_dir() && holds_projects(path))
}

/// Whether the folder holds a project or a solution file ([`project_files`], [`solution_files`]); `true` too when it
/// cannot be listed, so that [`in_folder`] says why.
fn holds_projects(folder: &Path) -> bool {
    let holds = |files: std::io::Result<Vec<PathBuf>>| !matches!(files, Ok(files) if files.is_empty());
    holds(project_files(folder)) || holds(solution_files(folder))
}

/// The dotnet command's own words after its verb: from its dotnet word, the first non-empty word or the word just after
/// a wrapping script's first `--`, up to the next `--`, the verb being the first non-empty word after the dotnet word.
/// None for words that hold no dotnet command or no verb.
fn after_verb(words: &[String]) -> &[String] {
    let Some(first) = words.iter().position(|word| !word.is_empty()) else {
        return &[];
    };
    let start = if names_program(&words[first], DOTNET) {
        first
    } else {
        match words[first..].iter().position(|word| word == SEPARATOR) {
            Some(at) if words.get(first + at + 1).is_some_and(|word| names_program(word, DOTNET)) => first + at + 1,
            _ => return &[],
        }
    };
    let rest = &words[start + 1..];
    let own = &rest[..rest.iter().position(|word| word == SEPARATOR).unwrap_or(rest.len())];
    own.iter().position(|word| !word.is_empty()).map_or(&[], |verb| &own[verb + 1..])
}

/// The one project file in `folder`, else its one solution file.
fn in_folder(folder: &Path) -> Result<Tested, String> {
    let projects = project_files(folder).map_err(|error| cannot(folder, &error.to_string()))?;
    match projects.as_slice() {
        [one] => return Ok(Tested::Project(one.clone())),
        [] => {}
        _ => return Err(format!("{} holds more than one project file", shown(folder))),
    }
    let solutions = solution_files(folder).map_err(|error| cannot(folder, &error.to_string()))?;
    match solutions.as_slice() {
        [one] => Ok(Tested::Solution(one.clone())),
        [] => Err(format!("{} holds no project or solution file", shown(folder))),
        _ => Err(format!("{} holds more than one solution file", shown(folder))),
    }
}

/// The files in `folder` whose name ends in a solution extension ([`SOLUTION_EXTENSIONS`]), ignoring letter case.
fn solution_files(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(folder)? {
        let path = entry?.path();
        if is_solution_file(&path) && path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

/// Whether a member of the solution at `solution` in the worktree at `root` ([`members`]) is a test project
/// ([`is_test_project`]) that is Windows-only ([`windows_only`]).
fn solution_windows_only(root: &Path, solution: &Path) -> Result<bool, String> {
    let text = read_text(solution)?;
    for member in members(root, solution, &text).map_err(|why| cannot(solution, &why))? {
        let text = read_text(&member)?;
        if is_test_project(&text) && windows_only(&text).map_err(|why| cannot(&member, &why))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The project files the solution at `solution`, of text `text`, lists, each resolved against the folder its entries
/// are relative to: the `.sln`'s `Project(` lines and the `.slnx`'s `<Project Path=` elements against the solution's
/// folder, the `.slnf`'s `solution.projects` against the folder of the solution its `solution.path` names. An entry
/// that is no project file (a solution folder) is left out; `..` may climb, inside the worktree at `root`.
///
/// # Errors
///
/// A solution that is not of its format, or a path that leads outside the worktree.
fn members(root: &Path, solution: &Path, text: &str) -> Result<Vec<PathBuf>, String> {
    let folder = solution.parent().unwrap_or_else(|| Path::new(""));
    let extension = solution.extension().map(|extension| extension.to_string_lossy().to_lowercase());
    let (base, entries) = match extension.as_deref() {
        Some("slnx") if text.to_lowercase().contains(SLNX_ROOT) => (folder.to_owned(), slnx_entries(text)),
        Some("slnf") => filter_entries(root, folder, text)?,
        Some("sln") if text.trim_start_matches(['\u{feff}', '\r', '\n', ' ', '\t']).starts_with(SLN_HEADER) => (folder.to_owned(), sln_entries(text)),
        _ => return Err("it is not a solution of its format".to_owned()),
    };
    entries
        .iter()
        .filter(|entry| is_project_file(entry))
        .map(|entry| inside(root, &base.join(entry)))
        .collect()
}

/// The folder a `.slnf`'s entries are relative to, the folder of the solution its `solution.path` names read against
/// the filter's `folder`, inside the worktree at `root`, and the entries of its `solution.projects`.
fn filter_entries(root: &Path, folder: &Path, text: &str) -> Result<(PathBuf, Vec<String>), String> {
    let filter: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("it is not a solution filter: {error}"))?;
    let named = filter["solution"]["path"].as_str().ok_or("it names no solution.path")?;
    let projects = filter["solution"]["projects"].as_array().ok_or("it has no solution.projects list")?;
    let entries = projects
        .iter()
        .map(|project| project.as_str().map(str::to_owned).ok_or("an entry of its solution.projects is not text"))
        .collect::<Result<Vec<_>, _>>()?;
    let named = inside(root, &folder.join(named))?;
    Ok((named.parent().unwrap_or(&named).to_owned(), entries))
}

/// Whether `entry` ends in a project extension ([`PROJECT_EXTENSIONS`]), ignoring letter case.
fn is_project_file(entry: &str) -> bool {
    let entry = entry.to_lowercase();
    PROJECT_EXTENSIONS.iter().any(|extension| entry.ends_with(extension))
}

/// `path` with each `.` left out and each `..` taking the part before it away, when that stays inside `root`.
fn inside(root: &Path, path: &Path) -> Result<PathBuf, String> {
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
    if kept.starts_with(root) {
        Ok(kept)
    } else {
        Err(format!("{} is outside the worktree", shown(path)))
    }
}

/// Whether a project file's text names any of [`TEST_MARKERS`] as a whole word ([`has_word`]), ignoring letter case.
fn is_test_project(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    TEST_MARKERS.iter().any(|marker| has_word(&text, &marker.to_ascii_lowercase()))
}

/// Whether `text` holds `word` with no ASCII letter, digit, `_` or `.` just before it and no ASCII letter, digit or `_`
/// just after it: a package's name may go on with `.` (`xunit.v3`), but never runs into a longer name.
fn has_word(text: &str, word: &str) -> bool {
    let joins = |letter: char| letter.is_ascii_alphanumeric() || letter == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(|letter| joins(letter) || letter == '.') && !after.is_some_and(joins)
    })
}

/// Whether a project file's text, its comments left out, makes it Windows-only: every value of its own
/// [`FRAMEWORK_ELEMENTS`], under any condition, is a Windows target framework ([`WINDOWS_FRAMEWORK`]), there being at
/// least one; or one of its [`TEST_SWITCH_ELEMENTS`] turns tests off off Windows ([`has_windows_test_switch`]).
///
/// # Errors
///
/// A target framework set from a property ([`PROPERTY_START`]), which only `MSBuild` can tell.
fn windows_only(text: &str) -> Result<bool, String> {
    let text = without_comments(text);
    let frameworks = framework_values(&text)?;
    let by_framework = !frameworks.is_empty() && frameworks.iter().all(|framework| framework.to_lowercase().contains(WINDOWS_FRAMEWORK));
    Ok(by_framework || has_windows_test_switch(&text))
}

/// Each value of the text's [`FRAMEWORK_ELEMENTS`], split at `;` and trimmed, empty ones left out.
fn framework_values(text: &str) -> Result<Vec<&str>, String> {
    let mut values = Vec::new();
    for (_, value) in elements(text, &FRAMEWORK_ELEMENTS) {
        for framework in value.split(';').map(str::trim).filter(|framework| !framework.is_empty()) {
            if framework.contains(PROPERTY_START) {
                return Err(format!("its target framework is set from a property: {}", escaped(framework)));
            }
            values.push(framework);
        }
    }
    Ok(values)
}

/// Whether one of the text's [`TEST_SWITCH_ELEMENTS`] turns tests off off Windows: its value, trimmed, is
/// [`SWITCHED_OFF`] in any letter case, and its `Condition`, once its `&quot;` and `&apos;` are read as quotes, its white
/// space left out and its letters lower-cased, holds one of [`OFF_WINDOWS_CONDITIONS`].
fn has_windows_test_switch(text: &str) -> bool {
    elements(text, &TEST_SWITCH_ELEMENTS).iter().any(|(attributes, value)| {
        value.trim().eq_ignore_ascii_case(SWITCHED_OFF)
            && attribute(attributes, "Condition").is_some_and(|condition| {
                let folded: String = condition
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .chars()
                    .filter(|letter| !letter.is_whitespace())
                    .collect::<String>()
                    .to_lowercase();
                OFF_WINDOWS_CONDITIONS.iter().any(|asks| folded.contains(asks))
            })
    })
}

/// Each element of the text named one of `names`, ignoring letter case: the attributes of its start tag and its text up
/// to the next `<`, empty for an element its start tag closes.
fn elements<'a>(text: &'a str, names: &[&str]) -> Vec<(&'a str, &'a str)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        let after = &rest[at + 1..];
        let name_end = after
            .find(|letter: char| letter.is_whitespace() || matches!(letter, '>' | '/'))
            .unwrap_or(after.len());
        let Some(tag_end) = tag_end(&after[name_end..]) else {
            break;
        };
        let attributes = &after[name_end..name_end + tag_end];
        let body = &after[name_end + tag_end + 1..];
        if names.iter().any(|name| after[..name_end].eq_ignore_ascii_case(name)) {
            let closed = attributes.trim_end().ends_with('/');
            let value = if closed { "" } else { &body[..body.find('<').unwrap_or(body.len())] };
            found.push((attributes, value));
        }
        rest = body;
    }
    found
}

/// Where a start tag's attributes `tag` end: its first `>` outside a quoted value.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (at, letter) in tag.char_indices() {
        match quote {
            Some(open) if letter == open => quote = None,
            None if matches!(letter, '"' | '\'') => quote = Some(letter),
            None if letter == '>' => return Some(at),
            Some(_) | None => {}
        }
    }
    None
}

/// The value of the attribute `name`, ignoring letter case, among a start tag's `attributes`, quoted with `"` or `'`.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let folded = attributes.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    folded.match_indices(&name).find_map(|(at, _)| {
        if !folded[..at].ends_with(char::is_whitespace) {
            return None;
        }
        let value = attributes[at + name.len()..].trim_start().strip_prefix('=')?.trim_start();
        let quote = value.chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
        value[1..].split(quote).next().map(str::to_owned)
    })
}

/// The text with each `<!-- ... -->` comment left out; an unclosed one runs to the end.
fn without_comments(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("<!--") {
        kept.push_str(&rest[..at]);
        match rest[at + 4..].find("-->") {
            Some(end) => rest = &rest[at + 4 + end + 3..],
            None => return kept,
        }
    }
    kept.push_str(rest);
    kept
}

/// The text of the file at `path` ([`read_regular`]).
fn read_text(path: &Path) -> Result<String, String> {
    read_regular(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| cannot(path, &error.to_string()))
}

/// `cannot read <path>: <why>`, the path escaped.
fn cannot(path: &Path, why: &str) -> String {
    format!("cannot read {}: {why}", shown(path))
}

/// The path, escaped.
fn shown(path: &Path) -> String {
    escaped(&path.to_string_lossy())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::path::{Path, PathBuf};

    use gate_router::CrossTarget;

    use super::{is_test_project, one_project, windows_only, windows_only_tests};
    use crate::no_build::FILE_CAP;

    /// The yaat-server form of a test project the tests of turn off off Windows.
    const SWITCHED_OFF: &str = "<Project><PropertyGroup><TargetFramework>net10.0</TargetFramework>\
        <IsTestProject Condition=\"!$([MSBuild]::IsOSPlatform('Windows'))\">false</IsTestProject></PropertyGroup></Project>";
    /// A cross-platform test project.
    const CROSS_TEST: &str = "<Project><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>\
        <ItemGroup><PackageReference Include=\"xunit\" /></ItemGroup></Project>";
    /// A Windows-only test project.
    const WINDOWS_TEST: &str = "<Project><PropertyGroup><TargetFramework>net10.0-windows</TargetFramework></PropertyGroup>\
        <ItemGroup><PackageReference Include=\"Microsoft.NET.Test.Sdk\" /></ItemGroup></Project>";
    /// A Windows-only project that is no test project.
    const WINDOWS_APP: &str = "<Project><PropertyGroup><TargetFramework>net10.0-windows</TargetFramework></PropertyGroup></Project>";

    /// A folder of this test process's own named `name`, empty.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-dotnet-projects-{name}-{}", std::process::id()));
        if folder.exists() {
            fs::remove_dir_all(&folder).expect("a folder left by an earlier run can be removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// Writes `text` as the file `path` below `root`, making its folders.
    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the folders can be made");
        fs::write(path, text).expect("the file can be written");
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    /// The verdict on a project file holding the one property group `properties`.
    fn judged(properties: &str) -> Result<bool, String> {
        windows_only(&format!("<Project><PropertyGroup>{properties}</PropertyGroup></Project>"))
    }

    #[test]
    fn a_windows_target_framework_makes_a_project_windows_only() {
        assert_eq!(judged("<TargetFramework>net10.0-windows</TargetFramework>"), Ok(true));
        assert_eq!(judged("<targetframework> net8.0-windows10.0.19041.0 </targetframework>"), Ok(true));
        assert_eq!(judged("<TargetFrameworks>net8.0-Windows;net10.0-windows;</TargetFrameworks>"), Ok(true));
    }

    #[test]
    fn a_target_framework_that_is_not_windows_keeps_a_project_cross_platform() {
        assert_eq!(judged("<TargetFramework>net10.0</TargetFramework>"), Ok(false));
        assert_eq!(judged("<TargetFrameworks>net10.0;net10.0-windows</TargetFrameworks>"), Ok(false));
        assert_eq!(
            judged("<TargetFramework>net10.0-windows</TargetFramework><TargetFramework Condition=\"'$(X)' == 'y'\">net10.0</TargetFramework>"),
            Ok(false),
            "every element counts, whatever its condition"
        );
        assert_eq!(judged("<OutputType>Exe</OutputType>"), Ok(false), "no framework of its own");
        assert_eq!(
            judged("<!-- <TargetFramework>net10.0</TargetFramework> --><TargetFramework>net10.0-windows</TargetFramework>"),
            Ok(true),
            "a comment is no framework"
        );
    }

    #[test]
    fn a_target_framework_set_from_a_property_cannot_be_told() {
        let judged = judged("<TargetFramework>$(Frameworks)</TargetFramework>");
        assert!(judged.as_ref().is_err_and(|why| why.contains("$(Frameworks)")), "{judged:?}");
    }

    #[test]
    fn a_test_switch_conditioned_on_windows_makes_a_project_windows_only() {
        assert_eq!(windows_only(SWITCHED_OFF), Ok(true));
        assert_eq!(
            judged("<IsTestingPlatformApplication condition='!$([MSBuild]::IsOSPlatform(\"windows\"))'>false</IsTestingPlatformApplication>"),
            Ok(true)
        );
    }

    #[test]
    fn a_test_switch_without_a_windows_platform_condition_keeps_a_project_cross_platform() {
        assert_eq!(judged("<IsTestProject>true</IsTestProject>"), Ok(false));
        assert_eq!(
            judged("<IsTestProject Condition=\"'$(OS)' != 'Windows_NT'\">false</IsTestProject>"),
            Ok(false)
        );
    }

    #[test]
    fn a_folder_with_one_project_is_judged_by_it_named_or_as_the_gate_s_folder() {
        let root = scratch("one-project");
        write(&root, "tests/Win/Win.csproj", WINDOWS_TEST);
        write(&root, "tests/Cross/Cross.csproj", CROSS_TEST);
        assert_eq!(windows_only_tests(&root, "", &words(&["dotnet", "test", "tests/Win"])), Ok(true));
        assert_eq!(
            windows_only_tests(&root, "", &words(&["dotnet", "test", "-c", "Release", "tests/Cross"])),
            Ok(false)
        );
        assert_eq!(windows_only_tests(&root, "tests/Win/", &words(&["dotnet", "test"])), Ok(true));
        assert_eq!(
            windows_only_tests(&root, "tests/", &words(&["dotnet", "test", "Cross/Cross.csproj"])),
            Ok(false)
        );
    }

    #[test]
    fn a_folder_with_one_solution_is_judged_by_its_windows_only_test_member() {
        let root = scratch("one-solution");
        write(&root, "tests/Win/Win.csproj", WINDOWS_TEST);
        write(&root, "tests/Cross/Cross.csproj", CROSS_TEST);
        let members = "<Solution><Project Path=\"tests/Cross/Cross.csproj\" /><Project Path=\"tests/Win/Win.csproj\" /></Solution>";
        write(&root, "All.slnx", members);
        assert_eq!(windows_only_tests(&root, "", &words(&["dotnet", "test"])), Ok(true));
    }

    #[test]
    fn a_solution_whose_only_windows_only_member_is_no_test_project_is_not_windows_only() {
        let root = scratch("app-solution");
        write(&root, "src/App/App.csproj", WINDOWS_APP);
        write(&root, "tests/Cross/Cross.csproj", CROSS_TEST);
        let members = "<Solution><Project Path=\"src/App/App.csproj\" /><Project Path=\"tests/Cross/Cross.csproj\" /></Solution>";
        write(&root, "All.slnx", members);
        assert_eq!(windows_only_tests(&root, "", &words(&["dotnet", "test", "All.slnx"])), Ok(false));
    }

    #[test]
    fn a_folder_with_no_project_or_two_cannot_be_told() {
        let root = scratch("no-or-two");
        write(&root, "empty/a.txt", "a");
        write(&root, "two/A.csproj", CROSS_TEST);
        write(&root, "two/B.csproj", CROSS_TEST);
        for folder in ["empty", "two"] {
            let judged = windows_only_tests(&root, "", &words(&["dotnet", "test", folder]));
            assert!(judged.is_err(), "{folder}: {judged:?}");
        }
    }

    /// Whether `resolved` is no one project, its why holding `why`.
    fn not_one(resolved: &Result<PathBuf, CrossTarget>, why: &str) -> bool {
        matches!(resolved, Err(CrossTarget::NotOneProject(said)) if said.contains(why))
    }

    #[test]
    fn one_project_is_a_named_project_itself_or_a_folder_s_one_project_file() {
        let root = scratch("one-project-resolved");
        write(&root, "tests/A/A.csproj", CROSS_TEST);
        let project = root.join("tests").join("A").join("A.csproj");
        assert_eq!(one_project(&root, "", Some("tests/A")), Ok(project.clone()));
        assert_eq!(one_project(&root, "tests/A/", None), Ok(project.clone()));
        assert_eq!(one_project(&root, "tests/", Some("A")), Ok(project));
        assert_eq!(one_project(&root, "", Some("tests/A/A.csproj")), Ok(root.join("tests/A/A.csproj")));
    }

    #[test]
    fn one_project_of_a_solution_named_or_held_alone_is_a_solution() {
        let root = scratch("one-project-solution");
        write(&root, "All.slnx", "<Solution><Project Path=\"tests/A/A.csproj\" /></Solution>");
        write(&root, "tests/A/A.csproj", CROSS_TEST);
        assert_eq!(one_project(&root, "", None), Err(CrossTarget::Solution));
        assert_eq!(one_project(&root, "", Some("All.slnx")), Err(CrossTarget::Solution));
    }

    #[test]
    fn one_project_of_a_folder_with_two_projects_or_none_is_not_one_saying_why() {
        let root = scratch("one-project-unresolved");
        write(&root, "empty/a.txt", "a");
        write(&root, "two/A.csproj", CROSS_TEST);
        write(&root, "two/B.csproj", CROSS_TEST);
        for (prefix, why) in [
            ("two/", "holds more than one project file"),
            ("empty/", "holds no project or solution file"),
        ] {
            let resolved = one_project(&root, prefix, None);
            assert!(not_one(&resolved, why), "{prefix}: {resolved:?}");
        }
    }

    #[test]
    fn a_named_folder_is_resolved_itself_and_never_the_gate_s_folder_in_its_place() {
        let root = scratch("one-project-named-empty");
        write(&root, "App.csproj", CROSS_TEST);
        write(&root, "src/Empty/a.txt", "a");
        let resolved = one_project(&root, "", Some("src/Empty"));
        assert!(not_one(&resolved, "holds no project or solution file"), "{resolved:?}");

        let solution = scratch("one-project-named-empty-solution");
        write(&solution, "All.slnx", "<Solution />");
        write(&solution, "src/Empty/a.txt", "a");
        let resolved = one_project(&solution, "", Some("src/Empty"));
        assert!(
            not_one(&resolved, "holds no project or solution file"),
            "never the root's solution: {resolved:?}"
        );
    }

    #[test]
    fn a_folder_holding_a_project_and_a_solution_is_not_one_project() {
        let root = scratch("one-project-and-solution");
        write(&root, "App.csproj", CROSS_TEST);
        write(&root, "App.sln", &sln(&["App.csproj"]));
        write(&root, "src/App/App.csproj", CROSS_TEST);
        write(&root, "src/App/Other.slnx", "<Solution />");
        for target in [None, Some("src/App")] {
            let resolved = one_project(&root, "", target);
            assert!(not_one(&resolved, "holds a project and a solution file"), "{target:?}: {resolved:?}");
        }
    }

    #[test]
    fn a_named_path_that_is_not_there_or_is_no_project_is_not_one_project() {
        let root = scratch("one-project-missing");
        write(&root, "App.csproj", CROSS_TEST);
        write(&root, "notes.txt", "a");
        for named in ["src/Missing", "gone/Gone.csproj"] {
            let resolved = one_project(&root, "", Some(named));
            assert!(not_one(&resolved, "does not exist"), "{named}: {resolved:?}");
        }
        let resolved = one_project(&root, "", Some("notes.txt"));
        assert!(not_one(&resolved, "is no project file"), "{resolved:?}");
    }

    #[test]
    fn a_missing_or_over_cap_project_cannot_be_told() {
        let root = scratch("unreadable");
        write(&root, "big/Big.csproj", &" ".repeat(FILE_CAP + 1));
        for project in ["gone/Gone.csproj", "big/Big.csproj"] {
            let judged = windows_only_tests(&root, "", &words(&["dotnet", "test", project]));
            assert!(judged.as_ref().is_err_and(|why| why.starts_with("cannot read ")), "{project}: {judged:?}");
        }
    }

    #[test]
    fn words_naming_nothing_on_disk_are_option_values_and_skipped() {
        let cross = scratch("option-values-cross");
        write(&cross, "App.csproj", CROSS_TEST);
        assert_eq!(
            windows_only_tests(&cross, "", &words(&["dotnet", "test", "--blame-hang-timeout", "5m"])),
            Ok(false)
        );
        assert_eq!(
            windows_only_tests(&cross, "", &words(&["dotnet", "test", "--filter-class", "X"])),
            Ok(false)
        );

        let windows = scratch("option-values-windows");
        write(&windows, "App.csproj", WINDOWS_TEST);
        write(&windows, "tests/Cross/Cross.csproj", CROSS_TEST);
        assert_eq!(
            windows_only_tests(&windows, "", &words(&["dotnet", "test", "-e", "FOO=1", "tests/Cross"])),
            Ok(false),
            "the folder is judged, not the gate's"
        );
    }

    #[test]
    fn an_existing_file_or_a_folder_without_projects_is_an_option_value_and_skipped() {
        let root = scratch("option-paths");
        write(&root, "App.csproj", CROSS_TEST);
        write(&root, "ci.runsettings", "<RunSettings />");
        write(&root, "TestResults/a.trx", "<TestRun />");
        write(&root, "tests/Win/Win.csproj", WINDOWS_TEST);
        fs::create_dir_all(root.join("out")).expect("the empty folder can be made");
        assert_eq!(
            windows_only_tests(&root, "", &words(&["dotnet", "test", "--settings", "ci.runsettings"])),
            Ok(false)
        );
        assert_eq!(
            windows_only_tests(&root, "", &words(&["dotnet", "test", "--results-directory", "TestResults"])),
            Ok(false)
        );
        assert_eq!(
            windows_only_tests(&root, "", &words(&["dotnet", "test", "--results-directory", "out", "tests/Win"])),
            Ok(true),
            "the project's folder is judged"
        );
    }

    /// A `.sln` listing the projects `paths`, as written.
    fn sln(paths: &[&str]) -> String {
        let mut text = "Microsoft Visual Studio Solution File, Format Version 12.00\n".to_owned();
        for (at, path) in paths.iter().enumerate() {
            write!(
                text,
                "Project(\"{{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}}\") = \"P{at}\", \"{path}\", \"{{{at}}}\"\nEndProject\n"
            )
            .expect("a String takes any text");
        }
        text
    }

    #[test]
    fn a_solution_member_is_read_against_the_solution_s_folder_and_inside_the_root() {
        let root = scratch("climbing-member");
        write(&root, "tests/Win/Win.csproj", WINDOWS_TEST);
        write(&root, "src/App.sln", &sln(&[r"..\tests\Win\Win.csproj"]));
        assert_eq!(windows_only_tests(&root, "", &words(&["dotnet", "test", "src/App.sln"])), Ok(true));

        write(&root, "src/Out.sln", &sln(&[r"..\..\elsewhere\Out.csproj"]));
        write(&root, "Bad.slnf", "not json");
        for solution in ["src/Out.sln", "Bad.slnf"] {
            let judged = windows_only_tests(&root, "", &words(&["dotnet", "test", solution]));
            assert!(judged.is_err(), "{solution}: {judged:?}");
        }
    }

    #[test]
    fn a_test_marker_counts_only_as_a_whole_word() {
        for named in ["RunUnitTests", "ProductUnit", "SystemsTests"] {
            let text = format!("<Project><PropertyGroup><AssemblyName>{named}</AssemblyName></PropertyGroup></Project>");
            assert!(!is_test_project(&text), "{named}");
        }
        assert!(is_test_project("<PackageReference Include=\"xunit.v3\" />"));
        assert!(is_test_project("<PackageReference Include=\"NUnit\" Version=\"4.0.0\" />"));
    }

    #[test]
    fn a_test_switch_counts_only_when_it_turns_tests_off_off_windows() {
        assert_eq!(
            judged("<IsTestProject Condition=\"$([MSBuild]::IsOSPlatform('Windows'))\">false</IsTestProject>"),
            Ok(false),
            "on Windows, not off it"
        );
        assert_eq!(
            judged("<IsTestProject Condition=\"!$([MSBuild]::IsOSPlatform('Windows'))\">true</IsTestProject>"),
            Ok(false),
            "it turns tests on"
        );
        assert_eq!(
            judged("<IsTestProject Condition=\"! $([MSBuild]::IsOSPlatform('Windows'))\"> False </IsTestProject>"),
            Ok(true)
        );
    }
}

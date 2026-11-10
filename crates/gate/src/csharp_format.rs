//! `gate csharp format`: one C# repo's pre-commit format hook as a subcommand. A project formatted without its restored
//! `obj` loads no source generators, so `dotnet format` adds `readonly` to an `[ObservableProperty]` field and the next
//! build breaks with CS0191; each touched project is therefore restored first unless its `project.assets.json` is
//! already there. The files are formatted until they stop changing, a changed file is staged only when every one of the
//! hook's files was staged before it ran, and the commit is refused on a dead store.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use gate_args::CsharpFormatRequest;

use crate::answer::FAILED_STATUS;
use crate::console::Console;
use crate::environment::Variables;
use crate::run::find_program;

/// The most format passes one run makes: IDE0008's fixer takes one of its sub-rules per pass (dotnet/roslyn#80349).
const MOST_PASSES: u32 = 3;

/// Runs the command's child programs: the one seam the unit tests script.
trait Tools {
    /// Runs `program` with `words`, its output inherited, and returns its exit status.
    fn run(&mut self, program: &str, words: &[OsString]) -> io::Result<u32>;

    /// Runs `program` with `words`, its output discarded, and returns its exit status.
    fn check(&mut self, program: &str, words: &[OsString]) -> io::Result<u32>;
}

/// The real programs: the format passes inherit their output, the index checks discard theirs.
struct Programs;

impl Tools for Programs {
    fn run(&mut self, program: &str, words: &[OsString]) -> io::Result<u32> {
        spawn_with(program, words, |_| {})
    }

    fn check(&mut self, program: &str, words: &[OsString]) -> io::Result<u32> {
        spawn_with(program, words, |command| {
            command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        })
    }
}

/// Runs `program` with `words`, `set_up` setting up the command's standard streams, and returns its exit status.
fn spawn_with(program: &str, words: &[OsString], set_up: impl FnOnce(&mut Command)) -> io::Result<u32> {
    let folder = std::env::current_dir()?;
    let found = find_program(program, &Variables::from_process(), &folder).map_err(io::Error::other)?;
    let mut command = Command::new(found);
    command.args(words);
    set_up(&mut command);
    let status = gate_win32::spawn(&mut command)?.wait()?;
    Ok(status.code().map_or(u32::from(FAILED_STATUS), i32::cast_unsigned))
}

/// Runs `gate csharp format`, and returns the exit status.
pub(crate) fn format(request: &CsharpFormatRequest, console: &mut Console) -> u32 {
    format_with(&request.files, &mut Programs, console)
}

/// The command's steps, each child run through `tools`.
fn format_with(files: &[OsString], tools: &mut impl Tools, console: &mut Console) -> u32 {
    if files.is_empty() {
        return 0;
    }
    let projects = match projects(files, console) {
        Ok(projects) => projects,
        Err(status) => return status,
    };
    let status = restore(&projects, tools, console);
    if status != 0 {
        return status;
    }
    let staged = match staged_before(files, tools, console) {
        Ok(staged) => staged,
        Err(status) => return status,
    };
    let changed = match format_passes(files, tools, console) {
        Ok(changed) => changed,
        Err(status) => return status,
    };
    let status = stage(&changed, staged, tools, console);
    if status != 0 {
        return status;
    }
    verify(files, tools, console)
}

/// The projects `files` belong to, sorted without repeats: each file's nearest project folder, with every project in
/// it.
fn projects(files: &[OsString], console: &mut Console) -> Result<BTreeSet<PathBuf>, u32> {
    let mut projects = BTreeSet::new();
    for file in files {
        add_projects(&mut projects, file, console)?;
    }
    Ok(projects)
}

/// Adds every project of `file`'s nearest project folder, walking up from its own folder to the drive root; a folder
/// that is not there carries on upward, a folder that cannot be read fails, and a file with no project above it gets a
/// notice and adds none.
fn add_projects(projects: &mut BTreeSet<PathBuf>, file: &OsString, console: &mut Console) -> Result<(), u32> {
    let full = std::path::absolute(file).map_err(|error| {
        console.err(&format!("gate: csharp format: cannot read {}: {error}", Path::new(file).display()));
        u32::from(FAILED_STATUS)
    })?;
    let mut folder = full.parent();
    while let Some(current) = folder {
        match csprojs(current) {
            Ok(found) if !found.is_empty() => {
                projects.extend(found);
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                console.err(&format!("gate: csharp format: cannot read {}: {error}", current.display()));
                return Err(u32::from(FAILED_STATUS));
            }
        }
        folder = current.parent();
    }
    console.err(&format!(
        "gate: csharp format: no project above {}; it gets no restore",
        Path::new(file).display()
    ));
    Ok(())
}

/// The `.csproj` files `folder` holds, whatever case the extension is spelled in; a folder spelled like one is not.
fn csprojs(folder: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("csproj")) {
            found.push(path);
        }
    }
    Ok(found)
}

/// Restores each project that has no `obj/project.assets.json`, in order, saying each one it restores.
fn restore(projects: &BTreeSet<PathBuf>, tools: &mut impl Tools, console: &mut Console) -> u32 {
    for project in projects {
        if !needs_restore(project) {
            continue;
        }
        let words = [OsString::from("restore"), project.clone().into_os_string()];
        let status = match start(tools, "dotnet", &words, console) {
            Ok(status) => status,
            Err(status) => return status,
        };
        if status != 0 {
            console.err(&format!(
                "gate: csharp format: dotnet restore {} failed (status {status})",
                project.display()
            ));
            return status;
        }
        console.err(&format!("gate: csharp format: restored {}", project.display()));
    }
    0
}

/// `project`'s `obj/project.assets.json`, the file a restore writes.
fn assets(project: &Path) -> PathBuf {
    project.with_file_name("obj").join("project.assets.json")
}

/// The files that make a project stale when newer than its restored assets, looked for in the project's own folder and
/// every folder above it to the drive root.
const BUILD_INPUTS: [&str; 4] = [
    "Directory.Packages.props",
    "Directory.Build.props",
    "Directory.Build.targets",
    "global.json",
];

/// Whether `project` needs a restore: its assets file cannot be read, the project itself is newer than it, or a build
/// file that shapes the project is.
fn needs_restore(project: &Path) -> bool {
    let Ok(assets_time) = modified(&assets(project)) else {
        return true;
    };
    let newer = |path: &Path| match modified(path) {
        Ok(time) => time > assets_time,
        Err(_) => true,
    };
    if newer(project) {
        return true;
    }
    let mut folder = project.parent();
    while let Some(current) = folder {
        for name in BUILD_INPUTS {
            let input = current.join(name);
            if input.is_file() && newer(&input) {
                return true;
            }
        }
        folder = current.parent();
    }
    false
}

/// `path`'s modified time.
fn modified(path: &Path) -> io::Result<SystemTime> {
    path.metadata()?.modified()
}

/// The format passes: repeat until the files stop changing, at most [`MOST_PASSES`], and answer the files a pass
/// changed.
fn format_passes(files: &[OsString], tools: &mut impl Tools, console: &mut Console) -> Result<Vec<OsString>, u32> {
    let mut passes = 0;
    let first = snapshots(files);
    let mut before = first.clone();
    let mut after;
    loop {
        passes += 1;
        let status = start(tools, "dotnet", &format_words(files), console)?;
        if status != 0 {
            console.err(&format!("gate: csharp format: dotnet format failed (status {status})"));
            return Err(status);
        }
        after = snapshots(files);
        if after == before || passes >= MOST_PASSES {
            break;
        }
        before.clone_from(&after);
    }
    if passes > 1 {
        console.err(&format!(
            "gate: csharp format: {passes} passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)"
        ));
    }
    Ok(changed_files(files, &first, &after))
}

/// The files whose contents in `last` differ from those in `first`, in the order given.
fn changed_files(files: &[OsString], first: &[Option<Vec<u8>>], last: &[Option<Vec<u8>>]) -> Vec<OsString> {
    files
        .iter()
        .zip(first.iter().zip(last))
        .filter(|(_, (start, now))| start != now)
        .map(|(file, _)| file.clone())
        .collect()
}

/// The contents of `files` as a pass finds them; a file that cannot be read has none.
fn snapshots(files: &[OsString]) -> Vec<Option<Vec<u8>>> {
    files.iter().map(|file| fs::read(file).ok()).collect()
}

/// The words of a format pass: every fixer at info severity but the dead-store and namespace ones, `--include` last
/// because it is variadic.
fn format_words(files: &[OsString]) -> Vec<OsString> {
    let mut words = os_words(&[
        "format",
        "style",
        "--severity",
        "info",
        "--no-restore",
        "--exclude-diagnostics",
        "IDE0059",
        "IDE0060",
        "IDE0130",
        "--include",
    ]);
    words.extend(files.iter().cloned());
    words
}

/// The words of the dead-store check: the two rules with no fixer run.
fn verify_words(files: &[OsString]) -> Vec<OsString> {
    let mut words = os_words(&[
        "format",
        "style",
        "--verify-no-changes",
        "--severity",
        "info",
        "--no-restore",
        "--diagnostics",
        "IDE0059",
        "IDE0060",
        "--include",
    ]);
    words.extend(files.iter().cloned());
    words
}

/// `words` as the gate gives them to a program.
fn os_words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(|word| OsString::from(*word)).collect()
}

/// Whether every one of the hook's files was tracked with no unstaged change before the run, so a pass's change can be
/// staged without moving the user's own work into the index.
fn staged_before(files: &[OsString], tools: &mut impl Tools, console: &mut Console) -> Result<bool, u32> {
    let status = start_quiet(tools, "git", &index_words("ls-files", "--error-unmatch", files), console)?;
    match status {
        0 => {}
        1 => return Ok(false),
        status => {
            console.err(&format!("gate: csharp format: git ls-files failed (status {status})"));
            return Err(status);
        }
    }
    let status = start_quiet(tools, "git", &index_words("diff", "--quiet", files), console)?;
    match status {
        0 => Ok(true),
        1 => Ok(false),
        status => {
            console.err(&format!("gate: csharp format: git diff failed (status {status})"));
            Err(status)
        }
    }
}

/// The words of an index check: the `git` verb, its flag, then the files.
fn index_words(verb: &str, flag: &str, files: &[OsString]) -> Vec<OsString> {
    let mut words = os_words(&[verb, flag, "--"]);
    words.extend(files.iter().cloned());
    words
}

/// Stages the `changed` files a pass altered, when the hook's files were all `staged` before it ran, so the hook does
/// not fail the commit on them.
fn stage(changed: &[OsString], staged: bool, tools: &mut impl Tools, console: &mut Console) -> u32 {
    if changed.is_empty() {
        return 0;
    }
    if !staged {
        console.err(&format!(
            "gate: csharp format: left {} changed file(s) unstaged: some files had changes outside the index",
            changed.len()
        ));
        return 0;
    }
    let mut words = os_words(&["add", "--"]);
    words.extend(changed.iter().cloned());
    let status = match start(tools, "git", &words, console) {
        Ok(status) => status,
        Err(status) => return status,
    };
    if status != 0 {
        console.err(&format!("gate: csharp format: git add failed (status {status})"));
    }
    status
}

/// Refuses the commit on a dead store: the two rules are never fixed, so a finding leaves the file untouched.
fn verify(files: &[OsString], tools: &mut impl Tools, console: &mut Console) -> u32 {
    let status = match start(tools, "dotnet", &verify_words(files), console) {
        Ok(status) => status,
        Err(status) => return status,
    };
    if status != 0 {
        console.err(&format!(
            "gate: csharp format: the dead-store check failed (status {status}); IDE0059 and IDE0060 are never fixed for you"
        ));
    }
    status
}

/// Runs `program` with `words`, a program that cannot start said and answered for.
fn start(tools: &mut impl Tools, program: &str, words: &[OsString], console: &mut Console) -> Result<u32, u32> {
    started(tools.run(program, words), program, console)
}

/// Runs `program` with `words` quietly, a program that cannot start said and answered for.
fn start_quiet(tools: &mut impl Tools, program: &str, words: &[OsString], console: &mut Console) -> Result<u32, u32> {
    started(tools.check(program, words), program, console)
}

/// The result of starting `program`, a program that cannot start said and answered for.
fn started(result: io::Result<u32>, program: &str, console: &mut Console) -> Result<u32, u32> {
    result.map_err(|error| {
        console.err(&format!("gate: csharp format: cannot start {program}: {error}"));
        u32::from(FAILED_STATUS)
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folder, file or scripted program is missing must fail loudly")]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::{Component, Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    use super::{Tools, format_with};
    use crate::console::Console;
    use crate::test_support::temp_dir;

    /// One program the command ran.
    #[derive(Debug, PartialEq, Eq)]
    struct Call {
        program: String,
        words: Vec<String>,
        quiet: bool,
    }

    /// What one scripted program answers.
    enum Answer {
        /// Exits with this status, after doing what the test asks first, when it asks anything.
        Status(u32, Option<Box<dyn FnMut()>>),
        /// Cannot start at all.
        CannotStart,
    }

    /// The programs a test scripts, in the order the command runs them, and what each call was.
    #[derive(Default)]
    struct Script {
        answers: VecDeque<Answer>,
        calls: Vec<Call>,
    }

    impl Script {
        fn with(answers: Vec<Answer>) -> Script {
            Script {
                answers: VecDeque::from(answers),
                calls: Vec::new(),
            }
        }

        /// How many programs it ran.
        fn ran(&self) -> usize {
            self.calls.len()
        }

        /// The calls whose first word is `verb`.
        fn verb(&self, verb: &str) -> Vec<&Call> {
            self.calls
                .iter()
                .filter(|call| call.words.first().is_some_and(|word| word == verb))
                .collect()
        }

        /// How many format passes it ran, not counting the verify pass.
        fn formats(&self) -> usize {
            self.verb("format")
                .iter()
                .filter(|call| call.words.get(2).is_some_and(|word| word == "--severity"))
                .count()
        }

        /// How many dead-store verify passes it ran.
        fn verifies(&self) -> usize {
            self.verb("format")
                .iter()
                .filter(|call| call.words.get(2).is_some_and(|word| word == "--verify-no-changes"))
                .count()
        }

        /// Each program and its words, as one line.
        fn spoken(&self) -> Vec<String> {
            self.calls
                .iter()
                .map(|call| format!("{} {}", call.program, call.words.join(" ")))
                .collect()
        }
    }

    impl Tools for Script {
        fn run(&mut self, program: &str, words: &[OsString]) -> io::Result<u32> {
            self.record(program, words, false)
        }

        fn check(&mut self, program: &str, words: &[OsString]) -> io::Result<u32> {
            self.record(program, words, true)
        }
    }

    impl Script {
        /// Records the call, `quiet` when it is one of the index checks, and answers the next scripted program.
        fn record(&mut self, program: &str, words: &[OsString], quiet: bool) -> io::Result<u32> {
            self.calls.push(Call {
                program: program.to_owned(),
                words: words.iter().map(|word| word.to_string_lossy().into_owned()).collect(),
                quiet,
            });
            let Some(answer) = self.answers.pop_front() else {
                return Err(io::Error::other("the test scripted no such program"));
            };
            match answer {
                Answer::Status(status, action) => {
                    if let Some(mut action) = action {
                        action();
                    }
                    Ok(status)
                }
                Answer::CannotStart => Err(io::Error::other("the program is not there")),
            }
        }
    }

    /// Answers `status` at once.
    fn status(status: u32) -> Answer {
        Answer::Status(status, None)
    }

    /// Answers `status`, after `edit` has changed a file.
    fn edits(status: u32, edit: impl FnMut() + 'static) -> Answer {
        Answer::Status(status, Some(Box::new(edit)))
    }

    /// The two index answers of files that were all staged before the run.
    fn staged() -> Vec<Answer> {
        vec![status(0), status(0)]
    }

    /// An empty folder for `test`, without what an earlier run left in it.
    fn folder(test: &str) -> PathBuf {
        let folder = temp_dir().join(test);
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the test's folder can be made");
        folder
    }

    /// An empty file at `path`, the folders above it made.
    fn make_file(path: &Path) {
        put(path, "");
    }

    /// `text` written to `path`, the folders above it made.
    fn put(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the file's folder can be made");
        }
        fs::write(path, text).expect("the file can be written");
    }

    /// Runs the command on `files` through a script answering in order, and answers its status, the script and the
    /// lines it said.
    fn run(files: &[&Path], answers: Vec<Answer>) -> (u32, Script, Vec<String>) {
        let files: Vec<OsString> = files.iter().map(|path| path.as_os_str().to_os_string()).collect();
        let mut script = Script::with(answers);
        let mut console = Console::new();
        let status = format_with(&files, &mut script, &mut console);
        assert!(
            script.answers.is_empty(),
            "the command left {} scripted answer(s) unused: {:?}",
            script.answers.len(),
            script.spoken()
        );
        (status, script, console.take_notes())
    }

    /// `path` written from `base`: `..` for each folder they do not share.
    fn dotted(base: &Path, path: &Path) -> PathBuf {
        let base: Vec<Component> = base.components().collect();
        let parts: Vec<Component> = path.components().collect();
        let shared = base.iter().zip(&parts).take_while(|(left, right)| left == right).count();
        let mut word = PathBuf::new();
        for _ in shared..base.len() {
            word.push("..");
        }
        for part in &parts[shared..] {
            word.push(part);
        }
        word
    }

    #[test]
    fn no_files_runs_nothing() {
        let (status, script, said) = run(&[], Vec::new());

        assert_eq!(status, 0);
        assert_eq!(script.ran(), 0, "{:?}", script.spoken());
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_project_with_its_assets_is_not_restored() {
        let root = folder("csharp-format-with-assets");
        let project = root.join("a.csproj");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&root.join("obj").join("project.assets.json"));
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0);
        assert!(script.verb("restore").is_empty(), "{:?}", script.spoken());
        assert_eq!(script.formats(), 1, "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert_eq!(script.verifies(), 1, "{:?}", script.spoken());
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_project_without_its_assets_is_restored_first() {
        let root = folder("csharp-format-without-assets");
        let project = root.join("a.csproj");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let verbs: Vec<&str> = script.calls.iter().map(|call| call.words[0].as_str()).collect();
        assert_eq!(verbs, ["restore", "ls-files", "diff", "format", "format"], "{:?}", script.spoken());
        assert_eq!(script.calls[0].program, "dotnet");
        assert!(said.contains(&format!("gate: csharp format: restored {}", project.display())), "{said:?}");
    }

    #[test]
    fn the_format_and_verify_words_are_exact() {
        let root = folder("csharp-format-words");
        let project = root.join("a.csproj");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&root.join("obj").join("project.assets.json"));
        make_file(&file);

        let (status, script, _) = run(&[&file], vec![status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0);
        let shown = file.display().to_string();
        assert_eq!(
            script.spoken(),
            [
                format!("git ls-files --error-unmatch -- {shown}"),
                format!("git diff --quiet -- {shown}"),
                format!("dotnet format style --severity info --no-restore --exclude-diagnostics IDE0059 IDE0060 IDE0130 --include {shown}"),
                format!("dotnet format style --verify-no-changes --severity info --no-restore --diagnostics IDE0059 IDE0060 --include {shown}"),
            ]
        );
    }

    #[test]
    fn two_files_of_one_project_restore_it_once() {
        let root = folder("csharp-format-two-files");
        let project = root.join("a.csproj");
        let one = root.join("a.cs");
        let two = root.join("b.cs");
        make_file(&project);
        make_file(&one);
        make_file(&two);

        let (status, script, _) = run(&[&one, &two], vec![status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert_eq!(script.verb("restore").len(), 1, "{:?}", script.spoken());
    }

    #[test]
    fn a_restore_that_writes_the_next_projects_assets_skips_it() {
        let root = folder("csharp-format-restore-chain");
        let first = root.join("a");
        let second = root.join("b");
        make_file(&first.join("a.csproj"));
        make_file(&second.join("b.csproj"));
        let one = first.join("one.cs");
        let two = second.join("two.cs");
        make_file(&one);
        make_file(&two);
        let assets = second.join("obj").join("project.assets.json");
        let written = assets.clone();
        let answers = vec![edits(0, move || make_file(&written)), status(0), status(0), status(0), status(0)];

        let (status, script, _) = run(&[&one, &two], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert_eq!(script.verb("restore").len(), 1, "{:?}", script.spoken());
        assert!(assets.is_file(), "the first restore wrote the second project's assets");
    }

    #[test]
    fn a_failed_restore_stops_with_its_status() {
        let root = folder("csharp-format-failed-restore");
        let project = root.join("a.csproj");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(3)]);

        assert_eq!(status, 3);
        assert_eq!(script.formats(), 0, "{:?}", script.spoken());
        assert!(
            said.contains(&format!("gate: csharp format: dotnet restore {} failed (status 3)", project.display())),
            "{said:?}"
        );
    }

    #[test]
    fn a_file_outside_any_project_formats_with_no_restore() {
        let root = folder("csharp-format-no-project");
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("restore").is_empty(), "{:?}", script.spoken());
        assert_eq!(script.formats(), 1, "{:?}", script.spoken());
        assert_eq!(
            said,
            [format!("gate: csharp format: no project above {}; it gets no restore", file.display())],
            "{said:?}"
        );
    }

    #[test]
    fn two_files_outside_a_project_each_get_the_notice() {
        let root = folder("csharp-format-two-without-project");
        let one = root.join("a.cs");
        let two = root.join("b.cs");
        make_file(&one);
        make_file(&two);

        let (status, script, said) = run(&[&one, &two], vec![status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("restore").is_empty(), "{:?}", script.spoken());
        assert_eq!(
            said,
            [
                format!("gate: csharp format: no project above {}; it gets no restore", one.display()),
                format!("gate: csharp format: no project above {}; it gets no restore", two.display()),
            ],
            "{said:?}"
        );
    }

    #[test]
    fn the_nearest_project_wins() {
        let root = folder("csharp-format-nearest");
        let outer = root.join("a");
        let inner = outer.join("b");
        make_file(&outer.join("a.csproj"));
        make_file(&inner.join("b.csproj"));
        let file = inner.join("a.cs");
        make_file(&file);

        let (status, script, _) = run(&[&file], vec![status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let restores = script.verb("restore");
        assert_eq!(restores.len(), 1, "{:?}", script.spoken());
        assert_eq!(restores[0].words, ["restore".to_owned(), inner.join("b.csproj").display().to_string()]);
    }

    #[test]
    fn a_folder_with_two_projects_restores_both() {
        let root = folder("csharp-format-two-projects");
        let one = root.join("one.csproj");
        let two = root.join("two.csproj");
        make_file(&one);
        make_file(&two);
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, _) = run(&[&file], vec![status(0), status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert_eq!(
            script.calls[0].words,
            ["restore".to_owned(), one.display().to_string()],
            "{:?}",
            script.spoken()
        );
        assert_eq!(
            script.calls[1].words,
            ["restore".to_owned(), two.display().to_string()],
            "{:?}",
            script.spoken()
        );
    }

    #[test]
    fn one_unchanged_pass_ends_the_loop() {
        let root = folder("csharp-format-one-pass");
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0);
        assert_eq!(script.formats(), 1, "{:?}", script.spoken());
        assert!(!said.iter().any(|line| line.contains("passes")), "{said:?}");
    }

    #[test]
    fn a_change_runs_another_pass() {
        let root = folder("csharp-format-two-passes");
        let file = root.join("a.cs");
        make_file(&file);
        let edited = file.clone();
        let answers = vec![
            status(0),
            status(0),
            edits(0, move || put(&edited, "changed")),
            status(0),
            status(0),
            status(0),
        ];

        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0);
        assert_eq!(script.formats(), 2, "{:?}", script.spoken());
        assert!(
            said.contains(&"gate: csharp format: 2 passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)".to_owned()),
            "{said:?}"
        );
    }

    #[test]
    fn three_passes_is_the_most() {
        let root = folder("csharp-format-three-passes");
        let file = root.join("a.cs");
        make_file(&file);
        let edit = |text: &'static str| {
            let path = file.clone();
            edits(0, move || put(&path, text))
        };
        let answers = vec![status(0), status(0), edit("one"), edit("two"), edit("three"), status(0), status(0)];

        let (status, script, _) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert_eq!(script.formats(), 3, "{:?}", script.spoken());
        assert_eq!(script.verb("add").len(), 1, "{:?}", script.spoken());
        assert_eq!(script.verifies(), 1, "{:?}", script.spoken());
    }

    #[test]
    fn a_failed_format_stops_before_the_add() {
        let root = folder("csharp-format-failed-format");
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(5)]);

        assert_eq!(status, 5);
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert!(
            said.contains(&"gate: csharp format: dotnet format failed (status 5)".to_owned()),
            "{said:?}"
        );
    }

    #[test]
    fn a_failed_add_stops_before_the_verify() {
        let root = folder("csharp-format-failed-add");
        let file = root.join("a.cs");
        make_file(&file);
        let edited = file.clone();

        let answers = vec![status(0), status(0), edits(0, move || put(&edited, "changed")), status(0), status(6)];
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 6);
        assert_eq!(script.verifies(), 0, "{:?}", script.spoken());
        assert!(said.contains(&"gate: csharp format: git add failed (status 6)".to_owned()), "{said:?}");
    }

    #[test]
    fn a_failed_verify_returns_its_status() {
        let root = folder("csharp-format-failed-verify");
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), status(0), status(2)]);

        assert_eq!(status, 2);
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert!(
            said.contains(&"gate: csharp format: the dead-store check failed (status 2); IDE0059 and IDE0060 are never fixed for you".to_owned()),
            "{said:?}"
        );
    }

    #[test]
    fn a_missing_folder_carries_on_upward() {
        let root = folder("csharp-format-missing-folder");
        let project = root.join("a.csproj");
        make_file(&project);
        let missing = root.join("missing").join("a.cs");

        let (status, script, said) = run(&[&missing], vec![status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let restores = script.verb("restore");
        assert_eq!(restores.len(), 1, "{:?}", script.spoken());
        assert_eq!(restores[0].words, ["restore".to_owned(), project.display().to_string()]);
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert!(!said.iter().any(|line| line.contains("cannot read")), "{said:?}");
    }

    #[test]
    fn a_folder_named_like_a_project_is_not_one() {
        let root = folder("csharp-format-folder-named-project");
        let project = root.join("a.csproj");
        make_file(&project);
        fs::create_dir_all(root.join("sub").join("Foo.csproj")).expect("the lookalike folder can be made");
        let file = root.join("sub").join("a.cs");
        make_file(&file);

        let (status, script, _) = run(&[&file], vec![status(0), status(0), status(0), status(0), status(0)]);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let restores = script.verb("restore");
        assert_eq!(restores.len(), 1, "{:?}", script.spoken());
        assert_eq!(restores[0].words, ["restore".to_owned(), project.display().to_string()]);
    }

    #[test]
    fn a_relative_path_finds_its_project() {
        let root = folder("csharp-format-relative");
        let project = root.join("a.csproj");
        make_file(&project);
        let file = root.join("sub").join("a.cs");
        make_file(&file);
        let base = std::env::current_dir().expect("the test's process has a current folder");
        let word = dotted(&base, &file);

        let mut console = Console::new();
        let found = super::projects(&[word.into_os_string()], &mut console).expect("the projects are found");

        assert_eq!(found.iter().collect::<Vec<_>>(), [&project], "{found:?}");
        let said = console.take_notes();
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_program_that_cannot_start_fails() {
        let root = folder("csharp-format-cannot-start");
        let file = root.join("a.cs");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(0), Answer::CannotStart]);

        assert_eq!(status, 1, "{:?}", script.spoken());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("gate: csharp format: cannot start dotnet: "), "{said:?}");
    }

    #[test]
    fn an_unchanged_file_is_left_unstaged() {
        let root = folder("csharp-format-unchanged-unstaged");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);

        let mut answers = staged();
        answers.push(status(0));
        answers.push(status(0));
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert!(said.is_empty(), "{said:?}");
        assert_eq!(script.verifies(), 1, "{:?}", script.spoken());
    }

    #[test]
    fn only_the_changed_file_of_two_is_staged() {
        let root = folder("csharp-format-changed-of-two");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let one = root.join("a.cs");
        let two = root.join("b.cs");
        make_file(&one);
        make_file(&two);
        let edited = one.clone();

        let mut answers = staged();
        answers.push(edits(0, move || put(&edited, "changed")));
        answers.push(status(0));
        answers.push(status(0));
        answers.push(status(0));
        let (status, script, said) = run(&[&one, &two], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let adds = script.verb("add");
        assert_eq!(adds.len(), 1, "{:?}", script.spoken());
        assert_eq!(adds[0].program, "git", "{:?}", script.spoken());
        assert_eq!(adds[0].words, ["add".to_owned(), "--".to_owned(), one.display().to_string()]);
        assert_eq!(
            said,
            ["gate: csharp format: 2 passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)".to_owned()],
            "{said:?}"
        );
    }

    #[test]
    fn a_changed_file_with_unstaged_changes_is_left_unstaged() {
        let root = folder("csharp-format-unstaged-change");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);
        let edited = file.clone();

        let mut answers = vec![status(0), status(1)];
        answers.push(edits(0, move || put(&edited, "changed")));
        answers.push(status(0));
        answers.push(status(0));
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert_eq!(
            said,
            [
                "gate: csharp format: 2 passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)".to_owned(),
                "gate: csharp format: left 1 changed file(s) unstaged: some files had changes outside the index".to_owned(),
            ],
            "{said:?}"
        );
        assert_eq!(script.verifies(), 1, "{:?}", script.spoken());
    }

    #[test]
    fn a_changed_untracked_file_is_left_unstaged_without_a_diff() {
        let root = folder("csharp-format-untracked-change");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);
        let edited = file.clone();

        let mut answers = vec![status(1)];
        answers.push(edits(0, move || put(&edited, "changed")));
        answers.push(status(0));
        answers.push(status(0));
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("diff").is_empty(), "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert_eq!(
            said,
            [
                "gate: csharp format: 2 passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)".to_owned(),
                "gate: csharp format: left 1 changed file(s) unstaged: some files had changes outside the index".to_owned(),
            ],
            "{said:?}"
        );
    }

    #[test]
    fn unstaged_files_no_pass_changes_say_nothing() {
        let root = folder("csharp-format-unstaged-unchanged");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);

        let answers = vec![status(0), status(1), status(0), status(0)];
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn a_file_changed_and_changed_back_is_not_staged() {
        let root = folder("csharp-format-changed-back");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);
        let there = file.clone();
        let back = file.clone();

        let answers = vec![
            status(0),
            status(0),
            edits(0, move || put(&there, "x")),
            edits(0, move || put(&back, "")),
            status(0),
            status(0),
        ];
        let (status, script, said) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        assert_eq!(script.formats(), 3, "{:?}", script.spoken());
        assert!(script.verb("add").is_empty(), "{:?}", script.spoken());
        assert_eq!(
            said,
            ["gate: csharp format: 3 passes (IDE0008 fixes one sub-rule per pass, dotnet/roslyn#80349)".to_owned()],
            "{said:?}"
        );
    }

    #[test]
    fn a_failed_ls_files_stops_before_the_passes() {
        let root = folder("csharp-format-failed-ls-files");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(128)]);

        assert_eq!(status, 128);
        assert_eq!(script.formats(), 0, "{:?}", script.spoken());
        assert_eq!(said, ["gate: csharp format: git ls-files failed (status 128)".to_owned()], "{said:?}");
    }

    #[test]
    fn a_failed_diff_stops_before_the_passes() {
        let root = folder("csharp-format-failed-diff");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![status(0), status(129)]);

        assert_eq!(status, 129);
        assert_eq!(script.formats(), 0, "{:?}", script.spoken());
        assert_eq!(said, ["gate: csharp format: git diff failed (status 129)".to_owned()], "{said:?}");
    }

    #[test]
    fn a_git_that_cannot_start_fails_before_the_passes() {
        let root = folder("csharp-format-git-cannot-start");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);

        let (status, script, said) = run(&[&file], vec![Answer::CannotStart]);

        assert_eq!(status, 1, "{:?}", script.spoken());
        assert_eq!(script.formats(), 0, "{:?}", script.spoken());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("gate: csharp format: cannot start git: "), "{said:?}");
    }

    #[test]
    fn the_index_checks_run_quietly() {
        let root = folder("csharp-format-quiet-index");
        make_file(&root.join("a.csproj"));
        make_file(&root.join("obj").join("project.assets.json"));
        let file = root.join("a.cs");
        make_file(&file);
        let edited = file.clone();

        let mut answers = staged();
        answers.push(edits(0, move || put(&edited, "changed")));
        answers.push(status(0));
        answers.push(status(0));
        answers.push(status(0));
        let (status, script, _) = run(&[&file], answers);

        assert_eq!(status, 0, "{:?}", script.spoken());
        let checks: Vec<&Call> = script.verb("ls-files").into_iter().chain(script.verb("diff")).collect();
        assert_eq!(checks.len(), 2, "{:?}", script.spoken());
        assert!(checks.iter().all(|call| call.quiet), "{:?}", script.spoken());
        assert_eq!(script.verb("add").len(), 1, "{:?}", script.spoken());
        assert!(script.verb("format").iter().all(|call| !call.quiet), "{:?}", script.spoken());
        assert!(script.verb("add").iter().all(|call| !call.quiet), "{:?}", script.spoken());
    }

    /// An early time for the files a case stamps.
    const OLD: u64 = 1_000_000;
    /// A later time than [`OLD`], one the assets file can be newer or older than.
    const NEW: u64 = 2_000_000;

    /// Sets `path`'s modified time, `seconds` after the epoch, so no case depends on the order or speed of writes.
    fn stamp(path: &Path, seconds: u64) {
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("the file can be opened to set its time");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
            .expect("the file's time can be set");
    }

    /// Runs the command on `file`, scripting the programs it runs: four when its project needs no restore, five when it
    /// does.
    fn ran_with(file: &Path, restores: bool) -> (u32, Script, Vec<String>) {
        let answers = if restores {
            vec![status(0), status(0), status(0), status(0), status(0)]
        } else {
            vec![status(0), status(0), status(0), status(0)]
        };
        run(&[file], answers)
    }

    /// Asserts the command restored `project` once and said nothing but the restore.
    fn assert_restored(project: &Path, status: u32, script: &Script, said: &[String]) {
        assert_eq!(status, 0, "{:?}", script.spoken());
        let restores = script.verb("restore");
        assert_eq!(restores.len(), 1, "{:?}", script.spoken());
        assert_eq!(restores[0].words, ["restore".to_owned(), project.display().to_string()]);
        assert!(said.contains(&format!("gate: csharp format: restored {}", project.display())), "{said:?}");
    }

    /// Asserts the command restored nothing and said nothing.
    fn assert_not_restored(status: u32, script: &Script, said: &[String]) {
        assert_eq!(status, 0, "{:?}", script.spoken());
        assert!(script.verb("restore").is_empty(), "{:?}", script.spoken());
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn assets_newer_than_every_input_are_not_restored() {
        let root = folder("csharp-format-assets-newer");
        let project = root.join("a.csproj");
        let props = root.join("Directory.Build.props");
        let assets = root.join("obj").join("project.assets.json");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&props);
        make_file(&assets);
        stamp(&project, OLD);
        stamp(&props, OLD);
        stamp(&assets, NEW);

        let (status, script, said) = ran_with(&file, false);

        assert_not_restored(status, &script, &said);
    }

    #[test]
    fn assets_as_old_as_an_input_are_not_restored() {
        let root = folder("csharp-format-assets-as-old");
        let project = root.join("a.csproj");
        let assets = root.join("obj").join("project.assets.json");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&assets);
        stamp(&project, OLD);
        stamp(&assets, OLD);

        let (status, script, said) = ran_with(&file, false);

        assert_not_restored(status, &script, &said);
    }

    #[test]
    fn assets_older_than_the_project_are_restored() {
        let root = folder("csharp-format-stale-project");
        let project = root.join("a.csproj");
        let assets = root.join("obj").join("project.assets.json");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&assets);
        stamp(&assets, OLD);
        stamp(&project, NEW);

        let (status, script, said) = ran_with(&file, true);

        assert_restored(&project, status, &script, &said);
    }

    #[test]
    fn assets_older_than_central_packages_above_are_restored() {
        let root = folder("csharp-format-central-packages");
        let project_folder = root.join("a").join("b");
        let project = project_folder.join("a.csproj");
        let central = root.join("Directory.Packages.props");
        let assets = project_folder.join("obj").join("project.assets.json");
        let file = project_folder.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&central);
        make_file(&assets);
        stamp(&assets, OLD);
        stamp(&central, NEW);

        let (status, script, said) = ran_with(&file, true);

        assert_restored(&project, status, &script, &said);
    }

    #[test]
    fn assets_older_than_build_props_are_restored() {
        let root = folder("csharp-format-build-props");
        let project = root.join("a.csproj");
        let props = root.join("Directory.Build.props");
        let assets = root.join("obj").join("project.assets.json");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&props);
        make_file(&assets);
        stamp(&assets, OLD);
        stamp(&props, NEW);

        let (status, script, said) = ran_with(&file, true);

        assert_restored(&project, status, &script, &said);
    }

    #[test]
    fn assets_older_than_build_targets_are_restored() {
        let root = folder("csharp-format-build-targets");
        let project = root.join("sub").join("a.csproj");
        let targets = root.join("Directory.Build.targets");
        let assets = root.join("sub").join("obj").join("project.assets.json");
        let file = root.join("sub").join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&targets);
        make_file(&assets);
        stamp(&assets, OLD);
        stamp(&targets, NEW);

        let (status, script, said) = ran_with(&file, true);

        assert_restored(&project, status, &script, &said);
    }

    #[test]
    fn assets_older_than_global_json_are_restored() {
        let root = folder("csharp-format-global-json");
        let project_folder = root.join("a").join("b");
        let project = project_folder.join("a.csproj");
        let global = root.join("global.json");
        let assets = project_folder.join("obj").join("project.assets.json");
        let file = project_folder.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&global);
        make_file(&assets);
        stamp(&assets, OLD);
        stamp(&global, NEW);

        let (status, script, said) = ran_with(&file, true);

        assert_restored(&project, status, &script, &said);
    }

    #[test]
    fn a_project_with_no_other_inputs_needs_only_its_own_check() {
        let root = folder("csharp-format-own-check");
        let project = root.join("a.csproj");
        let assets = root.join("obj").join("project.assets.json");
        let file = root.join("a.cs");
        make_file(&project);
        make_file(&file);
        make_file(&assets);
        stamp(&project, OLD);
        stamp(&assets, NEW);

        let (status, script, said) = ran_with(&file, false);

        assert_not_restored(status, &script, &said);
    }
}

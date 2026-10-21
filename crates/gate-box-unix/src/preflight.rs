//! `gate-box box preflight`, the first call of every routed run: judges the builder's work root, readies the bare
//! repository and the work folder's tree, then says the version, warm, new work tree, boot, SDK and tool lines in the
//! order the PC's client reads them.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gate_wire::{Pair, PairSibling, PathForm, Payload, bare_stem, escaped};

use crate::console::Console;
use crate::lines::{
    FAILED_STATUS, NEEDS_REFUSED, NEW_WORK_TREE, PAIR_NOT_BUILT, ROOT_STATUS, USAGE_STATUS, created_line, folder_failure, is_git_variable,
    layout_text, login_path_line, names_another_layout, readies_single, root_flaw, root_unsafe_line, sdks_failure, sdks_line, step_unran, tool_line,
    unsynced_failure, version_line, warm_line, warm_text, work_layout, write_failure,
};
use crate::process::{NotRan, Ran, run_bounded, status_text};
use crate::{boot, files, probe, process};

/// How long one git step may take.
const GIT_LIMIT: Duration = Duration::from_secs(120);
/// The most a git step may write on its standard output.
const GIT_OUTPUT_CAP: usize = 1024 * 1024;
/// The most bytes of a record beside the work folder read.
const RECORD_LIMIT: u64 = 64 * 1024;
/// The most bytes of the warm record read: one past a hash, so a longer record is seen as not one.
const WARM_READ_LIMIT: u64 = 65;
/// The most lines of a failed git step's standard error said before its failure line.
const GIT_ERROR_LINES: usize = 10;

/// A step that failed: the lines saying so, on stderr, and the status the box exits with.
#[derive(Debug)]
struct Failed {
    lines: Vec<String>,
    status: u8,
}

impl Failed {
    /// A failure said in one line, with [`FAILED_STATUS`].
    fn line(line: String) -> Failed {
        Failed {
            lines: vec![line],
            status: FAILED_STATUS,
        }
    }
}

/// Why the work root is not used.
enum Refusal {
    /// A folder could not be made or is not the box user's own: this line, [`FAILED_STATUS`].
    Folder(String),
    /// The root is not trusted, for this reason: the version line, then the work-root line, [`ROOT_STATUS`].
    Untrusted(String),
}

/// Answers the preflight of `payload` through `console`; the status. Usage errors (a key the payload lacks or that does
/// not fit the work folder) exit [`USAGE_STATUS`] before anything is made; a work root another account may write in is
/// refused after the version line with the work-root line, as the Windows box refuses one.
pub fn preflight(payload: &Payload, console: &mut Console) -> u8 {
    let checked = folders(payload).and_then(|(bare, work, pair)| Ok((bare, work, checked_root(work, bare, pair.is_some())?, pair)));
    let (bare, work, (root, parent), pair) = match checked {
        Ok(checked) => checked,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    match trusted_root(&root, parent) {
        Ok(()) => {}
        Err(Refusal::Folder(line)) => {
            console.err(&line);
            return FAILED_STATUS;
        }
        Err(Refusal::Untrusted(why)) => {
            console.out(&version_line());
            console.err(&root_unsafe_line(&why));
            return ROOT_STATUS;
        }
    }
    // A pair's bare repositories are not this payload's `Bare`, which a pair leaves unused.
    let used = [Some(work), pair.is_none().then_some(bare)];
    if let Some(line) = used.into_iter().flatten().find_map(|folder| files::unusable_folder(Path::new(folder))) {
        console.err(&line);
        return FAILED_STATUS;
    }
    // Read before the tree is readied, which would make a missing `.git`, and so a stale record, look current.
    let layout = layout_record(work);
    let warm = warm_state(work, &layout);
    let interrupted = files::exists(Path::new(&format!("{work}.command")));
    let relaid = names_another_layout(&layout, &wanted_layout(pair.as_ref()));
    let readied = if pair.is_some() {
        console.err(PAIR_NOT_BUILT);
        Ok(false)
    } else {
        ready(&Git::of(&root, parent), bare, work, &layout, console)
    };
    let made = match readied {
        Ok(made) => made,
        Err(failed) => {
            failed.lines.iter().for_each(|line| console.err(line));
            return failed.status;
        }
    };
    console.out(&version_line());
    console.out(&warm_line(warm.as_deref()));
    if made || interrupted || relaid {
        console.out(NEW_WORK_TREE);
    }
    boot::say(console);
    say_sdks_and_tools(payload, console);
    0
}

/// The payload's `Bare`, `Work` and `Pair`, once `Key`, `Lock`, `Index`, `OutputFolder` and `Job` fit the work folder;
/// else the usage line.
fn folders(payload: &Payload) -> Result<(&str, &str, Option<Pair>), String> {
    let bare = payload.path("Bare")?;
    let (key, work) = payload.work_folder()?;
    payload.beside_work("Lock", work, "lock")?;
    payload.beside_work("Index", work, "index")?;
    payload.output_folder(work)?;
    payload.job(key)?;
    Ok((bare, work, payload.pair()?))
}

/// The work root of `work`, `<root>/work/<key>`, and its `<root>/work`, with `bare` at `<root>/<name>.git` unless
/// `paired` (a pair's bare repositories were checked as its layout was read); else the usage line.
fn checked_root<'w>(work: &'w str, bare: &str, paired: bool) -> Result<(String, &'w Path), String> {
    let (root, parent) = work_layout(work)?;
    if !paired && bare_stem(bare, &root, PathForm::Unix).is_none() {
        return Err(format!(
            "gate: box payload Bare must be <root>/<name>.git to lock its root down: {}",
            escaped(bare)
        ));
    }
    Ok((root, parent))
}

/// Makes the work root `root` with mode 0700 when it is missing, judges it ([`root_flaw`]), then makes `parent`, its
/// `<root>/work`, with mode 0700 when it is missing, and refuses it unless it is the box user's own plain folder.
fn trusted_root(root: &str, parent: &Path) -> Result<(), Refusal> {
    let path = Path::new(root);
    if !files::exists(path) {
        made_folder(path, true)?;
    }
    let facts = files::facts(path).map_err(|error| Refusal::Untrusted(format!("cannot read {root}: {error}")))?;
    if let Some(why) = root_flaw(facts, root, &files::user_name(facts.euid)) {
        return Err(Refusal::Untrusted(why));
    }
    if !files::exists(parent) {
        made_folder(parent, false)?;
    }
    files::unusable_folder(parent).map_or(Ok(()), |line| Err(Refusal::Folder(line)))
}

/// Makes the folder `path` with mode 0700, and the folders above it when `recursive`.
fn made_folder(path: &Path, recursive: bool) -> Result<(), Refusal> {
    DirBuilder::new()
        .recursive(recursive)
        .mode(0o700)
        .create(path)
        .map_err(|error| Refusal::Folder(folder_failure(&path.display().to_string(), &error.to_string())))
}

/// The layout record's text for the run: a single tree's, or the pair's.
fn wanted_layout(pair: Option<&Pair>) -> String {
    layout_text(pair.map(|pair| (pair.leaf(), pair.siblings().iter().map(PairSibling::name).collect())))
}

/// The work folder's layout record, `<work>.layout`, read as a plain file; `None` when it is missing.
fn layout_record(work: &str) -> io::Result<Option<String>> {
    match files::read_plain(Path::new(&format!("{work}.layout")), RECORD_LIMIT) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The hash the work folder's warm record, `<work>.warm`, names ([`warm_text`]); `None` when the layout is not a single
/// tree's, the tree's `.git` is not a plain folder, or the record is missing or not a plain file.
fn warm_state(work: &str, layout: &io::Result<Option<String>>) -> Option<String> {
    if !readies_single(layout) || !files::plain_folder(&Path::new(work).join(".git")) {
        return None;
    }
    let bytes = files::read_plain(Path::new(&format!("{work}.warm")), WARM_READ_LIMIT).ok()?;
    warm_text(&bytes)
}

/// Readies the bare repository `bare`, made when missing, and, when the layout record lets it ([`readies_single`]),
/// the tree at `work`: made, its stale warm record removed first, when nothing is at its `.git`; then, a folder at
/// `.git`, its `origin` set to `bare`, its line-ending config written and its alternates file naming `bare`'s objects.
/// A link or a file at `.git`, `.git/objects` or `.git/objects/info` is never followed. Whether the tree was made now.
fn ready(git: &Git<'_>, bare: &str, work: &str, layout: &io::Result<Option<String>>, console: &mut Console) -> Result<bool, Failed> {
    if !files::exists(Path::new(bare)) {
        console.out(&created_line(bare));
        git.step("git init --bare", &["init", "--bare", "-q", bare])?;
    }
    if !readies_single(layout) {
        return Ok(false);
    }
    let made = match fs::symlink_metadata(Path::new(work).join(".git")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warm_removed(work)?;
            git.step("git init", &["init", "-q", work])?;
            true
        }
        Ok(metadata) if metadata.is_dir() => false,
        // A link, a file or anything that cannot be looked at: the run repairs it under its lock.
        _ => return Ok(false),
    };
    let tree = Tree::at(work);
    origin_set(git, &tree, bare)?;
    for (setting, value) in [("core.autocrlf", "false"), ("core.eol", "lf")] {
        git.step(&format!("git config {setting}"), &tree.args(&["config", setting, value]))?;
    }
    alternates_written(work, bare)?;
    Ok(made)
}

/// Removes the warm record `<work>.warm` of a tree that is gone, so the tree made in its place is not taken as warm;
/// a record already missing is fine.
fn warm_removed(work: &str) -> Result<(), Failed> {
    let warm = format!("{work}.warm");
    match fs::remove_file(&warm) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Failed::line(format!(
            "gate: remote could not remove {}: {}",
            escaped(&warm),
            escaped(&error.to_string())
        ))),
        _ => Ok(()),
    }
}

/// The words naming a work folder's repository to git: its `.git` and its tree, so git never looks for another.
struct Tree {
    git_dir: String,
    work_tree: String,
}

impl Tree {
    /// The repository of the tree at `work`.
    fn at(work: &str) -> Tree {
        Tree {
            git_dir: format!("--git-dir={work}/.git"),
            work_tree: format!("--work-tree={work}"),
        }
    }

    /// The git words running `rest` in this repository.
    fn args<'a>(&'a self, rest: &[&'a str]) -> Vec<&'a str> {
        [self.git_dir.as_str(), self.work_tree.as_str()]
            .into_iter()
            .chain(rest.iter().copied())
            .collect()
    }
}

/// Sets the `origin` of the repository `tree` to `bare`: added when it has none, set when it names another.
fn origin_set(git: &Git<'_>, tree: &Tree, bare: &str) -> Result<(), Failed> {
    let step = "git config remote.origin.url";
    let ran = git.run(step, &tree.args(&["config", "--get", "remote.origin.url"]))?;
    match ran.status.code() {
        Some(0) if String::from_utf8_lossy(&ran.stdout).trim_end_matches(['\r', '\n']) == bare => Ok(()),
        Some(0) => git
            .step("git remote set-url", &tree.args(&["remote", "set-url", "origin", bare]))
            .map(drop),
        // `config --get` exits 1 for a key that is not set.
        Some(1) => git.step("git remote add", &tree.args(&["remote", "add", "origin", bare])).map(drop),
        _ => Err(step_failed(step, &ran)),
    }
}

/// Writes `<work>/.git/objects/info/alternates` as `<bare>/objects` and a line feed unless it already holds exactly
/// that, whole, so a git reading it meanwhile never finds it empty ([`files::replace_plain`]); `info` is made when
/// missing. A link or anything but a folder at `objects` or `info` leaves it unwritten. A folder that cannot be synced
/// after the rename fails the preflight with its own line, the new text in place.
fn alternates_written(work: &str, bare: &str) -> Result<(), Failed> {
    let objects = Path::new(work).join(".git").join("objects");
    if !files::plain_folder(&objects) {
        return Ok(());
    }
    let info = objects.join("info");
    match fs::symlink_metadata(&info) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&info).map_err(|error| Failed::line(folder_failure(&info.display().to_string(), &error.to_string())))?;
        }
        Ok(metadata) if metadata.is_dir() => {}
        _ => return Ok(()),
    }
    let file = info.join("alternates");
    let wanted = format!("{bare}/objects\n");
    if files::read_plain(&file, RECORD_LIMIT).is_ok_and(|held| held == wanted.as_bytes()) {
        return Ok(());
    }
    files::replace_plain(&file, wanted.as_bytes()).map_err(|failure| {
        let shown = file.display().to_string();
        Failed::line(match failure {
            files::ReplaceFailure::NotWritten(error) => write_failure(&shown, &error.to_string()),
            files::ReplaceFailure::Unsynced(error) => unsynced_failure(&shown, &error.to_string()),
        })
    })
}

/// How the preflight runs git: with no variable of git's own inherited ([`is_git_variable`]), so none points git at
/// another repository, config or program; in `folder`, the work root's `<root>/work`, so a step that names no
/// repository starts its search there and not in the box's inherited folder; and with `GIT_CEILING_DIRECTORIES` at
/// `ceiling`, the work root, which git never searches up into, so that search ends at `<root>/work` (git ignores a
/// ceiling that is the starting folder itself). The tree's steps name their repository besides ([`Tree`]).
struct Git<'a> {
    folder: &'a Path,
    ceiling: &'a str,
}

impl<'a> Git<'a> {
    /// The git of the work root `root`, whose work folders are in `parent`, its `<root>/work`.
    fn of(root: &'a str, parent: &'a Path) -> Git<'a> {
        Git {
            folder: parent,
            ceiling: root,
        }
    }

    /// Runs git with `args`, the git step `step`, to its exit; else the failure that it could not run.
    fn run(&self, step: &str, args: &[&str]) -> Result<Ran, Failed> {
        let mut command = Command::new("git");
        command.args(args).current_dir(self.folder);
        for (name, _) in std::env::vars_os().filter(|(name, _)| is_git_variable(name)) {
            command.env_remove(name);
        }
        command.env("GIT_CEILING_DIRECTORIES", self.ceiling);
        run_bounded(command, GIT_OUTPUT_CAP, Instant::now() + GIT_LIMIT).map_err(|not| {
            let why = match not {
                NotRan::Start(error) => error.to_string(),
                NotRan::TimedOut { .. } => format!("git did not answer in {} s", GIT_LIMIT.as_secs()),
                NotRan::OverCap => format!("git wrote more than {} MiB", GIT_OUTPUT_CAP / 1024 / 1024),
                NotRan::Failed(why) => why,
            };
            Failed::line(step_unran(step, &why))
        })
    }

    /// Runs the git step `step` with `args`; its standard output when it exits 0, else its failure.
    fn step(&self, step: &str, args: &[&str]) -> Result<Vec<u8>, Failed> {
        let ran = self.run(step, args)?;
        if ran.status.success() {
            Ok(ran.stdout)
        } else {
            Err(step_failed(step, &ran))
        }
    }
}

/// The failure of the git step `step` that `ran` to a non-zero exit: the last lines of git's standard error, then
/// `gate: remote <step> failed (status <n>)`, exiting with git's status; a git a signal ended fails with
/// [`FAILED_STATUS`].
fn step_failed(step: &str, ran: &Ran) -> Failed {
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let said: Vec<&str> = stderr.lines().filter(|line| !line.trim().is_empty()).collect();
    let mut lines: Vec<String> = said[said.len().saturating_sub(GIT_ERROR_LINES)..].iter().copied().map(escaped).collect();
    let status = match ran.status.code() {
        Some(code) => u8::try_from(code).ok().filter(|code| *code != 0).unwrap_or(FAILED_STATUS),
        None => FAILED_STATUS,
    };
    lines.push(match ran.status.code() {
        Some(code) => format!("gate: remote {step} failed (status {code})"),
        None => format!("gate: remote {step} failed: git ended with {}", status_text(ran.status)),
    });
    Failed { lines, status }
}

/// Says the SDK line and one line for each tool the payload's `Needs` names, in its order, the SDK list and the probes
/// read at the same time along the login shell's `PATH` ([`process::login_path`]), or the box's own `PATH`, said so,
/// when that cannot be read.
fn say_sdks_and_tools(payload: &Payload, console: &mut Console) {
    let path = match process::login_path() {
        Ok(path) => Some(path),
        Err(why) => {
            console.err(&login_path_line(&why));
            None
        }
    };
    let names: Vec<String> = if let Ok(needs) = payload.pairs("Needs") {
        needs.into_iter().map(|(name, _)| name).collect()
    } else {
        console.err(NEEDS_REFUSED);
        Vec::new()
    };
    let (sdks, tools) = thread::scope(|scope| {
        let sdks = scope.spawn(|| probe::sdks(path.as_deref()));
        let tools = probe::probe_all(&names, path.as_deref());
        (sdks.join().unwrap_or_else(|_| Err("the SDK read failed".to_owned())), tools)
    });
    match sdks {
        Ok(list) => console.out(&sdks_line(&list)),
        Err(message) => console.err(&sdks_failure(&message)),
    }
    for (name, probed) in names.iter().zip(&tools) {
        console.out(&tool_line(name, probed));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose temp folder or payload cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_wire::{PathForm, Payload};
    use serde_json::{Value, json};

    use super::{Git, preflight};
    use crate::console::Console;
    use crate::files::{effective_uid, user_name};
    use crate::lines::{NEW_WORK_TREE, PAIR_NOT_BUILT, created_line, root_unsafe_line, version_line, warm_line};

    const KEY: &str = "repo-0123abcd";
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
        /// The work root's name in [`Case::base`].
        leaf: &'static str,
    }

    impl Case {
        fn new(name: &str) -> Case {
            Case::rooted(name, "gate-remote")
        }

        /// A case whose work root is named `leaf`.
        fn rooted(name: &str, leaf: &'static str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            Case { base, leaf }
        }

        fn root(&self) -> String {
            self.base.join(self.leaf).to_str().expect("the temp folder is UTF-8").to_owned()
        }

        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.root())
        }

        fn bare(&self) -> String {
            format!("{}/repo.git", self.root())
        }

        /// The preflight payload of this case's work folder, with `extra` keys set over it.
        fn payload(&self, extra: &Value) -> Payload {
            let (root, work) = (self.root(), self.work());
            let mut json = json!({
                "Key": KEY,
                "Bare": self.bare(),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
            });
            if let (Some(object), Some(extra)) = (json.as_object_mut(), extra.as_object()) {
                object.extend(extra.clone());
            }
            Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Runs the preflight of `payload`; its status and console.
    fn preflighted(payload: &Payload) -> (u8, Console) {
        let mut console = Console::captured();
        let status = preflight(payload, &mut console);
        (status, console)
    }

    /// Checks `lines`, what follows the boot line, are at most one sdks line, then exactly `tools`.
    fn assert_sdks_then_tools(lines: &[String], tools: &[&str]) {
        let split = lines.len().saturating_sub(tools.len());
        assert_eq!(&lines[split..], tools, "{lines:?}");
        assert!(split <= 1, "{lines:?}");
        assert!(lines[..split].iter().all(|line| line.starts_with("gate: remote sdks ")), "{lines:?}");
    }

    #[test]
    fn a_first_preflight_readies_the_bare_and_the_tree_and_says_its_lines_in_order() {
        let case = Case::new("first");
        let payload = case.payload(&json!({ "Needs": { "godot": "", "bogus": "1.0" } }));
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        let out = &console.out;
        assert_eq!(
            out[..4],
            [created_line(&case.bare()), version_line(), warm_line(None), NEW_WORK_TREE.to_owned()],
            "{out:?}"
        );
        assert!(out[4].starts_with("gate: remote boot "), "{out:?}");
        assert_sdks_then_tools(
            &out[5..],
            &["gate: remote tool godot missing", "gate: remote tool bogus skipped: unknown tool"],
        );

        let work = case.work();
        assert!(PathBuf::from(case.bare()).join("HEAD").is_file(), "the bare repository is made");
        let alternates = fs::read_to_string(format!("{work}/.git/objects/info/alternates")).expect("the alternates file is written");
        assert_eq!(alternates, format!("{}/objects\n", case.bare()));
        let config = fs::read_to_string(format!("{work}/.git/config")).expect("the tree's config is written");
        assert!(config.contains(&format!("url = {}", case.bare())), "{config}");
        assert!(config.contains("autocrlf = false") && config.contains("eol = lf"), "{config}");
        let root_mode = fs::metadata(case.root()).expect("the root is made").mode();
        assert_eq!(root_mode & 0o077, 0, "the root is the box user's alone: {root_mode:o}");
    }

    #[test]
    fn a_second_preflight_is_idempotent_and_reads_the_warm_record() {
        let case = Case::new("again");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);

        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out[..2], [version_line(), warm_line(None)], "no created, {:?}", console.out);
        assert!(console.out[2].starts_with("gate: remote boot "), "no new work tree: {:?}", console.out);
        assert_sdks_then_tools(&console.out[3..], &[]);

        fs::write(format!("{}.warm", case.work()), HASH).expect("the warm record is written");
        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[1], warm_line(Some(HASH)));

        fs::write(format!("{}.warm", case.work()), format!("{HASH}\n")).expect("the warm record is written");
        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[1], warm_line(None), "a line end is not a hash's, as on the Windows box");
    }

    /// Runs git with `args` to set a case up, its output dropped; fails the case unless git exits 0.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this sets a case's repository up"
    )]
    fn git_in_case(args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git starts");
        assert!(status.success(), "git {args:?}: {status}");
    }

    #[test]
    fn a_git_folder_that_is_no_repository_fails_its_step_and_leaves_a_repository_above_alone() {
        let case = Case::new("parent-repo");
        git_in_case(&["init", "-q", case.base.to_str().expect("the temp folder is UTF-8")]);
        let config = case.base.join(".git").join("config");
        let before = fs::read(&config).expect("the parent's config is read");
        fs::create_dir_all(format!("{}/.git", case.work())).expect("an empty .git is made");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_ne!(status, 0, "{:?}", console.out);
        let last = console.err.last().expect("the failure is said");
        assert!(last.starts_with("gate: remote git ") && last.contains(" failed"), "{:?}", console.err);
        assert_eq!(
            fs::read(&config).expect("the parent's config is read"),
            before,
            "the parent repository is not touched"
        );
    }

    #[test]
    fn a_git_step_that_names_no_repository_finds_none_above_the_work_folders() {
        let case = Case::new("ceiling");
        git_in_case(&["init", "-q", case.base.to_str().expect("the temp folder is UTF-8")]);
        let (root, parent) = (case.root(), format!("{}/work", case.root()));
        fs::create_dir_all(&parent).expect("the work folders' parent is made");

        let git = Git::of(&root, Path::new(&parent));
        let ran = git.run("git rev-parse", &["rev-parse", "--git-dir"]).expect("git runs");
        assert!(
            !ran.status.success(),
            "the repository above the root is not found: {}",
            String::from_utf8_lossy(&ran.stdout)
        );
    }

    /// Checks the preflight of `case` is refused with the folder line for `folder` and `why`, status 1.
    fn assert_folder_refused(case: &Case, folder: &str, why: &str) {
        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(console.err, [format!("gate: remote could not create the folder {folder}: {why}")]);
    }

    #[test]
    fn a_link_at_the_work_folders_parent_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-parent");
        let target = case.base.join("elsewhere");
        fs::create_dir_all(&target).expect("the link's target is made");
        fs::create_dir_all(case.root()).expect("the root is made");
        let parent = format!("{}/work", case.root());
        symlink(&target, &parent).expect("the link is made");

        assert_folder_refused(&case, &parent, "it is a symbolic link");
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn a_link_at_the_bare_repository_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-bare");
        let target = case.base.join("elsewhere.git");
        fs::create_dir_all(&target).expect("the link's target is made");
        fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
        symlink(&target, case.bare()).expect("the link is made");

        assert_folder_refused(&case, &case.bare(), "it is a symbolic link");
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn a_file_where_the_work_folder_belongs_is_refused_and_kept() {
        let case = Case::new("file-work");
        fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
        fs::write(case.work(), "kept\n").expect("the file is written");

        assert_folder_refused(&case, &case.work(), "it is not a folder");
        assert_eq!(fs::read(case.work()).expect("the file is read"), b"kept\n");
    }

    #[test]
    fn a_new_tree_drops_a_stale_warm_record() {
        let case = Case::new("stale-warm");
        fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
        let warm = format!("{}.warm", case.work());
        fs::write(&warm, HASH).expect("the warm record is written");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.out[..3],
            [created_line(&case.bare()), version_line(), warm_line(None)],
            "{:?}",
            console.out
        );
        assert!(!Path::new(&warm).exists(), "the record of a tree that is gone is removed");
    }

    #[test]
    fn a_tree_without_the_line_end_config_gets_it_on_its_next_preflight() {
        let case = Case::new("config");
        fs::create_dir_all(case.work()).expect("the work folder is made");
        git_in_case(&["init", "-q", &case.work()]);

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 0, "{:?}", console.err);
        let config = fs::read_to_string(format!("{}/.git/config", case.work())).expect("the tree's config is read");
        assert!(config.contains("autocrlf = false") && config.contains("eol = lf"), "{config}");
    }

    #[test]
    fn a_link_at_the_work_folder_is_refused_and_its_target_untouched() {
        let case = Case::new("linked-work");
        let target = case.base.join("elsewhere");
        fs::create_dir_all(&target).expect("the link's target is made");
        fs::create_dir_all(format!("{}/work", case.root())).expect("the work folders' parent is made");
        symlink(&target, case.work()).expect("the link is made");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1, "{:?}", console.out);
        assert_eq!(
            console.err,
            [format!("gate: remote could not create the folder {}: it is a symbolic link", case.work())]
        );
        assert_eq!(
            fs::read_dir(&target).expect("the target is read").count(),
            0,
            "nothing is made through the link"
        );
    }

    #[test]
    fn an_interrupted_command_record_makes_the_tree_new() {
        let case = Case::new("interrupted");
        let payload = case.payload(&json!({}));
        assert_eq!(preflighted(&payload).0, 0);
        fs::write(format!("{}.command", case.work()), "x").expect("the record is written");

        let (_, console) = preflighted(&payload);
        assert_eq!(console.out[2], NEW_WORK_TREE, "{:?}", console.out);
    }

    #[test]
    fn a_root_others_may_write_in_is_refused_as_not_locked_down() {
        let case = Case::new("open-root");
        let root = case.root();
        fs::create_dir(&root).expect("the root is made");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).expect("the root is opened to everyone");

        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 1);
        assert_eq!(console.out, [version_line()]);
        let why = format!("{root} is not owned by {} or is writable by others", user_name(effective_uid()));
        assert_eq!(console.err, [root_unsafe_line(&why)]);
        assert!(!PathBuf::from(&root).join("work").exists(), "nothing is made in a refused root");
    }

    #[test]
    fn a_bare_repository_outside_the_root_is_a_usage_error() {
        let case = Case::new("bare-outside");
        let bare = format!("{}/other/repo.git", case.root());
        let (status, console) = preflighted(&case.payload(&json!({ "Bare": bare })));
        assert_eq!(status, 2);
        assert_eq!(
            console.err,
            [format!("gate: box payload Bare must be <root>/<name>.git to lock its root down: {bare}")]
        );
        assert!(!PathBuf::from(case.root()).exists(), "nothing is made");
    }

    #[test]
    fn a_work_root_holding_a_colon_is_a_usage_error_and_nothing_is_made() {
        let case = Case::rooted("colon-root", "gate:remote");
        let (status, console) = preflighted(&case.payload(&json!({})));
        assert_eq!(status, 2, "{:?}", console.out);
        assert_eq!(
            console.err,
            [format!(
                "gate: box payload Work must be <root>/work/<key> with no ':' in <root>, which GIT_CEILING_DIRECTORIES cannot hold: {}",
                case.work()
            )]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(!PathBuf::from(case.root()).exists(), "nothing is made");
    }

    #[test]
    fn a_job_of_another_work_folder_is_a_usage_error() {
        let case = Case::new("job");
        let (status, console) = preflighted(&case.payload(&json!({ "Job": "gate-remote-other-0123456789ab" })));
        assert_eq!(status, 2);
        assert_eq!(
            console.err,
            ["gate: box payload Job is not this work folder's job name: gate-remote-other-0123456789ab"]
        );
    }

    #[test]
    fn a_pair_is_not_laid_out_here_and_its_first_run_is_new() {
        let case = Case::new("pair");
        let payload = case.payload(&json!({ "Pair": { "Leaf": "repo", "Siblings": ["lib"] } }));
        let (status, console) = preflighted(&payload);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.err.first().map(String::as_str), Some(PAIR_NOT_BUILT));
        assert_eq!(
            console.out[..3],
            [version_line(), warm_line(None), NEW_WORK_TREE.to_owned()],
            "{:?}",
            console.out
        );
        assert!(!PathBuf::from(case.work()).join(".git").exists(), "no tree is made");
    }
}

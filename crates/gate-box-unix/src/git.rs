//! How the box runs git: with no variable of git's own inherited, in the work folders' parent and under the work root's
//! ceiling ([`Git`]); the preflight's steps bounded and their standard error kept for the failure line, and a run's
//! steps ([`RunGit`]) bounded as the Windows box bounds them, with their standard error forwarded to the client line by
//! line as git says it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_wire::escaped;

use crate::console::{Forwarding, LineSink, Output, Stamp, Stream};
use crate::files::{self, ReplaceFailure};
use crate::lines::{FAILED_STATUS, is_git_variable, step_unran};
use crate::process::{Ending, Forward, GroupRecord, NotRan, Ran, Watch, run_bounded, run_watched, status_text};

/// How long one preflight git step may take.
const GIT_LIMIT: Duration = Duration::from_secs(120);
/// The bound of a run's [`GitClass::Short`] step, as the Windows box's.
const GIT_SHORT_LIMIT: Duration = Duration::from_mins(5);
/// The bound of a run's [`GitClass::Long`] step, as the Windows box's.
const GIT_LONG_LIMIT: Duration = Duration::from_mins(30);
/// The line said when a process a git step left, out of its group's reach, still holds git's output open, as the
/// Windows box says it.
pub const GIT_LEFTOVER: &str = "gate: remote git's output is still held open by a leftover process; stopped reading";

/// The bound class a run's git step is given by its caller, as the Windows box gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitClass {
    /// A step that reads or writes refs, config or a listing: [`GIT_SHORT_LIMIT`].
    Short,
    /// A step that moves a tree's or a repository's content (`fetch`, `checkout`, `clean`, `lfs checkout`, `add`):
    /// [`GIT_LONG_LIMIT`].
    Long,
}
/// How long a run's git step asked to end with `SIGTERM` has before its group is killed.
const END_GRACE: Duration = Duration::from_secs(2);
/// The most a git step may write on its standard output.
const GIT_OUTPUT_CAP: usize = 1024 * 1024;
/// The most lines of a failed preflight git step's standard error said before its failure line.
const GIT_ERROR_LINES: usize = 10;
/// The status of a run whose client closed the connection, as a lost connection ends.
pub const CLOSED_STATUS: u8 = 255;
/// The variable, set to `1`, that has git read no system-wide attributes file, as the Windows box sets it.
const NO_SYSTEM_ATTRIBUTES: &str = "GIT_ATTR_NOSYSTEM";
/// The variable, set to `1`, that has git read no system-wide config file, as `gate push` sets it.
const NO_SYSTEM_CONFIG: &str = "GIT_CONFIG_NOSYSTEM";
/// The git every step runs: macOS's own, owned by root, so no folder on a `PATH` can put another in its place.
const GIT_PROGRAM: &str = "/usr/bin/git";

/// A step that failed: the lines saying so, on stderr, and the status the box exits with.
#[derive(Debug)]
pub struct Failed {
    /// The lines, in order.
    pub lines: Vec<String>,
    /// The status.
    pub status: u8,
}

impl Failed {
    /// A failure said in one line, with [`FAILED_STATUS`].
    #[must_use]
    pub fn line(line: String) -> Failed {
        Failed {
            lines: vec![line],
            status: FAILED_STATUS,
        }
    }
}

/// How the box runs git: with no variable of git's own inherited ([`is_git_variable`]), so none points git at another
/// repository, config or program; in `folder`, the work root's `<root>/work`, so a step that names no repository
/// starts its search there and not in the box's inherited folder; and with `GIT_CEILING_DIRECTORIES` at `ceiling`, the
/// work root, which git never searches up into, so that search ends at `<root>/work` (git ignores a ceiling that is
/// the starting folder itself). The tree's steps name their repository besides ([`Tree`]). A run's git is found along,
/// and runs with, the login shell's `PATH` ([`Git::along`]).
pub struct Git<'a> {
    folder: &'a Path,
    ceiling: &'a str,
    path: Option<&'a str>,
}

impl<'a> Git<'a> {
    /// The git of the work root `root`, whose work folders are in `parent`, its `<root>/work`, on the box's own `PATH`.
    #[must_use]
    pub fn of(root: &'a str, parent: &'a Path) -> Git<'a> {
        Git {
            folder: parent,
            ceiling: root,
            path: None,
        }
    }

    /// This git found along, and run with, `path` as its `PATH`; `None` keeps the box's own.
    #[must_use]
    pub fn along(self, path: Option<&'a str>) -> Git<'a> {
        Git { path, ..self }
    }

    /// The command running git with `args` as this git runs it: [`GIT_PROGRAM`] by its path, never one found along a
    /// `PATH`, which still reaches git's own helpers and filters; no system-wide config read.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(test_program::get().unwrap_or_else(|| PathBuf::from(GIT_PROGRAM)));
        crate::process::with_path(&mut command, self.path);
        command.args(args).current_dir(self.folder);
        for (name, _) in std::env::vars_os().filter(|(name, _)| is_git_variable(name)) {
            command.env_remove(name);
        }
        command.env("GIT_CEILING_DIRECTORIES", self.ceiling).env(NO_SYSTEM_CONFIG, "1");
        command
    }

    /// Runs git with `args`, the git step `step`, to its exit within [`GIT_LIMIT`], its standard error kept; else the
    /// failure that it could not run.
    ///
    /// # Errors
    ///
    /// The failure of a git that could not start, ran past the limit, wrote more than the cap or whose exit could not
    /// be read.
    pub fn run(&self, step: &str, args: &[&str]) -> Result<Ran, Failed> {
        run_bounded(self.command(args), GIT_OUTPUT_CAP, Instant::now() + GIT_LIMIT).map_err(|not| {
            let why = match not {
                NotRan::Start(error) => error.to_string(),
                NotRan::TimedOut { .. } => format!("git did not answer in {} s", GIT_LIMIT.as_secs()),
                NotRan::OverCap => format!("git wrote more than {} MiB", GIT_OUTPUT_CAP / 1024 / 1024),
                NotRan::Failed(why) => why,
            };
            Failed::line(step_unran(step, &why))
        })
    }
}

/// The steps a git runs, each to its exit, and how a step that exited non-zero fails.
pub trait Steps {
    /// Runs git with `args`, the git step `step`, to its exit; else the failure that it could not run.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit.
    fn ran(&self, step: &str, args: &[&str]) -> Result<Ran, Failed>;

    /// The failure of the git step `step` that `ran` to a non-zero exit.
    fn failed(&self, step: &str, ran: &Ran) -> Failed;

    /// Runs the git step `step` with `args`; its standard output when it exits 0, else its failure.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit or exited non-zero.
    fn step(&self, step: &str, args: &[&str]) -> Result<Vec<u8>, Failed> {
        let ran = self.ran(step, args)?;
        self.passed(step, ran)
    }

    /// The standard output of the git step `step` that `ran` when it exited 0, else its failure ([`Steps::failed`]).
    ///
    /// # Errors
    ///
    /// The failure of a git that exited non-zero.
    fn passed(&self, step: &str, ran: Ran) -> Result<Vec<u8>, Failed> {
        if ran.status.success() {
            Ok(ran.stdout)
        } else {
            Err(self.failed(step, &ran))
        }
    }
}

impl Steps for Git<'_> {
    fn ran(&self, step: &str, args: &[&str]) -> Result<Ran, Failed> {
        self.run(step, args)
    }

    fn failed(&self, step: &str, ran: &Ran) -> Failed {
        step_failed(step, ran)
    }
}

/// The failure of the preflight's git step `step` that `ran` to a non-zero exit: the last lines of git's standard
/// error, then the exit's line ([`exit_failed`]).
#[must_use]
pub fn step_failed(step: &str, ran: &Ran) -> Failed {
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let said: Vec<&str> = stderr.lines().filter(|line| !line.trim().is_empty()).collect();
    let mut lines: Vec<String> = said[said.len().saturating_sub(GIT_ERROR_LINES)..].iter().copied().map(escaped).collect();
    let exited = exit_failed(step, ran.status);
    lines.extend(exited.lines);
    Failed {
        lines,
        status: exited.status,
    }
}

/// The failure of the git step `step` that exited with `status`, not 0: `gate: remote <step> failed (status <n>)`,
/// exiting with git's status; a git a signal ended fails with [`FAILED_STATUS`] and says so.
#[must_use]
pub fn exit_failed(step: &str, status: ExitStatus) -> Failed {
    match status.code() {
        Some(code) => Failed {
            lines: vec![format!("gate: remote {step} failed (status {code})")],
            status: u8::try_from(code).ok().filter(|code| *code != 0).unwrap_or(FAILED_STATUS),
        },
        None => Failed::line(format!("gate: remote {step} failed: git ended with {}", status_text(status))),
    }
}

/// The failure of the git step `step` whose forwarded output found the client gone: the run ends with
/// [`CLOSED_STATUS`], as the Windows box ends it, the line naming git once though a tree-readying step's name starts
/// with it.
#[must_use]
pub fn client_gone(step: &str) -> Failed {
    let step = step.strip_prefix("git ").unwrap_or(step);
    Failed {
        lines: vec![format!("gate: remote the client closed the connection during git {step}")],
        status: CLOSED_STATUS,
    }
}

/// Why a git step past its bound `limit` failed, as the Windows box says it: `git did not answer within <n> s`, or
/// `<n> ms` for a bound that is not whole seconds.
fn late_git_line(limit: Duration) -> String {
    let spelled = match limit.as_millis() % 1000 {
        0 => format!("{} s", limit.as_secs()),
        _ => format!("{} ms", limit.as_millis()),
    };
    format!("git did not answer within {spelled}")
}

/// The words naming a work folder's repository to git: its `.git` and its tree, so git never looks for another; and,
/// on a pair run, the repo's name, which its layout steps' names carry.
pub struct Tree {
    git_dir_arg: String,
    work_tree_arg: String,
    name: Option<String>,
}

impl Tree {
    /// The repository of the tree at `work`, a run of one repo's.
    #[must_use]
    pub fn at(work: &str) -> Tree {
        Tree::named(work, None)
    }

    /// The repository of the tree at `tree`, of the repo `name` on a pair run.
    #[must_use]
    pub fn named(tree: &str, name: Option<&str>) -> Tree {
        Tree {
            git_dir_arg: format!("--git-dir={tree}/.git"),
            work_tree_arg: format!("--work-tree={tree}"),
            name: name.map(str::to_owned),
        }
    }

    /// The layout step `step` in this repository as it is run ([`for_repo`]).
    fn step(&self, step: &str) -> String {
        for_repo(step, self.name.as_deref())
    }

    /// The failure `failed` of the layout step `step` in this repository ([`failed_for`]).
    fn failed_for(&self, step: &str, failed: Failed) -> Failed {
        failed_for(failed, step, self.name.as_deref())
    }

    /// Runs the layout step `step` with `args` in this repository through `git`, named for its repo ([`for_repo`]).
    fn layout_step(&self, git: &impl Steps, step: &str, args: &[&str]) -> Result<(), Failed> {
        git.step(&self.step(step), &self.args(args))
            .map(drop)
            .map_err(|failed| self.failed_for(step, failed))
    }

    /// The git words running `rest` in this repository.
    #[must_use]
    pub fn args<'a>(&'a self, rest: &[&'a str]) -> Vec<&'a str> {
        [self.git_dir_arg.as_str(), self.work_tree_arg.as_str()]
            .into_iter()
            .chain(rest.iter().copied())
            .collect()
    }
}

/// The layout step `step` as a pair run's step naming the repo `name` is said: `<step> for <name>`, as the Windows
/// box's `for_repo` names it, which cannot be read as one of the step's own words; `step` on a run of one repo.
#[must_use]
pub fn for_repo(step: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{step} for {name}"),
        None => step.to_owned(),
    }
}

/// The failure `failed` of the layout step `step`, run as [`for_repo`] names it: on a pair run its own line, wherever
/// it falls among git's error lines, says `<step> failed for <name>`, as the Windows box's `failed_for` says it; as it
/// is on a run of one repo.
#[must_use]
pub fn failed_for(failed: Failed, step: &str, name: Option<&str>) -> Failed {
    let Some(name) = name else {
        return failed;
    };
    let (said, windows) = (
        format!("gate: remote {step} for {name} failed"),
        format!("gate: remote {step} failed for {name}"),
    );
    let lines = failed
        .lines
        .into_iter()
        .map(|line| match line.strip_prefix(&said) {
            Some(rest) => format!("{windows}{rest}"),
            None => line,
        })
        .collect();
    Failed { lines, ..failed }
}

/// Sets the `origin` of the repository `tree` to `bare`: added when it has none, set when it names another. Each step
/// is named for the repo on a pair run ([`Tree::step`]).
///
/// # Errors
///
/// The failure of a git step.
pub fn origin_set(git: &impl Steps, tree: &Tree, bare: &str) -> Result<(), Failed> {
    let step = "git config remote.origin.url";
    let named = tree.step(step);
    let ran = git
        .ran(&named, &tree.args(&["config", "--get", "remote.origin.url"]))
        .map_err(|failed| tree.failed_for(step, failed))?;
    match ran.status.code() {
        Some(0) if String::from_utf8_lossy(&ran.stdout).trim_end_matches(['\r', '\n']) == bare => Ok(()),
        Some(0) => tree.layout_step(git, "git remote set-url", &["remote", "set-url", "origin", bare]),
        // `config --get` exits 1 for a key that is not set.
        Some(1) => tree.layout_step(git, "git remote add", &["remote", "add", "origin", bare]),
        _ => Err(tree.failed_for(step, git.failed(&named, &ran))),
    }
}

/// Writes the line-ending config of the repository `tree`, so the checkout and the sync-back keep the snapshot's bytes
/// whatever the builder's system and global git config say. Each step is named for the repo on a pair run
/// ([`Tree::step`]).
///
/// # Errors
///
/// The failure of a git step.
pub fn line_ends_set(git: &impl Steps, tree: &Tree) -> Result<(), Failed> {
    for (setting, value) in [("core.autocrlf", "false"), ("core.eol", "lf")] {
        tree.layout_step(git, &format!("git config {setting}"), &["config", setting, value])?;
    }
    Ok(())
}

/// Why a tree's alternates file was not written ([`alternates_written`]).
#[derive(Debug)]
pub enum AlternatesFailure {
    /// The folder `path`, `.git/objects/info`, could not be made.
    Folder {
        /// The folder.
        path: PathBuf,
        /// Why not.
        error: io::Error,
    },
    /// The file `path` could not be replaced, or its folder synced after.
    Write {
        /// The alternates file.
        path: PathBuf,
        /// Why not.
        failure: ReplaceFailure,
    },
}

/// Writes `<info>/alternates`, `info` a tree's `.git/objects/info`, as `<bare>/objects` and a line feed unless it
/// already holds exactly that, whole, so a git reading it meanwhile never finds it empty ([`files::replace_plain`]);
/// `info` is made when nothing is at its name. The caller judges a link at `info` before.
///
/// # Errors
///
/// The folder that could not be made, or the file that could not be replaced.
pub fn alternates_written(info: &Path, bare: &str) -> Result<(), AlternatesFailure> {
    if !files::exists(info) {
        fs::create_dir(info).map_err(|error| AlternatesFailure::Folder {
            path: info.to_owned(),
            error,
        })?;
    }
    let file = info.join("alternates");
    let wanted = format!("{bare}/objects\n");
    if files::read_plain(&file, files::RECORD_LIMIT).is_ok_and(|held| held == wanted.as_bytes()) {
        return Ok(());
    }
    files::replace_plain(&file, wanted.as_bytes()).map_err(|failure| AlternatesFailure::Write { path: file, failure })
}

/// How a run runs git ([`Git`]), with the Windows box's safety options before every step's words: its hooks read from
/// `<Lock>/hooks`, which cannot exist as the lock is a file; no file-system monitor; no attributes file but the tree's
/// own, nor the system's; and an LFS store below the lock, which cannot be made. Its standard error is forwarded to the
/// client line by line as git says it, from the readers' own threads, and its standard output kept for the caller, up
/// to [`GIT_OUTPUT_CAP`]. A step is bounded by the [`GitClass`] its caller gives it ([`step_limit`]); past its bound, or
/// once the client is gone, its group is asked to end with `SIGTERM` and killed after [`END_GRACE`], unless the step
/// writes the shared bare repository. Each step's group is written to the run's group record while it runs, when the
/// run gives one, so `box stop` ends it.
pub struct RunGit<'a, S: LineSink + 'static> {
    git: Git<'a>,
    options: [String; 8],
    output: &'a Arc<Output<S>>,
    record: Option<GroupRecord<'a>>,
    /// How the steps' forwarded lines and the runner's own lines about them are stamped: `gat` in `box run`.
    stamp: Stamp,
}

/// What a run's git step does with git's standard error and with a client gone.
#[derive(Clone, Copy)]
struct Mode {
    /// Whether git's standard error is forwarded; else it is read and dropped.
    forward: bool,
    /// Whether a failed forward ends git, and a client already gone keeps it from starting.
    ends_on_gone: bool,
    /// The most bytes of git's standard output kept; more fails the step.
    cap: usize,
}

impl<'a, S: LineSink + 'static> RunGit<'a, S> {
    /// The run's git `git`, the run holding the work lock `lock`, its lines said through `output`, each step's group
    /// written to `record` while it runs when one is given.
    #[must_use]
    pub fn new(git: Git<'a>, lock: &str, output: &'a Arc<Output<S>>, record: Option<GroupRecord<'a>>, stamp: Stamp) -> RunGit<'a, S> {
        let options = [
            "-c".to_owned(),
            format!("core.hooksPath={lock}/hooks"),
            "-c".to_owned(),
            "core.fsmonitor=false".to_owned(),
            "-c".to_owned(),
            "core.attributesFile=".to_owned(),
            "-c".to_owned(),
            format!("lfs.storage={lock}/lfs"),
        ];
        RunGit {
            git,
            options,
            output,
            record,
            stamp,
        }
    }

    /// The safety options, then `args`.
    fn args<'s>(&'s self, args: &[&'s str]) -> Vec<&'s str> {
        self.options.iter().map(String::as_str).chain(args.iter().copied()).collect()
    }

    /// Runs the step `step` of the class `class` with `args`, its standard error forwarded.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit, or a client gone.
    pub fn forwarded(&self, step: &str, args: &[&str], class: GitClass) -> Result<Ran, Failed> {
        let mode = Mode {
            forward: true,
            ends_on_gone: true,
            cap: GIT_OUTPUT_CAP,
        };
        self.ran_as(step, args, mode, class, &[])
    }

    /// Runs the step `step` of the class `class` with `args`, its standard output when it exits 0, else its failure.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit or exited non-zero, or a client gone.
    pub fn step_as(&self, step: &str, args: &[&str], class: GitClass) -> Result<Vec<u8>, Failed> {
        self.step_with(step, args, class, &[])
    }

    /// [`RunGit::step_as`], git given the variables `env` besides, set after the inherited git variables are removed.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit or exited non-zero, or a client gone.
    pub fn step_with(&self, step: &str, args: &[&str], class: GitClass, env: &[(&str, &str)]) -> Result<Vec<u8>, Failed> {
        let mode = Mode {
            forward: true,
            ends_on_gone: true,
            cap: GIT_OUTPUT_CAP,
        };
        let ran = self.ran_as(step, args, mode, class, env)?;
        self.passed(step, ran)
    }

    /// [`RunGit::step_as`], git's standard output kept whole however long it is, as the Windows box reads a step whose
    /// output lists paths.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit or exited non-zero, or a client gone.
    pub fn step_whole(&self, step: &str, args: &[&str], class: GitClass) -> Result<Vec<u8>, Failed> {
        let mode = Mode {
            forward: true,
            ends_on_gone: true,
            cap: usize::MAX,
        };
        let ran = self.ran_as(step, args, mode, class, &[])?;
        self.passed(step, ran)
    }

    /// Runs the step `step` of the class `class` with `args`, its standard error dropped, as the Windows box drops
    /// `cat-file`'s.
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit, or a client gone.
    pub fn quiet(&self, step: &str, args: &[&str], class: GitClass) -> Result<Ran, Failed> {
        let mode = Mode {
            forward: false,
            ends_on_gone: true,
            cap: GIT_OUTPUT_CAP,
        };
        self.ran_as(step, args, mode, class, &[])
    }

    /// Runs the step `step` of the class `class`, which writes the shared bare repository, with `args`, its standard
    /// error forwarded: a client gone neither keeps it from starting nor cuts it short, so the bare repository is never
    /// left half written, though the step still fails with [`client_gone`].
    ///
    /// # Errors
    ///
    /// The failure of a git that did not run to an exit or exited non-zero, or a client gone.
    pub fn bare_step(&self, step: &str, args: &[&str], class: GitClass) -> Result<(), Failed> {
        let mode = Mode {
            forward: true,
            ends_on_gone: false,
            cap: GIT_OUTPUT_CAP,
        };
        let ran = self.ran_as(step, args, mode, class, &[])?;
        self.passed(step, ran).map(drop)
    }

    /// Runs the step `step` with `args` as `mode` says, to its exit or the bound of `class`, git given the variables
    /// `env` besides. A client gone, seen by a failed write of any kind or by a probe of the box's output, fails the step
    /// with [`client_gone`].
    fn ran_as(&self, step: &str, args: &[&str], mode: Mode, class: GitClass, env: &[(&str, &str)]) -> Result<Ran, Failed> {
        if mode.ends_on_gone && self.output.client_gone_probe() {
            return Err(client_gone(step));
        }
        let mut command = self.git.command(&self.args(args));
        command.env(NO_SYSTEM_ATTRIBUTES, "1").envs(env.iter().copied());
        let limit = step_limit(step, class);
        let output: Arc<dyn Forwarding> = Arc::clone(self.output) as Arc<dyn Forwarding>;
        let watch = Watch {
            cap: mode.cap,
            deadline: Some(Instant::now() + limit),
            ending: Ending::TermThenKill(END_GRACE),
            output: Some(output),
            forward: if mode.forward { Forward::Err(self.stamp) } else { Forward::Kept },
            ends_on_gone: mode.ends_on_gone,
            record: self.record.as_ref(),
            check: None,
            on_started: None,
            on_leader_exit: None,
            processes: None,
        };
        let ran = run_watched(command, &watch);
        if self.output.client_gone() {
            return Err(client_gone(step));
        }
        // A quiet step forwards nothing, so, as on the Windows box, a leftover holding its output is not said.
        if mode.forward && ran.as_ref().is_ok_and(|ran| ran.leftover) {
            // A client that has gone has no one left to tell; the next step finds it gone.
            let _ = self.output.say(Stream::Err, self.stamp, GIT_LEFTOVER);
        }
        ran.map_err(|not| {
            let why = match not {
                NotRan::Start(error) => error.to_string(),
                NotRan::TimedOut { .. } => late_git_line(limit),
                NotRan::OverCap => format!("it printed more than {} bytes", mode.cap),
                NotRan::Failed(why) => why,
            };
            Failed::line(step_unran(step, &why))
        })
    }
}

impl<S: LineSink + 'static> Steps for RunGit<'_, S> {
    fn ran(&self, step: &str, args: &[&str]) -> Result<Ran, Failed> {
        self.forwarded(step, args, GitClass::Short)
    }

    fn failed(&self, step: &str, ran: &Ran) -> Failed {
        exit_failed(step, ran.status)
    }
}

/// The bound of the run's git step `step` of the class `class`, as the Windows box bounds it; a test's own bound in
/// place of both classes when it set one. A test build notes each step's class ([`test_limit::classes`]).
fn step_limit(step: &str, class: GitClass) -> Duration {
    test_limit::note(step, class);
    if let Some(limit) = test_limit::get() {
        return limit;
    }
    match class {
        GitClass::Short => GIT_SHORT_LIMIT,
        GitClass::Long => GIT_LONG_LIMIT,
    }
}

/// A bound a test sets for every run git step on its own thread, so it proves a bound in seconds; and the class each
/// step on the thread was given.
#[cfg(test)]
pub mod test_limit {
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use super::GitClass;

    thread_local! {
        static LIMIT: Cell<Option<Duration>> = const { Cell::new(None) };
        static CLASSES: RefCell<Vec<(String, GitClass)>> = const { RefCell::new(Vec::new()) };
    }

    /// Sets the bound of every run git step this thread runs from now on.
    pub fn set(limit: Duration) {
        LIMIT.with(|cell| cell.set(Some(limit)));
    }

    /// The bound this thread set, if any.
    pub fn get() -> Option<Duration> {
        LIMIT.with(Cell::get)
    }

    /// Notes that the step `step` was given `class`.
    pub fn note(step: &str, class: GitClass) {
        CLASSES.with(|classes| classes.borrow_mut().push((step.to_owned(), class)));
    }

    /// Each step this thread ran since the last call, with its class, in order.
    #[must_use]
    pub fn classes() -> Vec<(String, GitClass)> {
        CLASSES.with(|classes| std::mem::take(&mut *classes.borrow_mut()))
    }
}

/// No test bound outside the tests.
#[cfg(not(test))]
mod test_limit {
    use std::time::Duration;

    use super::GitClass;

    /// None.
    pub fn get() -> Option<Duration> {
        None
    }

    /// Nothing is noted.
    pub fn note(_step: &str, _class: GitClass) {}
}

/// A git program a test puts in place of [`GIT_PROGRAM`] for every step on its own thread, such as a shim that acts
/// between steps.
#[cfg(test)]
pub mod test_program {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static PROGRAM: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// Runs every git step this thread runs from now on with `program`.
    pub fn set(program: &Path) {
        PROGRAM.with(|cell| *cell.borrow_mut() = Some(program.to_owned()));
    }

    /// The program this thread set, if any.
    #[must_use]
    pub fn get() -> Option<PathBuf> {
        PROGRAM.with(|cell| cell.borrow().clone())
    }

    /// The first folder along `PATH` holding `git`, which a shim hands every step it does not act on.
    ///
    /// # Panics
    ///
    /// When `PATH` is unset or holds no `git`.
    #[must_use]
    #[expect(clippy::expect_used, reason = "a test without git on PATH must fail loudly")]
    pub fn real_git() -> PathBuf {
        let path = std::env::var_os("PATH").expect("PATH is set");
        std::env::split_paths(&path)
            .map(|folder| folder.join("git"))
            .find(|git| git.is_file())
            .expect("git is on PATH")
    }
}

/// No test program outside the tests.
#[cfg(not(test))]
mod test_program {
    use std::path::PathBuf;

    /// None.
    pub fn get() -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose temp folder cannot be made must fail loudly")]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{CLOSED_STATUS, GIT_LEFTOVER, GIT_OUTPUT_CAP, Git, GitClass, RunGit, Steps, test_limit};
    use crate::console::{FailingSink, HungUpSink, LineSink, Output, RecordingSink, Stamp, Stream};
    use crate::files::held_for_test;
    use crate::process::{GroupRecord, signal_log};

    /// A test's hang guard: far past what a step the case ends early takes, far short of the sleep it ends.
    const ENDED_WELL_BEFORE: Duration = Duration::from_secs(20);

    /// A temp folder of the case `name`, private to this process, removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-git-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(base.join("work")).expect("the case folder is made");
            Case { base }
        }

        fn root(&self) -> &str {
            self.base.to_str().expect("the temp folder is UTF-8")
        }

        fn parent(&self) -> PathBuf {
            self.base.join("work")
        }

        fn lock(&self) -> String {
            format!("{}/k.lock", self.parent().display())
        }

        fn record(&self) -> PathBuf {
            self.parent().join("k.pgid")
        }

        /// Runs `check` with the run's git of this case writing to `output`, its groups recorded when `recorded`.
        fn with_git_recorded<S: LineSink + 'static>(&self, output: &Arc<Output<S>>, recorded: bool, check: impl FnOnce(&RunGit<'_, S>)) {
            let parent = self.parent();
            let lock = self.lock();
            let held = recorded.then(|| held_for_test(Path::new(&lock)));
            let record = held
                .as_ref()
                .map(|held| GroupRecord::new(self.record(), "gate-remote-k-0123456789ab", held));
            check(&RunGit::new(Git::of(self.root(), Path::new(&parent)), &lock, output, record, Stamp::Bare));
        }

        /// Runs `check` with the run's git of this case writing to `output`.
        fn with_git<S: LineSink + 'static>(&self, output: &Arc<Output<S>>, check: impl FnOnce(&RunGit<'_, S>)) {
            self.with_git_recorded(output, false, check);
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// The words running the shell `script` as the git alias `x`.
    fn alias(script: &str) -> [String; 3] {
        ["-c".to_owned(), format!("alias.x=!{script}"), "x".to_owned()]
    }

    fn words(owned: &[String]) -> Vec<&str> {
        owned.iter().map(String::as_str).collect()
    }

    fn recording() -> Arc<Output<RecordingSink>> {
        Arc::new(Output::new(RecordingSink::default()))
    }

    #[test]
    fn a_run_step_forwards_each_line_git_says_on_stderr_and_fails_with_git_s_status() {
        let case = Case::new("forward");
        let output = recording();
        case.with_git(&output, |git| {
            let failed = git
                .step("rev-parse", &["rev-parse", "--verify", "nope"])
                .expect_err("no repository is found");
            assert_eq!(failed.lines, ["gate: remote rev-parse failed (status 128)"]);
            assert_eq!(failed.status, 128);
        });
        let said = output.sink().lines();
        assert!(!said.is_empty(), "git's own error is forwarded");
        assert!(said.iter().all(|(stream, _)| *stream == Stream::Err), "{said:?}");
        assert!(said.iter().all(|(_, line)| !line.ends_with('\n')), "{said:?}");
    }

    #[test]
    fn a_forwarded_line_is_handed_on_unescaped() {
        let case = Case::new("unescaped");
        let output = recording();
        let script = alias(r"printf 'a\033[1mb\tc\n' >&2");
        case.with_git(&output, |git| {
            git.step("x", &words(&script)).expect("the alias runs");
        });
        assert_eq!(output.sink().lines(), [(Stream::Err, "a\u{1b}[1mb\tc".to_owned())]);
    }

    #[test]
    fn a_run_step_whose_forward_finds_the_client_gone_is_ended_at_once_with_the_closed_status() {
        let case = Case::new("gone");
        let output = Arc::new(Output::new(FailingSink::new(io::ErrorKind::BrokenPipe)));
        let script = alias("echo a >&2; sleep 60");
        case.with_git(&output, |git| {
            let started = Instant::now();
            let failed = git.forwarded("fetch", &words(&script), GitClass::Long).expect_err("the client is gone");
            assert!(started.elapsed() < ENDED_WELL_BEFORE, "the step is ended, not waited for");
            assert_eq!(failed.lines, ["gate: remote the client closed the connection during git fetch"]);
            assert_eq!(failed.status, CLOSED_STATUS);

            let again = git
                .forwarded("git init", &["--version"], GitClass::Short)
                .expect_err("a gone client starts no more steps");
            assert_eq!(again.lines, ["gate: remote the client closed the connection during git init"]);
        });
    }

    #[test]
    fn a_forward_that_fails_with_a_reset_connection_sets_the_shared_flag_and_ends_the_step() {
        let case = Case::new("failed-forward");
        let output = Arc::new(Output::new(FailingSink::new(io::ErrorKind::ConnectionReset)));
        let script = alias("echo a >&2; sleep 60");
        case.with_git(&output, |git| {
            let started = Instant::now();
            let failed = git.forwarded("clean", &words(&script), GitClass::Long).expect_err("the forward failed");
            assert!(started.elapsed() < ENDED_WELL_BEFORE, "the step is ended, not waited for");
            assert_eq!(failed.status, CLOSED_STATUS);
        });
        assert!(output.client_gone(), "the shared flag is set");
    }

    #[test]
    fn a_quiet_step_whose_client_hung_up_is_ended_by_the_probe() {
        let case = Case::new("probe");
        let output = Arc::new(Output::new(HungUpSink));
        let script = alias("sleep 60");
        case.with_git(&output, |git| {
            let started = Instant::now();
            let failed = git.quiet("cat-file", &words(&script), GitClass::Short).expect_err("the client hung up");
            assert!(started.elapsed() < ENDED_WELL_BEFORE, "the step is not started, or ended at once");
            assert_eq!(failed.status, CLOSED_STATUS);
        });
    }

    /// A sink whose reader hangs up only once `marker` is made, which the step's own program makes after its spawn.
    struct HangsUpLater {
        marker: PathBuf,
    }

    impl LineSink for HangsUpLater {
        fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
            Ok(())
        }

        fn hung_up(&self) -> bool {
            self.marker.exists()
        }
    }

    #[test]
    fn a_client_that_hangs_up_while_a_quiet_step_waits_ends_it_at_the_next_probe_with_the_closed_status() {
        let case = Case::new("probe-later");
        let marker = case.base.join("spawned");
        let output = Arc::new(Output::new(HangsUpLater { marker: marker.clone() }));
        let script = alias(&format!("touch '{}'; sleep 60", marker.display()));
        case.with_git(&output, |git| {
            let started = Instant::now();
            let failed = git.quiet("cat-file", &words(&script), GitClass::Short).expect_err("the client hung up");
            assert!(started.elapsed() < ENDED_WELL_BEFORE, "ended at a probe, well inside its bound");
            assert_eq!(failed.status, CLOSED_STATUS);
        });
        assert!(marker.exists(), "the step was spawned before the client hung up");
    }

    /// A sink whose first write never returns, a client still connected that has stopped reading; every later write
    /// is kept.
    #[derive(Default)]
    struct StuckSink {
        stuck: AtomicBool,
        later: Mutex<Vec<String>>,
    }

    impl LineSink for StuckSink {
        fn write_line(&self, _stream: Stream, line: &str) -> io::Result<()> {
            if !self.stuck.swap(true, Ordering::SeqCst) {
                loop {
                    thread::park();
                }
            }
            self.later.lock().unwrap_or_else(PoisonError::into_inner).push(line.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_client_stalled_in_a_write_ends_the_step_closed_within_the_drain_bound_and_says_no_leftover_line() {
        let case = Case::new("stalled");
        let output = Arc::new(Output::new(StuckSink::default()));
        let script = alias("echo a >&2; echo b >&2");
        let (sender, receiver) = mpsc::channel();
        let started = Instant::now();
        let stepping = Arc::clone(&output);
        // On a thread of its own: a step that writes to the stalled client after the drain would never return.
        thread::spawn(move || {
            case.with_git(&stepping, |git| {
                let _ = sender.send(git.forwarded("fetch", &words(&script), GitClass::Long).map(|ran| ran.status));
            });
        });
        let ended = receiver
            .recv_timeout(Duration::from_secs(90))
            .expect("the step ends within the drain bound");
        assert!(started.elapsed() < Duration::from_secs(45), "the drain's whole bound is 30 s");
        let failed = ended.expect_err("the client is taken for gone");
        assert_eq!(failed.status, CLOSED_STATUS);
        assert!(output.client_gone(), "the shared flag is set");
        let later = output.sink().later.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(later.is_empty(), "nothing more is written: {later:?}");
    }

    #[test]
    fn a_bare_step_runs_to_its_end_though_the_client_is_gone() {
        let case = Case::new("bare-gone");
        let output = Arc::new(Output::new(FailingSink::new(io::ErrorKind::BrokenPipe)));
        let marker = case.base.join("finished");
        let script = alias(&format!("echo a >&2; sleep 1; touch '{}'", marker.display()));
        case.with_git(&output, |git| {
            let failed = git
                .bare_step("back ref reset", &words(&script), GitClass::Short)
                .expect_err("the client is gone");
            assert_eq!(failed.status, CLOSED_STATUS);
            assert!(marker.exists(), "the step was not cut short");
            let again = git
                .bare_step("back ref reset", &words(&script), GitClass::Short)
                .expect_err("the client is still gone");
            assert_eq!(again.status, CLOSED_STATUS, "a bare step still starts");
        });
    }

    #[test]
    fn a_step_past_its_bound_is_ended_with_the_windows_box_s_line() {
        let case = Case::new("bound");
        let output = recording();
        let script = alias("sleep 60");
        test_limit::set(Duration::from_millis(500));
        case.with_git(&output, |git| {
            let started = Instant::now();
            let failed = git
                .forwarded("fetch", &words(&script), GitClass::Long)
                .expect_err("the step is past its bound");
            assert!(started.elapsed() < ENDED_WELL_BEFORE, "the step is ended at its bound");
            assert_eq!(failed.lines, ["gate: remote fetch failed: git did not answer within 500 ms"]);
            assert_eq!(failed.status, 1);
        });
        let sent = signal_log::take();
        assert!(!sent.is_empty() && sent.iter().all(|sent| sent.unreaped), "{sent:?}");
    }

    #[test]
    fn a_step_s_group_is_recorded_while_it_runs_and_the_record_removed_after() {
        let case = Case::new("record");
        let output = recording();
        let record = case.record();
        let script = alias(&format!("cat '{}'", record.display()));
        case.with_git_recorded(&output, true, |git| {
            let said = git.step("x", &words(&script)).expect("the alias runs");
            let said = String::from_utf8_lossy(&said).into_owned();
            let mut lines = said.lines();
            assert_eq!(lines.next(), Some("gate-remote-k-0123456789ab"), "{said}");
            let group: i32 = lines.next().and_then(|line| line.parse().ok()).expect("the group is recorded");
            assert!(group > 1, "{said}");
        });
        assert!(!record.exists(), "the record is removed once the step ends");
        let names: Vec<String> = fs::read_dir(case.parent())
            .expect("the folder is read")
            .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|name| !name.contains("gate-new")), "no temp file is left: {names:?}");
    }

    #[test]
    fn a_step_whose_work_lock_is_no_longer_held_is_not_started() {
        let case = Case::new("lock-lost-before");
        let output = recording();
        let marker = case.base.join("started");
        let script = alias(&format!("touch '{}'", marker.display()));
        let lock = case.lock();
        case.with_git_recorded(&output, true, |git| {
            fs::remove_file(&lock).expect("the lock file is removed");
            let failed = git.step("x", &words(&script)).expect_err("the lock is lost");
            assert_eq!(
                failed.lines,
                [format!("gate: remote x failed: the work folder lock {lock} is no longer held")]
            );
            assert_eq!(failed.status, 1);
        });
        assert!(!marker.exists(), "the step was not started");
        assert!(!case.record().exists(), "no record is written");
    }

    #[test]
    fn a_step_whose_work_lock_is_lost_while_it_runs_fails_and_leaves_its_record() {
        let case = Case::new("lock-lost-during");
        let output = recording();
        let lock = case.lock();
        let script = alias(&format!("rm '{lock}'"));
        case.with_git_recorded(&output, true, |git| {
            let failed = git.step("x", &words(&script)).expect_err("the lock is lost");
            assert_eq!(
                failed.lines,
                [format!("gate: remote x failed: the work folder lock {lock} is no longer held")]
            );
            assert_eq!(failed.status, 1);
        });
        assert!(case.record().exists(), "the record is changed only under a held lock");
    }

    #[test]
    fn a_leftover_holding_git_s_output_is_said_and_git_s_status_kept() {
        let case = Case::new("leftover");
        let output = recording();
        // Git exits only once the perl has left its group, so the perl is out of reach of the leftovers' ending.
        let marker = case.base.join("escaped");
        let script = alias(&format!(
            "perl -e 'use POSIX; POSIX::setsid(); open(F, \">\", $ARGV[0]); close F; sleep 30' '{0}' >&2 & \
             while [ ! -e '{0}' ]; do sleep 0.05; done; echo $!",
            marker.display()
        ));
        case.with_git(&output, |git| {
            let started = Instant::now();
            let ran = git.forwarded("x", &words(&script), GitClass::Short).expect("the alias runs");
            let escaped: i32 = String::from_utf8_lossy(&ran.stdout).trim().parse().expect("the pid is said");
            // SAFETY: `escaped` is the perl this case started; the kill only ends it.
            unsafe {
                libc::kill(escaped, libc::SIGKILL);
            }
            assert!(started.elapsed() < ENDED_WELL_BEFORE);
            assert!(ran.status.success());
        });
        assert_eq!(output.sink().lines(), [(Stream::Err, GIT_LEFTOVER.to_owned())]);
    }

    #[test]
    fn a_leftover_holding_a_quiet_step_s_output_is_not_said_as_the_windows_box_says_none() {
        let case = Case::new("quiet-leftover");
        let output = recording();
        let marker = case.base.join("escaped");
        let script = alias(&format!(
            "perl -e 'use POSIX; POSIX::setsid(); open(F, \">\", $ARGV[0]); close F; sleep 30' '{0}' >&2 & \
             while [ ! -e '{0}' ]; do sleep 0.05; done; echo $!",
            marker.display()
        ));
        case.with_git(&output, |git| {
            let ran = git.quiet("x", &words(&script), GitClass::Short).expect("the alias runs");
            let escaped: i32 = String::from_utf8_lossy(&ran.stdout).trim().parse().expect("the pid is said");
            // SAFETY: `escaped` is the perl this case started; the kill only ends it.
            unsafe {
                libc::kill(escaped, libc::SIGKILL);
            }
            assert!(ran.leftover, "the escaped perl held git's standard error open");
            assert!(ran.status.success());
        });
        assert!(output.sink().lines().is_empty(), "{:?}", output.sink().lines());
    }

    #[test]
    fn standard_output_past_one_mib_fails_the_step_and_one_mib_fits() {
        let case = Case::new("stdout-cap");
        let output = recording();
        let over = alias(&format!("head -c {} /dev/zero", GIT_OUTPUT_CAP + 1));
        let fits = alias(&format!("head -c {GIT_OUTPUT_CAP} /dev/zero"));
        case.with_git(&output, |git| {
            let failed = git
                .forwarded("x", &words(&over), GitClass::Short)
                .expect_err("the output is over the cap");
            assert_eq!(failed.lines, ["gate: remote x failed: it printed more than 1048576 bytes"]);
            let ran = git.forwarded("x", &words(&fits), GitClass::Short).expect("exactly the cap fits");
            assert_eq!(ran.stdout.len(), GIT_OUTPUT_CAP);
        });
    }

    #[test]
    fn a_quiet_run_step_says_nothing_of_git_s_stderr() {
        let case = Case::new("quiet");
        let output = recording();
        case.with_git(&output, |git| {
            let ran = git
                .quiet("cat-file", &["rev-parse", "--verify", "nope"], GitClass::Short)
                .expect("git runs to its exit");
            assert!(!ran.status.success());
        });
        assert!(output.sink().lines().is_empty(), "{:?}", output.sink().lines());
    }

    #[test]
    fn every_git_step_runs_macos_s_own_git_by_its_path_reading_no_system_config() {
        let case = Case::new("program");
        let parent = case.parent();
        let command = Git::of(case.root(), &parent)
            .along(Some("/opt/homebrew/bin:/usr/bin"))
            .command(&["status"]);
        assert_eq!(command.get_program(), std::ffi::OsStr::new("/usr/bin/git"));
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for wanted in [("GIT_CONFIG_NOSYSTEM", "1"), ("PATH", "/opt/homebrew/bin:/usr/bin")] {
            assert!(envs.contains(&(wanted.0.to_owned(), Some(wanted.1.to_owned()))), "{wanted:?}: {envs:?}");
        }
    }

    #[test]
    fn a_run_step_carries_the_windows_box_s_safety_options() {
        let case = Case::new("options");
        let output = recording();
        let lock = case.lock();
        case.with_git(&output, |git| {
            for (key, value) in [
                ("core.hooksPath", format!("{lock}/hooks")),
                ("core.fsmonitor", "false".to_owned()),
                ("core.attributesFile", String::new()),
                ("lfs.storage", format!("{lock}/lfs")),
            ] {
                let said = git.step("git config", &["config", key]).expect("the option is set");
                assert_eq!(String::from_utf8_lossy(&said).trim_end(), value, "{key}");
            }
            let said = git.step("x", &words(&alias("printf %s \"$GIT_ATTR_NOSYSTEM\""))).expect("the alias runs");
            assert_eq!(said, b"1");
        });
    }
}

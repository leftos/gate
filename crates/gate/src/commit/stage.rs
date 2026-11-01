//! Whether a commit run's pre-commit hook stage may run on a builder as one run: a plain `git commit` of a repo
//! `remote.json`'s `commits` names, on a heavy or light slot, under no covering parent gate, in a repository whose
//! state the builder can reproduce from the staged tree alone. Everything else keeps the commit's hooks here.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gate_args::Slot;
use gate_router::escaped;

use super::{VALUED_OPTIONS, is_commit};
use crate::remote::git::{self, Git, GitBounds, GitError};
use crate::remote::syncback::{self, Attributes};

/// Why a commit's hook stage keeps to this PC; its [`fmt::Display`] text says it in a line's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ineligible {
    /// The command is no `git commit` at all.
    NotCommit,
    /// The commit is given this word, which no plain commit of the staged tree takes.
    Option(String),
    /// git is given this word before `commit`: another repository, worktree or folder, or a `core.hooksPath` of the
    /// caller's own.
    GlobalOption(String),
    /// This variable is set in the caller's environment.
    Environment(&'static str),
    /// The commit runs on no heavy or light slot.
    Slot,
    /// A parent gate covers the commit.
    Covered,
    /// `remote.json`'s `commits` does not name the repo.
    NotConsented,
    /// A merge, cherry-pick, revert or rebase is in progress: the name of the file or folder that says so.
    InProgress(&'static str),
    /// `HEAD` names no commit yet.
    UnbornHead,
    /// The repository is a shallow clone.
    Shallow,
    /// The pre-commit hook is missing or is not prek's shim.
    NotPrekHook,
    /// A `pre-commit.legacy` hook sits beside prek's shim, which prek would run too.
    LegacyHook,
    /// This prek config file has changes that are not staged.
    PrekConfig(String),
    /// The commit stages this path, which a sync-back never writes.
    StagedPath(String),
    /// The commit stages this path through this checkout filter.
    FilteredPath(String, String),
}

impl fmt::Display for Ineligible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ineligible::NotCommit => f.write_str("the command is no git commit"),
            Ineligible::Option(word) => write!(f, "the commit is given {}", escaped(word)),
            Ineligible::GlobalOption(word) => write!(f, "git is given {} before commit", escaped(word)),
            Ineligible::Environment(name) => write!(f, "{name} is set"),
            Ineligible::Slot => f.write_str("the commit runs on no heavy or light slot"),
            Ineligible::Covered => f.write_str("a parent gate covers the commit"),
            Ineligible::NotConsented => f.write_str("remote.json's commits does not name the repo"),
            Ineligible::InProgress(name) => write!(f, "a merge, cherry-pick, revert or rebase is in progress ({name})"),
            Ineligible::UnbornHead => f.write_str("HEAD names no commit yet"),
            Ineligible::Shallow => f.write_str("the repository is shallow"),
            Ineligible::NotPrekHook => f.write_str("the pre-commit hook is not prek's"),
            Ineligible::LegacyHook => f.write_str("a pre-commit.legacy hook sits beside prek's"),
            Ineligible::PrekConfig(path) => write!(f, "{} has changes that are not staged", escaped(path)),
            Ineligible::StagedPath(path) => write!(f, "the commit stages {}, which a sync-back never writes", escaped(path)),
            Ineligible::FilteredPath(path, attribute) => {
                write!(f, "the commit stages {} through {}", escaped(path), escaped(attribute))
            }
        }
    }
}

/// What the run knows of a commit before it asks the repository anything.
pub(crate) struct StageInput<'a> {
    /// The commit's command, `git` first.
    pub words: &'a [OsString],
    /// Whether the caller's environment sets the variable of this name, to any value, an empty one too; on Windows the
    /// lookup is the process environment's, which matches names in any letter case.
    pub is_set: &'a dyn Fn(&str) -> bool,
    /// The slot the commit runs on, after [`super::as_run`].
    pub slot: Slot,
    /// Whether a parent gate covers the commit.
    pub covered: bool,
    /// Whether `remote.json`'s `commits` names the commit's repo.
    pub consented: bool,
}

/// What the pre-commit hook the commit would run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hook {
    /// prek's shim, with nothing beside it.
    Prek,
    /// Missing, or a hook of another kind.
    NotPrek,
    /// prek's shim with a `pre-commit.legacy` beside it.
    Legacy,
}

/// The repository's facts a commit's stage is judged on, all read by [`stage_facts`] whatever any of them says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageFacts {
    /// The first of [`IN_PROGRESS`] that exists in the git folder.
    in_progress: Option<&'static str>,
    /// Whether `HEAD` names no commit.
    unborn: bool,
    /// Whether the repository is a shallow clone.
    shallow: bool,
    hook: Hook,
    /// The first prek config file with unstaged changes or untracked.
    prek_config: Option<String>,
    /// The first staged path a sync-back never writes; `None` for an unborn `HEAD`, which has nothing to diff against.
    staged_path: Option<String>,
    /// The first staged path written through a checkout filter, and the filter; `None` for an unborn `HEAD`.
    filtered_path: Option<(String, String)>,
}

/// The files and folders in the git folder that say a merge, cherry-pick, revert or rebase is in progress.
const IN_PROGRESS: [&str; 5] = ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"];
/// The variables whose being set in the caller's environment keeps the stage here: a commit of another index, git
/// folder, worktree or object store, one given config the gate's own git calls never see, which may name another
/// hooks folder, or one that skips hooks.
pub(crate) const BLOCKING_VARIABLES: [&str; 11] = [
    "GIT_INDEX_FILE",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "SKIP",
    "PREK_SKIP",
];
/// The prek config files whose unstaged changes the stage would not send.
const PREK_CONFIGS: [&str; 3] = ["prek.toml", ".pre-commit-config.yaml", ".pre-commit-config.yml"];
/// The line prek's shim carries.
const PREK_MARK: &str = "# File generated by prek";
/// The text a pre-commit shim of prek's carries.
const PRE_COMMIT_TYPE: &str = "--hook-type=pre-commit";

/// The commit options that take their value as the next word, or joined (`-mX`, `--message=X`).
const VALUED: [&str; 10] = [
    "-m",
    "--message",
    "-F",
    "--file",
    "--author",
    "--date",
    "--cleanup",
    "-t",
    "--template",
    "--trailer",
];
/// The commit options that take no value.
const FLAGS: [&str; 16] = [
    "-s",
    "--signoff",
    "--no-signoff",
    "-q",
    "--quiet",
    "-v",
    "--verbose",
    "-e",
    "--edit",
    "--no-edit",
    "--no-gpg-sign",
    "--allow-empty",
    "--allow-empty-message",
    "--no-post-rewrite",
    "--status",
    "--no-status",
];
/// The short commit options whose optional value is only ever joined (`-S<key>`, `-u<mode>`).
const OPTIONAL_SHORT: [&str; 2] = ["-S", "-u"];
/// The long commit options whose optional value is only ever joined by `=`.
const OPTIONAL_LONG: [&str; 2] = ["--gpg-sign", "--untracked-files"];
/// The git options before the subcommand that point git at another folder, repository or worktree.
const PLACING: [&str; 3] = ["-C", "--git-dir", "--work-tree"];

/// Whether the commit `input` describes, in a repository of `facts`, may run its hook stage on a builder; else the
/// first reason it may not, judged in this order: its words, the caller's environment, its slot, a covering parent
/// gate, `commits`' consent, then the repository's facts.
///
/// # Errors
///
/// The first [`Ineligible`] reason that keeps the stage here.
pub(crate) fn stage_eligible(input: &StageInput<'_>, facts: &StageFacts) -> Result<(), Ineligible> {
    judge_words(input.words)?;
    if let Some(name) = BLOCKING_VARIABLES.into_iter().find(|name| (input.is_set)(name)) {
        return Err(Ineligible::Environment(name));
    }
    let reasons = [
        (!matches!(input.slot, Slot::Heavy | Slot::Light)).then_some(Ineligible::Slot),
        input.covered.then_some(Ineligible::Covered),
        (!input.consented).then_some(Ineligible::NotConsented),
    ];
    match reasons.into_iter().flatten().next() {
        Some(reason) => Err(reason),
        None => judge_facts(facts),
    }
}

/// The first of `facts` that keeps the stage here, in [`StageFacts`]' order.
fn judge_facts(facts: &StageFacts) -> Result<(), Ineligible> {
    let reasons = [
        facts.in_progress.map(Ineligible::InProgress),
        facts.unborn.then_some(Ineligible::UnbornHead),
        facts.shallow.then_some(Ineligible::Shallow),
        (facts.hook == Hook::NotPrek).then_some(Ineligible::NotPrekHook),
        (facts.hook == Hook::Legacy).then_some(Ineligible::LegacyHook),
        facts.prek_config.clone().map(Ineligible::PrekConfig),
        facts.staged_path.clone().map(Ineligible::StagedPath),
        facts
            .filtered_path
            .clone()
            .map(|(path, attribute)| Ineligible::FilteredPath(path, attribute)),
    ];
    reasons.into_iter().flatten().next().map_or(Ok(()), Err)
}

/// Whether `words` are a plain `git commit` of the staged tree: git given nothing before `commit` that places it
/// elsewhere or sets `core.hooksPath`, and the commit given only options of the allow-list.
fn judge_words(words: &[OsString]) -> Result<(), Ineligible> {
    if !is_commit(words) {
        return Err(Ineligible::NotCommit);
    }
    let mut rest = words.iter().skip(1).map(|word| word.to_string_lossy());
    judge_global(&mut rest)?;
    judge_commit_options(rest)
}

/// Reads the words after `git` up to and including `commit`, refusing a placing option or a `core.hooksPath` setting.
fn judge_global<'a>(words: &mut impl Iterator<Item = Cow<'a, str>>) -> Result<(), Ineligible> {
    while let Some(word) = words.next() {
        if !word.starts_with('-') {
            return Ok(());
        }
        if places(&word) {
            return Err(Ineligible::GlobalOption(word.into_owned()));
        }
        let setting = match setting_of(&word) {
            Some(Setting::Joined(joined)) => Some(joined.to_owned()),
            Some(Setting::Next) => words.next().map(Cow::into_owned),
            None => {
                if VALUED_OPTIONS.contains(&word.as_ref()) {
                    words.next();
                }
                None
            }
        };
        if let Some(setting) = setting.filter(|setting| sets_hooks_path(setting)) {
            return Err(Ineligible::GlobalOption(setting));
        }
    }
    Ok(())
}

/// Whether the git option `word` points git at another folder, repository or worktree, in any form.
fn places(word: &str) -> bool {
    word.starts_with("-C")
        || PLACING[1..]
            .iter()
            .any(|option| word.strip_prefix(option).is_some_and(|rest| rest.is_empty() || rest.starts_with('=')))
}

/// Where a git option that sets a config value has its setting.
enum Setting<'a> {
    /// Joined to the option: `-c<key>=<value>`, `--config-env=<key>=<variable>`.
    Joined(&'a str),
    /// The next word.
    Next,
}

/// Where the git option `word` has the setting it makes; `None` for an option that sets no config value.
fn setting_of(word: &str) -> Option<Setting<'_>> {
    if word == "-c" || word == "--config-env" {
        return Some(Setting::Next);
    }
    word.strip_prefix("--config-env=")
        .or_else(|| word.strip_prefix("-c"))
        .map(Setting::Joined)
}

/// Whether the setting `text`, `<key>=<value>` or a bare key, sets `core.hooksPath`, its letter case aside.
fn sets_hooks_path(text: &str) -> bool {
    let key = text.split_once('=').map_or(text, |(key, _)| key);
    key.eq_ignore_ascii_case("core.hooksPath")
}

/// Reads the commit's own words, refusing any word the allow-list does not name; a value-taking option's value is
/// skipped whatever it starts with.
fn judge_commit_options<'a>(mut words: impl Iterator<Item = Cow<'a, str>>) -> Result<(), Ineligible> {
    while let Some(word) = words.next() {
        if VALUED.contains(&word.as_ref()) {
            words.next();
        } else if !allowed_alone(&word) {
            return Err(Ineligible::Option(word.into_owned()));
        }
    }
    Ok(())
}

/// Whether the commit word `word` is allowed and takes no next word: a flag, a value-taking option with its value
/// joined, or an option whose optional value is only ever joined.
fn allowed_alone(word: &str) -> bool {
    let joined_value = VALUED.iter().any(|option| match word.strip_prefix(option) {
        Some(rest) if option.starts_with("--") => rest.starts_with('='),
        Some(rest) => !rest.is_empty(),
        None => false,
    });
    let optional = OPTIONAL_SHORT.iter().any(|option| word.starts_with(option))
        || OPTIONAL_LONG
            .iter()
            .any(|option| word.strip_prefix(option).is_some_and(|rest| rest.is_empty() || rest.starts_with('=')));
    FLAGS.contains(&word) || joined_value || optional
}

/// git asked about one worktree under the bounds every call of the read shares.
struct Asked<'a> {
    git: &'a Git,
    root: &'a Path,
    bounds: GitBounds,
}

impl Asked<'_> {
    /// What git wrote on standard output for `args`, fed `stdin` when given.
    fn output(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, GitError> {
        self.git.output(&git::Call {
            stdin,
            ..git::Call::bounded(self.root, args, self.bounds)
        })
    }
}

/// The facts of the repository whose worktree is at `root`, every one read whatever another says, each git call of
/// `git` under `bounds`.
///
/// # Errors
///
/// Why a git call failed, or why the pre-commit hook could not be read.
pub(crate) fn stage_facts(root: &Path, git: &Git, bounds: GitBounds) -> Result<StageFacts, GitError> {
    let asked = Asked { git, root, bounds };
    let places = git_places(&asked)?;
    let hook = hook_of(&hook_path(&asked, &places.common)?)?;
    let prek_config = prek_config(&asked)?;
    let (staged_path, filtered_path) = if places.unborn { (None, None) } else { staged(&asked)? };
    Ok(StageFacts {
        in_progress: places.in_progress,
        unborn: places.unborn,
        shallow: places.shallow,
        hook,
        prek_config,
        staged_path,
        filtered_path,
    })
}

/// A staged path written through a checkout filter, and the filter as the sync-back names it.
type Filtered = (String, String);

/// What one `git rev-parse` call says of the repository.
struct Places {
    in_progress: Option<&'static str>,
    /// The repository's common git folder, which holds its own hooks folder.
    common: PathBuf,
    shallow: bool,
    unborn: bool,
}

/// The repository's [`Places`], from one `git rev-parse`: a `--git-path` answer for each of [`IN_PROGRESS`], then
/// `--git-common-dir`'s, then `--is-shallow-repository`'s `true` or `false`, then the commit of a born `HEAD`, which
/// git prints for no other.
fn git_places(asked: &Asked<'_>) -> Result<Places, GitError> {
    let mut args = vec!["rev-parse"];
    for name in IN_PROGRESS {
        args.extend(["--git-path", name]);
    }
    args.extend(["--git-common-dir", "--is-shallow-repository", "--revs-only", "HEAD^{commit}"]);
    let printed = asked.output(&args, None)?;
    let lines: Vec<&str> = printed.lines().map(str::trim_end).collect();
    let unreadable = || GitError::Failed(format!("git rev-parse did not print every path it was asked: {}", printed.trim()));
    let (Some(paths), Some(&shallow)) = (lines.get(..=IN_PROGRESS.len()), lines.get(IN_PROGRESS.len() + 1)) else {
        return Err(unreadable());
    };
    let shallow = match shallow {
        "true" => true,
        "false" => false,
        _ => return Err(unreadable()),
    };
    let head = lines.get(IN_PROGRESS.len() + 2);
    Ok(Places {
        in_progress: IN_PROGRESS
            .into_iter()
            .zip(paths)
            .find(|(_, path)| asked.root.join(path).exists())
            .map(|(name, _)| name),
        common: asked.root.join(paths[IN_PROGRESS.len()]),
        shallow,
        unborn: !head.is_some_and(|head| gate_wire::is_lower_hex(head, 40) || gate_wire::is_lower_hex(head, 64)),
    })
}

/// The hooks folder a commit in the worktree at `root` runs its hooks from ([`hooks_folder`]), each git call of `git`
/// under `bounds`.
///
/// # Errors
///
/// Why a git call failed.
pub(crate) fn real_hooks_folder(root: &Path, git: &Git, bounds: GitBounds) -> Result<PathBuf, GitError> {
    let asked = Asked { git, root, bounds };
    let places = git_places(&asked)?;
    hooks_folder(&asked, &places.common)
}

/// The pre-commit hook the commit would run, in its [`hooks_folder`].
fn hook_path(asked: &Asked<'_>, common: &Path) -> Result<PathBuf, GitError> {
    Ok(hooks_folder(asked, common)?.join("pre-commit"))
}

/// The folder the commit's hooks run from: the one the repository's own config last sets `core.hooksPath` to, read
/// against the worktree root, else the `hooks` folder of its `common` git folder. Every git call of the gate is given a
/// `core.hooksPath` of its own on the command line, so `--git-path hooks` would name that one; the config's values are
/// read with their origins instead, and the command line's left out.
fn hooks_folder(asked: &Asked<'_>, common: &Path) -> Result<PathBuf, GitError> {
    let listed = hooks_listing(asked)?;
    Ok(match configured_hooks(listed.as_deref()) {
        Some(folder) => asked.root.join(folder),
        None => common.join("hooks"),
    })
}

/// What `git config -z --show-origin --type=path --get-all core.hooksPath` wrote: `None` for its status 1, which says
/// the key is set nowhere; any other status but 0 is the call's failure.
fn hooks_listing(asked: &Asked<'_>) -> Result<Option<String>, GitError> {
    let args = ["config", "-z", "--show-origin", "--type=path", "--get-all", "core.hooksPath"];
    let call = git::Call::bounded(asked.root, &args, asked.bounds);
    let (status, output, errors) = asked.git.status_within(&call, git::OUTPUT_CAP)?;
    match status.code() {
        Some(0) => String::from_utf8(output)
            .map(Some)
            .map_err(|_| GitError::Failed("git's output is not valid UTF-8".to_owned())),
        Some(1) => Ok(None),
        code => Err(GitError::Failed(git::failed_line(&call, code, &errors, &output))),
    }
}

/// The last folder the repository's own config sets `core.hooksPath` to, from `git config -z --show-origin --get-all`'s
/// `listed` origin and value pairs, the command line's left out; `None` when `listed` is `None`, git config's answer
/// for a key set nowhere, or names only the command line's.
fn configured_hooks(listed: Option<&str>) -> Option<&str> {
    let fields: Vec<&str> = listed?.split('\0').collect();
    fields
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|[origin, _]| !origin.starts_with("command line:"))
        .map(|[_, folder]| *folder)
        .next_back()
}

/// What the pre-commit hook at `path` is: prek's shim carries [`PREK_MARK`] at a line's start and
/// [`PRE_COMMIT_TYPE`]; a missing file is no shim.
fn hook_of(path: &Path) -> Result<Hook, GitError> {
    let prek = match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            text.lines().any(|line| line.starts_with(PREK_MARK)) && text.contains(PRE_COMMIT_TYPE)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(GitError::Failed(format!("cannot read the pre-commit hook {}: {error}", path.display())));
        }
    };
    Ok(match (prek, path.with_file_name("pre-commit.legacy").exists()) {
        (false, _) => Hook::NotPrek,
        (true, true) => Hook::Legacy,
        (true, false) => Hook::Prek,
    })
}

/// The first prek config file with changes not staged, else the first untracked one, as git lists them. The plumbing
/// `diff-files` never refreshes the index, so it never writes the real one, and reads a file whose times alone changed
/// as changed.
fn prek_config(asked: &Asked<'_>) -> Result<Option<String>, GitError> {
    let mut changed = vec!["diff-files", "--name-only", "-z", "--"];
    changed.extend(PREK_CONFIGS);
    let mut untracked = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    untracked.extend(PREK_CONFIGS);
    let changed = asked.output(&changed, None)?;
    let untracked = asked.output(&untracked, None)?;
    Ok(first_path(&changed).or_else(|| first_path(&untracked)))
}

/// The first path of a `-z` listing.
fn first_path(listing: &str) -> Option<String> {
    listing.split('\0').find(|path| !path.is_empty()).map(str::to_owned)
}

/// The first staged path a sync-back never writes, and the first written through a checkout filter with its
/// filter, read from the raw listing of the index against `HEAD` as the sync-back reads a back commit's. The plumbing
/// `diff-index` reads none of the porcelain's diff config, so a `diff.ignoreSubmodules` hides no gitlink, and detects
/// no renames.
fn staged(asked: &Asked<'_>) -> Result<(Option<String>, Option<Filtered>), GitError> {
    let listing = asked.output(&["diff-index", "--cached", "-r", "-z", "--raw", "--no-abbrev", "HEAD"], None)?;
    let forbidden = syncback::forbidden(&listing);
    let paths = syncback::listed_paths(&listing);
    if paths.is_empty() {
        return Ok((forbidden, None));
    }
    let attributes = attributes(asked, &paths)?;
    let config = asked.output(&["config", "-z", "--list"], None)?;
    let filtered = syncback::filtered(&paths, &attributes, syncback::names_odd_driver(&config)).map(|(path, attribute)| (path.to_owned(), attribute));
    Ok((forbidden, filtered))
}

/// The worktree's checkout attributes of each of `paths`, in its order, read as the sync-back reads them, fed in
/// slices whose answers stay under git's output cap.
fn attributes(asked: &Asked<'_>, paths: &[&str]) -> Result<Vec<Attributes>, GitError> {
    let mut read = Vec::with_capacity(paths.len());
    for slice in syncback::attribute_slices(paths, git::OUTPUT_CAP) {
        let fed: String = slice.iter().flat_map(|path| [*path, "\0"]).collect();
        let answer = asked.output(&syncback::CHECK_ATTR_ARGS, Some(fed.as_bytes()))?;
        let attributes = syncback::read_attributes(&answer, slice)
            .map_err(|why| GitError::Failed(format!("git check-attr failed in {}: {why}", asked.root.display())))?;
        read.extend(attributes);
    }
    Ok(read)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its repository must fail loudly")]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use gate_args::Slot;

    use super::{Hook, Ineligible, StageFacts, StageInput, configured_hooks, stage_eligible, stage_facts};
    use crate::remote::git::{self, Git};
    use crate::test_support::temp_dir;

    /// prek 0.5's pre-commit shim.
    const SHIM: &str = "#!/bin/sh\n# File generated by prek: https://github.com/j178/prek\n# ID: 182c10f181da4464a3eec51b83331688\n\
                        HERE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\n\
                        exec prek hook-impl --hook-dir \"$HERE\" --script-version 4 --hook-type=pre-commit -- \"$@\"\n";

    /// Facts no reason keeps here.
    fn clean_facts() -> StageFacts {
        StageFacts {
            in_progress: None,
            unborn: false,
            shallow: false,
            hook: Hook::Prek,
            prek_config: None,
            staged_path: None,
            filtered_path: None,
        }
    }

    fn words(text: &[&str]) -> Vec<OsString> {
        text.iter().map(OsString::from).collect()
    }

    /// The judgement of a consented, uncovered heavy commit of `text` with no variable set, on clean facts.
    fn judged(text: &[&str]) -> Result<(), Ineligible> {
        judged_with(text, &|_: &str| false, Slot::Heavy)
    }

    fn judged_with(text: &[&str], is_set: &dyn Fn(&str) -> bool, slot: Slot) -> Result<(), Ineligible> {
        let words = words(text);
        let input = StageInput {
            words: &words,
            is_set,
            slot,
            covered: false,
            consented: true,
        };
        stage_eligible(&input, &clean_facts())
    }

    #[test]
    fn a_commit_given_a_message_or_file_in_any_form_is_eligible() {
        let cases: &[&[&str]] = &[
            &["git", "commit", "-m", "x"],
            &["git", "commit", "--message=x"],
            &["git", "commit", "--message", "x"],
            &["git", "commit", "-mx"],
            &["git", "commit", "-m", "-x"],
            &["git", "commit", "-F", "msg.txt"],
            &["git", "commit", "--file=msg.txt"],
            &["git", "commit", "--author", "A <a@b>", "--date=now", "--cleanup", "strip"],
            &["git", "commit", "-t", "t.txt", "--template=t.txt", "--trailer", "k=v", "--trailer=k=v"],
        ];
        for text in cases {
            assert_eq!(judged(text), Ok(()), "{text:?}");
        }
    }

    #[test]
    fn a_commit_given_flags_of_the_allow_list_is_eligible() {
        assert_eq!(judged(&["git", "commit", "-F", "msg.txt", "-s", "--no-edit"]), Ok(()));
        for flag in super::FLAGS {
            assert_eq!(judged(&["git", "commit", flag, "-m", "x"]), Ok(()), "{flag}");
        }
    }

    #[test]
    fn a_commit_given_an_optional_value_joined_is_eligible() {
        for word in [
            "-S",
            "-Skey",
            "--gpg-sign",
            "--gpg-sign=key",
            "--untracked-files",
            "--untracked-files=no",
            "-u",
            "-uno",
        ] {
            assert_eq!(judged(&["git", "commit", word, "-m", "x"]), Ok(()), "{word}");
        }
    }

    #[test]
    fn git_options_before_commit_that_neither_place_git_nor_set_hooks_are_eligible() {
        assert_eq!(judged(&["git", "-c", "user.name=x", "--no-pager", "commit", "-m", "y"]), Ok(()));
        assert_eq!(judged(&["git", "--attr-source", "HEAD", "commit", "-m", "y"]), Ok(()));
        assert_eq!(judged(&["git", "--config-env=user.name=NAME", "commit"]), Ok(()));
    }

    #[test]
    fn each_commit_option_outside_the_allow_list_is_refused_naming_it() {
        let cases: &[(&[&str], &str)] = &[
            (&["git", "commit", "-a", "-m", "x"], "-a"),
            (&["git", "commit", "-i", "a.txt"], "-i"),
            (&["git", "commit", "-o", "a.txt"], "-o"),
            (&["git", "commit", "-m", "x", "a.txt"], "a.txt"),
            (&["git", "commit", "--pathspec-from-file=f"], "--pathspec-from-file=f"),
            (&["git", "commit", "--interactive"], "--interactive"),
            (&["git", "commit", "-p"], "-p"),
            (&["git", "commit", "--patch"], "--patch"),
            (&["git", "commit", "--amend", "--no-edit"], "--amend"),
            (&["git", "commit", "-n", "-m", "x"], "-n"),
            (&["git", "commit", "--no-verify", "-m", "x"], "--no-verify"),
            (&["git", "commit", "--dry-run"], "--dry-run"),
            (&["git", "commit", "-m", "x", "--"], "--"),
            (&["git", "commit", "-am", "x"], "-am"),
            (&["git", "commit", "-sm", "x"], "-sm"),
            (&["git", "commit", "-C", "HEAD"], "-C"),
            (&["git", "commit", "--fixup=x"], "--fixup=x"),
        ];
        for (text, word) in cases {
            assert_eq!(judged(text), Err(Ineligible::Option((*word).to_owned())), "{text:?}");
        }
    }

    #[test]
    fn git_placed_elsewhere_or_given_its_own_hooks_path_before_commit_is_refused() {
        let cases: &[(&[&str], &str)] = &[
            (&["git", "-C", "x", "commit"], "-C"),
            (&["git", "--git-dir=.git", "commit"], "--git-dir=.git"),
            (&["git", "--git-dir", ".git", "commit"], "--git-dir"),
            (&["git", "--work-tree=.", "commit"], "--work-tree=."),
            (&["git", "--work-tree", ".", "commit"], "--work-tree"),
            (&["git", "-c", "core.hooksPath=h", "commit"], "core.hooksPath=h"),
            (&["git", "-c", "core.HooksPath=h", "commit"], "core.HooksPath=h"),
            (&["git", "-ccore.hookspath=h", "commit"], "core.hookspath=h"),
            (&["git", "--config-env", "core.hooksPath=V", "commit"], "core.hooksPath=V"),
            (&["git", "--config-env=CORE.HOOKSPATH=V", "commit"], "CORE.HOOKSPATH=V"),
        ];
        for (text, word) in cases {
            assert_eq!(judged(text), Err(Ineligible::GlobalOption((*word).to_owned())), "{text:?}");
        }
    }

    #[test]
    fn a_command_that_is_no_commit_is_refused() {
        assert_eq!(judged(&["git", "log"]), Err(Ineligible::NotCommit));
        assert_eq!(judged(&["prek", "run"]), Err(Ineligible::NotCommit));
    }

    #[test]
    fn each_variable_set_in_the_environment_is_refused_naming_it() {
        for name in [
            "GIT_INDEX_FILE",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "SKIP",
            "PREK_SKIP",
        ] {
            let is_set = |asked: &str| asked == name;
            assert_eq!(
                judged_with(&["git", "commit", "-m", "x"], &is_set, Slot::Heavy),
                Err(Ineligible::Environment(name)),
                "{name}"
            );
        }
    }

    #[test]
    fn the_words_are_judged_before_the_environment() {
        assert_eq!(
            judged_with(&["git", "commit", "-a"], &|_: &str| true, Slot::None),
            Err(Ineligible::Option("-a".to_owned()))
        );
    }

    #[test]
    fn a_commit_on_no_heavy_or_light_slot_is_refused() {
        let unset = |_: &str| false;
        assert_eq!(judged_with(&["git", "commit"], &unset, Slot::Light), Ok(()));
        for slot in [Slot::None, Slot::Critical] {
            assert_eq!(judged_with(&["git", "commit"], &unset, slot), Err(Ineligible::Slot), "{slot:?}");
        }
    }

    #[test]
    fn a_covered_or_unconsented_commit_is_refused() {
        let words = words(&["git", "commit"]);
        let unset = |_: &str| false;
        let input = |covered, consented| StageInput {
            words: &words,
            is_set: &unset,
            slot: Slot::Heavy,
            covered,
            consented,
        };
        assert_eq!(stage_eligible(&input(true, false), &clean_facts()), Err(Ineligible::Covered));
        assert_eq!(stage_eligible(&input(false, false), &clean_facts()), Err(Ineligible::NotConsented));
    }

    #[test]
    fn each_reason_says_its_line() {
        for (reason, line) in [
            (Ineligible::NotCommit, "the command is no git commit"),
            (Ineligible::Option("-a".to_owned()), "the commit is given -a"),
            (Ineligible::Option("x\u{202e}".to_owned()), r"the commit is given x\u{202e}"),
            (Ineligible::GlobalOption("-C".to_owned()), "git is given -C before commit"),
            (Ineligible::Environment("SKIP"), "SKIP is set"),
            (Ineligible::Slot, "the commit runs on no heavy or light slot"),
            (Ineligible::Covered, "a parent gate covers the commit"),
            (Ineligible::NotConsented, "remote.json's commits does not name the repo"),
            (
                Ineligible::InProgress("MERGE_HEAD"),
                "a merge, cherry-pick, revert or rebase is in progress (MERGE_HEAD)",
            ),
            (Ineligible::UnbornHead, "HEAD names no commit yet"),
            (Ineligible::Shallow, "the repository is shallow"),
            (Ineligible::NotPrekHook, "the pre-commit hook is not prek's"),
            (Ineligible::LegacyHook, "a pre-commit.legacy hook sits beside prek's"),
            (
                Ineligible::PrekConfig("prek.toml".to_owned()),
                "prek.toml has changes that are not staged",
            ),
            (
                Ineligible::StagedPath(".gitattributes".to_owned()),
                "the commit stages .gitattributes, which a sync-back never writes",
            ),
            (
                Ineligible::FilteredPath("x.bin".to_owned(), "filter=lfs".to_owned()),
                "the commit stages x.bin through filter=lfs",
            ),
        ] {
            assert_eq!(reason.to_string(), line);
        }
    }

    /// What git with `args` wrote in `folder`.
    fn git_in(folder: &Path, args: &[&str]) -> String {
        let git = Git::find(folder).expect("git is found");
        git.output(&git::Call::bounded(folder, args, git::GIT_BOUNDS))
            .expect("a test's git call succeeds")
    }

    /// A new, empty git repository in the unit tests' temp folder for the test `name`.
    fn folder(name: &str) -> PathBuf {
        let root = temp_dir().join(format!("gate-stage-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("a repository left by an earlier run can be removed");
        }
        fs::create_dir_all(&root).expect("the repository's folder can be made");
        git_in(&root, &["init", "--quiet"]);
        root
    }

    /// Stages every change in `root` and commits it with no hook run.
    fn commit_all(root: &Path, message: &str) {
        git_in(root, &["add", "-A"]);
        git_in(
            root,
            &[
                "-c",
                "user.name=gate-test",
                "-c",
                "user.email=gate-test@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ],
        );
    }

    /// A repository for the test `name` with `a.txt` committed and prek's shim as its pre-commit hook.
    fn repository(name: &str) -> PathBuf {
        let root = folder(name);
        fs::write(root.join("a.txt"), "a\n").expect("a file can be written");
        commit_all(&root, "first");
        write_hook(&root.join(".git").join("hooks"), SHIM);
        root
    }

    fn write_hook(folder: &Path, text: &str) {
        fs::create_dir_all(folder).expect("the hooks folder can be made");
        fs::write(folder.join("pre-commit"), text).expect("the hook can be written");
    }

    /// Writes `text` to `path` in `root` and stages it.
    fn stage(root: &Path, path: &str, text: &str) {
        fs::write(root.join(path), text).expect("a file can be written");
        git_in(root, &["add", "--", path]);
    }

    /// The judgement of a plain consented heavy commit in the repository at `root`.
    fn judged_in(root: &Path) -> Result<(), Ineligible> {
        let git = Git::find(root).expect("git is found");
        let facts = stage_facts(root, &git, git::GIT_BOUNDS).expect("the facts can be read");
        let words = words(&["git", "commit", "-m", "x"]);
        let unset = |_: &str| false;
        let input = StageInput {
            words: &words,
            is_set: &unset,
            slot: Slot::Heavy,
            covered: false,
            consented: true,
        };
        stage_eligible(&input, &facts)
    }

    #[test]
    fn a_clean_repository_with_prek_s_shim_and_a_staged_change_is_eligible() {
        let root = repository("eligible");
        stage(&root, "b.txt", "b\n");
        assert_eq!(judged_in(&root), Ok(()));
        removed(&root);
    }

    /// Removes the test repository at `root`.
    fn removed(root: &Path) {
        fs::remove_dir_all(root).expect("the repository can be removed");
    }

    #[test]
    fn a_staged_gitlink_is_refused_whatever_diff_ignore_submodules_says() {
        let root = repository("gitlink");
        git_in(&root, &["config", "diff.ignoreSubmodules", "all"]);
        git_in(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,sub",
            ],
        );
        assert_eq!(judged_in(&root), Err(Ineligible::StagedPath("sub".to_owned())));
        removed(&root);
    }

    #[test]
    fn reading_the_facts_leaves_the_real_index_as_it_was() {
        let root = repository("index-kept");
        fs::write(root.join("prek.toml"), "[[repos]]\n").expect("a file can be written");
        commit_all(&root, "prek");
        fs::write(root.join("prek.toml"), "[[repos]]\n").expect("a file can be rewritten");
        let later = SystemTime::now() + Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(root.join("prek.toml"))
            .expect("the file can be opened")
            .set_modified(later)
            .expect("its time can be set");
        let index = root.join(".git").join("index");
        let before = fs::read(&index).expect("the index can be read");

        let git = Git::find(&root).expect("git is found");
        stage_facts(&root, &git, git::GIT_BOUNDS).expect("the facts can be read");

        assert_eq!(fs::read(&index).expect("the index can be read"), before, "the real index is unchanged");
        removed(&root);
    }

    #[test]
    fn the_real_hooks_folder_is_the_configured_one_read_against_the_root_else_the_common_folder_s() {
        let root = repository("hooks-folder");
        let git = Git::find(&root).expect("git is found");

        let default = super::real_hooks_folder(&root, &git, git::GIT_BOUNDS).expect("the folder is read");
        git_in(&root, &["config", "core.hooksPath", "my-hooks"]);
        let configured = super::real_hooks_folder(&root, &git, git::GIT_BOUNDS).expect("the folder is read");

        assert_eq!(default, root.join(".git").join("hooks"), "the common git folder's hooks");
        assert_eq!(configured, root.join("my-hooks"), "the configured folder, read against the worktree root");
        removed(&root);
    }

    #[test]
    fn a_hooks_path_set_nowhere_or_only_on_the_command_line_is_none() {
        assert_eq!(configured_hooks(None), None, "git config's status 1: the key is set nowhere");
        assert_eq!(configured_hooks(Some("command line:\0C:/git/hooks\0")), None, "the gate's own");
        assert_eq!(
            configured_hooks(Some("file:.git/config\0shared\0command line:\0C:/git/hooks\0file:.git/config\0last\0")),
            Some("last"),
            "the config's last value"
        );
    }

    #[test]
    fn a_merge_in_progress_is_refused() {
        let root = repository("merging");
        stage(&root, "b.txt", "b\n");
        let head = git_in(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join(".git").join("MERGE_HEAD"), head).expect("MERGE_HEAD can be written");
        assert_eq!(judged_in(&root), Err(Ineligible::InProgress("MERGE_HEAD")));
        removed(&root);
    }

    #[test]
    fn an_unborn_head_is_refused() {
        let root = folder("unborn");
        write_hook(&root.join(".git").join("hooks"), SHIM);
        stage(&root, "a.txt", "a\n");
        assert_eq!(judged_in(&root), Err(Ineligible::UnbornHead));
        removed(&root);
    }

    #[test]
    fn a_shallow_clone_is_refused() {
        let source = repository("shallow-source");
        fs::write(source.join("a.txt"), "b\n").expect("a file can be written");
        commit_all(&source, "second");
        let clone = temp_dir().join(format!("gate-stage-shallow-clone-{}", std::process::id()));
        if clone.exists() {
            fs::remove_dir_all(&clone).expect("a clone left by an earlier run can be removed");
        }
        let path = source.to_string_lossy().replace('\\', "/");
        let url = if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        };
        git_in(temp_dir(), &["clone", "--quiet", "--depth", "1", &url, &clone.to_string_lossy()]);
        write_hook(&clone.join(".git").join("hooks"), SHIM);
        stage(&clone, "b.txt", "b\n");
        assert_eq!(judged_in(&clone), Err(Ineligible::Shallow));
        removed(&clone);
        removed(&source);
    }

    #[test]
    fn a_missing_hook_or_one_not_prek_s_is_refused() {
        let root = repository("no-hook");
        stage(&root, "b.txt", "b\n");
        fs::remove_file(root.join(".git").join("hooks").join("pre-commit")).expect("the hook can be removed");
        assert_eq!(judged_in(&root), Err(Ineligible::NotPrekHook), "no hook");
        write_hook(&root.join(".git").join("hooks"), "#!/bin/sh\nexit 0\n");
        assert_eq!(judged_in(&root), Err(Ineligible::NotPrekHook), "another hook");
        write_hook(
            &root.join(".git").join("hooks"),
            &SHIM.replace("--hook-type=pre-commit", "--hook-type=pre-push"),
        );
        assert_eq!(judged_in(&root), Err(Ineligible::NotPrekHook), "prek's shim of another stage");
        removed(&root);
    }

    #[test]
    fn a_legacy_hook_beside_prek_s_is_refused() {
        let root = repository("legacy");
        stage(&root, "b.txt", "b\n");
        fs::write(root.join(".git").join("hooks").join("pre-commit.legacy"), "#!/bin/sh\n").expect("the hook can be written");
        assert_eq!(judged_in(&root), Err(Ineligible::LegacyHook));
        removed(&root);
    }

    #[test]
    fn an_unstaged_edit_to_the_prek_config_is_refused() {
        let root = repository("prek-edited");
        fs::write(root.join("prek.toml"), "[[repos]]\n").expect("a file can be written");
        commit_all(&root, "prek");
        fs::write(root.join("prek.toml"), "[[repos]]\nrepo = \"local\"\n").expect("a file can be written");
        stage(&root, "b.txt", "b\n");
        assert_eq!(judged_in(&root), Err(Ineligible::PrekConfig("prek.toml".to_owned())));
        removed(&root);
    }

    #[test]
    fn an_untracked_prek_config_is_refused() {
        let root = repository("prek-untracked");
        fs::write(root.join("prek.toml"), "[[repos]]\n").expect("a file can be written");
        stage(&root, "b.txt", "b\n");
        assert_eq!(judged_in(&root), Err(Ineligible::PrekConfig("prek.toml".to_owned())));
        removed(&root);
    }

    #[test]
    fn a_staged_path_a_sync_back_never_writes_is_refused() {
        let root = repository("forbidden");
        stage(&root, ".gitattributes", "*.txt text\n");
        assert_eq!(judged_in(&root), Err(Ineligible::StagedPath(".gitattributes".to_owned())));
        removed(&root);
    }

    #[test]
    fn a_staged_path_through_a_checkout_filter_is_refused() {
        let root = repository("filtered");
        fs::write(root.join(".gitattributes"), "*.bin filter=lfs -text\n").expect("a file can be written");
        commit_all(&root, "attributes");
        stage(&root, "x.bin", "x\n");
        assert_eq!(
            judged_in(&root),
            Err(Ineligible::FilteredPath("x.bin".to_owned(), "filter=lfs".to_owned()))
        );
        removed(&root);
    }

    #[test]
    fn prek_s_shim_where_core_hooks_path_points_is_eligible() {
        let root = repository("hooks-path");
        fs::remove_file(root.join(".git").join("hooks").join("pre-commit")).expect("the hook can be removed");
        git_in(&root, &["config", "core.hooksPath", "shared-hooks"]);
        write_hook(&root.join("shared-hooks"), SHIM);
        stage(&root, "b.txt", "b\n");
        assert_eq!(judged_in(&root), Ok(()));
        removed(&root);
    }
}

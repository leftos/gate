//! The routing decision: whether a gate's command runs on the builder and, when it does, the words, folder, key and
//! outputs of that run. The rules apply in order, and each fact is asked of the caller ([`Facts`]) only at the rule
//! that needs it: a light or critical gate, `--local` or a covering parent read no routing file, and a machine with no
//! routing file costs no git call.

use std::path::PathBuf;

use crate::config::{Config, ConfigProblem, ignored_line};
use crate::key::{repo_name, work_folder_key};
use crate::like::eq_ignoring_case;
use crate::rewrite::{drive_letters, rewrite_words};

/// The line said when `--no-build` was dropped from the builder's words.
pub const DROPPED_NO_BUILD_LINE: &str = "gate: dropped --no-build: the last build may have run on the other machine";
/// The line said when a `--no-build` command brings no outputs back.
pub const NO_OUTPUTS_LINE: &str = "gate: outputs: none brought back for a --no-build command";

const NO_BUILD: &str = "--no-build";

/// The word that ends a dotnet command's own arguments: what follows it goes to the program the command runs.
const SEPARATOR: &str = "--";
/// The `MSBuild` property a routed build pins the worktree's `HEAD` sha to, so the builder does not throw away a
/// build whose revision changed.
const SOURCE_REVISION: &str = "SourceRevisionId";
/// The switches that set an `MSBuild` property; their case and the property name's are ignored.
const PROPERTY_SWITCHES: [&str; 5] = ["-p:", "/p:", "-property:", "/property:", "--property:"];
/// The dotnet verbs that build: the ones a command-line property reaches `MSBuild` through.
const BUILD_VERBS: [&str; 6] = ["build", "test", "publish", "pack", "run", "msbuild"];

/// The slot kind a gate asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Heavy,
    Light,
    Critical,
    /// No slot: a run that takes nothing, never routed.
    None,
}

/// What the caller knows before the decision asks anything.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The gate's slot kind; only a heavy gate routes.
    pub slot: Slot,
    /// Whether a covering or lending parent slot holds this gate, which then runs on the parent's slot.
    pub covering_parent: bool,
    /// Whether `--local` was given.
    pub local: bool,
    /// Whether `GIT_INDEX_FILE` is set to a non-empty value: the gate runs inside a git hook.
    pub git_hook: bool,
    /// The command's words as given.
    pub words: &'a [String],
}

/// The gate's place in its git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The worktree root as `git rev-parse --show-toplevel` gives it.
    pub root: String,
    /// The gate's folder below the root as `git rev-parse --show-prefix` gives it: `sub/dir/`, empty at the root.
    pub prefix: String,
    /// The main checkout's folder name, the parent of the common git folder, as written.
    pub checkout: String,
}

/// The facts the decision asks the caller for, each only when a rule needs it.
pub trait Facts {
    /// The routing file, read and given to [`Config::parse`]: `Ok(None)` when there is no such file. Asked only of a
    /// heavy gate under no covering or lending parent and without `--local`.
    ///
    /// # Errors
    ///
    /// The file and why it cannot be used, when it is there but is not a file, cannot be read or is refused; the gate
    /// then runs here, saying it ignored the file.
    fn config(&self) -> Result<Option<Config>, ConfigProblem>;

    /// The gate's place, `Ok(None)` when git cannot place its folder: any non-zero `git rev-parse` exit (no worktree
    /// among them) runs here silently, as the PowerShell gate's `Get-GatePlace` does.
    ///
    /// # Errors
    ///
    /// Only when git cannot be started or its output cannot be read; the gate then runs here, saying so.
    fn place(&self) -> Result<Option<Place>, String>;

    /// Whether the gate's folder holds a file the snapshot sends, tracked or untracked and not ignored. Asked only
    /// below the root.
    ///
    /// # Errors
    ///
    /// Why it could not be told; the gate then runs here, saying so.
    fn folder_has_files(&self, place: &Place) -> Result<bool, String>;

    /// The sha of the worktree root's `HEAD`, `Ok(None)` when it has none: a `git rev-parse --verify -q HEAD` that
    /// fails (an unborn branch among them) or writes anything but one sha is no sha. Asked only of a command that gets
    /// the source-revision word.
    ///
    /// # Errors
    ///
    /// Only when git cannot be started, outlives its limit, or its output cannot be read; the run then routes without
    /// the word, as it does for `Ok(None)`.
    fn head_sha(&self, place: &Place) -> Result<Option<String>, String>;

    /// Whether a file of exactly this name is in the gate's folder.
    fn file_in_folder(&self, name: &str) -> bool;

    /// What the drive `letter` (upper case) stands for, as `QueryDosDevice` gives it (`\??\X:\dev` for a subst drive);
    /// `None` when there is no such device.
    fn subst_target(&self, letter: char) -> Option<String>;
}

/// Where the command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Here, with the line to say when there is one.
    Local { reason: Option<LocalReason> },
    /// On the builder.
    Routed(Box<RemoteRun>),
}

/// Why a command runs here, for each case that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalReason {
    /// `--local` was given on a heavy gate.
    LocalFlag,
    /// The routing file at `path` is there but cannot be used, for `reason`.
    Ignored { path: PathBuf, reason: String },
    /// A matched command's word, as given, names a path outside the worktree.
    Outside { word: String },
    /// The gate's folder holds nothing the snapshot sends; `subfolder` is the prefix without its trailing `/`.
    NothingSent { subfolder: String },
    /// A fact could not be read; `reason` is on one line.
    Failed { reason: String },
}

impl LocalReason {
    /// The line the gate says, on standard error and in the log.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::LocalFlag => "gate: not routed: --local".to_owned(),
            Self::Ignored { path, reason } => ignored_line(path, reason),
            Self::Outside { word } => format!("gate: not routed: {word} is outside the worktree"),
            Self::NothingSent { subfolder } => format!("gate: not routed: {subfolder} holds nothing the snapshot sends"),
            Self::Failed { reason } => format!("gate: not routed: {reason}"),
        }
    }
}

/// A command routed to the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRun {
    /// The builder's host alias.
    pub host: String,
    /// The remote pool's size.
    pub slots: u32,
    /// The worktree root as git gives it.
    pub root: String,
    /// The repo name the routing file listed.
    pub repo: String,
    /// The builder's work folder ([`work_folder_key`]).
    pub key: String,
    /// The gate's folder below the root, without a trailing `/`; empty at the root.
    pub subfolder: String,
    /// The pattern that matched.
    pub pattern: String,
    /// The words the builder runs: paths rewritten, `--no-build` dropped from a dotnet command, the worktree's `HEAD`
    /// sha pinned onto a dotnet command that builds as `SourceRevisionId`, and a bare first word naming a file of the
    /// gate's folder written `./name`.
    pub words: Vec<String>,
    /// The words every fallback here runs: the command as given, each exact `--no-build` word dropped from a dotnet
    /// command.
    pub fallback: Vec<String>,
    /// The output globs the run brings back; none for a dotnet `--no-build` command.
    pub outputs: Vec<String>,
    /// Whether `--no-build` was dropped from the builder's words.
    pub dropped_no_build: bool,
    /// Whether outputs were withheld from a `--no-build` command.
    pub outputs_withheld: bool,
}

impl RemoteRun {
    /// The lines the gate says for this route, in order.
    #[must_use]
    pub fn lines(&self) -> Vec<&'static str> {
        let mut lines = Vec::new();
        if self.dropped_no_build {
            lines.push(DROPPED_NO_BUILD_LINE);
        }
        if self.outputs_withheld {
            lines.push(NO_OUTPUTS_LINE);
        }
        lines
    }
}

/// Where a gate's command runs. In order: only a heavy gate routes; a covering or lending parent keeps it here; `--local`
/// keeps it here saying so; then the routing file is read, and no file keeps it here, an unusable one saying it was
/// ignored; a git hook, no non-empty word, or a first word whose file name without extension is `git` or `prek` keep it
/// here; so does a folder in no git worktree, a repo the routing file does not list, and words (paths rewritten, joined
/// by single spaces) none of the repo's patterns match, the first that matches winning. A matched command still runs
/// here, saying why, when a word names a path outside the worktree or the gate's folder holds nothing the snapshot
/// sends; and so does one whose facts cannot be read.
#[must_use]
pub fn decide(inputs: &Inputs<'_>, facts: &dyn Facts) -> Route {
    match route(inputs, facts) {
        Ok(run) => Route::Routed(Box::new(run)),
        Err(reason) => Route::Local { reason },
    }
}

/// A decision to run here: the reason to say, if any.
type Here = Option<LocalReason>;

/// A command a pattern matched, with what the rest of the decision needs.
struct Matched<'a> {
    config: &'a Config,
    place: Place,
    repo: String,
    pattern: String,
    words: Vec<String>,
    outside: Option<String>,
}

fn route(inputs: &Inputs<'_>, facts: &dyn Facts) -> Result<RemoteRun, Here> {
    let config = eligible(inputs, facts)?;
    let matched = matched(&config, inputs.words, facts)?;
    if let Some(word) = matched.outside.clone() {
        return Err(Some(LocalReason::Outside { word }));
    }
    let subfolder = matched.place.prefix.trim_end_matches('/').to_owned();
    if !matched.place.prefix.is_empty() && !facts.folder_has_files(&matched.place).map_err(|reason| Some(failed(&reason)))? {
        return Err(Some(LocalReason::NothingSent { subfolder }));
    }
    Ok(remote_run(matched, subfolder, inputs.words, facts))
}

/// The routing file when the gate may route at all, before any git fact is asked; the file is read only after the
/// slot, parent and `--local` checks.
fn eligible(inputs: &Inputs<'_>, facts: &dyn Facts) -> Result<Config, Here> {
    if inputs.slot != Slot::Heavy || inputs.covering_parent {
        return Err(None);
    }
    if inputs.local {
        return Err(Some(LocalReason::LocalFlag));
    }
    let config = facts
        .config()
        .map_err(|ConfigProblem { path, reason }| Some(LocalReason::Ignored { path, reason }))?
        .ok_or(None)?;
    if inputs.git_hook || stays_local(inputs.words) {
        return Err(None);
    }
    Ok(config)
}

/// Whether the command can never route: it has no non-empty word, or its first one is `git` or `prek`, however
/// spelled (`git.exe`, a full path to it).
fn stays_local(words: &[String]) -> bool {
    first_word(words).is_none_or(|(_, word)| {
        let stem = file_stem(word);
        eq_ignoring_case(stem, "git") || eq_ignoring_case(stem, "prek")
    })
}

fn is_dotnet(words: &[String]) -> bool {
    first_word(words).is_some_and(|(_, word)| eq_ignoring_case(file_stem(word), "dotnet"))
}

/// The first non-empty word and its index.
fn first_word(words: &[String]) -> Option<(usize, &str)> {
    words
        .iter()
        .enumerate()
        .find(|(_, word)| !word.is_empty())
        .map(|(index, word)| (index, word.as_str()))
}

/// A path's file name without its extension, as .NET's `Path.GetFileNameWithoutExtension` gives it on Windows.
fn file_stem(path: &str) -> &str {
    let path = match path.as_bytes() {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => &path[2..],
        _ => path,
    };
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rfind('.').map_or(name, |dot| &name[..dot])
}

/// The place, repo and rewritten words of a command one of its repo's patterns matches.
fn matched<'a>(config: &'a Config, words: &[String], facts: &dyn Facts) -> Result<Matched<'a>, Here> {
    let place = facts.place().map_err(|reason| Some(failed(&reason)))?.ok_or(None)?;
    let repo = repo_name(&place.checkout);
    let patterns = config.patterns(&repo).ok_or(None)?;
    let drives = subst_drives(&place.root, words, facts);
    let sent = rewrite_words(&place.root, &place.prefix, words, &drives);
    let command = sent.words.join(" ");
    let pattern = patterns.iter().find(|pattern| pattern.is_match(&command)).ok_or(None)?;
    Ok(Matched {
        config,
        repo,
        pattern: pattern.as_str().to_owned(),
        words: sent.words,
        outside: sent.outside,
        place,
    })
}

/// The subst table for the drive letters the words use other than the root's: each letter whose device target names
/// a path (`\??\X:\dev`), mapped to that path. A volume or a missing device adds nothing.
fn subst_drives(root: &str, words: &[String], facts: &dyn Facts) -> Vec<(char, String)> {
    drive_letters(root, words)
        .into_iter()
        .filter_map(|letter| {
            let target = facts.subst_target(letter)?;
            target.strip_prefix(r"\??\").map(|path| (letter, path.to_owned()))
        })
        .collect()
}

fn remote_run(matched: Matched<'_>, subfolder: String, given: &[String], facts: &dyn Facts) -> RemoteRun {
    let dotnet = is_dotnet(given);
    let sent_count = matched.words.len();
    let mut words = without_no_build(matched.words, dotnet);
    let dropped_no_build = words.len() != sent_count;
    let fallback = without_no_build(given.to_vec(), dotnet);
    let mut outputs = matched.config.outputs(&matched.repo);
    let outputs_withheld = fallback.len() != given.len() && !outputs.is_empty();
    if outputs_withheld {
        outputs.clear();
    }
    mark_bare_first_word(&mut words, facts);
    stamp_source_revision(&mut words, dotnet, &matched.place, facts);
    RemoteRun {
        host: matched.config.host().to_owned(),
        slots: matched.config.slots(),
        key: work_folder_key(&matched.place.root),
        root: matched.place.root,
        repo: matched.repo,
        subfolder,
        pattern: matched.pattern,
        words,
        fallback,
        outputs,
        dropped_no_build,
        outputs_withheld,
    }
}

/// The words without each that is exactly `--no-build`, for a dotnet command; any other command's words unchanged.
fn without_no_build(words: Vec<String>, dotnet: bool) -> Vec<String> {
    if !dotnet {
        return words;
    }
    words.into_iter().filter(|word| word != NO_BUILD).collect()
}

/// Writes the first non-empty word `./name` when it is a bare name (no separator, drive or `.`/`..`) of a file in the
/// gate's folder, which the builder would otherwise look for on its `PATH`.
fn mark_bare_first_word(words: &mut [String], facts: &dyn Facts) {
    let Some((index, word)) = first_word(words) else {
        return;
    };
    let bare = !word.contains(['\\', '/', ':']) && word != "." && word != "..";
    if bare && facts.file_in_folder(word) {
        words[index] = format!("./{word}");
    }
}

/// Writes `-p:SourceRevisionId=<the worktree's HEAD sha>` into the builder's words of a dotnet command that builds,
/// before the first word that is exactly `--` (or after the last word when it has none), so the builder's build is
/// pinned to the revision the snapshot came from. The words of a command that is no such dotnet command, that already
/// sets `SourceRevisionId` before its `--`, or whose HEAD cannot be read are left as they are.
fn stamp_source_revision(words: &mut Vec<String>, dotnet: bool, place: &Place, facts: &dyn Facts) {
    let Some(at) = stamp_at(words, dotnet) else {
        return;
    };
    if let Ok(Some(sha)) = facts.head_sha(place) {
        words.insert(at, format!("-p:{SOURCE_REVISION}={sha}"));
    }
}

/// Where the source-revision word goes in the builder's words, or `None` when the command gets none: not a dotnet
/// command, no verb that builds, or a word before the `--` that already sets `SourceRevisionId`.
fn stamp_at(words: &[String], dotnet: bool) -> Option<usize> {
    if !dotnet || !builds(words) {
        return None;
    }
    let before = words.iter().position(|word| word == SEPARATOR).unwrap_or(words.len());
    (!words[..before].iter().any(|word| sets_property(word, SOURCE_REVISION))).then_some(before)
}

/// Whether a dotnet command's words build: the next non-empty word after the dotnet word is one of [`BUILD_VERBS`],
/// compared ignoring letter case.
fn builds(words: &[String]) -> bool {
    first_word(words).is_some_and(|(index, _)| {
        words[index + 1..]
            .iter()
            .find(|word| !word.is_empty())
            .is_some_and(|verb| BUILD_VERBS.iter().any(|building| eq_ignoring_case(verb, building)))
    })
}

/// Whether a word sets the `MSBuild` property `name`: one of [`PROPERTY_SWITCHES`] and a `;`- or `,`-separated list in
/// which one setting's name (the text before its `=`, the whole setting when it has none) is `name`, the case ignored.
fn sets_property(word: &str, name: &str) -> bool {
    let Some(switch) = PROPERTY_SWITCHES
        .iter()
        .copied()
        .find(|switch| word.get(..switch.len()).is_some_and(|start| eq_ignoring_case(start, switch)))
    else {
        return false;
    };
    word[switch.len()..]
        .split([',', ';'])
        .any(|setting| eq_ignoring_case(setting.split_once('=').map_or(setting, |(setting, _)| setting), name))
}

/// A fact's failure as the reason said, its line breaks and the white space around them folded into one space.
fn failed(reason: &str) -> LocalReason {
    LocalReason::Failed { reason: fold_lines(reason) }
}

fn fold_lines(text: &str) -> String {
    let parts: Vec<&str> = text.split(['\r', '\n']).collect();
    let [first, middle @ .., last] = parts.as_slice() else {
        return text.to_owned();
    };
    let mut pieces = vec![first.trim_end()];
    pieces.extend(middle.iter().map(|part| part.trim()).filter(|part| !part.is_empty()));
    pieces.push(last.trim_start());
    pieces.join(" ")
}

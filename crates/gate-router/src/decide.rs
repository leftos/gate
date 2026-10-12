//! The routing decision: whether a gate's command runs on the builder and, when it does, the words, folder, key and
//! outputs of that run. The rules apply in order, and each fact is asked of the caller ([`Facts`]) only at the rule
//! that needs it: a light or critical gate, `--local` or a covering parent read no routing file, and a machine with no
//! routing file costs no git call.

use std::ops::Range;
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

/// The word that ends a dotnet command's own arguments: what follows it goes to the program the command runs. After a
/// wrapping script's words it starts the dotnet command the script is handed.
const SEPARATOR: &str = "--";
/// The program name of a dotnet command.
const DOTNET: &str = "dotnet";
/// The program names whose script, handed a dotnet command after its `--`, counts as that dotnet command.
const WRAPPERS: [&str; 2] = ["pwsh", "powershell"];
/// The `MSBuild` property a routed build pins the worktree's `HEAD` sha to, so the builder does not throw away a
/// build whose revision changed.
const SOURCE_REVISION: &str = "SourceRevisionId";
/// The switches that set an `MSBuild` property; their case and the property name's are ignored.
const PROPERTY_SWITCHES: [&str; 5] = ["-p:", "/p:", "-property:", "/property:", "--property:"];
/// The dotnet verbs that build: the ones a command-line property reaches `MSBuild` through.
const BUILD_VERBS: [&str; 6] = ["build", "test", "publish", "pack", "run", "msbuild"];
/// The dotnet verbs whose `--no-build` the gate drops; `pack` and `publish` keep theirs.
const NO_BUILD_DROP_VERBS: [&str; 2] = ["test", "run"];
/// The dotnet verb that hands its words to `MSBuild` as they are: a target such as `-t:Restore` may build nothing.
const MSBUILD: &str = "msbuild";
/// The option that names the project a dotnet command builds.
const PROJECT_OPTION: &str = "--project";
/// The file extensions of a project file, compared ignoring letter case.
pub const PROJECT_EXTENSIONS: [&str; 3] = [".csproj", ".fsproj", ".vbproj"];
/// The file extensions of a solution file, compared ignoring letter case.
const SOLUTION_EXTENSIONS: [&str; 3] = [".sln", ".slnx", ".slnf"];
/// The start of a dotnet word that names a response file, whose words the gate cannot see.
const RESPONSE_FILE: char = '@';
/// The spellings of the options [`BuildSettings`] reads: short, then long.
const CONFIGURATION_OPTIONS: [&str; 2] = ["-c", "--configuration"];
const FRAMEWORK_OPTIONS: [&str; 2] = ["-f", "--framework"];
const RUNTIME_OPTIONS: [&str; 2] = ["-r", "--runtime"];
/// The other dotnet build options that take the next word as their value when written without `=` or `:`; a value of
/// an option not listed reads as a named target ([`BuildTarget::Other`]).
const VALUE_OPTIONS: [&str; 18] = [
    "-o",
    "--output",
    "-v",
    "--verbosity",
    "-a",
    "--arch",
    "--os",
    "-s",
    "--source",
    "--settings",
    "-l",
    "--logger",
    "--filter",
    "--results-directory",
    "--collect",
    "--version-suffix",
    "-lp",
    "--launch-profile",
];

/// The configuration, framework and runtime a dotnet command's own words set (`-c`, `-f`, `-r` and their long forms),
/// each as written and `None` when the words set none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildSettings {
    pub configuration: Option<String>,
    pub framework: Option<String>,
    pub runtime: Option<String>,
}

impl BuildSettings {
    /// Whether `other` sets each of the three as these do, ignoring letter case; one set and one absent differ.
    #[must_use]
    pub fn same_as(&self, other: &BuildSettings) -> bool {
        let same = |left: &Option<String>, right: &Option<String>| match (left, right) {
            (Some(left), Some(right)) => left.to_lowercase() == right.to_lowercase(),
            (None, None) => true,
            _ => false,
        };
        same(&self.configuration, &other.configuration) && same(&self.framework, &other.framework) && same(&self.runtime, &other.runtime)
    }
}

/// What a dotnet build's own words name ([`sdk_build_target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildTarget {
    /// A solution file.
    Solution,
    /// Nothing: the build is of the folder it runs in.
    Unnamed,
    /// Anything else: a project, `--project`, a folder, a file-based app or a word the gate cannot read.
    Other,
}

/// What a dotnet command's own words name to build ([`dotnet_target`]), as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotnetTarget {
    /// One project: a project file, or the value of `--project`, a project file or its folder.
    Project(String),
    /// A solution file, which lists the projects it builds.
    Solution(String),
}

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

/// Whether the words are a dotnet command: the first non-empty word's file name without extension is `dotnet`, or the
/// first non-empty word's is `pwsh` or `powershell` and the word just after its first `--` is such a `dotnet` word (a
/// script handed a dotnet command to run), each name compared ignoring letter case.
#[must_use]
pub fn is_dotnet(words: &[String]) -> bool {
    dotnet_start(words).is_some()
}

/// Whether the words are a dotnet command ([`is_dotnet`]) that builds: the dotnet command's verb, the next non-empty
/// word after its dotnet word, is one of [`BUILD_VERBS`] ignoring letter case, and none of its own words is exactly
/// `--no-build`.
#[must_use]
pub fn dotnet_builds(words: &[String]) -> bool {
    dotnet_own(words).is_some_and(|own| builds(&words[own.clone()]) && !words[own].iter().any(|word| word == NO_BUILD))
}

/// The project or solution a dotnet command ([`is_dotnet`]) names among its own words: the value of `--project`
/// (`--project X`, `--project=X` or `--project:X`; empty when it has none), or the first word not starting with `-`
/// or `/` whose extension, ignoring letter case, is a project's or a solution's. `None` for a command that names
/// neither, a folder argument included, and for any other command.
#[must_use]
pub fn dotnet_target(words: &[String]) -> Option<DotnetTarget> {
    let own = dotnet_own(words)?;
    let mut rest = words[own].iter().skip(1);
    while let Some(word) = rest.next() {
        if word == PROJECT_OPTION {
            return Some(DotnetTarget::Project(rest.next().cloned().unwrap_or_default()));
        }
        if let Some(value) = word.strip_prefix(PROJECT_OPTION).and_then(|after| after.strip_prefix(['=', ':'])) {
            return Some(DotnetTarget::Project(value.to_owned()));
        }
        if word.starts_with(['-', '/']) {
            continue;
        }
        if has_extension(word, &PROJECT_EXTENSIONS) {
            return Some(DotnetTarget::Project(word.clone()));
        }
        if has_extension(word, &SOLUTION_EXTENSIONS) {
            return Some(DotnetTarget::Solution(word.clone()));
        }
    }
    None
}

/// The configuration, framework and runtime a dotnet command's own words set, the last of each spelling winning; all
/// `None` for a command that is no dotnet command ([`is_dotnet`]) or names a response file.
#[must_use]
pub fn build_settings(words: &[String]) -> BuildSettings {
    let Some(own) = dotnet_own(words) else {
        return BuildSettings::default();
    };
    let own = &words[own];
    BuildSettings {
        configuration: option_value(own, &CONFIGURATION_OPTIONS),
        framework: option_value(own, &FRAMEWORK_OPTIONS),
        runtime: option_value(own, &RUNTIME_OPTIONS),
    }
}

/// The value of the last of `own`'s words spelled as one of `spellings`: `-c X`, `-c=X` or `-c:X`.
fn option_value(own: &[String], spellings: &[&str]) -> Option<String> {
    let mut value = None;
    let mut rest = own.iter();
    while let Some(word) = rest.next() {
        for spelling in spellings {
            if word == spelling {
                value = rest.next().cloned();
            } else if let Some(inline) = word.strip_prefix(spelling).and_then(|after| after.strip_prefix(['=', ':'])) {
                value = Some(inline.to_owned());
            }
        }
    }
    value
}

/// What a dotnet build names among its own words, for a command that builds ([`dotnet_builds`]) with an SDK verb other
/// than `msbuild`: a solution file, nothing, or anything else ([`BuildTarget`]). Every word not starting with `-` or
/// `/` that is no value of a known option ([`BuildSettings`]'s, [`VALUE_OPTIONS`]) is a named target. `None` for any
/// other command.
#[must_use]
pub fn sdk_build_target(words: &[String]) -> Option<BuildTarget> {
    let own = dotnet_own(words)?;
    let own = &words[own];
    if !dotnet_builds(words) || verb(own).is_some_and(|verb| eq_ignoring_case(verb, MSBUILD)) {
        return None;
    }
    if matches!(dotnet_target(words), Some(DotnetTarget::Project(_))) {
        return Some(BuildTarget::Other);
    }
    let takes_value = |word: &str| {
        [&CONFIGURATION_OPTIONS[..], &FRAMEWORK_OPTIONS, &RUNTIME_OPTIONS, &VALUE_OPTIONS]
            .iter()
            .any(|spellings| spellings.contains(&word))
    };
    let mut named = Vec::new();
    let mut rest = own.iter().skip(verb_at(own)? + 1);
    while let Some(word) = rest.next() {
        if takes_value(word) {
            rest.next();
        } else if !word.is_empty() && !word.starts_with(['-', '/']) {
            named.push(word);
        }
    }
    Some(match named.as_slice() {
        [] => BuildTarget::Unnamed,
        [one] if has_extension(one, &SOLUTION_EXTENSIONS) => BuildTarget::Solution,
        _ => BuildTarget::Other,
    })
}

/// The verb of a dotnet command's own words, its dotnet word first: the next non-empty word after it.
fn verb(own: &[String]) -> Option<&str> {
    verb_at(own).map(|at| own[at].as_str())
}

/// The index of the verb ([`verb`]) among a dotnet command's own words.
fn verb_at(own: &[String]) -> Option<usize> {
    let (index, _) = first_word(own)?;
    own[index + 1..].iter().position(|word| !word.is_empty()).map(|at| index + 1 + at)
}

/// Whether a dotnet command's own words, its dotnet word first, have a verb ([`verb`]) that is one of `verbs`,
/// compared ignoring letter case.
fn verb_in(own: &[String], verbs: &[&str]) -> bool {
    verb(own).is_some_and(|verb| verbs.iter().any(|listed| eq_ignoring_case(verb, listed)))
}

/// Whether `word` ends in one of `extensions`, ignoring letter case.
fn has_extension(word: &str, extensions: &[&str]) -> bool {
    extensions.iter().any(|extension| {
        word.len() > extension.len()
            && word
                .get(word.len() - extension.len()..)
                .is_some_and(|end| eq_ignoring_case(end, extension))
    })
}

/// The index of the dotnet word of a dotnet command ([`is_dotnet`]); `None` for any other command.
fn dotnet_start(words: &[String]) -> Option<usize> {
    let (index, first) = first_word(words)?;
    if is_named(first, DOTNET) {
        return Some(index);
    }
    if !WRAPPERS.iter().any(|wrapper| is_named(first, wrapper)) {
        return None;
    }
    let after = index + words[index..].iter().position(|word| word == SEPARATOR)? + 1;
    words.get(after).filter(|word| is_named(word, DOTNET)).map(|_| after)
}

/// The dotnet command's own words: from its dotnet word up to, not including, the first `--` after it, whose words go
/// to the program the command runs; `None` for a command that is no dotnet command, and for one whose own words name
/// a response file (a word starting with `@`), whose words the gate cannot see: no rule reads such a command.
fn dotnet_own(words: &[String]) -> Option<Range<usize>> {
    let start = dotnet_start(words)?;
    let end = words[start + 1..]
        .iter()
        .position(|word| word == SEPARATOR)
        .map_or(words.len(), |at| start + 1 + at);
    let opaque = words[start..end].iter().any(|word| word.starts_with(RESPONSE_FILE));
    (!opaque).then_some(start..end)
}

/// Whether a word names the program `name`: its file name without extension is `name`, ignoring letter case.
fn is_named(word: &str, name: &str) -> bool {
    eq_ignoring_case(file_stem(word), name)
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
    let sent_count = matched.words.len();
    let mut words = without_no_build(matched.words);
    let dropped_no_build = words.len() != sent_count;
    let fallback = without_no_build(given.to_vec());
    let mut outputs = matched.config.outputs(&matched.repo);
    let outputs_withheld = fallback.len() != given.len() && !outputs.is_empty();
    if outputs_withheld {
        outputs.clear();
    }
    mark_bare_first_word(&mut words, facts);
    stamp_source_revision(&mut words, &matched.place, facts);
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

/// The words without each of the dotnet command's own words ([`is_dotnet`]) that is exactly `--no-build`, when its
/// verb is one of [`NO_BUILD_DROP_VERBS`]: a word before a wrapping script's `--` or after the dotnet command's own
/// `--` stays, and the words of any other command, `dotnet pack`, `dotnet publish`, `dotnet app.dll`, `dotnet exec`,
/// `dotnet tool run` and a command naming a response file among them, are unchanged.
#[must_use]
pub fn without_no_build(words: Vec<String>) -> Vec<String> {
    let Some(own) = dotnet_own(&words).filter(|own| verb_in(&words[own.clone()], &NO_BUILD_DROP_VERBS)) else {
        return words;
    };
    words
        .into_iter()
        .enumerate()
        .filter(|(index, word)| !(own.contains(index) && word == NO_BUILD))
        .map(|(_, word)| word)
        .collect()
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
/// after the dotnet command's own words (before its own `--`, or after the last word when it has none), so the
/// builder's build is pinned to the revision the snapshot came from. The words of a command that is no such dotnet
/// command, whose own words already set `SourceRevisionId`, or whose HEAD cannot be read are left as they are.
fn stamp_source_revision(words: &mut Vec<String>, place: &Place, facts: &dyn Facts) {
    let Some(at) = stamp_at(words) else {
        return;
    };
    if let Ok(Some(sha)) = facts.head_sha(place) {
        words.insert(at, format!("-p:{SOURCE_REVISION}={sha}"));
    }
}

/// Where the source-revision word goes in the builder's words, or `None` when the command gets none: not a dotnet
/// command, no verb that builds, or one of the dotnet command's own words already sets `SourceRevisionId`.
fn stamp_at(words: &[String]) -> Option<usize> {
    let own = dotnet_own(words)?;
    let own_words = &words[own.clone()];
    (builds(own_words) && !own_words.iter().any(|word| sets_property(word, SOURCE_REVISION))).then_some(own.end)
}

/// Whether a dotnet command's words, its dotnet word first, build: its verb ([`verb`]) is one of [`BUILD_VERBS`].
fn builds(words: &[String]) -> bool {
    verb_in(words, &BUILD_VERBS)
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

//! The routing decision: whether a gate's command runs on the builder and, when it does, the words, folder, key and
//! outputs of that run. The rules apply in order, and each fact is asked of the caller ([`Facts`]) only at the rule
//! that needs it: a light or critical gate, `--local` or a covering parent read no routing file, and a machine with no
//! routing file costs no git call.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::config::{Builder, BuilderOs, CARGO_OUTPUTS, Config, ConfigProblem, DEFAULT_OUTPUTS, ignored_line};
use crate::escape::escaped;
use crate::key::{is_layout_name, repo_name, work_folder_key};
use crate::like::{Pattern, eq_ignoring_case};
use crate::rewrite::{RewritePlace, Rewritten, Separators, SiblingRoot, drive_letters, is_script_text, rewrite_named, root_letter, split_root};

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
/// The program name of a cargo command.
const CARGO: &str = "cargo";
/// The program names whose script, handed a dotnet command after its `--`, counts as that dotnet command.
const WRAPPERS: [&str; 2] = ["pwsh", "powershell"];
/// The `MSBuild` property a routed build pins the worktree's `HEAD` sha to, so the builder does not throw away a
/// build whose revision changed.
const SOURCE_REVISION: &str = "SourceRevisionId";
/// The `MSBuild` property through which a routed build maps each tree the builder checks the run out in to the PC root
/// it stands for, so its outputs record the paths a build on this PC records.
const PATH_MAP: &str = "PathMap";
/// The `MSBuild` properties that, set by the dotnet command's own words, keep the path-map word off: the caller then
/// chose the build's recorded paths itself.
const PATH_MAP_OPT_OUTS: [&str; 3] = [PATH_MAP, "ContinuousIntegrationBuild", "DeterministicSourcePaths"];
/// The comma between two path-map pairs, escaped: a bare one would end the `-p:` setting and start another property.
const PATH_MAP_PAIR_SEPARATOR: &str = "%2C";
/// The names of the switch that sets an `MSBuild` property ([`property_list`]), the longer first; their case and the
/// property name's are ignored.
const PROPERTY_SWITCH_NAMES: [&str; 2] = ["property", "p"];
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
    /// The gate's folder as the gate spells it, which may be on a subst drive where git gives the root by its real
    /// spelling (`D:\gate` for the root `X:/dev/gate`).
    pub folder: String,
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

    /// Each tree the builder checks a run of `target` out in, with the PC root it stands for, as the outputs' install
    /// pairs them: the main repo's tree with the worktree root, then each carried sibling's with the sibling's root, in
    /// carried order. Empty when the builder's root or a PC root cannot be told, or a PC root cannot stand in the word;
    /// the run then routes without the path-map word. Asked only of a dotnet command that gets the word.
    fn box_trees(&self, target: &Target) -> Vec<(String, String)>;

    /// Whether the build files of the project in `folder` below the worktree root of `place` (`/`-separated and
    /// `/`-ended, empty at the root) may move its build output out of its `bin\{configuration}` folder: the project
    /// file `file`, or each project file in `folder` when the command named the folder, and every
    /// `Directory.Build.props` and `Directory.Build.targets` from `folder` up to the root, any of them naming one of
    /// [`OUTPUT_PROPERTIES`]; `true` too when such a file is there and cannot be read. Asked only of a routed
    /// `dotnet test --no-build` that could keep its `--no-build` by a send ([`send_bin`]).
    fn moves_build_output(&self, place: &Place, folder: &str, file: Option<&str>) -> bool;

    /// Whether a file of exactly this name is in the gate's folder.
    fn file_in_folder(&self, name: &str) -> bool;

    /// Which of `paths`, each `/`-separated and relative to the worktree root of `place`, the snapshot carries: a file
    /// `git ls-files --cached --others --exclude-standard` lists, or a folder holding one. None when that cannot be told.
    /// Asked once, with every candidate, only of a macOS pick whose words hold a relative path written with `\`.
    fn snapshot_carries(&self, place: &Place, paths: &[String]) -> Vec<String>;

    /// What the drive `letter` (upper case) stands for, as `QueryDosDevice` gives it (`\??\X:\dev` for a subst drive);
    /// `None` when there is no such device.
    fn subst_target(&self, letter: char) -> Option<String>;

    /// The folder names the worktree root's `.gate/siblings.toml` declares, read from disk as it stands; none when
    /// there is no such file. Asked only once the routing file lists the repo, before the words are rewritten.
    ///
    /// # Errors
    ///
    /// The file's first problem, when it cannot be read, does not parse or has any problem; the gate then runs here,
    /// saying so.
    fn siblings(&self, place: &Place) -> Result<Vec<String>, String>;

    /// The patterns the worktree root's `.gate/desktop.toml` declares, read from disk as it stands; none when there is
    /// no such file. Asked only of a command one of its repo's patterns matched, once its siblings pass their checks.
    ///
    /// # Errors
    ///
    /// The file's first problem, when it cannot be read, does not parse or has any problem; the gate then runs here,
    /// saying so.
    fn desktop(&self, place: &Place) -> Result<Vec<Pattern>, String>;

    /// The patterns the worktree root's `.gate/local.toml` declares, read from disk as it stands; none when there is no
    /// such file. Asked only of a command one of its repo's patterns matched, before its siblings are checked.
    ///
    /// # Errors
    ///
    /// The file's first problem, when it cannot be read, does not parse or has any problem; the gate then runs here,
    /// saying so.
    fn local(&self, place: &Place) -> Result<Vec<Pattern>, String>;

    /// How the lines name the declaration file `name` (`siblings.toml`, `desktop.toml` or `local.toml`) of the worktree
    /// at `place`, as [`Facts::siblings`], [`Facts::desktop`] or [`Facts::local`] read it: `.gate/<name>` for the
    /// worktree's own, else the full path of the copy it was read from. Asked once that file was read, for a line about
    /// it.
    fn declaration_said(&self, place: &Place, name: &str) -> String;

    /// The folder of the sibling whose root is `root`, which the decision builds as the worktree root's parent and the
    /// sibling's name: its parent opened following links, then the name opened beneath it without following one.
    /// Missing when nothing is there or it holds no `.git`; refused, with the reason, when it is a link or a junction,
    /// not a folder, or its `.git` is a link or a junction; else present, with its id. Asked for each declared sibling,
    /// in declared order, once every name is allowed.
    ///
    /// # Errors
    ///
    /// What could not be read; the gate then runs here, saying so.
    fn sibling_folder(&self, root: &str) -> Result<SiblingFolder, String>;

    /// The id of the worktree root's folder, opened following links. Asked for every matched command with no siblings
    /// file problem, once every declared sibling is present.
    ///
    /// # Errors
    ///
    /// What could not be read; the gate then runs here, saying so.
    fn root_id(&self, root: &str) -> Result<FolderId, String>;

    /// The folder taken as the git worktree `path` lies in: the nearest of `path` and the folders above it that holds a
    /// `.git` file or folder, read from metadata alone with no git started; `None` when none on the way holds one, and
    /// when `path` is not absolute or lies on a drive that is remote or not there. Asked only of a matched command whose
    /// outside word names a path ([`Rewritten::outside_path`]).
    fn worktree_of(&self, path: &Path) -> Option<PathBuf>;

    /// The folder name of the main checkout of the worktree at `tree`, as [`Place::checkout`] names the run's own: the
    /// parent of its common git folder, read from its `.git` alone with no git started; `None` when it cannot be read.
    /// Asked only of a tree [`Facts::worktree_of`] found that is not the run's own.
    fn checkout_of(&self, tree: &Path) -> Option<String>;
}

/// The id of an open folder: its volume's serial number and its 128-bit file id. Two handles give one id only when they
/// are of one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FolderId {
    pub volume: u64,
    pub file: [u8; 16],
}

/// A sibling a routed run carries: its folder name as declared, its root as [`sibling_root`] spells it, and the id
/// [`Facts::sibling_folder`] gave its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedSibling {
    pub name: String,
    pub root: String,
    pub id: FolderId,
}

/// A declared sibling's folder as [`Facts::sibling_folder`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiblingFolder {
    /// Nothing is there, or it holds no `.git`.
    Missing,
    /// Something is there that is not a sibling to carry; the reason reads after the sibling's name
    /// (`is a link or a junction`).
    Refused(String),
    /// A folder, not a link or a junction, holding a `.git` that is not one either.
    Present(FolderId),
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
    /// A matched command's outside word names a path in the git worktree `tree`, neither the run's own nor a declared
    /// sibling's: a gate started there, or given `--cd` it, can route the command.
    OtherWorktree { tree: PathBuf },
    /// The gate's folder holds nothing the snapshot sends; `subfolder` is the prefix without its trailing `/`.
    NothingSent { subfolder: String },
    /// A fact could not be read; `reason` is on one line.
    Failed { reason: String },
    /// The siblings file `file` ([`Facts::declaration_said`]) cannot be read or has a problem; `problem` is its first,
    /// on one line.
    SiblingsFile { file: String, problem: String },
    /// The declared sibling `name` has no git worktree beside the worktree root.
    SiblingMissing { name: String },
    /// The declared sibling `name` is there but cannot be carried, for `reason`, which reads after its name.
    SiblingRefused { name: String, reason: String },
    /// The declared siblings `names`, in declared order, are not among those `remote.json`'s `siblings` allows the
    /// repo; `problem` is why that value cannot be used, when it cannot.
    SiblingsNotAllowed { names: Vec<String>, problem: Option<String> },
    /// The worktree root, which declares siblings, has a folder name `leaf` the builder cannot lay out.
    LeafNotLaidOut { leaf: String },
    /// The desktop file `file` ([`Facts::declaration_said`]) cannot be read or has a problem; `problem` is its first,
    /// on one line.
    DesktopFile { file: String, problem: String },
    /// The first declared desktop pattern the command matches, `pattern`, is not among those `remote.json`'s `desktop`
    /// allows the repo; `problem` is why that value cannot be used, when it cannot.
    DesktopNotAllowed { pattern: String, problem: Option<String> },
    /// The local file `file` ([`Facts::declaration_said`]) cannot be read or has a problem; `problem` is its first, on
    /// one line.
    LocalFile { file: String, problem: String },
    /// The local file `file` ([`Facts::declaration_said`]) declares `pattern`, the first of its patterns the command
    /// matches.
    DeclaredLocal { file: String, pattern: String },
}

impl LocalReason {
    /// The line the gate says, on standard error and in the log.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::LocalFlag => "gate: not routed: --local".to_owned(),
            Self::Ignored { path, reason } => ignored_line(path, reason),
            Self::Outside { word } => format!("gate: not routed: {word} is outside the worktree"),
            Self::OtherWorktree { tree } => {
                let tree = cd_value(tree);
                format!("gate: not routed: the command works in {tree}; run it with --cd {tree}")
            }
            Self::NothingSent { subfolder } => format!("gate: not routed: {subfolder} holds nothing the snapshot sends"),
            Self::Failed { reason } => format!("gate: not routed: {reason}"),
            Self::SiblingsFile { file, problem } | Self::DesktopFile { file, problem } | Self::LocalFile { file, problem } => {
                format!("gate: not routed: {file}: {problem}")
            }
            Self::DeclaredLocal { file, pattern } => format!("gate: not routed: {file} keeps it here ({})", escaped(pattern)),
            Self::SiblingMissing { name } => format!("gate: not routed: sibling {name} not found beside the worktree"),
            Self::SiblingRefused { name, reason } => format!("gate: not routed: sibling {name} {reason}"),
            Self::SiblingsNotAllowed { names, problem } => not_allowed_line(names, problem.as_deref()),
            Self::LeafNotLaidOut { leaf } => format!("gate: not routed: the worktree folder name {leaf} cannot be laid out on the builder"),
            Self::DesktopNotAllowed { pattern, problem } => {
                let problem = problem.as_ref().map_or_else(String::new, |problem| format!(": {problem}"));
                format!(
                    "gate: not routed: desktop pattern '{}' not allowed in remote.json{problem}",
                    escaped(pattern)
                )
            }
        }
    }
}

/// The characters that put the `--cd` line's tree in double quotes: a space and those a shell reads apart from a word.
const CD_QUOTED: [char; 10] = [' ', ';', '&', '\'', '(', ')', ',', '=', '$', '`'];

/// The worktree `tree` as the `--cd` line shows it: [`escaped`], written with `\`, and in double quotes when it holds
/// any of [`CD_QUOTED`], so it pastes as one word.
fn cd_value(tree: &Path) -> String {
    let tree = escaped(&tree.to_string_lossy().replace('/', "\\"));
    if tree.contains(CD_QUOTED) { format!("\"{tree}\"") } else { tree }
}

/// The line of declared siblings `remote.json` does not allow: `sibling b is` for one, `siblings b, c are` for more,
/// then `not allowed in remote.json`, and the reason its `siblings` value cannot be used after a colon when it cannot.
/// Each name is shown [`escaped`].
fn not_allowed_line(names: &[String], problem: Option<&str>) -> String {
    let names: Vec<String> = names.iter().map(|name| escaped(name)).collect();
    let refused = match names.as_slice() {
        [name] => format!("sibling {name} is"),
        _ => format!("siblings {} are", names.join(", ")),
    };
    let problem = problem.map_or_else(String::new, |problem| format!(": {problem}"));
    format!("gate: not routed: {refused} not allowed in remote.json{problem}")
}

/// A command routed to the builder.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact of the route that a caller reads by name; no two form a state machine"
)]
pub struct RemoteRun {
    /// The builder this run goes to: the command's pick, the first `builders` entry listing the repo, in file order,
    /// with a pattern that matches, else `host`.
    pub builder: Builder,
    /// The worktree root as git gives it.
    pub root: String,
    /// The id of the worktree root's folder, read following links ([`Facts::root_id`]): what the run's worktree lock
    /// checks the root against at use.
    pub root_id: FolderId,
    /// The worktree root's folder name: its last segment.
    pub leaf: String,
    /// The siblings the run carries: those declared, each allowed and checked, in declared order; none for a repo that
    /// declares none.
    pub siblings: Vec<CarriedSibling>,
    /// The repo name the routing file listed.
    pub repo: String,
    /// The builder's work folder ([`work_folder_key`]).
    pub key: String,
    /// The gate's folder below the root, without a trailing `/`; empty at the root.
    pub subfolder: String,
    /// The pattern that matched.
    pub pattern: String,
    /// The words the builder runs: paths rewritten, `--no-build` dropped from a dotnet command, the worktree's `HEAD`
    /// sha pinned onto a dotnet command that builds as `SourceRevisionId`, a bare first word naming a file of the
    /// gate's folder written `./name`, and a first word naming a program by a path outside the worktree sent as the
    /// program's name ([`RemoteRun::program_named`]), which is never the file of the folder.
    pub words: Vec<String>,
    /// The command as given, each exact `--no-build` word of a dotnet command dropped from it: read only to say the
    /// first word the caller gave, for a run whose first word was sent as a program's name
    /// ([`RemoteRun::program_line`]). A run here after a fallback decides its own words.
    pub fallback: Vec<String>,
    /// Whether the rewrite sent the command's first word as a program's name ([`Rewritten::program_named`]):
    /// [`RemoteRun::lines`] says the word as given and the name it is sent as.
    pub program_named: bool,
    /// The output globs the run brings back; none for a dotnet `--no-build` command.
    pub outputs: Vec<String>,
    /// The input globs the run sends: the repo's `inputs` list, none when it has no entry; kept for a dotnet
    /// `--no-build` command, whose outputs alone are withheld.
    pub inputs: Vec<String>,
    /// Whether `--no-build` was dropped from the builder's words.
    pub dropped_no_build: bool,
    /// The send that lets the builder keep a routed `dotnet test`'s `--no-build` ([`send_bin`]), for a repo the routing
    /// file's `sendBuild` lists ([`Config::send_build_allowed`]): the PC's build of its one project sent first, then the
    /// kept words run. `None` for every other run; a send that does not land runs
    /// [`RemoteRun::words`], `--no-build` dropped.
    pub send: Option<SendPlan>,
    /// Whether outputs were withheld from a `--no-build` command.
    pub outputs_withheld: bool,
    /// Whether the run asks for the builder's desktop: the first `.gate/desktop.toml` pattern its rewritten words match
    /// is one `remote.json`'s `desktop` allows the repo.
    pub desktop: bool,
    /// The worktree's `HEAD` sha, read once for every routed run, which the run sends the builder as the
    /// `SourceRevisionId` variable so any build there is pinned to it; `None` when the branch is unborn or git cannot
    /// read it.
    pub source_revision: Option<String>,
    /// The builders the run may try next when it falls back, in the order it would try them, from the config alone:
    /// each `builders` entry listing the repo, in file order, then `host`, minus the builder this run goes to, each
    /// with the first pattern of its own list that the command matched. A caller that falls back checks a candidate
    /// through the pick's steer-off rules and builds its run when it tries it; nothing is built here.
    pub later: Vec<LaterCandidate>,
}

/// A builder a routed run may try next when it falls back: the builder, and the first pattern of its own list for the
/// repo that the command matched ([`first_match`]), as the config gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaterCandidate {
    /// The builder the run would go to.
    pub builder: Builder,
    /// The first pattern of the builder's own list for the repo that the command matched.
    pub pattern: String,
}

/// How a routed `dotnet test --no-build` keeps its `--no-build` ([`RemoteRun::send`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPlan {
    /// The words the builder runs once the send lands: the rewritten words with `--no-build` kept and the bare first
    /// word marked, never stamped, as a command that does not build is not ([`stamp`]).
    pub words: Vec<String>,
    /// The project's bin folder of the run's configuration below the worktree root, `/`-separated:
    /// `{project}/bin/{configuration}`, or `bin/{configuration}` for a project at the root.
    pub bin: String,
}

impl RemoteRun {
    /// The lines the gate says for this route, in order: [`RemoteRun::program_line`], [`DROPPED_NO_BUILD_LINE`] unless
    /// the run has a send, whose client says it only when the send does not land, [`NO_OUTPUTS_LINE`], then, for a run
    /// carrying siblings, `gate: routed with siblings: {name} ({root}), ...` naming each in declared order.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(line) = self.program_line() {
            lines.push(line);
        }
        if self.dropped_no_build && self.send.is_none() {
            lines.push(DROPPED_NO_BUILD_LINE.to_owned());
        }
        if self.outputs_withheld {
            lines.push(NO_OUTPUTS_LINE.to_owned());
        }
        if !self.siblings.is_empty() {
            let named: Vec<String> = self
                .siblings
                .iter()
                .map(|sibling| format!("{} ({})", sibling.name, sibling.root))
                .collect();
            lines.push(format!("gate: routed with siblings: {}", named.join(", ")));
        }
        lines
    }

    /// The line `gate: routed {given} as {name}`, for a run whose rewrite sent the command's first non-empty word as a
    /// program's name ([`RemoteRun::program_named`]): the word as the caller gave it and the name the builder looks the
    /// program up by, each escaped as the run's other lines escape words. `None` for any other run.
    fn program_line(&self) -> Option<String> {
        if !self.program_named {
            return None;
        }
        let (_, given) = first_word(&self.fallback)?;
        let (_, name) = first_word(&self.words)?;
        Some(format!("gate: routed {} as {}", escaped(given), escaped(name)))
    }
}

/// Where a gate's command runs. In order: only a heavy gate routes; a covering or lending parent keeps it here; `--local`
/// keeps it here saying so; then the routing file is read, and no file keeps it here, an unusable one saying it was
/// ignored; a git hook whose `hooks` names no repo, no non-empty word, or a first word whose file name without extension
/// is `git` or `prek` keep it here; so does a folder in no git worktree, a repo the routing file does not list, a git
/// hook whose repo its `hooks` does not name, and words (paths rewritten, joined by single spaces) none of the repo's
/// patterns match, the first that matches winning. A first word the rewrite sends as a program's name that names `git`
/// or `prek` keeps it here too. A matched command runs here, saying why, when the worktree's `.gate/local.toml`
/// declares a pattern its words match or has a problem, before any sibling or desktop check. It still runs here, saying
/// why, when the worktree's `.gate/siblings.toml` has a problem or declares a sibling the routing file's `siblings` does
/// not allow the repo, or declares siblings from a root folder name the builder cannot lay out, a sibling named like
/// that folder or one with no git worktree beside the root; when its `.gate/desktop.toml` has a problem, or the first
/// pattern it declares that the words match is not one the routing file's `desktop` allows the repo, listed exactly;
/// when a word names a path outside the worktree and its siblings; or when the gate's folder holds nothing the snapshot
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

/// Where a worktree's routed work runs on the builder and how its work folder is laid out: the same for every routed
/// run of the worktree and for its warm-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The builder: the one a command's pick chose, and `host` for a warm-up's target ([`target`]).
    pub builder: Builder,
    /// The gate's place in the worktree.
    pub place: Place,
    /// The repo name the routing file lists.
    pub repo: String,
    /// The worktree root's folder name: its last segment.
    pub leaf: String,
    /// The id of the worktree root's folder ([`Facts::root_id`]).
    pub root_id: FolderId,
    /// The siblings carried: those declared, each allowed and checked, in declared order.
    pub siblings: Vec<CarriedSibling>,
    /// The builder's work folder ([`work_folder_key`]).
    pub key: String,
    /// The input globs sent: the repo's `inputs` list, none when it has no entry.
    pub inputs: Vec<String>,
}

/// A command's words for the builder of a [`Target`] ([`builder_words`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderWords {
    /// The words: paths rewritten, a bare first word naming a file of the gate's folder written `./name` (never a first
    /// word sent as a program's name), and the worktree's `HEAD` sha pinned onto a dotnet command that builds as
    /// `SourceRevisionId`, then the builder's trees mapped to their PC roots as `PathMap`.
    pub words: Vec<String>,
    /// The first word, as given, that names a path outside the worktree and its siblings, when one does.
    pub outside: Option<String>,
    /// Whether the rewrite sent the command's first word as a program's name ([`Rewritten::program_named`]), which a
    /// caller of it this PC keeps ([`stays_local`]) holds as a problem.
    pub program_named: bool,
}

/// The target of the worktree at `place` under the routing file `config`, on `host`, asked of the facts in the order a
/// routed run asks them: the siblings file, then each sibling's folder and the root's id. A repo the routing file's
/// top-level `repos` does not list has none, saying nothing, even when a `builders` entry lists it; a siblings problem
/// keeps the work here as it keeps a routed run ([`decide`]).
///
/// # Errors
///
/// The reason to say, if any, when the worktree has no target.
pub fn target(config: &Config, place: Place, facts: &dyn Facts) -> Result<Target, Option<LocalReason>> {
    let opened = open(place, facts, |repo| config.patterns(repo).is_some())?;
    settle(config, opened, config.host_builder().clone(), facts)
}

/// The words the builder of `target` runs for the command `words`: `--no-build` dropped as a routed run drops it, then
/// the paths rewritten as for `host`, the bare first word marked and the builder's own words written in ([`stamp`]), as
/// a routed run's are; the worktree's `HEAD` is asked only when the source-revision word may go in.
#[must_use]
pub fn builder_words(target: &Target, words: &[String], facts: &dyn Facts) -> BuilderWords {
    let drives = subst_drives(&target.place.root, words, facts);
    let siblings = carried_pairs(&target.siblings);
    let sent = rewritten(&target.place, siblings, words, &drives, Separators::AsWritten);
    let mut words = without_no_build(sent.words);
    mark_bare_first_word(&mut words, facts, sent.program_named);
    stamp(&mut words, || head_of(&target.place, facts), target, facts);
    BuilderWords {
        words,
        outside: sent.outside,
        program_named: sent.program_named,
    }
}

/// A command a pattern matched, with what the rest of the decision needs.
struct Matched {
    target: Target,
    pattern: String,
    words: Vec<String>,
    outside: Option<String>,
    /// The path the outside word names ([`Rewritten::outside_path`]).
    outside_path: Option<PathBuf>,
    /// Whether the rewrite sent the command's first word as a program's name ([`Rewritten::program_named`]).
    program_named: bool,
    /// Whether the command asks for the builder's desktop ([`check_desktop`]).
    desktop: bool,
    /// The builders the run may try next, in try order ([`later_candidates`]).
    later: Vec<LaterCandidate>,
}

fn route(inputs: &Inputs<'_>, facts: &dyn Facts) -> Result<RemoteRun, Here> {
    let config = eligible(inputs, facts)?;
    let matched = matched(&config, inputs.git_hook, inputs.words, facts)?;
    if let Some(word) = matched.outside.clone() {
        return Err(Some(outside_reason(&config, word, &matched, facts)));
    }
    let place = &matched.target.place;
    let subfolder = place.prefix.trim_end_matches('/').to_owned();
    if !place.prefix.is_empty() && !facts.folder_has_files(place).map_err(|reason| Some(failed(&reason)))? {
        return Err(Some(LocalReason::NothingSent { subfolder }));
    }
    Ok(remote_run(&config, matched, subfolder, inputs.words, facts))
}

/// Why the `matched` command whose `word` names a path outside the worktree runs here: the git worktree its outside
/// path lies in ([`Facts::worktree_of`]) when there is one, it is neither the run's own root nor a carried sibling's
/// ([`is_own_tree`]), and the routing file `config` lists its repo, named from its main checkout's folder
/// ([`Facts::checkout_of`]) as the run names its own; else the word itself. A word naming no such path asks nothing.
fn outside_reason(config: &Config, word: String, matched: &Matched, facts: &dyn Facts) -> LocalReason {
    let tree = matched
        .outside_path
        .as_deref()
        .and_then(|path| facts.worktree_of(path))
        .filter(|tree| !is_own_tree(tree, &matched.target, facts))
        .filter(|tree| facts.checkout_of(tree).is_some_and(|checkout| config.is_routed(&repo_name(&checkout))));
    match tree {
        Some(tree) => LocalReason::OtherWorktree { tree },
        None => LocalReason::Outside { word },
    }
}

/// Whether `tree` is the root of `target` or of one of its carried siblings: its folder id ([`Facts::root_id`]) is the
/// root's or a sibling's, so a junction, a short name or any other spelling of them is caught. When its id cannot be
/// read, the spellings are compared instead ([`spelled_as_own`]): a tree that cannot be opened is no folder to name a
/// different one by.
fn is_own_tree(tree: &Path, target: &Target, facts: &dyn Facts) -> bool {
    match facts.root_id(&tree.to_string_lossy()) {
        Ok(id) => id == target.root_id || target.siblings.iter().any(|sibling| sibling.id == id),
        Err(_) => spelled_as_own(tree, target),
    }
}

/// Whether `tree` spells the root of `target` or of one of its carried siblings, compared in any letter case and with
/// either separator, trailing ones aside.
fn spelled_as_own(tree: &Path, target: &Target) -> bool {
    let folded = |path: &str| path.replace('\\', "/").trim_end_matches('/').to_owned();
    let tree = folded(&tree.to_string_lossy());
    std::iter::once(target.place.root.as_str())
        .chain(target.siblings.iter().map(|sibling| sibling.root.as_str()))
        .any(|root| eq_ignoring_case(&folded(root), &tree))
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
    if stays_local(inputs.words) {
        return Err(None);
    }
    if inputs.git_hook && !config.hooks_name_a_repo() {
        return Err(None);
    }
    Ok(config)
}

/// Whether the command can never route: it has no non-empty word, or its first one is `git` or `prek`, however
/// spelled (`git.exe`, a full path to it). A caller holding words the rewrite has sent ([`Rewritten::words`]) asks the
/// same of those, so a `git` the rewrite sends a name for stays here too.
#[must_use]
pub fn stays_local(words: &[String]) -> bool {
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

/// Whether the words are a cargo command: the first non-empty word's file name without extension is `cargo`, compared
/// ignoring letter case (`cargo`, `Cargo.exe`, a full path to it). A cargo command a wrapping script is handed after its
/// `--` is not read as one.
#[must_use]
pub fn is_cargo(words: &[String]) -> bool {
    first_word(words).is_some_and(|(_, word)| names_program(word, CARGO))
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
    if names_program(first, DOTNET) {
        return Some(index);
    }
    if !WRAPPERS.iter().any(|wrapper| names_program(first, wrapper)) {
        return None;
    }
    let after = index + words[index..].iter().position(|word| word == SEPARATOR)? + 1;
    words.get(after).filter(|word| names_program(word, DOTNET)).map(|_| after)
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

/// Whether a word names the program `name`: its file name without extension, as Windows reads a path, is `name`,
/// ignoring letter case (`git`, `git.exe` and a full path to it all name `git`).
#[must_use]
pub fn names_program(word: &str, name: &str) -> bool {
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

/// The place, repo, siblings and rewritten words of a command one of its repo's patterns matches. The patterns and the
/// siblings file come from [`open`], which reads them once the routing file lists the repo, and the words are rewritten
/// with the siblings it declares, or with none when it has a problem, then matched, the first pattern that matches
/// winning; only a matched command is kept here for a sibling ([`check_siblings`]). A command whose first word the
/// rewrite sent as a program's name ([`Rewritten::program_named`]) that stays on this PC by that name ([`stays_local`])
/// is no match: a `git` or `prek` under a path is read as one however the path is spelled. A gate started inside a git
/// hook whose repo the routing file's `hooks` does not name stays here silently, before its siblings file is read, and
/// so does one whose place cannot be read.
///
/// A macOS pick runs the words rewritten with `/` ([`Separators::Slash`]) and its relative `\` paths the snapshot carries
/// written with `/` ([`unix_form`]); a pick whose words still hold a `\` goes on, silently, to the next builder that is
/// no macOS one ([`steer_off_backslashes`]).
fn matched(config: &Config, git_hook: bool, words: &[String], facts: &dyn Facts) -> Result<Matched, Here> {
    let place = match facts.place() {
        Ok(Some(place)) => place,
        Ok(None) => return Err(None),
        Err(_) if git_hook => return Err(None),
        Err(reason) => return Err(Some(failed(&reason))),
    };
    if git_hook && config.hooks_allowed(&repo_name(&place.checkout)) != Ok(true) {
        return Err(None);
    }
    let opened = open(place, facts, |repo| config.is_routed(repo))?;
    let drives = subst_drives(&opened.place.root, words, facts);
    let sent = rewritten(&opened.place, declared_pairs(&opened.siblings), words, &drives, Separators::AsWritten);
    if sent.program_named && stays_local(&sent.words) {
        return Err(None);
    }
    let first = pick(config, &opened.repo, &sent.words, |_| true).ok_or(None)?;
    check_local(&opened.place, &sent.words, facts)?;
    let (builder, pattern, found) = steer_off_macos(config, &opened, &sent.words, facts, first)?;
    let slashed =
        (builder.os == BuilderOs::Macos).then(|| rewritten(&opened.place, declared_pairs(&opened.siblings), words, &drives, Separators::Slash).words);
    let (builder, pattern, unix_words) = steer_off_backslashes(config, &opened, &sent.words, facts, (builder, pattern, slashed))?;
    let target = settle(config, opened, builder, facts)?;
    let desktop = check_desktop(config, &target, found, &sent.words, facts)?;
    let later = later_candidates(config, &target, &sent.words);
    Ok(Matched {
        target,
        pattern,
        words: unix_words.unwrap_or(sent.words),
        outside: sent.outside,
        outside_path: sent.outside_path,
        program_named: sent.program_named,
        desktop,
        later,
    })
}

/// The builders the run may try next, in the order it would try them: [`Config::routes`] order, minus the pick, each
/// with the first of its own patterns that matches the `sent` words. The config alone decides them; no fact is asked.
fn later_candidates(config: &Config, target: &Target, sent: &[String]) -> Vec<LaterCandidate> {
    let mut later = Vec::new();
    for (builder, patterns) in config.routes(&target.repo) {
        if *builder == target.builder {
            continue;
        }
        if let Some(pattern) = first_match(patterns, sent) {
            later.push(LaterCandidate {
                builder: builder.clone(),
                pattern,
            });
        }
    }
    later
}

/// The pick `picked` of the opened worktree's command, with the words a macOS pick runs, rewritten with `/`: kept, its
/// words in their Unix form ([`unix_form`]), unless a word of that form still holds a `\`, which the Mac would read
/// another way; then the pick goes on, silently, to the next builder that is no macOS one, which runs the `sent` words,
/// as [`steer_off_macos`] does. A pick of another builder carries no rewritten words and is kept as it is. `Err(None)`
/// when no other builder's pattern matches.
fn steer_off_backslashes(
    config: &Config,
    opened: &Opened,
    sent: &[String],
    facts: &dyn Facts,
    picked: (Builder, String, Option<Vec<String>>),
) -> Result<(Builder, String, Option<Vec<String>>), Here> {
    let (builder, pattern, slashed) = picked;
    let Some(slashed) = slashed else {
        return Ok((builder, pattern, None));
    };
    if let Some(unix) = unix_form(&opened.place, slashed, facts) {
        return Ok((builder, pattern, Some(unix)));
    }
    let (builder, pattern) = pick(config, &opened.repo, sent, |other| other.os != BuilderOs::Macos).ok_or(None)?;
    Ok((builder, pattern, None))
}

/// The PowerShell programs whose `-c` or `-Command` switch, in any letter case, makes every later word a script's text.
const PWSH_HOSTS: [&str; 2] = ["pwsh", "powershell"];

/// The Unix shells whose `-c` switch makes every later word a script's text and its arguments.
const SH_HOSTS: [&str; 2] = ["bash", "sh"];

/// `words`, a macOS pick's words rewritten with `/`, with each relative path written with `\` that the snapshot carries
/// ([`slash_candidate`]) written with `/`: the snapshot is asked once, for every candidate before a script's text
/// ([`script_start`]) in word order, and only when there is one. `None` when a word still holds a `\`, script text
/// included: a regex, an escape, a list, a response file or a path the snapshot does not carry.
fn unix_form(place: &Place, mut words: Vec<String>, facts: &dyn Facts) -> Option<Vec<String>> {
    let plain = script_start(&words);
    let candidates: Vec<(usize, SlashCandidate)> = words[..plain]
        .iter()
        .enumerate()
        .filter_map(|(index, word)| slash_candidate(&place.prefix, word).map(|found| (index, found)))
        .collect();
    if !candidates.is_empty() {
        let mut asked: Vec<String> = Vec::new();
        for (_, found) in &candidates {
            if !asked.contains(&found.path) {
                asked.push(found.path.clone());
            }
        }
        let carried = facts.snapshot_carries(place, &asked);
        for (index, found) in candidates {
            if carried.contains(&found.path) {
                words[index] = found.slashed;
            }
        }
    }
    let leftover = words.iter().any(|word| word.contains('\\'));
    (!leftover).then_some(words)
}

/// The index of the first word of a script's text: the word after the host's own `-c` switch, of a command whose first
/// non-empty word names one of [`PWSH_HOSTS`] ([`pwsh_script_at`]) or [`SH_HOSTS`] ([`sh_script_at`]); the word count
/// for any other command, and for one whose host runs a script file instead.
fn script_start(words: &[String]) -> usize {
    let Some((first, program)) = first_word(words) else {
        return words.len();
    };
    let switches = &words[first + 1..];
    let found = if PWSH_HOSTS.iter().any(|host| names_program(program, host)) {
        pwsh_script_at(switches)
    } else if SH_HOSTS.iter().any(|host| names_program(program, host)) {
        sh_script_at(switches)
    } else {
        None
    };
    found.map_or(words.len(), |at| first + 1 + at)
}

/// The index in a PowerShell host's `switches` of the word after its `-c` or `-Command`, in any letter case; `None` when
/// a `-File`, `-f` or `--` comes first, which ends the host's own switches.
fn pwsh_script_at(switches: &[String]) -> Option<usize> {
    for (at, word) in switches.iter().enumerate() {
        if word == SEPARATOR || eq_ignoring_case(word, "-file") || eq_ignoring_case(word, "-f") {
            return None;
        }
        if eq_ignoring_case(word, "-c") || eq_ignoring_case(word, "-command") {
            return Some(at + 1);
        }
    }
    None
}

/// The index in a Unix shell's `switches` of the word after its `-c`, matched in its letter case (`-C` is another
/// switch); `None` when a `--` or a word that is no switch (the script's path) comes first. The value of `-o`, `-O`,
/// `+o` and `+O` is skipped.
fn sh_script_at(switches: &[String]) -> Option<usize> {
    let mut words = switches.iter().enumerate();
    while let Some((at, word)) = words.next() {
        match word.as_str() {
            "-c" => return Some(at + 1),
            "-o" | "-O" | "+o" | "+O" => {
                words.next();
            }
            SEPARATOR => return None,
            switch if switch.starts_with(['-', '+']) => {}
            _ => return None,
        }
    }
    None
}

/// A word of a macOS pick that may name a relative path written with `\` ([`slash_candidate`]).
struct SlashCandidate {
    /// The word with its value written with `/`.
    slashed: String,
    /// The path the snapshot is asked about, `/`-separated from the worktree root.
    path: String,
}

/// The candidate `word` is: its value ([`switch_value`]) written with `/` ([`relative_backslash_value`]), and the path it
/// names ([`judged_path`]) below the gate's folder `prefix` (`sub/dir/`, empty at the root). `None` for script text, a
/// response file (`@`), a switch without an `=`, a value the rewrite of a relative `\` path refuses, and a path that
/// names nothing or starts with `/`.
fn slash_candidate(prefix: &str, word: &str) -> Option<SlashCandidate> {
    if word.starts_with(RESPONSE_FILE) || is_script_text(word) {
        return None;
    }
    let at = switch_value(word)?;
    let slashed = relative_backslash_value(&word[at..])?;
    let path = format!("{prefix}{}", judged_path(&slashed)?);
    if path.starts_with('/') {
        return None;
    }
    Some(SlashCandidate {
        slashed: format!("{}{slashed}", &word[..at]),
        path,
    })
}

/// Where the value judged in `word` starts: after the first `=` of a switch (a word starting with `-`, or with `/` and
/// holding a `:`, as in `/p:Name=value`), at the start of any other word; `None` for a switch without an `=`.
fn switch_value(word: &str) -> Option<usize> {
    let switch = word.starts_with('-') || (word.starts_with('/') && word.contains(':'));
    if switch { word.find('=').map(|equals| equals + 1) } else { Some(0) }
}

/// `value` with each `\` written as `/`, when it is a relative path written with `\`; `None` for a value holding no
/// `\`, starting with one, starting with a drive or holding a `..` segment.
fn relative_backslash_value(value: &str) -> Option<String> {
    let drive = matches!(value.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
    if !value.contains('\\') || value.starts_with('\\') || drive {
        return None;
    }
    let slashed = value.replace('\\', "/");
    (!slashed.split('/').any(|segment| segment == "..")).then_some(slashed)
}

/// The path a `/`-separated relative value names: its leading `./` dropped, a glob cut to the folder before its first
/// wildcard (`*`, `?`, `[`), a trailing `/` dropped; `None` when nothing is left.
fn judged_path(slashed: &str) -> Option<&str> {
    let mut path = slashed;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    if let Some(wildcard) = path.find(['*', '?', '[']) {
        path = &path[..path[..wildcard].rfind('/').unwrap_or(0)];
    }
    let path = path.trim_end_matches('/');
    (!path.is_empty()).then_some(path)
}

/// Keeps a matched command here, saying why, when the worktree's `.gate/local.toml` at `place` declares a pattern its
/// rewritten `words` match, joined by single spaces as the repo's patterns see them, naming the first that matches, or
/// when the file has a problem, shown [`escaped`]: a command the repo keeps here must not route because its file
/// cannot be read.
fn check_local(place: &Place, words: &[String], facts: &dyn Facts) -> Result<(), Here> {
    let declared = facts.local(place).map_err(|problem| {
        Some(LocalReason::LocalFile {
            file: escaped(&facts.declaration_said(place, "local.toml")),
            problem: escaped(&fold_lines(&problem)),
        })
    })?;
    match first_match(&declared, words) {
        Some(pattern) => Err(Some(LocalReason::DeclaredLocal {
            file: escaped(&facts.declaration_said(place, "local.toml")),
            pattern,
        })),
        None => Ok(()),
    }
}

/// The first pattern of the worktree's `.gate/desktop.toml` at `place` that the rewritten `words` match, joined by single
/// spaces; `None` when none does. Keeps the command here, saying why, when the file has a problem, shown [`escaped`].
fn desktop_pattern(place: &Place, words: &[String], facts: &dyn Facts) -> Result<Option<String>, Here> {
    let declared = facts.desktop(place).map_err(|problem| {
        Some(LocalReason::DesktopFile {
            file: escaped(&facts.declaration_said(place, "desktop.toml")),
            problem: escaped(&fold_lines(&problem)),
        })
    })?;
    Ok(first_match(&declared, words))
}

/// The pick `first` of the opened worktree's command `words`, kept unless it is a macOS builder and a declared desktop
/// pattern matches the command ([`desktop_pattern`]): a macOS builder has no desktop to run it on, so the pick goes on
/// to the next builder that is no macOS one. Also what it read of the desktop file; `Err(None)` when no other
/// builder's pattern matches.
fn steer_off_macos(
    config: &Config,
    opened: &Opened,
    words: &[String],
    facts: &dyn Facts,
    first: (Builder, String),
) -> Result<(Builder, String, DesktopRead), Here> {
    if first.0.os != BuilderOs::Macos {
        return Ok((first.0, first.1, DesktopRead::NotRead));
    }
    let found = desktop_pattern(&opened.place, words, facts)?;
    if found.is_none() {
        return Ok((first.0, first.1, DesktopRead::Read(found)));
    }
    let (builder, pattern) = pick(config, &opened.repo, words, |builder| builder.os != BuilderOs::Macos).ok_or(None)?;
    Ok((builder, pattern, DesktopRead::Read(found)))
}

/// What the pick read of the worktree's `.gate/desktop.toml` ([`steer_off_macos`]).
enum DesktopRead {
    /// The pick did not read it.
    NotRead,
    /// The first declared pattern the command matches, if any.
    Read(Option<String>),
}

/// Whether a matched command asks for the builder's desktop: the first pattern of the worktree's `.gate/desktop.toml`
/// that its rewritten `words` match, joined by single spaces as the repo's patterns see them, is one `remote.json`'s
/// `desktop` allows the repo, listed exactly (letter case included); `false` when no declared pattern matches. Keeps the
/// command here, saying why, when the file has a problem, shown [`escaped`], or when that pattern is not allowed, every
/// one when `desktop` cannot be used.
///
/// `read` is what the pick already read of the file ([`steer_off_macos`]); a file it did not read is read here.
fn check_desktop(config: &Config, target: &Target, read: DesktopRead, words: &[String], facts: &dyn Facts) -> Result<bool, Here> {
    let found = match read {
        DesktopRead::Read(found) => found,
        DesktopRead::NotRead => desktop_pattern(&target.place, words, facts)?,
    };
    let Some(pattern) = found else {
        return Ok(false);
    };
    let (allowed, problem) = match config.allowed_desktop(&target.repo) {
        Ok(patterns) => (patterns, None),
        Err(problem) => (&[][..], Some(fold_lines(problem))),
    };
    if allowed.contains(&pattern) {
        return Ok(true);
    }
    Err(Some(LocalReason::DesktopNotAllowed { pattern, problem }))
}

/// A worktree whose repo the routing file lists, with the siblings its siblings file declares, not yet checked.
struct Opened {
    place: Place,
    repo: String,
    leaf: String,
    /// The declared siblings, in declared order; none when the file has a problem.
    siblings: Vec<SiblingRoot>,
    /// The siblings file's problem, when it has one.
    problem: Option<String>,
    /// How the lines name the siblings file, shown [`escaped`].
    file: String,
}

/// The worktree at `place` when `listed` says the routing file lists its repo, with its siblings file read; `Err(None)`
/// when it does not list it, whose file is then not read at all.
fn open(place: Place, facts: &dyn Facts, listed: impl FnOnce(&str) -> bool) -> Result<Opened, Here> {
    let repo = repo_name(&place.checkout);
    if !listed(&repo) {
        return Err(None);
    }
    let leaf = split_root(&place.root).2.to_owned();
    let (siblings, problem) = match facts.siblings(&place) {
        Ok(names) => (sibling_roots(&place.root, names), None),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    let file = escaped(&facts.declaration_said(&place, "siblings.toml"));
    Ok(Opened {
        place,
        repo,
        leaf,
        siblings,
        problem,
        file,
    })
}

/// `words` rewritten for the builder of the worktree at `place` carrying the siblings `siblings`, each a declared name
/// and its root, under the subst table `drives` ([`subst_drives`]), each rewritten path's separators written as
/// `separators` says.
fn rewritten<'s>(
    place: &Place,
    siblings: impl Iterator<Item = (&'s str, &'s str)>,
    words: &[String],
    drives: &[(char, String)],
    separators: Separators,
) -> Rewritten {
    let rewrite_place = RewritePlace {
        root: &place.root,
        prefix: &place.prefix,
        folder: &place.folder,
        leaf: split_root(&place.root).2,
    };
    rewrite_named(&rewrite_place, words, drives, siblings, separators)
}

/// The builder that runs `repo`'s command `words`, rewritten for Windows, and the pattern that chose it: the first of
/// [`Config::routes`] that `eligible` lets run it with a pattern that matches, the first such pattern of its list.
fn pick(config: &Config, repo: &str, words: &[String], eligible: impl Fn(&Builder) -> bool) -> Option<(Builder, String)> {
    config
        .routes(repo)
        .into_iter()
        .filter(|(builder, _)| eligible(builder))
        .find_map(|(builder, patterns)| first_match(patterns, words).map(|pattern| (builder.clone(), pattern)))
}

/// The declared siblings as the rewrite reads them: each declared name and its root, borrowed.
fn declared_pairs(siblings: &[SiblingRoot]) -> impl Iterator<Item = (&str, &str)> {
    siblings.iter().map(|sibling| (sibling.name.as_str(), sibling.root.as_str()))
}

/// The carried siblings as the rewrite reads them: each declared name and its root, borrowed.
fn carried_pairs(siblings: &[CarriedSibling]) -> impl Iterator<Item = (&str, &str)> {
    siblings.iter().map(|sibling| (sibling.name.as_str(), sibling.root.as_str()))
}

/// The target on `builder` of an opened worktree once its siblings pass their checks ([`check_siblings`]).
fn settle(config: &Config, opened: Opened, builder: Builder, facts: &dyn Facts) -> Result<Target, Here> {
    let Opened {
        place,
        repo,
        leaf,
        siblings,
        problem,
        file,
        ..
    } = opened;
    let declared = Declared {
        problem,
        file: &file,
        root: &place.root,
        leaf: &leaf,
        siblings: &siblings,
    };
    let (root_id, ids) = check_siblings(&declared, config.allowed_siblings(&repo), facts)?;
    let siblings = siblings
        .into_iter()
        .zip(ids)
        .map(|(SiblingRoot { name, root }, id)| CarriedSibling { name, root, id })
        .collect();
    Ok(Target {
        builder,
        key: work_folder_key(&place.root),
        inputs: config.inputs(&repo),
        place,
        repo,
        leaf,
        root_id,
        siblings,
    })
}

/// The first of `patterns` that matches `words` joined by single spaces.
fn first_match(patterns: &[Pattern], words: &[String]) -> Option<String> {
    let command = words.join(" ");
    patterns
        .iter()
        .find(|pattern| pattern.is_match(&command))
        .map(|pattern| pattern.as_str().to_owned())
}

/// The roots of the siblings `names` of the worktree root `root`, each as [`sibling_root`] spells it, in declared
/// order.
fn sibling_roots(root: &str, names: Vec<String>) -> Vec<SiblingRoot> {
    names
        .into_iter()
        .map(|name| SiblingRoot {
            root: sibling_root(root, &name),
            name,
        })
        .collect()
}

/// Where the sibling `name` of the worktree root `root` lies: beside the root, its parent and `name` joined by the
/// separator before the root's last segment, trailing separators aside. A root with no separator has the sibling
/// `/name`.
#[must_use]
pub fn sibling_root(root: &str, name: &str) -> String {
    let (parent, separator, _) = split_root(root);
    format!("{parent}{separator}{name}")
}

/// What a matched command's `.gate/siblings.toml` declared, with what the checks of its siblings need.
struct Declared<'a> {
    /// The file's first problem, when it has one; it then declares none.
    problem: Option<String>,
    /// How the lines name the file, shown [`escaped`].
    file: &'a str,
    /// The worktree root as git gives it.
    root: &'a str,
    /// The worktree root's folder name.
    leaf: &'a str,
    /// The declared siblings, in declared order.
    siblings: &'a [SiblingRoot],
}

/// Keeps a matched command here, saying why, for its siblings, checked in this order: the siblings file's problem,
/// shown [`escaped`]; a sibling name the builder cannot lay out; a declared sibling `allowed` does not name exactly
/// (letter case included), or every one when `allowed` is the reason `remote.json`'s `siblings` cannot be used; a root
/// folder name the builder cannot lay out; a sibling named like that folder (ASCII letter case aside); a sibling with
/// no git worktree or one refused ([`Facts::sibling_folder`]); a sibling whose folder is the root's or an earlier
/// sibling's. No sibling is looked for until every name is allowed
/// and known to be another folder's. With each sibling's folder id, in declared order. The root's own id is read for
/// every matched command, siblings or none, after the siblings are found ([`Facts::root_id`]): a root whose id cannot be
/// read keeps the run here, as its sync-back's lock needs it. With the root's id first.
fn check_siblings(declared: &Declared<'_>, allowed: Result<&[String], &str>, facts: &dyn Facts) -> Result<(FolderId, Vec<FolderId>), Here> {
    let &Declared {
        ref problem,
        file,
        root,
        leaf,
        siblings,
    } = declared;
    if let Some(problem) = problem {
        return Err(Some(LocalReason::SiblingsFile {
            file: file.to_owned(),
            problem: escaped(&fold_lines(problem)),
        }));
    }
    let root_id = || facts.root_id(root).map_err(|reason| Some(failed(&reason)));
    if siblings.is_empty() {
        return Ok((root_id()?, Vec::new()));
    }
    check_layout_names(siblings, file)?;
    check_consent(siblings, allowed)?;
    check_leaf(siblings, leaf, file)?;
    let ids = folder_ids(siblings, facts)?;
    let root_id = root_id()?;
    check_distinct(siblings, &ids, root_id)?;
    Ok((root_id, ids))
}

/// Keeps the run here for a sibling's name the builder cannot lay out, checked again here whatever the siblings file's
/// reader checked, so no sibling's root, the root's parent and its name joined, holds a separator or `..`. It reads no
/// folder and comes before consent, so a bad name is refused as such whatever `remote.json` allows. The line names the
/// siblings file as `file`.
fn check_layout_names(siblings: &[SiblingRoot], file: &str) -> Result<(), Here> {
    if let Some((index, sibling)) = siblings.iter().enumerate().find(|(_, sibling)| !is_layout_name(&sibling.name)) {
        return Err(Some(LocalReason::SiblingsFile {
            file: file.to_owned(),
            problem: format!("siblings[{index}] is not a folder name the builder can lay out: {:?}", sibling.name),
        }));
    }
    Ok(())
}

/// Keeps the run here when the root's folder name `leaf` is no name the builder can lay out, then for a sibling named
/// like that folder (ASCII letter case aside), the line naming the siblings file as `file`.
fn check_leaf(siblings: &[SiblingRoot], leaf: &str, file: &str) -> Result<(), Here> {
    if !is_layout_name(leaf) {
        return Err(Some(LocalReason::LeafNotLaidOut { leaf: leaf.to_owned() }));
    }
    if let Some((index, sibling)) = siblings.iter().enumerate().find(|(_, sibling)| sibling.name.eq_ignore_ascii_case(leaf)) {
        return Err(Some(LocalReason::SiblingsFile {
            file: file.to_owned(),
            problem: format!("siblings[{index}] is the worktree's own folder: {:?}", sibling.name),
        }));
    }
    Ok(())
}

/// Each sibling's folder id, in declared order, asked one after another; the first sibling that is missing or refused,
/// or whose folder cannot be read, keeps the run here, saying so, and no later one is asked.
fn folder_ids(siblings: &[SiblingRoot], facts: &dyn Facts) -> Result<Vec<FolderId>, Here> {
    let mut ids = Vec::with_capacity(siblings.len());
    for sibling in siblings {
        let name = sibling.name.clone();
        match facts.sibling_folder(&sibling.root).map_err(|reason| Some(failed(&reason)))? {
            SiblingFolder::Missing => return Err(Some(LocalReason::SiblingMissing { name })),
            SiblingFolder::Refused(reason) => return Err(Some(LocalReason::SiblingRefused { name, reason })),
            SiblingFolder::Present(id) => ids.push(id),
        }
    }
    Ok(ids)
}

/// Keeps the run here when a sibling's folder id `ids` holds, in declared order, is the worktree root's `root_id` or an
/// earlier sibling's: one folder reached by two names.
fn check_distinct(siblings: &[SiblingRoot], ids: &[FolderId], root_id: FolderId) -> Result<(), Here> {
    for (index, (sibling, id)) in siblings.iter().zip(ids).enumerate() {
        let reason = if *id == root_id {
            "is the worktree's own folder".to_owned()
        } else if let Some(earlier) = ids[..index].iter().position(|other| other == id) {
            format!("is the same folder as sibling {}", siblings[earlier].name)
        } else {
            continue;
        };
        return Err(Some(LocalReason::SiblingRefused {
            name: sibling.name.clone(),
            reason,
        }));
    }
    Ok(())
}

/// Keeps the run here, naming each declared sibling `allowed` does not name exactly, in declared order; every one when
/// `allowed` is why `remote.json`'s `siblings` cannot be used, which then allows none.
fn check_consent(siblings: &[SiblingRoot], allowed: Result<&[String], &str>) -> Result<(), Here> {
    let (allowed, problem) = match allowed {
        Ok(names) => (names, None),
        Err(problem) => (&[][..], Some(fold_lines(problem))),
    };
    let names: Vec<String> = siblings
        .iter()
        .filter(|sibling| !allowed.contains(&sibling.name))
        .map(|sibling| sibling.name.clone())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(Some(LocalReason::SiblingsNotAllowed { names, problem }))
}

/// The subst table for the drive letters the words use, other than the `root`'s: each letter whose device target names
/// a path (`\??\X:\dev`), mapped to that path followed through the subst drives it names ([`subst_path`]). Each letter
/// is asked once.
fn subst_drives(root: &str, words: &[String], facts: &dyn Facts) -> Vec<(char, String)> {
    let own = root_letter(root);
    let mut answers = Vec::new();
    drive_letters(root, words)
        .into_iter()
        .filter_map(|letter| subst_path(letter, own, facts, &mut answers).map(|path| (letter, path)))
        .collect()
}

/// The most subst drives a chain is followed through.
const SUBST_HOPS: usize = 26;

/// The path the subst drive `letter` stands for, its target followed while it starts on another drive than the root's
/// (`own`): `S:` standing for `T:\a` and `T:` for `X:\dev` gives `X:\dev\a`. `None` when a drive on the way is a volume
/// or no device, when the chain comes back to a drive it passed or runs past [`SUBST_HOPS`] drives. `answers` holds
/// each letter asked and its path, so no letter is asked twice.
fn subst_path(letter: char, own: Option<char>, facts: &dyn Facts, answers: &mut Vec<(char, Option<String>)>) -> Option<String> {
    let mut passed = vec![letter];
    let mut path = device_path(letter, facts, answers)?;
    for _ in 0..SUBST_HOPS {
        let Some(next) = root_letter(&path).filter(|next| Some(*next) != own) else {
            return Some(path);
        };
        if passed.contains(&next) {
            return None;
        }
        passed.push(next);
        let target = device_path(next, facts, answers)?;
        path = format!("{}{}", target.trim_end_matches(['\\', '/']), &path[2..]);
    }
    None
}

/// The path the device of the drive `letter` names (`\??\X:\dev` gives `X:\dev`), asked once and kept in `answers`;
/// `None` for a volume or no device.
fn device_path(letter: char, facts: &dyn Facts, answers: &mut Vec<(char, Option<String>)>) -> Option<String> {
    if let Some((_, path)) = answers.iter().find(|(asked, _)| *asked == letter) {
        return path.clone();
    }
    let path = facts
        .subst_target(letter)
        .and_then(|target| target.strip_prefix(r"\??\").map(str::to_owned));
    answers.push((letter, path.clone()));
    path
}

/// The routed run of a matched command: its target, its rewritten words with `--no-build` dropped, then marked and
/// stamped as [`builder_words`] marks and stamps them, the worktree's `HEAD` sha, asked once whatever the command, and
/// the builders the run may try next ([`RemoteRun::later`]), which the config alone gives.
fn remote_run(config: &Config, matched: Matched, subfolder: String, given: &[String], facts: &dyn Facts) -> RemoteRun {
    let Matched {
        target,
        pattern,
        words: sent,
        program_named,
        desktop,
        later,
        ..
    } = matched;
    let sent_count = sent.len();
    let cargo = is_cargo(&sent);
    // A macOS builder builds the project itself: the PC's Windows build is no use to it.
    let listed = target.builder.os == BuilderOs::Windows && config.send_build_allowed(&target.repo) == Ok(true);
    let send = listed
        .then(|| send_bin(given, &target.place, |folder, file| facts.moves_build_output(&target.place, folder, file)))
        .flatten()
        .map(|bin| {
            let mut kept = sent.clone();
            mark_bare_first_word(&mut kept, facts, program_named);
            SendPlan { words: kept, bin }
        });
    let mut words = without_no_build(sent);
    let dropped_no_build = words.len() != sent_count;
    let fallback = without_no_build(given.to_vec());
    let mut outputs = match config.own_outputs_on(&target.builder, &target.repo) {
        Some(globs) => globs,
        None if target.builder.os == BuilderOs::Macos => Vec::new(),
        None if cargo => CARGO_OUTPUTS.iter().map(|glob| (*glob).to_owned()).collect(),
        None => DEFAULT_OUTPUTS.iter().map(|glob| (*glob).to_owned()).collect(),
    };
    let outputs_withheld = fallback.len() != given.len() && !outputs.is_empty();
    if outputs_withheld {
        outputs.clear();
    }
    mark_bare_first_word(&mut words, facts, program_named);
    let source_revision = head_of(&target.place, facts);
    stamp(&mut words, || source_revision.clone(), &target, facts);
    RemoteRun {
        builder: target.builder,
        key: target.key,
        leaf: target.leaf,
        root_id: target.root_id,
        root: target.place.root,
        siblings: target.siblings,
        repo: target.repo,
        subfolder,
        pattern,
        words,
        fallback,
        program_named,
        outputs,
        inputs: target.inputs,
        dropped_no_build,
        send,
        outputs_withheld,
        desktop,
        source_revision,
        later,
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
/// gate's folder, which the builder would otherwise look for on its `PATH`. A first word the rewrite sent as a
/// program's name (`program_named`) is left as it stands: the caller asked for the program outside the worktree, not
/// for a file of this folder.
fn mark_bare_first_word(words: &mut [String], facts: &dyn Facts, program_named: bool) {
    if program_named {
        return;
    }
    let Some((index, word)) = first_word(words) else {
        return;
    };
    let bare = !word.contains(['\\', '/', ':']) && word != "." && word != "..";
    if bare && facts.file_in_folder(word) {
        words[index] = format!("./{word}");
    }
}

/// Writes the builder's own words into the words of a dotnet command that builds, after the dotnet command's own words
/// (before its own `--`, or after the last word when it has none): `-p:SourceRevisionId=<sha>`, `sha` the worktree's
/// `HEAD`, so the builder's build is pinned to the revision the snapshot came from; then the path-map word
/// ([`path_map_word`]) of the trees [`Facts::box_trees`] gives for `target`, so its outputs record this PC's paths.
/// The source-revision word is left out when the command's own words set `SourceRevisionId` or `sha` gives none, which
/// is asked only when the word may go in; the path-map word when they set any of [`PATH_MAP_OPT_OUTS`] or there is no
/// tree. The words of a command that is no such dotnet command, or that does not build ([`dotnet_builds`]), a kept
/// `--no-build` among them, are left as they are.
fn stamp(words: &mut Vec<String>, sha: impl FnOnce() -> Option<String>, target: &Target, facts: &dyn Facts) {
    if !dotnet_builds(words) {
        return;
    }
    let Some(own) = dotnet_own(words) else {
        return;
    };
    let own_words = &words[own.clone()];
    let sets_any = |names: &[&str]| names.iter().any(|name| sets_property(own_words, name));
    let revision = if sets_any(&[SOURCE_REVISION]) { None } else { sha() };
    let path_map = if sets_any(&PATH_MAP_OPT_OUTS) {
        None
    } else {
        path_map_word(&facts.box_trees(target), target.builder.os)
    };
    let added = revision.map(|sha| format!("-p:{SOURCE_REVISION}={sha}")).into_iter().chain(path_map);
    words.splice(own.end..own.end, added);
}

/// The `-p:PathMap=` word of `trees`, each a tree on a builder running `os` and the PC root it stands for, in order:
/// `tree\=root`, the box side ending in exactly one of the builder's separators (`/` on macOS: `tree/=root`), so
/// `work\key` never maps `work\key2`, and the PC side in no `\`, since the compiler adds it (one more there records a
/// doubled separator once the compiler quotes the value), the pairs joined with [`PATH_MAP_PAIR_SEPARATOR`]; `None`
/// when there is no tree.
fn path_map_word(trees: &[(String, String)], os: BuilderOs) -> Option<String> {
    if trees.is_empty() {
        return None;
    }
    let separator = match os {
        BuilderOs::Windows => '\\',
        BuilderOs::Macos => '/',
    };
    let pairs: Vec<String> = trees
        .iter()
        .map(|(tree, root)| format!("{}{separator}={}", tree.trim_end_matches(separator), root.trim_end_matches('\\')))
        .collect();
    Some(format!("-p:{PATH_MAP}={}", pairs.join(PATH_MAP_PAIR_SEPARATOR)))
}

/// The worktree's `HEAD` sha at `place`, or `None` when the branch is unborn or git cannot read it: a run without
/// the sha routes as it would with it, unpinned.
fn head_of(place: &Place, facts: &dyn Facts) -> Option<String> {
    facts.head_sha(place).ok().flatten()
}

/// Whether a dotnet command's words, its dotnet word first, build: its verb ([`verb`]) is one of [`BUILD_VERBS`].
fn builds(words: &[String]) -> bool {
    verb_in(words, &BUILD_VERBS)
}

/// Whether one of `words` sets the `MSBuild` property `name`: its property list ([`property_list`]) is a `;`- or
/// `,`-separated list in which one setting's name (the text before its `=`, the whole setting when it has none, white
/// space and double quotes around it trimmed) is `name`, the case ignored.
fn sets_property(words: &[String], name: &str) -> bool {
    words.iter().enumerate().any(|(at, word)| {
        property_list(word, words.get(at + 1)).is_some_and(|list| {
            list.split([',', ';']).any(|setting| {
                let setting_name = setting.split_once('=').map_or(setting, |(setting, _)| setting);
                eq_ignoring_case(setting_name.trim_matches(|c: char| c.is_whitespace() || c == '"'), name)
            })
        })
    })
}

/// The property list `word` sets, `next` the word after it. A property switch is an optional `--`, `-` or `/`, then one
/// of [`PROPERTY_SWITCH_NAMES`] (any case): followed in the word by `:` or `=`, the rest of the word is the list; ending
/// the word, `next` is, when it holds `=`. A word after a bare `-p` without `=` names a project to an older SDK's
/// `dotnet run`, and sets nothing. A word read as a switch that is none only keeps the gate's word off.
fn property_list<'w>(word: &'w str, next: Option<&'w String>) -> Option<&'w str> {
    let unprefixed = word.strip_prefix("--").or_else(|| word.strip_prefix(['-', '/'])).unwrap_or(word);
    for switch in PROPERTY_SWITCH_NAMES {
        let Some(rest) = unprefixed
            .get(..switch.len())
            .filter(|start| eq_ignoring_case(start, switch))
            .map(|_| &unprefixed[switch.len()..])
        else {
            continue;
        };
        if rest.is_empty() {
            return next.filter(|next| next.contains('=')).map(String::as_str);
        }
        if let Some(list) = rest.strip_prefix([':', '=']) {
            return Some(list);
        }
    }
    None
}

/// A fact's failure as the reason said, its line breaks and the white space around them folded into one space.
fn failed(reason: &str) -> LocalReason {
    LocalReason::Failed { reason: fold_lines(reason) }
}

pub(crate) fn fold_lines(text: &str) -> String {
    let parts: Vec<&str> = text.split(['\r', '\n']).collect();
    let [first, middle @ .., last] = parts.as_slice() else {
        return text.to_owned();
    };
    let mut pieces = vec![first.trim_end()];
    pieces.extend(middle.iter().map(|part| part.trim()).filter(|part| !part.is_empty()));
    pieces.push(last.trim_start());
    pieces.join(" ")
}

/// The dotnet verb whose `--no-build` a send lets the builder keep.
const SEND_VERB: [&str; 1] = ["test"];
/// The options that move a dotnet command's build output, whose `--no-build` no send keeps.
const OUTPUT_OPTIONS: [&str; 3] = ["-o", "--output", "--artifacts-path"];
/// The `MSBuild` properties that move a project's build output out of its `bin\{configuration}` folder, whose
/// `--no-build` no send keeps: `Platform` among them, as a platform other than `AnyCPU` builds into
/// `bin\{platform}\{configuration}`.
pub const OUTPUT_PROPERTIES: [&str; 5] = ["OutputPath", "BaseOutputPath", "UseArtifactsOutput", "OutDir", "Platform"];

/// The bin folder whose files a routed `dotnet test --no-build` sends so the builder keeps its `--no-build`
/// ([`SendPlan::bin`]), read from the command `words` as given at `place`: the dotnet command's verb is `test`, its own
/// words hold `--no-build`, name one project ([`DotnetTarget::Project`]) inside the worktree and set the configuration
/// (`-c` or `--configuration`) to one plain name, and neither name an output option ([`OUTPUT_OPTIONS`]) nor set an
/// output property ([`OUTPUT_PROPERTIES`]); `moves_output`, asked last of the project's folder below the root and the
/// project file's name when one is named, says whether its build files may move it ([`Facts::moves_build_output`]).
/// `None` for every other command, whose `--no-build` is dropped as before.
fn send_bin(words: &[String], place: &Place, moves_output: impl FnOnce(&str, Option<&str>) -> bool) -> Option<String> {
    let own = &words[dotnet_own(words)?];
    if !verb_in(own, &SEND_VERB) || !own.iter().any(|word| word == NO_BUILD) {
        return None;
    }
    let Some(DotnetTarget::Project(project)) = dotnet_target(words) else {
        return None;
    };
    let configuration = option_value(own, &CONFIGURATION_OPTIONS).filter(|value| is_plain_name(value))?;
    if own.iter().any(|word| is_output_option(word)) || OUTPUT_PROPERTIES.iter().any(|name| sets_property(own, name)) {
        return None;
    }
    let folder = project_folder(&project, place)?;
    let file = has_extension(&project, &PROJECT_EXTENSIONS).then(|| project.rsplit(['/', '\\']).next().unwrap_or(&project));
    if moves_output(&folder, file) {
        return None;
    }
    Some(format!("{folder}bin/{configuration}"))
}

/// Whether `word` is one of [`OUTPUT_OPTIONS`], alone or with its value after `=` or `:`, ignoring letter case.
fn is_output_option(word: &str) -> bool {
    OUTPUT_OPTIONS.iter().any(|option| {
        word.get(..option.len()).is_some_and(|head| eq_ignoring_case(head, option))
            && matches!(word.as_bytes().get(option.len()), None | Some(b'=' | b':'))
    })
}

/// Whether `value` is one folder name: not empty, `.` or `..`, and with no separator, drive colon or wildcard.
fn is_plain_name(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
}

/// The folder of the project `project` names (a project file's folder, or the folder `--project` names) below the
/// worktree root of `place`, `/`-separated and `/`-ended, empty at the root: a relative path is read against the gate's
/// folder, an absolute one against the root or the gate's folder as spelled. `None` when it climbs out of the worktree
/// or is absolute elsewhere.
fn project_folder(project: &str, place: &Place) -> Option<String> {
    let project = project.replace('\\', "/");
    let folder = if has_extension(&project, &PROJECT_EXTENSIONS) {
        project.rsplit_once('/').map_or("", |(folder, _)| folder)
    } else {
        project.as_str()
    };
    let absolute = folder.starts_with('/') || folder.as_bytes().get(1) == Some(&b':');
    let below = if absolute {
        let below = |root: &str| {
            let root = root.replace('\\', "/").trim_end_matches('/').to_owned();
            let head = folder.get(..root.len())?;
            let rest = &folder[root.len()..];
            (eq_ignoring_case(head, &root) && (rest.is_empty() || rest.starts_with('/'))).then(|| rest.to_owned())
        };
        below(&place.root).or_else(|| below(&place.folder).map(|rest| format!("{}{rest}", place.prefix)))?
    } else {
        format!("{}{folder}", place.prefix)
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in below.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            name => segments.push(name),
        }
    }
    let mut folder = String::new();
    for segment in segments {
        folder.push_str(segment);
        folder.push('/');
    }
    Some(folder)
}

#[cfg(test)]
mod tests {
    use super::{Place, send_bin};

    /// The place of a gate at `prefix` below the worktree root `X:/dev/app`.
    fn place(prefix: &str) -> Place {
        Place {
            root: "X:/dev/app".to_owned(),
            prefix: prefix.to_owned(),
            checkout: "app".to_owned(),
            folder: format!(r"X:\dev\app\{}", prefix.replace('/', "\\")),
        }
    }

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    /// The bin folder [`send_bin`] gives `text` at the root, the build files moving nothing.
    fn sent(text: &str) -> Option<String> {
        send_bin(&words(text), &place(""), |_, _| false)
    }

    #[test]
    fn a_test_of_one_project_with_no_build_and_a_configuration_sends_its_bin() {
        assert_eq!(
            sent(r"dotnet test --project tests\App.Tests\App.Tests.csproj -c Release --no-build"),
            Some("tests/App.Tests/bin/Release".to_owned())
        );
        assert_eq!(
            sent("dotnet test App.csproj --configuration=Debug --no-build"),
            Some("bin/Debug".to_owned())
        );
        assert_eq!(
            sent("dotnet test --project tests/App.Tests -c:Release --no-build"),
            Some("tests/App.Tests/bin/Release".to_owned())
        );
    }

    #[test]
    fn a_project_is_read_against_the_gate_s_folder_or_the_root() {
        let relative = send_bin(
            &words(r"dotnet test ..\other\Other.csproj -c Release --no-build"),
            &place("tests/"),
            |_, _| false,
        );
        assert_eq!(relative, Some("other/bin/Release".to_owned()));
        let absolute = send_bin(
            &words(r"dotnet test X:\dev\app\src\App.csproj -c Release --no-build"),
            &place("tests/"),
            |_, _| false,
        );
        assert_eq!(absolute, Some("src/bin/Release".to_owned()));
        assert_eq!(
            sent(r"dotnet test ..\elsewhere\App.csproj -c Release --no-build"),
            None,
            "outside the worktree"
        );
        assert_eq!(sent(r"dotnet test D:\elsewhere\App.csproj -c Release --no-build"), None, "another drive");
    }

    #[test]
    fn each_output_option_keeps_the_drop() {
        for option in [
            "-o out",
            "--output out",
            "--artifacts-path out",
            "-o:out",
            "--output=out",
            "--ARTIFACTS-PATH:out",
        ] {
            let command = format!("dotnet test App.csproj -c Release --no-build {option}");
            assert_eq!(sent(&command), None, "{command}");
        }
    }

    #[test]
    fn each_output_property_keeps_the_drop() {
        for property in [
            "-p:OutputPath=out",
            "-p:BaseOutputPath=out",
            "/p:UseArtifactsOutput=true",
            "--property:outputpath=x",
        ] {
            let command = format!("dotnet test App.csproj -c Release --no-build {property}");
            assert_eq!(sent(&command), None, "{command}");
        }
    }

    /// Each spelling of setting `property` in the words: `-p:`, `/p:` and `-property:`, in any letter case.
    fn assert_property_keeps_the_drop(property: &str) {
        let lower = property.to_lowercase();
        for set in [
            format!("-p:{property}=x"),
            format!("/p:{property}=x"),
            format!("-property:{property}=x"),
            format!("-P:{lower}=x"),
            format!("/property:{}=x", property.to_uppercase()),
        ] {
            let command = format!("dotnet test App.csproj -c Release --no-build {set}");
            assert_eq!(sent(&command), None, "{command}");
        }
    }

    #[test]
    fn out_dir_in_the_words_keeps_the_drop() {
        assert_property_keeps_the_drop("OutDir");
    }

    #[test]
    fn platform_in_the_words_keeps_the_drop() {
        assert_property_keeps_the_drop("Platform");
    }

    #[test]
    fn the_build_files_are_asked_of_the_project_s_folder_and_file() {
        let asked = |text: &str| {
            let mut asked = None;
            send_bin(&words(text), &place(""), |folder, file| {
                asked = Some((folder.to_owned(), file.map(str::to_owned)));
                false
            });
            asked
        };
        assert_eq!(
            asked(r"dotnet test tests\App.Tests\App.Tests.csproj -c Release --no-build"),
            Some(("tests/App.Tests/".to_owned(), Some("App.Tests.csproj".to_owned())))
        );
        assert_eq!(
            asked("dotnet test --project tests/App.Tests -c Release --no-build"),
            Some(("tests/App.Tests/".to_owned(), None)),
            "a folder named: its project files are read"
        );
    }

    #[test]
    fn build_files_that_move_the_output_keep_the_drop() {
        assert_eq!(
            send_bin(&words("dotnet test App.csproj -c Release --no-build"), &place(""), |_, _| true),
            None
        );
    }

    #[test]
    fn a_run_with_no_build_keeps_the_drop() {
        assert_eq!(sent("dotnet run --project App.csproj -c Release --no-build"), None);
    }

    #[test]
    fn a_test_without_a_configuration_keeps_the_drop() {
        assert_eq!(sent("dotnet test App.csproj --no-build"), None);
        assert_eq!(
            sent("dotnet test App.csproj -c ../x --no-build"),
            None,
            "a configuration that is no plain name"
        );
    }

    #[test]
    fn a_test_of_a_solution_or_of_nothing_named_keeps_the_drop() {
        assert_eq!(sent("dotnet test App.slnx -c Release --no-build"), None);
        assert_eq!(sent("dotnet test -c Release --no-build"), None);
    }

    #[test]
    fn a_test_that_builds_sends_nothing() {
        assert_eq!(sent("dotnet test App.csproj -c Release"), None);
    }
}

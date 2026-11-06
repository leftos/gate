//! A routed run's ignored inputs on the PC: the files under its repo's input globs that git reports ignored in the
//! worktree, hashed into the inputs manifest before anything is sent, each file whose stamp the inputs hash cache holds
//! taking its hash from it. The globs are relative to the worktree root, `/`-separated and matched without regard to
//! case, as output globs are, with the inputs' own exclusions: any `.git*` path, a `.lfsconfig`, a `.env`, `*.env`,
//! `.env.<name>` or `.envrc` file, the gate's own archives and stages under `.tmp` (named for the work folder's key) and
//! the run's own log and `.err` file are never sent. A match git does not report ignored is in the snapshot, and one
//! inside a folder holding a `.git` of its own is another repository's; each is skipped with a line. A run also carries
//! the files of each initialized submodule its `.gitmodules` lists, as git lists them there, at their paths below the
//! worktree root; a sibling pair's run carries each tree's, at their paths below that tree.

use std::cell::{Cell, OnceCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_inputs::manifest::{refused_path, tree_split};
use gate_inputs::names::{is_git_s, is_plain_path};
use gate_router::{BuilderOs, CARRIED_INPUT, PAIR_CAPS, escaped, work_folder_key};
use gate_state::{FallbackReason, v1};
use gate_win32::Interrupt;
use gate_wire::layout_text;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_READ};

use super::channel;
use super::client::{Attempt, Called, End, RawSink, Sent, Watch};
use super::git::{Call, Git, GitBounds, GitError, OUTPUT_CAP, git_failure};
use super::outputs::{Archive, DELETE, GENERIC_WRITE, Unbuilt, sha256_for};
use super::payload::{self, InputsStep, Location};
use super::watchdog::{self, Cut};
use crate::outputs::archive::{final_path, pack, pack_rooted};
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{Chosen, HeldBy, MANIFEST_LIMIT, Manifest, Wrote, build_manifest_with};
use crate::outputs::walk::{Found, in_context, stamp_of, walk};

/// The most files one put carries: the archive's member limit.
pub(super) const MEMBERS: usize = 50_000;
/// The paths inputs never carry, whatever the globs: anything under a `.git` folder, a `.env.<name>` file, a `.env` or
/// `*.env` file, and a `.envrc` file. Matched without regard to case; the gate's own files are added per run
/// ([`exclusions`]).
pub(crate) const EXCLUDED: &str = r"(^|/)\.git/|(^|/)\.env\.[^/]*$|\.env$|(^|/)\.envrc$";
/// The worktree's folder the inputs' diff answer and archive are made in, held from before the diff until the put ends.
pub(super) const TMP: &str = ".tmp";
/// `GENERIC_READ`, which lives in `Win32_Foundation`, a feature this crate does not enable: the access the diff answer
/// and the archive are read back through.
const GENERIC_READ: u32 = 0x8000_0000;

/// The caps an inputs manifest is built under: the most files, and the most bytes of its line.
#[derive(Clone, Copy)]
pub(super) struct Caps {
    pub(super) members: usize,
    pub(super) manifest: usize,
}

impl Caps {
    /// The caps of every run: [`MEMBERS`] files and a line of [`MANIFEST_LIMIT`] bytes.
    pub(super) const FULL: Caps = Caps {
        members: MEMBERS,
        manifest: MANIFEST_LIMIT,
    };
}

/// What a run's inputs manifest is built from: the worktree `root`, the repo's input `globs`, the work folder's `key`,
/// which names the gate's own archives and stages under `.tmp`, the run's `log`, the inputs hash cache's file `cache`,
/// the `caps` it is built under, and the bounds of the `git check-ignore` call, answering to the console
/// `interrupt`.
pub(super) struct Asked<'a> {
    pub(super) root: &'a Path,
    pub(super) globs: &'a [String],
    pub(super) key: &'a str,
    pub(super) log: &'a Path,
    pub(super) cache: &'a Path,
    pub(super) caps: Caps,
    pub(super) bounds: GitBounds,
    pub(super) interrupt: Option<Interrupt>,
}

/// A run's inputs manifest as built: its line, newline included, the SHA-256 of that line without its newline (the
/// run call's `InputsDigest`), the manifest it holds, the files it names as the walk found them, the log-only lines
/// the build said: each glob that matched nothing, each folder that is another repository and each match that is in
/// the snapshot, and what its landing does with the work folder's carried marker.
pub(super) struct Built {
    pub(super) line: String,
    pub(super) digest: String,
    pub(super) manifest: Manifest,
    pub(super) found: Vec<Found>,
    pub(super) said: Vec<String>,
    pub(super) marking: Marking,
    /// On a sibling pair's run, each tree its manifest's paths are prefixed with, the main repo's first; empty for one
    /// repo, whose files all lie below the worktree root.
    pub(super) trees: Vec<Tree>,
}

/// One tree of a sibling pair's inputs: the folder name its manifest paths are prefixed with, its root on the PC, which
/// its files are packed from and must resolve inside, and its own inputs hash cache, keyed by paths relative to it.
pub(super) struct Tree {
    prefix: String,
    root: PathBuf,
    cache: PathBuf,
}

impl Tree {
    /// The prefixes of `trees`, in order, for [`Tree::of`]: made once per pack, not once per file.
    fn names(trees: &[Tree]) -> Vec<&str> {
        trees.iter().map(|tree| tree.prefix.as_str()).collect()
    }

    /// The tree of `trees`, whose prefixes are `names` ([`Tree::names`]), the prefixed `path` lies in, and the path
    /// relative to it; `None` for a path of no tree, and always for one repo, which has no trees.
    fn of<'t, 'p>(trees: &'t [Tree], names: &[&str], path: &'p str) -> Option<(&'t Tree, &'p str)> {
        let (at, rest) = tree_split(path, names)?;
        Some((trees.get(at)?, rest))
    }
}

/// What a landed inputs step does with its work folder's carried marker ([`v1::inputs_carried_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Marking {
    /// Nothing: the step carried no path a command named, or it is a sibling pair's, whose pair inputs marker sweeps
    /// the paths it carried ([`pair_marked`]).
    Leave,
    /// Writes the marker: a single repo's step carried a path a command named, or an initialized submodule's files.
    Write,
    /// Removes the marker and the inputs hash cache: the step was the empty manifest that removed the carried copies.
    Clear,
    /// Removes the pair inputs marker and every inputs hash cache of the work folder: the step was a sibling pair's
    /// empty manifest, which removed the copies an earlier pair run's inputs put, so the next run with no inputs sends
    /// none.
    Swept,
}

/// What a single repo's run sends for its inputs ([`single`]): the manifest built, or none, with the lines its build
/// said.
enum Single {
    Built(Built),
    None(Vec<String>),
}

/// The inputs of a single repo's run `asked` names: its initialized submodules' files ([`submodules`]) beside its globs'
/// files ([`built`]), git found once for both; none, with the lines said, when it has no globs and no submodule file is
/// carried. [`Unbuilt::Interrupted`] once `go_on` says no or git was interrupted; else why the manifest is not sent,
/// a set with the submodule files past a cap among them.
fn single(asked: &Asked<'_>, go_on: impl Fn() -> bool) -> Result<Single, Unbuilt> {
    let finder = Finder::new(asked.root);
    let carried = submodules(asked, &finder, &go_on)?;
    if asked.globs.is_empty() && carried.sent.is_empty() {
        return Ok(Single::None(carried.said));
    }
    built(asked, &finder, &carried, &go_on).map(Single::Built)
}

/// git found for one build at most once, when a step first asks for it.
struct Finder<'a> {
    root: &'a Path,
    git: OnceCell<Result<Git, String>>,
}

impl<'a> Finder<'a> {
    fn new(root: &'a Path) -> Finder<'a> {
        Finder { root, git: OnceCell::new() }
    }

    /// git, found along the gate's own `PATH` read against the root the first time; else why it cannot be found.
    fn git(&self) -> Result<&Git, String> {
        self.git.get_or_init(|| Git::find(self.root)).as_ref().map_err(Clone::clone)
    }
}

/// The inputs manifest `asked` names: the matched files git reports ignored, outside any folder holding a `.git` of its
/// own, with the `carried` files of the worktree's initialized submodules ([`with_submodules`]), each hashed once its
/// stamp is read through its handle, or taking the hash the inputs hash cache holds at that stamp; `go_on` is asked
/// before each file and each read. The cache the build leaves is saved, unless it equals the one it read. A set past
/// `asked`'s caps is refused: the globbed files alone as they always were, and the set with the submodule files only
/// when the globbed ones alone fit. [`Unbuilt::Interrupted`] once `go_on` says no or git was interrupted; else why the
/// globs, the walk, git or a file failed, or why the set is not sent.
fn built(asked: &Asked<'_>, finder: &Finder<'_>, carried: &Carried, go_on: &impl Fn() -> bool) -> Result<Built, Unbuilt> {
    let mut picked = if asked.globs.is_empty() {
        Picked::none()
    } else {
        picked(asked, finder)?
    };
    let merged = with_submodules(asked, &mut picked, carried, go_on)?;
    let Picked { kept, said, marking, .. } = picked;
    let (manifest, line, kept, marking) = if let Some((manifest, line, files)) = merged {
        (manifest, line, files, Marking::Write)
    } else {
        let (manifest, line) = globbed_manifest(asked, &kept, go_on)?;
        (manifest, line, kept, marking)
    };
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    Ok(Built {
        line,
        digest,
        manifest,
        found: kept,
        said,
        marking,
        trees: Vec::new(),
    })
}

/// The manifest of the globbed files `kept` alone ([`hashed`]) and its line, refused past `asked`'s member cap or its
/// manifest's cap.
fn globbed_manifest(asked: &Asked<'_>, kept: &[Found], go_on: &impl Fn() -> bool) -> Result<(Manifest, String), Unbuilt> {
    if kept.len() > asked.caps.members {
        return Err(Unbuilt::Failed(past_members(kept.len(), asked.caps.members)));
    }
    let manifest = hashed(asked, kept, go_on).map_err(Unmade::unbuilt)?;
    let line = manifest.line();
    if line.len() > asked.caps.manifest {
        return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned()));
    }
    Ok((manifest, line))
}

/// Why a set with submodule files whose manifest would pass the box's cap is not sent.
const SUBMODULES_PAST_THE_MANIFEST: &str = "the submodules' files would make the inputs manifest larger than the box takes (64 MiB)";

/// The manifest of the globbed files `picked` chose with the `carried` submodule files, its line, and those files,
/// ordered by path, the submodules' lines said into `picked`'s ([`said_carried`]); `None` when none is carried. A set
/// past a cap is refused, so the run stays here rather than run on the builder without the submodules: the members
/// before any file is hashed, then the manifest's line as built, each blamed on the globbed files when they alone pass
/// it ([`past_members`], [`PAST_THE_CAP`]) and on the submodules' files only when the globbed ones alone fit.
/// [`Unbuilt::Interrupted`] once `go_on` says no; else why a file could not be hashed.
fn with_submodules(
    asked: &Asked<'_>,
    picked: &mut Picked,
    carried: &Carried,
    go_on: &impl Fn() -> bool,
) -> Result<Option<(Manifest, String, Vec<Found>)>, Unbuilt> {
    picked.said.extend(carried.said.iter().cloned());
    if carried.sent.is_empty() {
        return Ok(None);
    }
    let cap = asked.caps.members;
    if picked.kept.len() > cap {
        return Err(Unbuilt::Failed(past_members(picked.kept.len(), cap)));
    }
    // `outside_nested` drops every globbed file inside a folder holding a `.git`, so the two sets share no path.
    let mut files = std::mem::take(&mut picked.kept);
    files.extend(carried.found.iter().cloned());
    files.sort_by(|first, second| first.path.cmp(&second.path));
    if files.len() > cap {
        return Err(Unbuilt::Failed(past_members_with_submodules(files.len(), cap)));
    }
    let line = match hashed(asked, &files, go_on) {
        Ok(manifest) => Some((manifest.line(), manifest)),
        Err(Unmade::Capped(_)) => None,
        Err(Unmade::Unbuilt(unbuilt)) => return Err(unbuilt),
    };
    let Some((line, manifest)) = line.filter(|(line, _)| line.len() <= asked.caps.manifest) else {
        return Err(past_the_manifest(asked, &files, carried, go_on));
    };
    said_carried(&mut picked.said, carried, &picked.nested);
    Ok(Some((manifest, line, files)))
}

/// Why a set of `count` files with the submodules' files, past the member cap `cap`, is not sent.
fn past_members_with_submodules(count: usize, cap: usize) -> String {
    format!("{count} files with the submodules' files, more than one put carries ({cap})")
}

/// Why the set `files` with the `carried` submodule files, whose manifest passes `asked`'s cap, is not sent: the
/// globbed files' own refusal when they alone pass it ([`globbed_manifest`]), else [`SUBMODULES_PAST_THE_MANIFEST`];
/// the interrupt, or why a globbed file could not be hashed, as it is.
fn past_the_manifest(asked: &Asked<'_>, files: &[Found], carried: &Carried, go_on: &impl Fn() -> bool) -> Unbuilt {
    let submodule: HashSet<&str> = carried.found.iter().map(|file| file.path.as_str()).collect();
    let globbed: Vec<Found> = files.iter().filter(|file| !submodule.contains(file.path.as_str())).cloned().collect();
    match globbed_manifest(asked, &globbed, go_on) {
        Ok(_) => Unbuilt::Failed(SUBMODULES_PAST_THE_MANIFEST.to_owned()),
        Err(unbuilt) => unbuilt,
    }
}

/// Says into `said` each submodule `carried` sends, with its count of files; and drops the line saying a folder is
/// another repository, the folder `nested` names spelled as the disk does and matched to the submodule's path without
/// regard to ASCII case, when every globbed file `nested` says was dropped there is among the carried files, so the
/// line stays while a globbed file there is still not sent.
fn said_carried(said: &mut Vec<String>, carried: &Carried, nested: &BTreeMap<String, Vec<String>>) {
    let sent: HashSet<String> = carried.found.iter().map(|file| file.path.to_ascii_lowercase()).collect();
    for (folder, files) in &carried.sent {
        let covered = nested
            .iter()
            .filter(|(spelled, _)| spelled.eq_ignore_ascii_case(folder))
            .filter(|(_, dropped)| dropped.iter().all(|path| sent.contains(&path.to_ascii_lowercase())));
        for (spelled, _) in covered {
            let line = another_repository(spelled);
            said.retain(|said| *said != line);
        }
        let plural = if *files == 1 { "" } else { "s" };
        said.push(format!(
            "gate: inputs: submodule {}: {files} file{plural} sent as inputs",
            escaped(folder)
        ));
    }
}

/// The files `asked`'s globs choose before any is hashed: the matched files git reports ignored, outside any folder
/// holding a `.git` of its own; the log-only lines the choice said; and what its landing does with the carried marker.
struct Picked {
    kept: Vec<Found>,
    said: Vec<String>,
    marking: Marking,
    /// Each folder holding a `.git` of its own that a glob matched into, with the matched files it holds, none sent.
    nested: BTreeMap<String, Vec<String>>,
}

impl Picked {
    /// What a run with no globs chooses: nothing.
    fn none() -> Picked {
        Picked {
            kept: Vec::new(),
            said: Vec::new(),
            marking: Marking::Leave,
            nested: BTreeMap::new(),
        }
    }
}

/// Why a manifest's files were not hashed: past a cap, its line kept apart from every other failure so a caller tells
/// the caps' refusals by their kind, never by their wording; or the interrupt, or why a file failed.
enum Unmade {
    Capped(String),
    Unbuilt(Unbuilt),
}

impl Unmade {
    /// The failure as [`Unbuilt`] says it: a cap's line is one failure among others.
    fn unbuilt(self) -> Unbuilt {
        match self {
            Unmade::Capped(why) => Unbuilt::Failed(why),
            Unmade::Unbuilt(unbuilt) => unbuilt,
        }
    }
}

/// The files `asked` chooses ([`Picked`]): [`Unbuilt::Interrupted`] once git was interrupted; else why the globs, the
/// walk or git failed, or the path a command named that could not be sent.
fn picked(asked: &Asked<'_>, finder: &Finder<'_>) -> Result<Picked, Unbuilt> {
    let excluded = exclusions(asked.root, asked.key, asked.log);
    let own = regex::Regex::new(&excluded).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    // A carried path the exclusions match, such as the run's own log, is neither sent nor reported missing.
    let asked_globs: Vec<String> = asked
        .globs
        .iter()
        .filter(|glob| glob.strip_prefix(CARRIED_INPUT).is_none_or(|path| !own.is_match(path)))
        .cloned()
        .collect();
    let matched: Vec<String> = asked_globs
        .iter()
        .map(|glob| glob.strip_prefix(CARRIED_INPUT).unwrap_or(glob).to_owned())
        .collect();
    let globs = Globs::inputs(&matched, &excluded).map_err(Unbuilt::Failed)?;
    let found = walk(asked.root, &globs).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    if let Some(path) = unsent_carried(&asked_globs, &found) {
        return Err(Unbuilt::Failed(format!(
            "{} was named by the command but could not be sent",
            escaped(path)
        )));
    }
    let mut said = unmatched(&asked_globs, &globs, &found);
    let (found, nested) = outside_nested(asked.root, found);
    said.extend(nested.keys().map(|folder| another_repository(folder)));
    let ignored = ignored(asked, finder, &found)?;
    let (kept, tracked): (Vec<Found>, Vec<Found>) = found.into_iter().partition(|file| ignored.contains(&file.path));
    said.extend(
        tracked
            .iter()
            .map(|file| format!("gate: inputs: {} is in the snapshot; not sent as an input", escaped(&file.path))),
    );
    let marking = if asked_globs.iter().any(|glob| glob.starts_with(CARRIED_INPUT)) {
        Marking::Write
    } else {
        Marking::Leave
    };
    Ok(Picked { kept, said, marking, nested })
}

/// The line saying the files of `folder`, which holds a `.git` of its own, are not sent.
fn another_repository(folder: &str) -> String {
    format!("gate: inputs: {} is another repository; not sent as an input", escaped(folder))
}

/// The files a run carries from its worktree's initialized submodules ([`submodules`]): each file, its path below the
/// worktree root; the log-only lines the look said; and each submodule carried, its path and its count of files.
#[derive(Default)]
struct Carried {
    found: Vec<Found>,
    said: Vec<String>,
    sent: Vec<(String, usize)>,
}

/// The files of the initialized submodules of `asked`'s worktree ([`Carried`]), git found through `finder`. None, and no
/// git process started, when its `.gitmodules` is not a regular file; else each path `git config` lists there, of a
/// form the gate carries ([`carried_form`]) and a gitlink in the index at that very spelling ([`gitlinks`]), whose
/// folder is a real one holding a `.git` entry: its files ([`submodule_files`]). A path of another form, or one through
/// a link, is said and passed over. [`Unbuilt::Interrupted`] once `go_on` says no or git was interrupted; else why git
/// cannot be found, or the `.gitmodules`, the index or a submodule cannot be read, which keeps the run here.
fn submodules(asked: &Asked<'_>, finder: &Finder<'_>, go_on: &impl Fn() -> bool) -> Result<Carried, Unbuilt> {
    let mut carried = Carried::default();
    let unread = |why: String| Unbuilt::Failed(format!(".gitmodules cannot be read: {why}"));
    if !gitmodules_at(asked.root).map_err(unread)? {
        return Ok(carried);
    }
    let git = finder.git().map_err(|why| Unbuilt::Failed(format!("git cannot be found: {why}")))?;
    let paths = submodule_paths(git, asked).map_err(|unbuilt| unbuilt.failed_as(unread))?;
    let (paths, refused): (Vec<String>, Vec<String>) = paths.into_iter().partition(|path| carried_form(path));
    carried.said.extend(refused.iter().map(|path| not_carried(path)));
    let linked = gitlinks(git, asked, &paths)
        .map_err(|unbuilt| unbuilt.failed_as(|why| Unbuilt::Failed(format!("the index's submodules cannot be read: {why}"))))?;
    let excluded = regex::Regex::new(&exclusions(asked.root, asked.key, asked.log)).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    let look = Look {
        git,
        asked,
        excluded: &excluded,
        go_on,
    };
    for path in paths.iter().filter(|path| linked.contains(*path)) {
        carry(&look, path, &mut carried)?;
    }
    Ok(carried)
}

/// Whether `root` holds a `.gitmodules` that is a regular file, read without following a link: `false` when there is none,
/// or it is anything else. Else why it cannot be read.
fn gitmodules_at(root: &Path) -> Result<bool, String> {
    Ok(entry_at(&root.join(".gitmodules"))?.is_some_and(|metadata| metadata.is_file()))
}

/// What each submodule is looked at with: git, the run's `asked`, the inputs' exclusions at the worktree root, and the
/// run's `go_on`.
struct Look<'a> {
    git: &'a Git,
    asked: &'a Asked<'a>,
    excluded: &'a regex::Regex,
    go_on: &'a dyn Fn() -> bool,
}

impl Unbuilt {
    /// This failure with its reason made over by `as_failed`; the interrupt as it is.
    fn failed_as(self, as_failed: impl FnOnce(String) -> Unbuilt) -> Unbuilt {
        match self {
            Unbuilt::Failed(why) => as_failed(why),
            Unbuilt::Interrupted => Unbuilt::Interrupted,
        }
    }
}

/// The line of a `.gitmodules` path the gate does not carry.
fn not_carried(path: &str) -> String {
    format!("gate: inputs: submodule path {} is not one the gate carries; not sent", escaped(path))
}

/// Adds to `carried` the files of the submodule at the `.gitmodules` path `path`, when its folder is a real one holding
/// a `.git` entry ([`submodule_folder`]): nothing for an uninitialized one, its line for one through a link, a junction
/// or a file. [`Unbuilt::Interrupted`] once `look`'s `go_on` says no or git was interrupted; else why it cannot be
/// listed.
fn carry(look: &Look<'_>, path: &str, carried: &mut Carried) -> Result<(), Unbuilt> {
    let unlisted = |why: String| Unbuilt::Failed(format!("submodule {} cannot be listed: {why}", escaped(path)));
    let folder = match submodule_folder(look.asked.root, path).map_err(unlisted)? {
        Placed::Absent => return Ok(()),
        Placed::Refused => {
            carried.said.push(not_carried(path));
            return Ok(());
        }
        Placed::Ready(folder) => folder,
    };
    let files = submodule_files(look, &folder, path, &mut carried.said).map_err(|unbuilt| unbuilt.failed_as(unlisted))?;
    if !files.is_empty() {
        carried.sent.push((path.to_owned(), files.len()));
        carried.found.extend(files);
    }
    Ok(())
}

/// What git wrote, as text; else that it is not UTF-8.
fn utf8(output: Vec<u8>) -> Result<String, Unbuilt> {
    String::from_utf8(output).map_err(|_| Unbuilt::Failed("git's output is not UTF-8".to_owned()))
}

/// Each path the `.gitmodules` at `asked`'s root gives a submodule, once each, sorted, read by one `git config` call
/// bounded as the run's git calls are; none when git finds no such key (status 1); else the interrupt, or why git
/// failed.
fn submodule_paths(git: &Git, asked: &Asked<'_>) -> Result<BTreeSet<String>, Unbuilt> {
    let args = ["config", "-z", "-f", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"];
    let call = Call {
        interrupt: asked.interrupt,
        ..Call::bounded(asked.root, &args, asked.bounds)
    };
    let (status, output, errors) = git.status_within(&call, OUTPUT_CAP).map_err(super::lfs::unbuilt)?;
    match status.code() {
        Some(0) => {}
        Some(1) => return Ok(BTreeSet::new()),
        code => return Err(Unbuilt::Failed(git_failure(code, &errors))),
    }
    // `-z` writes each entry as its key, a line feed and its value, ended by a NUL; a key with no value has no line feed.
    Ok(utf8(output)?
        .split('\0')
        .filter_map(|entry| entry.split_once('\n'))
        .map(|(_, value)| value.to_owned())
        .collect())
}

/// The mode git's index gives a submodule: a gitlink.
const GITLINK: &str = "160000";

/// Those of `paths` the index at `asked`'s root holds as a gitlink at that very spelling, read by one bounded
/// `git ls-files --stage` call naming them as literal paths; none, and no git process started, when `paths` is empty.
/// Else the interrupt, or why git failed.
fn gitlinks(git: &Git, asked: &Asked<'_>, paths: &[String]) -> Result<HashSet<String>, Unbuilt> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut args = vec!["--literal-pathspecs", "-c", "core.quotepath=off", "ls-files", "-z", "--stage", "--"];
    args.extend(paths.iter().map(String::as_str));
    let call = Call {
        interrupt: asked.interrupt,
        ..Call::bounded(asked.root, &args, asked.bounds)
    };
    let listed = utf8(git.output_bytes(&call).map_err(super::lfs::unbuilt)?)?;
    // Each entry is `<mode> <object> <stage>\t<path>`, ended by a NUL.
    Ok(listed
        .split('\0')
        .filter_map(|entry| entry.split_once('\t'))
        .filter(|(staged, _)| staged.split(' ').next() == Some(GITLINK))
        .map(|(_, path)| path.to_owned())
        .collect())
}

/// Where a submodule's `.gitmodules` path leads in the worktree.
#[derive(Debug, PartialEq, Eq)]
enum Placed {
    /// No folder there, or one holding no `.git` entry: an uninitialized submodule.
    Absent,
    /// A path through a link, a junction or a file.
    Refused,
    /// A real folder holding a `.git` entry: an initialized submodule, its full path.
    Ready(PathBuf),
}

/// Whether `metadata` is of a link or a junction, or of anything else Windows reparses.
fn reparsed(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// The metadata of `path`, read without following a link; `None` when nothing is there. Else why it cannot be read.
fn entry_at(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(in_context(path, &error).to_string()),
    }
}

/// Where the submodule at the `.gitmodules` path `path` below `root` leads ([`Placed`]), each folder on the way read
/// without following a link or a junction; else why a folder on the way, or its `.git` entry, cannot be read.
fn submodule_folder(root: &Path, path: &str) -> Result<Placed, String> {
    let mut folder = root.to_path_buf();
    for part in path.split('/') {
        folder.push(part);
        let Some(metadata) = entry_at(&folder)? else {
            return Ok(Placed::Absent);
        };
        if reparsed(&metadata) || !metadata.is_dir() {
            return Ok(Placed::Refused);
        }
    }
    Ok(match entry_at(&folder.join(".git"))? {
        Some(_) => Placed::Ready(folder),
        None => Placed::Absent,
    })
}

/// Whether the `.gitmodules` path `path` is one the gate carries, by the rules the box reads a manifest's paths with:
/// plain ([`is_plain_path`]: relative, within the depth limit, each part a name Windows keeps as it is) and with no
/// `.git*` part, 8.3 aliases included ([`is_git_s`]).
fn carried_form(path: &str) -> bool {
    is_plain_path(path) && !is_git_s(path)
}

/// The files of the initialized submodule at `folder`, its `.gitmodules` path `path`, that git lists there, tracked and
/// untracked but not ignored, each looked at in turn ([`looked_at`]), `look`'s `go_on` asked before each, its path
/// below the worktree root spelled as git lists it; each nested submodule's line said into `said`. [`Unbuilt::Interrupted`]
/// once `go_on` says no or git was interrupted; else why git failed, or a file or a folder cannot be read.
fn submodule_files(look: &Look<'_>, folder: &Path, path: &str, said: &mut Vec<String>) -> Result<Vec<Found>, Unbuilt> {
    let args = ["-c", "core.quotepath=off", "ls-files", "-z", "--cached", "--others", "--exclude-standard"];
    let call = Call {
        interrupt: look.asked.interrupt,
        ..Call::bounded(folder, &args, look.asked.bounds)
    };
    let listed = utf8(look.git.output_bytes(&call).map_err(super::lfs::unbuilt)?)?;
    // A file in conflict is listed once per stage.
    let listed: BTreeSet<&str> = listed.split('\0').filter(|file| !file.is_empty()).collect();
    let mut plain = HashMap::new();
    let mut found = Vec::new();
    for file in listed {
        if !(look.go_on)() {
            return Err(Unbuilt::Interrupted);
        }
        let below = format!("{path}/{file}");
        if !sendable(&below, look.excluded) {
            continue;
        }
        match looked_at(folder, below, file, &mut plain).map_err(Unbuilt::Failed)? {
            Looked::File(file) => found.push(file),
            Looked::Nested(below) => said.push(format!("gate: inputs: submodule {} is inside a submodule; not carried", escaped(&below))),
            Looked::Passed => {}
        }
    }
    Ok(found)
}

/// What an entry git lists in a submodule is ([`looked_at`]).
enum Looked {
    /// A regular file, as an input.
    File(Found),
    /// A folder holding a `.git` entry, a submodule of the submodule, at its path below the worktree.
    Nested(String),
    /// Gone, below a folder that is not a plain one, a link, or anything else: passed over.
    Passed,
}

/// What the entry git lists at `file` in the submodule at `folder`, at `below` the worktree, is ([`Looked`]), each
/// folder on its way judged once, kept in `plain`, and every entry read without following a link. Else why it, its
/// `.git` entry or a folder on its way cannot be read.
fn looked_at(folder: &Path, below: String, file: &str, plain: &mut HashMap<String, bool>) -> Result<Looked, String> {
    if !plain_folders(folder, file, plain)? {
        return Ok(Looked::Passed);
    }
    let full = file.split('/').fold(folder.to_path_buf(), |full, part| full.join(part));
    let Some(metadata) = entry_at(&full)?.filter(|metadata| !reparsed(metadata)) else {
        return Ok(Looked::Passed);
    };
    if metadata.is_file() {
        return Ok(Looked::File(Found {
            path: below,
            full,
            stamp: stamp_of(&metadata),
        }));
    }
    let nested = metadata.is_dir() && entry_at(&full.join(".git"))?.is_some();
    Ok(if nested { Looked::Nested(below) } else { Looked::Passed })
}

/// Whether the path `below` the worktree is one the box takes in a manifest and the inputs' exclusions `excluded` do not
/// match.
fn sendable(below: &str, excluded: &regex::Regex) -> bool {
    refused_path(below).is_none() && !excluded.is_match(below)
}

/// Whether each folder from the submodule at `folder` down to the parent of its file `file` is a plain folder, not a
/// link, a junction or gone; each folder judged once, its answer kept in `plain` by its path below `folder`. Else why a
/// folder on the way cannot be read.
fn plain_folders(folder: &Path, file: &str, plain: &mut HashMap<String, bool>) -> Result<bool, String> {
    for (end, _) in file.match_indices('/') {
        let below = file.get(..end).unwrap_or(file);
        let is_plain = if let Some(known) = plain.get(below) {
            *known
        } else {
            let full = below.split('/').fold(folder.to_path_buf(), |full, part| full.join(part));
            let judged = entry_at(&full)?.is_some_and(|metadata| metadata.is_dir() && !reparsed(&metadata));
            plain.insert(below.to_owned(), judged);
            judged
        };
        if !is_plain {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Why an inputs manifest past its cap is not sent.
const PAST_THE_CAP: &str = "the inputs manifest is larger than the box takes (64 MiB)";

/// The line saying `members` files match, more than the `cap` one put carries.
fn past_members(members: usize, cap: usize) -> String {
    format!("{members} files match the input globs, more than one put carries ({cap})")
}

/// One tree of a sibling pair's run as its inputs are built: the folder name its paths are prefixed with, its repo's
/// name, which its lines are named by, its root on the PC, its repo's input globs, the work folder key its own gate
/// files under `.tmp` are named for, and its own inputs hash cache.
pub(super) struct PairTree<'a> {
    pub(super) prefix: &'a str,
    pub(super) repo: &'a str,
    pub(super) root: &'a Path,
    pub(super) globs: &'a [String],
    pub(super) key: String,
    pub(super) cache: PathBuf,
}

/// What a sibling pair's inputs manifest is built from besides its trees: the pair's layout text, the run's log, the
/// caps the whole manifest is held to, and the bounds of each tree's `git check-ignore` call, answering to `interrupt`.
pub(super) struct PairAsked<'a> {
    pub(super) layout: String,
    pub(super) log: &'a Path,
    pub(super) caps: Caps,
    pub(super) bounds: GitBounds,
    pub(super) interrupt: Option<Interrupt>,
}

impl PairTree<'_> {
    /// What this tree's files are chosen and hashed under, from the pair's `asked`.
    fn asked<'t>(&'t self, asked: &'t PairAsked<'_>) -> Asked<'t> {
        Asked {
            root: self.root,
            globs: self.globs,
            key: &self.key,
            log: asked.log,
            cache: &self.cache,
            caps: asked.caps,
            bounds: asked.bounds,
            interrupt: asked.interrupt,
        }
    }

    /// The failure `why` of this tree, naming its repo.
    fn failed(&self, why: &str) -> Unbuilt {
        Unbuilt::Failed(format!("{}: {why}", escaped(self.repo)))
    }
}

/// The inputs manifest of a sibling pair's `trees`: each tree's globbed files chosen on its own ([`picked`]), from its
/// own root, with its own exclusions, nested-repo skip and `git check-ignore`, beside the files of each initialized
/// submodule of that tree ([`submodules`]); a tree with no globs whose submodules carry no file is not chosen, its lines
/// still said. The pair is refused as soon as the trees chosen so far pass the member cap together and before any file
/// is hashed; then each tree is hashed within its own read boundary and hash cache, with its submodules' files
/// ([`with_submodules`]), each path prefixed with its tree's folder name, into one manifest naming the pair's layout,
/// held to the caps as a whole, its lines naming the tree's repo. A tree past a cap on its own is refused naming its
/// repo; only the trees together past one add [`PAIR_CAPS`]. A tree's paths a command named (its `[carried]` entries)
/// are chosen as any single tree's are, and the pair's marking stays [`Marking::Leave`]: the pair inputs marker, not
/// the carried one, sweeps them ([`pair_marked`]). [`Unbuilt::Interrupted`] once `go_on` says no; else why it is not
/// sent.
pub(super) fn built_pair(trees: &[PairTree<'_>], asked: &PairAsked<'_>, go_on: impl Fn() -> bool) -> Result<Built, Unbuilt> {
    let cap = asked.caps.members;
    let mut looked = Vec::new();
    let mut members = 0_usize;
    let mut carries = false;
    for tree in trees {
        let (chosen, picked, carried) = pair_tree_looked(tree, asked, &go_on)?;
        if chosen {
            members = members.saturating_add(picked.kept.len()).saturating_add(carried.found.len());
            carries |= !carried.found.is_empty();
            if members > cap {
                return Err(pair_past_members(members, cap, carries));
            }
        }
        looked.push((tree, chosen, picked, carried));
    }
    let mut manifest = Manifest::default();
    let mut found = Vec::new();
    let mut said = Vec::new();
    for (tree, chosen, mut picked, carried) in looked {
        if !chosen {
            said.extend(carried.said.iter().map(|line| repo_line(tree.repo, line)));
            continue;
        }
        let (held, files) = tree_merged(tree, asked, &mut picked, &carried, &go_on)?;
        said.extend(picked.said.iter().map(|line| repo_line(tree.repo, line)));
        for (path, held) in held.files() {
            manifest.insert(format!("{}/{path}", tree.prefix), held.clone());
        }
        found.extend(files.into_iter().map(|file| Found {
            path: format!("{}/{}", tree.prefix, file.path),
            ..file
        }));
    }
    let manifest = manifest.with_layout(asked.layout.clone());
    let line = manifest.line();
    if line.len() > asked.caps.manifest {
        return Err(Unbuilt::Failed(format!("{PAST_THE_CAP}{PAIR_CAPS}")));
    }
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    Ok(Built {
        line,
        digest,
        manifest,
        found,
        said,
        marking: Marking::Leave,
        trees: pair_trees(trees),
    })
}

/// One tree of a pair as [`built_pair`] looks at it before any file is hashed: whether it is chosen, its globbed files
/// ([`picked`], none without globs) and its submodules' files ([`submodules`]), git found once for both. A tree with no
/// globs whose submodules carry no file is not chosen. Refused naming its repo when its globbed files alone, or with its
/// submodules' files, pass the member cap. [`Unbuilt::Interrupted`] once `go_on` says no; else why it cannot be built.
fn pair_tree_looked(tree: &PairTree<'_>, asked: &PairAsked<'_>, go_on: &impl Fn() -> bool) -> Result<(bool, Picked, Carried), Unbuilt> {
    let cap = asked.caps.members;
    let tree_asked = tree.asked(asked);
    let finder = Finder::new(tree.root);
    let failed = |unbuilt: Unbuilt| unbuilt.failed_as(|why| tree.failed(&why));
    let picked = if tree.globs.is_empty() {
        Picked::none()
    } else {
        picked(&tree_asked, &finder).map_err(failed)?
    };
    if picked.kept.len() > cap {
        return Err(tree.failed(&past_members(picked.kept.len(), cap)));
    }
    let carried = submodules(&tree_asked, &finder, go_on).map_err(failed)?;
    let chosen = !tree.globs.is_empty() || !carried.sent.is_empty();
    let count = picked.kept.len().saturating_add(carried.found.len());
    if chosen && count > cap {
        return Err(tree.failed(&past_members_with_submodules(count, cap)));
    }
    Ok((chosen, picked, carried))
}

/// Why a pair whose chosen trees hold `members` files together, past the member cap `cap`, is not sent, naming the
/// submodules' files when `carries` says any tree carried some, and [`PAIR_CAPS`].
fn pair_past_members(members: usize, cap: usize, carries: bool) -> Unbuilt {
    let why = if carries {
        past_members_with_submodules(members, cap)
    } else {
        past_members(members, cap)
    };
    Unbuilt::Failed(format!("{why}{PAIR_CAPS}"))
}

/// The manifest of the chosen pair tree `tree` and its files, paths below the tree: its globbed files `picked` chose
/// with its `carried` submodules' files ([`with_submodules`]), else the globbed files alone ([`hashed`]); the
/// submodules' lines said into `picked`'s. A tree's own manifest past the cap is that tree's, refused naming its repo:
/// only [`built_pair`]'s combined check names the pair. [`Unbuilt::Interrupted`] once `go_on` says no.
fn tree_merged(
    tree: &PairTree<'_>,
    asked: &PairAsked<'_>,
    picked: &mut Picked,
    carried: &Carried,
    go_on: &impl Fn() -> bool,
) -> Result<(Manifest, Vec<Found>), Unbuilt> {
    let tree_asked = tree.asked(asked);
    let failed = |unbuilt: Unbuilt| unbuilt.failed_as(|why| tree.failed(&why));
    if let Some((manifest, _, files)) = with_submodules(&tree_asked, picked, carried, go_on).map_err(failed)? {
        return Ok((manifest, files));
    }
    let held = hashed(&tree_asked, &picked.kept, go_on).map_err(|unmade| failed(unmade.unbuilt()))?;
    Ok((held, std::mem::take(&mut picked.kept)))
}

/// The trees of a pair's built manifest, for its pack and its hash caches.
fn pair_trees(trees: &[PairTree<'_>]) -> Vec<Tree> {
    trees
        .iter()
        .map(|tree| Tree {
            prefix: tree.prefix.to_owned(),
            root: tree.root.to_owned(),
            cache: tree.cache.clone(),
        })
        .collect()
}

/// The inputs log line `line` of a pair's tree, naming its repo `repo` as the LFS lines do: `gate: inputs: {repo}: …`.
fn repo_line(repo: &str, line: &str) -> String {
    match line.strip_prefix("gate: inputs: ") {
        Some(rest) => format!("gate: inputs: {}: {rest}", escaped(repo)),
        None => format!("{}: {line}", escaped(repo)),
    }
}

/// The manifest of `files`, below `asked`'s root, each hashed as [`sha256_for`] hashes an output, the inputs hash cache
/// read before and saved after when it changed; else the interrupt, why a file failed, or the line past its cap
/// ([`Unmade::Capped`]).
fn hashed(asked: &Asked<'_>, files: &[Found], go_on: impl Fn() -> bool) -> Result<Manifest, Unmade> {
    let failed = |why: String| Unmade::Unbuilt(Unbuilt::Failed(why));
    let previous = HashCache::load(asked.cache);
    let none = HashCache::default();
    let cache = previous.as_ref().unwrap_or(&none);
    let inside = fs::canonicalize(asked.root).map_err(|error| failed(in_context(asked.root, &error).to_string()))?;
    let started = file_time_now();
    let stopped = Cell::new(false);
    let mut next = HashCache::default();
    let built = build_manifest_with(files, asked.caps.manifest, |file| {
        let (sha256, stamp) = sha256_for(file, &inside, cache, &go_on, &stopped)?;
        next.keep(&file.path, stamp, &sha256, started);
        Ok((sha256, stamp))
    });
    if stopped.get() {
        return Err(Unmade::Unbuilt(Unbuilt::Interrupted));
    }
    let manifest = match built {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => return Err(Unmade::Capped(PAST_THE_CAP.to_owned())),
        Err(error) => return Err(failed(error.to_string())),
    };
    if previous.as_ref() != Some(&next) {
        next.save(asked.cache);
    }
    Ok(manifest)
}

/// The inputs' exclusions for a run of the work folder `key` logging to `log` in the worktree `root`: [`EXCLUDED`],
/// every `.tmp/<key>-…` entry at the root (the outputs' archives and stages, and the inputs' own archive), and the log
/// and its `.err` file when the log is inside the worktree.
fn exclusions(root: &Path, key: &str, log: &Path) -> String {
    let mut own = vec![format!(r"\.tmp/{}-", regex::escape(key))];
    if let Some(log) = relative(root, log) {
        own.push(format!("{}$", regex::escape(&log)));
        own.push(format!(r"{}\.err$", regex::escape(&log)));
    }
    format!("(?i){EXCLUDED}|^(?:{})", own.join("|"))
}

/// The file `path` as a `/`-separated path relative to `root`, compared without regard to case, both as the system
/// resolves them: the root's canonical path, and the file's folder's canonical path with its name. `None` when it is
/// not below the root, or when either cannot be resolved: a file whose folder cannot be resolved is none a walk finds.
pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
    let spelled = |path: &Path| path.to_string_lossy().replace('\\', "/");
    let path = std::path::absolute(path).ok()?;
    let folder = fs::canonicalize(path.parent()?).ok()?;
    let (root, path) = (spelled(&fs::canonicalize(root).ok()?), spelled(&folder.join(path.file_name()?)));
    let root = root.trim_end_matches('/');
    let head = path.get(..root.len())?;
    let rest = path.get(root.len()..)?.strip_prefix('/')?;
    (head.eq_ignore_ascii_case(root) && !rest.is_empty()).then(|| rest.to_owned())
}

/// The first exact path of `asked` that a command carries ([`CARRIED_INPUT`]) that the walk's `found` does not hold,
/// compared without regard to ASCII letter case: one gone since the decision, or one the exclusions drop. A carried
/// folder (`<folder>/**`, the only carried entry holding a `*`) is never one: it may hold no file at all.
fn unsent_carried<'a>(asked: &'a [String], found: &[Found]) -> Option<&'a str> {
    asked
        .iter()
        .filter_map(|glob| glob.strip_prefix(CARRIED_INPUT))
        .filter(|path| !path.contains('*'))
        .find(|path| !found.iter().any(|file| file.path.eq_ignore_ascii_case(path)))
}

/// The line of each of the repo's input globs among `asked` that matches none of `found`, matched by `globs`, built
/// from them; an exact path a command carries ([`CARRIED_INPUT`]) is never one.
fn unmatched(asked: &[String], globs: &Globs, found: &[Found]) -> Vec<String> {
    asked
        .iter()
        .enumerate()
        .filter(|(_, glob)| !glob.starts_with(CARRIED_INPUT))
        .filter(|(index, _)| !found.iter().any(|file| globs.matched_by(*index, &file.path)))
        .map(|(_, glob)| format!("gate: inputs: {} matched nothing", escaped(glob)))
        .collect()
}

/// `found` without each file inside a folder below `root` that holds a `.git` of its own, a submodule or a nested
/// repository, which git cannot be asked about; and those folders, each the outermost such one, in order, with the paths
/// of the files dropped inside it.
fn outside_nested(root: &Path, found: Vec<Found>) -> (Vec<Found>, BTreeMap<String, Vec<String>>) {
    let mut known = HashMap::new();
    let mut nested: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut kept = Vec::new();
    for file in found {
        match nested_folder(root, &file.path, &mut known) {
            Some(folder) => nested.entry(folder).or_default().push(file.path),
            None => kept.push(file),
        }
    }
    (kept, nested)
}

/// The outermost folder of the `/`-separated `path` below `root` that holds a `.git` entry, each folder looked at once
/// and its answer kept in `known`. A `.git` whose entry cannot be read counts as none: git is asked about the path, and
/// says why it cannot answer.
pub(crate) fn nested_folder(root: &Path, path: &str, known: &mut HashMap<String, bool>) -> Option<String> {
    for (end, _) in path.match_indices('/') {
        let folder = path.get(..end)?;
        let holds = *known
            .entry(folder.to_owned())
            .or_insert_with(|| fs::symlink_metadata(root.join(folder).join(".git")).is_ok());
        if holds {
            return Some(folder.to_owned());
        }
    }
    None
}

/// The paths of `found` that `git check-ignore` reports ignored in `asked`'s worktree, the global excludes and
/// `.git/info/exclude` included and a tracked file never; else the interrupt, or why git failed, its status and the
/// last lines it wrote on standard error.
fn ignored(asked: &Asked<'_>, finder: &Finder<'_>, found: &[Found]) -> Result<HashSet<String>, Unbuilt> {
    if found.is_empty() {
        return Ok(HashSet::new());
    }
    let git = finder.git().map_err(Unbuilt::Failed)?;
    let input: Vec<u8> = found.iter().flat_map(|file| file.path.bytes().chain([0])).collect();
    let args = ["check-ignore", "-z", "--stdin"];
    let call = Call {
        stdin: Some(&input),
        interrupt: asked.interrupt,
        ..Call::bounded(asked.root, &args, asked.bounds)
    };
    let failed = |why: String| Unbuilt::Failed(format!("git check-ignore failed: {why}"));
    let (ran, errors) = git.run_kept(&call).map_err(|error| match error {
        GitError::Interrupted => Unbuilt::Interrupted,
        GitError::Failed(why) | GitError::OverCap(why) => failed(why),
    })?;
    // 1 says no path is ignored; anything but 0 and 1 is git's own failure.
    if !matches!(ran.status.code(), Some(0 | 1)) {
        return Err(failed(git_failure(ran.status.code(), &errors)));
    }
    let mut listed = Vec::new();
    let most = u64::try_from(input.len()).unwrap_or(u64::MAX).saturating_add(1);
    ran.stdout
        .take(most)
        .read_to_end(&mut listed)
        .map_err(|error| failed(error.to_string()))?;
    let listed = String::from_utf8(listed).map_err(|_| failed("its output is not UTF-8".to_owned()))?;
    Ok(listed.split('\0').filter(|path| !path.is_empty()).map(str::to_owned).collect())
}

/// The most bytes one put unpacks on the box, decompressed: 4 GiB, unless `--test-remote-inputs-budget-bytes` says
/// otherwise.
pub(super) const INPUTS_BUDGET: u64 = 4 * 1024 * 1024 * 1024;
/// A megabyte, as the inputs line says sizes.
pub(super) const MB: f64 = 1024.0 * 1024.0;
/// Why a run the box refused `inputs` falls back.
const NOT_HELD: &str = "the builder's work folder does not hold the inputs sent";
/// The box's diff and put lines.
const NEED: &str = "gate-remote-inputs: need ";
const PATH: &str = "gate-remote-inputs: path ";
const INSTALLED: &str = "gate-remote-inputs: installed ";

/// The line saying a run's inputs were not sent, for `why`.
pub(super) fn not_sent(why: &str) -> String {
    format!("gate: remote inputs not sent: {}", escaped(why))
}

/// Logs why the inputs were not sent, kept for the log of the run here; the route's `inputs` refusal.
fn refused(attempt: &mut Attempt<'_, '_>, why: &str) -> End {
    attempt.logged_kept(not_sent(why));
    End::Refused(FallbackReason::Inputs)
}

/// The end of a run call the box refused `inputs`: its work folder no longer holds the inputs sent, said in the log.
pub(super) fn run_refused(attempt: &mut Attempt<'_, '_>) -> End {
    refused(attempt, NOT_HELD)
}

/// The inputs manifest of `attempt`'s run, its lines logged: its globs' files and its initialized submodules' files
/// ([`single`]). A run with no input globs and no submodule file carried sends the empty manifest when an earlier run's
/// carried copies are to be removed ([`emptied`]), else none. A sibling pair's run sends each repo's inputs and its
/// initialized submodules' files in its own tree ([`pair_sent`]), on a Windows builder only ([`pair_inputs`]): a pair
/// any of whose trees has globs or a `.gitmodules` ([`pair_declared`]) refuses another builder. A manifest that cannot
/// be built or is past a cap refuses the route `inputs` before anything is sent.
pub(super) fn sent(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if !call.siblings.is_empty() {
        let trees = std::iter::once((Path::new(call.root), call.inputs))
            .chain(call.siblings.iter().map(|sibling| (Path::new(sibling.root()), sibling.inputs())));
        return match pair_inputs(call.builder.os, pair_declared(trees)) {
            PairInputs::Sent => pair_sent(attempt),
            PairInputs::None => Ok(None),
            PairInputs::Refused(why) => Err(refused(attempt, why)),
        };
    }
    let cache = v1::inputs_hashes_path(call.state_dir, call.builder.id.as_deref(), call.key);
    let asked = Asked {
        root: Path::new(call.root),
        globs: call.inputs,
        key: call.key,
        log: call.log,
        cache: &cache,
        caps: Caps::FULL,
        bounds: attempt.git,
        interrupt: call.interrupt,
    };
    match single(&asked, || attempt.go_on().is_ok()) {
        Ok(Single::Built(built)) => logged_built(attempt, Ok(built)),
        Ok(Single::None(said)) => {
            for line in &said {
                attempt.logged(line);
            }
            emptied(attempt)
        }
        Err(unbuilt) => logged_built(attempt, Err(unbuilt)),
    }
}

/// The manifest `built` as a step sends it, its lines logged; the interrupt ends the run, and any other failure refuses
/// the route `inputs`, its reason in the log.
fn logged_built(attempt: &mut Attempt<'_, '_>, built: Result<Built, Unbuilt>) -> Result<Option<Built>, End> {
    match built {
        Ok(built) => {
            for line in &built.said {
                attempt.logged(line);
            }
            Ok(Some(built))
        }
        Err(Unbuilt::Interrupted) => Err(End::Interrupted),
        Err(Unbuilt::Failed(why)) => Err(refused(attempt, &why)),
    }
}

/// An empty inputs manifest, `manifest`, that removes copies the box holds, as `marking` says, on the `trees` of a
/// sibling pair or none: its line and digest, naming no file and saying nothing; else why its digest cannot be made.
fn empty_built(manifest: Manifest, marking: Marking, trees: Vec<Tree>) -> Result<Built, Unbuilt> {
    let line = manifest.line();
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    Ok(Built {
        line,
        digest,
        manifest,
        found: Vec::new(),
        said: Vec::new(),
        marking,
        trees,
    })
}

/// Why a sibling pair's run on a macOS builder sends no inputs: its box refuses a pair's inputs request.
const MACOS_PAIR: &str = "the macOS builder does not take a sibling pair's inputs yet";

/// What a sibling pair's run does with its inputs.
#[derive(Debug, PartialEq, Eq)]
enum PairInputs {
    /// Builds and sends them ([`pair_sent`]).
    Sent,
    /// Makes no inputs call: no sweep, its hash caches untouched.
    None,
    /// Refuses the route `inputs` before any inputs call, for the reason it holds.
    Refused(&'static str),
}

/// Whether a sibling pair's `trees`, each its root and its repo's input globs, may send inputs: any tree has globs or
/// holds a regular `.gitmodules` ([`gitmodules_at`]), one that cannot be read counting as held, so a builder that does
/// not take a pair's inputs refuses rather than run without a submodule's files.
fn pair_declared<'a>(trees: impl IntoIterator<Item = (&'a Path, &'a [String])>) -> bool {
    trees
        .into_iter()
        .any(|(root, globs)| !globs.is_empty() || !matches!(gitmodules_at(root), Ok(false)))
}

/// What a sibling pair's run on a builder of `os` does with its inputs, `declared` when any of its trees declares
/// some: a Windows builder takes them ([`PairInputs::Sent`]); any other, whose box refuses a pair's inputs request, makes
/// no inputs call when none is declared, and refuses ([`MACOS_PAIR`]) when one is, decided before any tree is hashed.
fn pair_inputs(os: BuilderOs, declared: bool) -> PairInputs {
    if os == BuilderOs::Windows {
        PairInputs::Sent
    } else if declared {
        PairInputs::Refused(MACOS_PAIR)
    } else {
        PairInputs::None
    }
}

/// The inputs manifest of a sibling pair's run ([`built_pair`]): the main repo's tree, its paths prefixed with the leaf
/// and cached in the work folder's own inputs hash cache, then each sibling's, prefixed with its folder name, its globs
/// its own repo's, its gate files named for its own work folder key and its hashes in a cache of its own; each tree
/// with its initialized submodules' files. Its lines are logged; one that cannot be built refuses the route `inputs`.
///
/// With no globs in any tree and no submodule file carried in any, its lines logged, it is none, unless the pair
/// inputs marker ([`v1::pair_inputs_path`]) shows an earlier pair run sent inputs: then the pair's empty manifest is
/// sent, so the box sweeps the copies it holds, and once it landed the marker and every inputs hash cache of the work
/// folder are removed ([`Marking::Swept`]) ([`pair_emptied`]). A tree's `.gitmodules` that cannot be read counts as
/// one that may carry files, so the build says why.
fn pair_sent(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    let id = call.builder.id.as_deref();
    let mut trees = vec![PairTree {
        prefix: call.leaf,
        repo: call.repo,
        root: Path::new(call.root),
        globs: call.inputs,
        key: call.key.to_owned(),
        cache: v1::inputs_hashes_path(call.state_dir, id, call.key),
    }];
    trees.extend(call.siblings.iter().map(|sibling| PairTree {
        prefix: sibling.name(),
        repo: sibling.repo(),
        root: Path::new(sibling.root()),
        globs: sibling.inputs(),
        key: work_folder_key(sibling.root()),
        cache: v1::sibling_inputs_hashes_path(call.state_dir, id, call.key, sibling.name()),
    }));
    let layout = layout_text(Some(call.leaf), call.siblings.iter().map(super::carried::Sibling::name).collect());
    let globless = trees.iter().all(|tree| tree.globs.is_empty());
    let asked = PairAsked {
        layout: layout.clone(),
        log: call.log,
        caps: Caps::FULL,
        bounds: attempt.git,
        interrupt: call.interrupt,
    };
    let built = built_pair(&trees, &asked, || attempt.go_on().is_ok());
    match built {
        Ok(built) if globless && built.found.is_empty() => {
            for line in &built.said {
                attempt.logged(line);
            }
            pair_emptied(attempt, layout, &trees)
        }
        built => logged_built(attempt, built),
    }
}

/// What a sibling pair's run with no inputs to send, its layout `layout`, sends for its `trees`: none, unless the pair
/// inputs marker shows an earlier pair run sent inputs; then the pair's empty manifest, which sweeps them.
fn pair_emptied(attempt: &mut Attempt<'_, '_>, layout: String, trees: &[PairTree<'_>]) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if !v1::pair_inputs_path(call.state_dir, call.builder.id.as_deref(), call.key).exists() {
        return Ok(None);
    }
    let swept = empty_built(Manifest::default().with_layout(layout), Marking::Swept, pair_trees(trees));
    logged_built(attempt, swept)
}

/// The empty inputs manifest of a run with no inputs whose work folder holds copies a command's carried paths put, its
/// carried marker there ([`v1::inputs_carried_path`]): the box's diff, needing nothing, deletes those copies, and
/// [`send`] removes the marker once it landed. `None` when there is no marker. A digest that cannot be made refuses the
/// route `inputs`.
fn emptied(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if !v1::inputs_carried_path(call.state_dir, call.builder.id.as_deref(), call.key).exists() {
        return Ok(None);
    }
    let cleared = empty_built(Manifest::default(), Marking::Clear, Vec::new());
    logged_built(attempt, cleared)
}

/// The log line of a run whose empty manifest removed the copies an earlier run carried.
const REMOVED: &str = "gate: inputs: removed the carried copies of an earlier run";
/// The log line of a sibling pair's run whose empty manifest removed the copies earlier runs' inputs put.
const SWEPT: &str = "gate: inputs: removed the old copies of every tree";

/// Does what the landed inputs step's `marking` says with `attempt`'s work folder's markers: a step that carried a path
/// a command named writes the carried marker ([`marker_written`]); a sibling pair's manifest naming no file, whose diff
/// removed every copy an earlier pair run put, removes the pair inputs marker, which a pair manifest naming a file wrote
/// before it was sent ([`pair_marked`]); a pair's empty manifest sweeps ([`swept_landed`]); the empty manifest that
/// removed carried copies clears ([`cleared`]). A marker that cannot be written, or a file that cannot be removed, is
/// said in the log: a later run then sends the empty manifest once fewer, or once more.
fn landed(attempt: &mut Attempt<'_, '_>, built: &Built) {
    let call = attempt.call;
    let id = call.builder.id.as_deref();
    match built.marking {
        Marking::Swept => swept_landed(attempt),
        Marking::Leave if !built.trees.is_empty() && built.manifest.files().next().is_none() => {
            removed(attempt, &v1::pair_inputs_path(call.state_dir, id, call.key));
        }
        Marking::Leave => {}
        Marking::Write => marker_written(attempt, &v1::inputs_carried_path(call.state_dir, id, call.key), "carried marker"),
        Marking::Clear => cleared(attempt),
    }
}

/// Writes the empty marker at `marker`, its folder made; else why not.
fn marker_made(marker: &Path) -> io::Result<()> {
    marker.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(marker, ""))
}

/// Writes the empty marker at `marker` ([`marker_made`]); one that cannot be written is said in the log, named `what`.
fn marker_written(attempt: &mut Attempt<'_, '_>, marker: &Path, what: &str) {
    if let Err(error) = marker_made(marker) {
        attempt.logged(&format!("gate: inputs: cannot write the {what} {}: {error}", marker.display()));
    }
}

/// Before a sibling pair's manifest `built` that names a file is sent: writes the pair inputs marker
/// ([`v1::pair_inputs_path`]), so the copies its put installs are swept by a later pair run with no inputs even when
/// this run never sees the put land. Nothing for any other manifest. Else why the marker cannot be written, which
/// refuses the step, so no copy lands unmarked.
fn pair_marked(attempt: &Attempt<'_, '_>, built: &Built) -> Result<(), String> {
    if built.marking != Marking::Leave || built.trees.is_empty() || built.manifest.files().next().is_none() {
        return Ok(());
    }
    let call = attempt.call;
    let marker = v1::pair_inputs_path(call.state_dir, call.builder.id.as_deref(), call.key);
    marker_made(&marker).map_err(|error| format!("cannot write the pair inputs marker {}: {error}", marker.display()))
}

/// After a sibling pair's empty manifest landed: removes the pair inputs marker and every inputs hash cache of the work
/// folder ([`key_caches`]), a sibling's no longer in the pair among them, so the next pair run with no inputs sends
/// none, and says [`SWEPT`].
fn swept_landed(attempt: &mut Attempt<'_, '_>) {
    let call = attempt.call;
    let mut paths = key_caches(attempt);
    paths.push(v1::pair_inputs_path(call.state_dir, call.builder.id.as_deref(), call.key));
    for path in &paths {
        removed(attempt, path);
    }
    attempt.logged(SWEPT);
}

/// After the empty manifest that removed an earlier run's carried copies landed: removes the carried marker, the pair
/// inputs marker and every inputs hash cache of the work folder ([`key_caches`]), so the next run with no inputs sends
/// none, and says [`REMOVED`].
fn cleared(attempt: &mut Attempt<'_, '_>) {
    let call = attempt.call;
    let id = call.builder.id.as_deref();
    let mut paths = key_caches(attempt);
    paths.push(v1::inputs_carried_path(call.state_dir, id, call.key));
    paths.push(v1::pair_inputs_path(call.state_dir, id, call.key));
    for path in &paths {
        removed(attempt, path);
    }
    attempt.logged(REMOVED);
}

/// Every inputs hash cache of `attempt`'s work folder: its own and each sibling's its cache folder holds, a sibling no
/// longer in the pair included ([`v1::sibling_inputs_hashes_paths`]). A cache folder that cannot be listed is said in
/// the log, and only the work folder's own cache is named.
fn key_caches(attempt: &mut Attempt<'_, '_>) -> Vec<PathBuf> {
    let call = attempt.call;
    let id = call.builder.id.as_deref();
    let mut paths = vec![v1::inputs_hashes_path(call.state_dir, id, call.key)];
    match v1::sibling_inputs_hashes_paths(call.state_dir, id, call.key) {
        Ok(siblings) => paths.extend(siblings),
        Err(error) => {
            let folder = v1::builder_cache_dir(call.state_dir, id);
            attempt.logged(&format!("gate: inputs: cannot list {}: {error}", folder.display()));
        }
    }
    paths
}

/// Removes the file at `path`; one that is there but cannot be removed is said in the log.
fn removed(attempt: &mut Attempt<'_, '_>, path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            attempt.logged(&format!("gate: inputs: cannot remove {}: {error}", path.display()));
        }
        _ => {}
    }
}

/// Why the inputs call ended `unsent`, as the log says it.
fn unsent_why(unsent: &Unsent) -> String {
    match unsent {
        Unsent::Ended(End::Failed(why)) | Unsent::Refused(why) => why.clone(),
        Unsent::Ended(End::Refused(reason)) => format!("the inputs call ended {}", reason.as_str()),
        Unsent::Ended(other) => format!("the inputs call ended {}", unexpected(other)),
    }
}

/// Sends `built` to the work folder at `location` for the run of `nonce`: the diff call, then, when the box needs any
/// file, the put of an archive of exactly those, timed in the run's split; a sibling pair's manifest naming a file
/// writes the pair inputs marker first, one that cannot be written refusing the route `inputs` before any call
/// ([`pair_marked`]); once it landed, the carried marker is kept as `built` says ([`landed`]). Whether it landed, so the run call carries its digest. A Ctrl+C ends the run. Any other
/// failure of an empty manifest that removes copies, carried ones ([`Marking::Clear`]) or a sibling pair's old ones
/// ([`Marking::Swept`]), is said in the log and the run goes on without it, its marker kept so a later run sends it
/// again; of any other manifest, it refuses the route `inputs`, its reason in the log. An end no inputs call returns
/// fails the run.
pub(super) fn send(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &Built) -> Result<bool, End> {
    if let Err(why) = pair_marked(attempt, built) {
        return Err(refused(attempt, &why));
    }
    let started = Instant::now();
    let request = Request {
        location,
        nonce,
        kind: Kind::Inputs,
    };
    let ended = sent_to(attempt, &request, built);
    attempt.split.inputs = Some(started.elapsed());
    match ended {
        Ok(()) => {
            landed(attempt, built);
            Ok(true)
        }
        Err(Unsent::Ended(End::Interrupted)) => Err(End::Interrupted),
        Err(unsent) if matches!(built.marking, Marking::Clear | Marking::Swept) => {
            let copies = if built.marking == Marking::Clear { "carried" } else { "old" };
            let why = unsent_why(&unsent);
            attempt.logged(&format!("gate: inputs: the {copies} copies were not removed: {}", escaped(&why)));
            Ok(false)
        }
        Err(Unsent::Ended(End::Failed(why)) | Unsent::Refused(why)) => Err(refused(attempt, &why)),
        // The box refused for its install, its line said: the run falls back for that, not for its inputs.
        Err(Unsent::Ended(End::Refused(FallbackReason::Install))) => Err(End::Refused(FallbackReason::Install)),
        // The builder's drain refused a session no longer live: the run waits behind it, then starts again.
        Err(Unsent::Ended(End::Draining(drain))) => Err(End::Draining(drain)),
        Err(Unsent::Ended(End::Refused(reason))) => Err(refused(attempt, &format!("the inputs call ended {}", reason.as_str()))),
        Err(Unsent::Ended(other)) => Err(End::Failed(format!(
            "the inputs call ended {}, which no inputs call does",
            unexpected(&other)
        ))),
    }
}

/// Why the PC's build of a project was not sent, `why`: the log's line, which the dropped line follows on the console.
pub(super) fn build_not_sent(why: &str) -> String {
    format!("gate: remote build not sent: {}", escaped(why))
}

/// The ends of the names a send carries from the project's `obj` folder beside its bin folder: the restore files.
const RESTORE_ENDS: [&str; 2] = [".nuget.g.props", ".nuget.g.targets"];

/// Sends the PC's build of the project whose bin folder below the worktree root is `bin`, `{project}/bin/{configuration}`,
/// to the work folder at `location`, so the builder keeps a routed `dotnet test`'s `--no-build`: the manifest of the
/// files under that folder and of the project's two restore files ([`build_built`]), then the diff and the put of what
/// the box lacks, as the inputs are sent ([`send`]), under a nonce, `.tmp` names, a hash cache and a part of the split of
/// its own. The digest of the manifest line once it landed, the run call's `SentDigest`.
///
/// `None` for a send that did not land, its reason in the log ([`build_not_sent`]): a restore file missing on the PC, no
/// file under the bin folder, a manifest past a cap, a refusal, a failure or a lost connection; the run then takes the
/// drop. Never the route's `inputs` refusal: a Ctrl+C ends the run, the box's `install` refusal falls back as an
/// inputs call's does, and the builder's drain refusal ends the attempt [`End::Draining`], as an inputs call's does.
pub(super) fn sent_build(attempt: &mut Attempt<'_, '_>, location: &Location, bin: &str) -> Result<Option<String>, End> {
    let started = Instant::now();
    let ended = build_sent(attempt, location, bin);
    attempt.split.sent_build = Some(started.elapsed());
    let why = match ended {
        Ok(digest) => return Ok(Some(digest)),
        Err(Unsent::Ended(End::Interrupted)) => return Err(End::Interrupted),
        Err(Unsent::Ended(End::Refused(FallbackReason::Install))) => return Err(End::Refused(FallbackReason::Install)),
        // The builder's drain refused the send: the run waits behind it, then sends again, rather than taking the drop.
        Err(Unsent::Ended(End::Draining(drain))) => return Err(End::Draining(drain)),
        Err(Unsent::Refused(why) | Unsent::Ended(End::Failed(why))) => why,
        Err(Unsent::Ended(other)) => format!("the inputs call ended {}", unexpected(&other)),
    };
    attempt.logged_kept(build_not_sent(&why));
    Ok(None)
}

/// The manifest of the PC's build of the project of `bin` ([`build_built`]), sent under a nonce new to it; its digest,
/// else why not or the end a call ended on.
fn build_sent(attempt: &mut Attempt<'_, '_>, location: &Location, bin: &str) -> Result<String, Unsent> {
    let built = match build_built(attempt, bin) {
        Ok(built) => built,
        Err(Unbuilt::Interrupted) => return Err(Unsent::Ended(End::Interrupted)),
        Err(Unbuilt::Failed(why)) => return Err(Unsent::Refused(why)),
    };
    let nonce = payload::run_nonce();
    let request = Request {
        location,
        nonce: &nonce,
        kind: Kind::Build(bin),
    };
    sent_to(attempt, &request, &built)?;
    Ok(built.digest)
}

/// The project's `obj` folder of the bin folder `bin`, `/`-ended: `{project}/obj/`, or `obj/` for a project at the root.
fn obj_of(bin: &str) -> String {
    match bin.rsplitn(3, '/').nth(2) {
        Some(project) => format!("{project}/obj/"),
        None => "obj/".to_owned(),
    }
}

/// The manifest of the PC's build of the project of `bin`: every file under `bin` and each file of the project's `obj`
/// folder whose name ends as a restore file does ([`RESTORE_ENDS`]), ignored by git or not, with the inputs' exclusions,
/// each hashed as an input is, taking its hash from the sent-build hash cache. A build with no file under `bin`, or
/// with either restore file missing, is not sent, nor is one past the inputs' caps.
fn build_built(attempt: &mut Attempt<'_, '_>, bin: &str) -> Result<Built, Unbuilt> {
    let call = attempt.call;
    let root = Path::new(call.root);
    let obj = obj_of(bin);
    let mut globs = vec![format!("{bin}/**")];
    globs.extend(RESTORE_ENDS.iter().map(|end| format!("{obj}*{end}")));
    let matched = Globs::inputs(&globs, &exclusions(root, call.key, call.log)).map_err(Unbuilt::Failed)?;
    let found = walk(root, &matched).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    if let Some(index) = (0..globs.len()).find(|index| !found.iter().any(|file| matched.matched_by(*index, &file.path))) {
        return Err(Unbuilt::Failed(if index == 0 {
            format!("the PC holds no file under {bin}")
        } else {
            format!("the PC holds no restore file {}", globs[index])
        }));
    }
    if found.len() > MEMBERS {
        return Err(Unbuilt::Failed(format!(
            "{} files make the PC's build, more than one put carries ({MEMBERS})",
            found.len()
        )));
    }
    let cache = v1::sent_build_hashes_path(call.state_dir, call.builder.id.as_deref(), call.key);
    let asked = Asked {
        root,
        globs: &globs,
        key: call.key,
        log: call.log,
        cache: &cache,
        caps: Caps::FULL,
        bounds: attempt.git,
        interrupt: call.interrupt,
    };
    let manifest = hashed(&asked, &found, || attempt.go_on().is_ok()).map_err(Unmade::unbuilt)?;
    let line = manifest.line();
    if line.len() > asked.caps.manifest {
        return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned()));
    }
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    Ok(Built {
        line,
        digest,
        manifest,
        found,
        said: Vec::new(),
        marking: Marking::Leave,
        trees: Vec::new(),
    })
}

/// How an end no inputs or LFS call returns is said: the run call's own ends, and a pair's.
pub(super) fn unexpected(end: &End) -> &'static str {
    match end {
        End::Status(_) => "with a command's status",
        End::Killed => "killed",
        End::Lost(_) => "with its connection lost",
        End::NotRouted(_) => "not routed",
        End::Refused(_) | End::OtherBuild(_) => "refused",
        End::PushFailed { .. } => "with a failed push",
        End::SentRefused => "refused for the PC's build",
        End::Draining(_) => "refused by the builder's drain",
        End::Failed(_) => "failed",
        End::TransferFailed(_) => "with a failed transfer",
        End::Interrupted => "interrupted",
    }
}

/// Why an inputs or LFS step was not done: a call that ended the attempt's way, or a reason of the step's own.
#[derive(Debug)]
pub(super) enum Unsent {
    Ended(End),
    Refused(String),
}

/// The inputs and LFS calls, as their lines name them.
#[derive(Clone, Copy)]
pub(super) enum Step {
    Diff,
    Put,
    LfsDiff,
    LfsPut,
}

impl Step {
    /// The call as a failure line names it.
    fn what(self) -> &'static str {
        match self {
            Step::Diff => "the inputs diff call",
            Step::Put => "the inputs put call",
            Step::LfsDiff => "the LFS diff call",
            Step::LfsPut => "the LFS put call",
        }
    }

    /// The call as the reason of its cut names it.
    fn short(self) -> &'static str {
        match self {
            Step::Diff | Step::LfsDiff => "the diff",
            Step::Put | Step::LfsPut => "the put",
        }
    }
}

/// Why the inputs or LFS call `step` was cut as `cut`: a stall, the put's counted on the bytes both ways and the diff's
/// on its output, or its transfer ceiling, each in whole seconds; any other cut as the watchdog words it.
fn cut_why(step: Step, cut: Cut) -> String {
    let seconds = watchdog::whole_seconds;
    match (step, cut) {
        (Step::Put | Step::LfsPut, Cut::Silent(window)) => format!("the put stalled: no bytes sent or received for {} s", seconds(window)),
        (Step::Diff | Step::LfsDiff, Cut::Silent(window)) => format!("the diff stalled: no output for {} s", seconds(window)),
        (_, Cut::Ceiling(ceiling)) => format!("{} passed its transfer ceiling of {} s", step.short(), seconds(ceiling)),
        _ => cut.reason(step.what()),
    }
}

/// What an inputs call sends: the repo's inputs, or the PC's build of one project, its bin folder below the worktree
/// root, `{project}/bin/{configuration}`, sent as `SentBin`.
#[derive(Clone, Copy)]
pub(crate) enum Kind<'k> {
    Inputs,
    Build(&'k str),
}

impl<'k> Kind<'k> {
    /// What the step's lines call what it sends.
    fn label(self) -> &'static str {
        match self {
            Kind::Inputs => "inputs",
            Kind::Build(_) => "sent build",
        }
    }

    /// What a line saying the put as it starts calls it.
    fn sent_what(self) -> &'static str {
        match self {
            Kind::Inputs => "inputs",
            Kind::Build(_) => "the PC's build",
        }
    }

    /// The name of its files in `.tmp`, after `{key}-{nonce}.`: its diff answer `{name}-diff` and its archive
    /// `{name}.tar.zst`.
    pub(crate) fn file(self) -> &'static str {
        match self {
            Kind::Inputs => "inputs",
            Kind::Build(_) => "sent-build",
        }
    }

    /// Its PC hash cache for the work folder `key` on the builder whose id is `id` in the state root `state`.
    fn cache(self, state: &Path, id: Option<&str>, key: &str) -> PathBuf {
        match self {
            Kind::Inputs => v1::inputs_hashes_path(state, id, key),
            Kind::Build(_) => v1::sent_build_hashes_path(state, id, key),
        }
    }

    /// The bin folder of a send of the PC's build, `None` for the inputs.
    fn bin(self) -> Option<&'k str> {
        match self {
            Kind::Inputs => None,
            Kind::Build(bin) => Some(bin),
        }
    }
}

/// Where an inputs call sends what, under which nonce.
struct Request<'r> {
    location: &'r Location,
    nonce: &'r str,
    kind: Kind<'r>,
}

/// The diff, then the put of what it needs ([`send`]), the worktree's `.tmp` held from before the diff's answer is
/// made until the put has ended. Says the put as it starts, with the files it sends, once the diff says there are any.
fn sent_to(attempt: &mut Attempt<'_, '_>, request: &Request<'_>, built: &Built) -> Result<(), Unsent> {
    let (nonce, kind) = (request.nonce, request.kind);
    let root = Path::new(attempt.call.root);
    let folder = root.join(TMP);
    let mut held = Archive::new(root, folder.clone());
    fs::create_dir_all(&folder)
        .and_then(|()| held.hold(&folder))
        .map_err(|error| Unsent::Refused(format!("cannot hold {}: {error}", folder.display())))?;
    let needed = diffed(attempt, request, built)?;
    if needed.is_empty() {
        // The empty manifest that removes carried copies says so once it landed ([`landed`]).
        if !matches!(built.marking, Marking::Clear | Marking::Swept) {
            attempt.logged(&format!("gate: {}: {} file(s) unchanged", kind.label(), built.manifest.files().count()));
        }
        return Ok(());
    }
    let bytes = needed.iter().fold(0_u64, |total, file| total.saturating_add(file.stamp.size));
    if bytes > attempt.inputs_budget {
        return Err(Unsent::Refused(format!(
            "the inputs the builder needs hold {bytes} bytes, more than one put carries ({} bytes)",
            attempt.inputs_budget
        )));
    }
    // Said as the pack begins, once the diff says there is something to send: a run that sends nothing says nothing new.
    attempt.say(format!(
        "gate: sending {} to {}: {} file(s)",
        kind.sent_what(),
        attempt.call.host(),
        needed.len()
    ));
    let path = folder.join(format!("{}-{nonce}.{}.tar.zst", attempt.call.key, kind.file()));
    let started = Instant::now();
    let archive = made_in_tmp(root, &path, &format!("the {} archive", kind.label()))?;
    let cache = kind.cache(attempt.call.state_dir, attempt.call.builder.id.as_deref(), attempt.call.key);
    let packing = Pack {
        root,
        files: &needed,
        manifest: &built.manifest,
        cache: &cache,
        trees: &built.trees,
    };
    // The archive's SHA-256 is read back only for a put that asks for the builder's data channel, which declares it.
    let hashed = matches!(channel::planned(attempt.call.builder), Ok(Some(_)));
    let (compressed, sha256) = packed(&archive, &path, &packing, &|| attempt.go_on().is_ok(), hashed)?;
    put(attempt, request, built, (&archive, (compressed, sha256.as_ref()), needed.len()))?;
    drop(archive);
    drop(held);
    attempt.say(sent_line(kind.label(), needed.len(), bytes, compressed, started.elapsed()));
    Ok(())
}

/// A new file at `path` in the held `.tmp` of the worktree `root`, `what` it holds: made new, read and written through
/// the handle it gives and its clones, shared for reading alone, and removed once its last handle closes, so a run
/// killed outright leaves none behind. One whose own final path is not inside the worktree is refused, and goes with
/// its handle. Else why it cannot be made.
fn made_in_tmp(root: &Path, path: &Path, what: &str) -> Result<File, Unsent> {
    tmp_file(root, path, what).map_err(Unsent::Refused)
}

/// The file [`made_in_tmp`] makes; else why not.
pub(super) fn tmp_file(root: &Path, path: &Path, what: &str) -> Result<File, String> {
    let cannot = |error: io::Error| format!("cannot make {what} {}: {error}", path.display());
    let inside = fs::canonicalize(root).map_err(cannot)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .create_new(true)
        .open(path)
        .map_err(cannot)?;
    if !final_path(&file).map_err(cannot)?.starts_with(&inside) {
        return Err(format!("{what} {} is not inside the worktree", path.display()));
    }
    Ok(file)
}

/// The console line of a put of what `label` names that sent `count` files of `bytes` bytes, `compressed` as sent, in
/// `took`.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn sent_line(label: &str, count: usize, bytes: u64, compressed: u64, took: Duration) -> String {
    format!(
        "gate: {label}: {count} file(s), {:.1} MB ({:.1} MB compressed) in {:.1} s",
        bytes as f64 / MB,
        compressed as f64 / MB,
        took.as_secs_f64()
    )
}

/// The files of `built` the box's diff call says its work folder lacks or holds otherwise; else why not. Its answer is
/// written raw into a file of the held `.tmp` and read back through the same handle; the call is watched by its own
/// output and a transfer ceiling counted from its own start, as a fetch is.
fn diffed<'b>(attempt: &mut Attempt<'_, '_>, request: &Request<'_>, built: &'b Built) -> Result<Vec<&'b Found>, Unsent> {
    let (nonce, kind) = (request.nonce, request.kind);
    // Each call's word carries a nonce new to it, which the box's refusal tags for the call name; the PC's own files
    // keep the run's.
    let call_nonce = payload::run_nonce();
    let word = payload::inputs_word(request.location, &call_nonce, InputsStep::Diff, kind.bin());
    let head = payload::input(&word, Some(&built.line));
    let root = Path::new(attempt.call.root);
    let path = root.join(TMP).join(format!("{}-{nonce}.{}-diff", attempt.call.key, kind.file()));
    let answer = made_in_tmp(root, &path, &format!("the {} diff answer", kind.label()))?;
    let cap = NonZeroU64::new(2 * (built.line.len() as u64) + 1024 * 1024).unwrap_or(NonZeroU64::MIN);
    let shown = path.display().to_string();
    // Each attempt writes the answer anew from its start, through a clone of the one handle. The clones share one
    // position, which is safe here: a call's raw copy writes nothing more once its call has ended, before any retry.
    let make = || -> Result<RawSink, String> {
        let cannot = |error: io::Error| format!("cannot write {shown}: {error}");
        let mut file = answer.try_clone().map_err(cannot)?;
        file.set_len(0).map_err(cannot)?;
        file.seek(SeekFrom::Start(0)).map_err(cannot)?;
        Ok(RawSink {
            file,
            shown: shown.clone(),
            cap,
        })
    };
    let watch = Watch::Fetch(attempt.limits_for("inputs-diff"), None);
    // A diff asks for no data channel: every attempt carries the one head.
    let made = |_: Option<&channel::Asked>| (call_nonce.clone(), Arc::clone(&head));
    let called = attempt.inputs_call(&made, None, Some(&make), watch).map_err(Unsent::Ended)?;
    judged(attempt, &called, Step::Diff)?;
    let mut text = Vec::new();
    (&answer)
        .seek(SeekFrom::Start(0))
        .and_then(|_| (&answer).take(cap.get()).read_to_end(&mut text))
        .map_err(|error| Unsent::Refused(format!("cannot read {shown}: {error}")))?;
    needed(&String::from_utf8_lossy(&text), built).map_err(Unsent::Refused)
}

/// Why the inputs or LFS call `step` that ended as `called` failed: a cut, said in the reason alone, as the preflight's is,
/// since the run falls back here and a watchdog line in its log would fail the run here; or a status other than 0, the
/// box's last error line when it said one, without its leading `gate: `, as the line that keeps it says its own.
pub(super) fn judged(attempt: &mut Attempt<'_, '_>, called: &Called, step: Step) -> Result<(), Unsent> {
    if let Some(cut) = called.cut {
        attempt.noted(called);
        return Err(Unsent::Refused(cut_why(step, cut)));
    }
    if called.status != 0 {
        return Err(Unsent::Refused(match &called.last_error {
            Some(line) => line.strip_prefix("gate: ").unwrap_or(line).to_owned(),
            None => format!("{} failed (status {})", step.what(), called.status),
        }));
    }
    Ok(())
}

/// The files of `built` the diff's standard output `said` names: `need <n>`, then `n` paths, each escaped as the box
/// escapes it, each a file of the manifest and none named twice; else why the answer is not one.
fn needed<'b>(said: &str, built: &'b Built) -> Result<Vec<&'b Found>, String> {
    let mut lines = said.lines().filter(|line| !line.trim().is_empty());
    let count: usize = lines
        .next()
        .and_then(|line| line.strip_prefix(NEED))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| format!("the builder printed no '{NEED}<n>' line"))?;
    let by_escaped: HashMap<String, &Found> = built.found.iter().map(|file| (escaped(&file.path), file)).collect();
    let mut seen = HashSet::new();
    let needed = lines
        .map(|line| {
            let path = line
                .strip_prefix(PATH)
                .ok_or_else(|| format!("the builder printed '{}'", escaped(line)))?;
            let file = by_escaped
                .get(path)
                .copied()
                .ok_or_else(|| format!("the builder named '{}', which the manifest does not hold", escaped(path)))?;
            if !seen.insert(path) {
                return Err(format!("the box named {} twice", escaped(path)));
            }
            Ok(file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if needed.len() != count {
        return Err(format!("the builder said it needs {count} file(s) and named {}", needed.len()));
    }
    Ok(needed)
}

/// What a put's archive is packed of: the `files` below the worktree `root`, each to hold what `manifest` says it held
/// when it was hashed, and the inputs hash cache file `cache` a file that does not is forgotten from.
struct Pack<'p> {
    root: &'p Path,
    files: &'p [&'p Found],
    manifest: &'p Manifest,
    cache: &'p Path,
    /// On a sibling pair's run, the trees the files' prefixed paths lie in: each file is packed from its own tree's
    /// root and forgotten from its own tree's cache, and `root` and `cache` are not used; empty for one repo.
    trees: &'p [Tree],
}

/// Packs `packing`'s files into the open `archive`, shown as `shown`, `go_on` asked before the pack and before each
/// write into the archive; each packed file is checked to hold what the manifest says it held when it was hashed, and
/// one that does not is forgotten from the inputs hash cache before the step is refused, so the next run hashes it
/// again. The archive's compressed size and, when `hashed`, the SHA-256 of those bytes, read back from the file once it
/// is packed, `go_on` asked before each read; else the interrupt, or why not.
fn packed(archive: &File, shown: &Path, packing: &Pack<'_>, go_on: &dyn Fn() -> bool, hashed: bool) -> Result<(u64, Option<[u8; 32]>), Unsent> {
    let stopped = Cell::new(false);
    if !go_on() {
        return Err(Unsent::Ended(End::Interrupted));
    }
    // The box's own unpack stamps nothing by the mark, so every input is listed as the run's.
    let chosen: Vec<Chosen<'_>> = packing.files.iter().map(|file| Chosen::new(file, Wrote::ThisRun)).collect();
    let sink = Asking {
        inner: archive,
        go_on,
        stopped: &stopped,
    };
    let names = Tree::names(packing.trees);
    let result = if packing.trees.is_empty() {
        pack(sink, packing.root, &chosen, &HeldBy::new())
    } else {
        let roots: Option<Vec<&Path>> = packing
            .files
            .iter()
            .map(|file| Tree::of(packing.trees, &names, &file.path).map(|(tree, _)| tree.root.as_path()))
            .collect();
        let roots = roots.ok_or_else(|| Unsent::Refused("an input names no tree of the pair".to_owned()))?;
        pack_rooted(sink, &chosen, &roots, &HeldBy::new())
    };
    if stopped.get() {
        return Err(Unsent::Ended(End::Interrupted));
    }
    let packed = result.map_err(|error| Unsent::Refused(format!("cannot pack the inputs into {}: {error}", shown.display())))?;
    if let Some(entry) = packed
        .entries
        .iter()
        .find(|entry| packing.manifest.get(&entry.path).is_none_or(|held| held.sha256 != entry.sha256))
    {
        let (cache_path, key) = match Tree::of(packing.trees, &names, &entry.path) {
            Some((tree, rest)) => (tree.cache.as_path(), rest),
            None => (packing.cache, entry.path.as_str()),
        };
        if let Some(mut cache) = HashCache::load(cache_path) {
            cache.forget(key);
            cache.save(cache_path);
        }
        return Err(Unsent::Refused(format!("{} changed after it was hashed", escaped(&entry.path))));
    }
    let sha256 = if hashed {
        Some(archive_sha256(archive, (shown, packed.compressed), go_on)?)
    } else {
        None
    };
    Ok((packed.compressed, sha256))
}

/// The SHA-256 of the first `length` bytes of `archive`, shown as `shown`, `go_on` asked before each read, as the pack
/// asks it before each write; else the interrupt, or why not, a file shorter than `length` included. Each read names
/// its own offset, counted from the file's start, so where the handle's position stood does not matter; on Windows
/// each read moves that position, which no later read of the archive relies on, as each names its offset too.
fn archive_sha256(archive: &File, (shown, length): (&Path, u64), go_on: &dyn Fn() -> bool) -> Result<[u8; 32], Unsent> {
    let cannot = |why: String| Unsent::Refused(format!("cannot read {} back: {why}", shown.display()));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        if !go_on() {
            return Err(Unsent::Ended(End::Interrupted));
        }
        let want = usize::try_from(length - offset).unwrap_or(usize::MAX).min(buffer.len());
        let read = archive
            .seek_read(&mut buffer[..want], offset)
            .map_err(|error| cannot(error.to_string()))?;
        if read == 0 {
            return Err(cannot(format!("the archive ends after {offset} of its {length} bytes")));
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(hasher.finalize().into())
}

/// A writer into the inputs archive that asks `go_on` before each write, and fails the write once it says no, with
/// `stopped` set, so a Ctrl+C during the pack ends it.
pub(super) struct Asking<'a, W: Write> {
    pub(super) inner: W,
    pub(super) go_on: &'a dyn Fn() -> bool,
    pub(super) stopped: &'a Cell<bool>,
}

impl<W: Write> Write for Asking<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !(self.go_on)() {
            self.stopped.set(true);
            return Err(io::Error::other("the run was interrupted"));
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The put call of the `sent` files packed into the open archive, its compressed length, its SHA-256 when the put may
/// take the builder's data channel, and their count: the request, the manifest and then the archive, over ssh or that
/// channel, watched as a put; else why not, a builder that installed another count than was sent included.
fn put(attempt: &mut Attempt<'_, '_>, request: &Request<'_>, built: &Built, sent: (&File, (u64, Option<&[u8; 32]>), usize)) -> Result<(), Unsent> {
    let (file, (length, sha256), count) = sent;
    // Each attempt's word carries a nonce new to it, which the box's refusal tags for the call name and the box's
    // stage is named by, and the data channel the attempt asks for, with proofs of its own.
    let head = |asked: Option<&channel::Asked>| {
        let nonce = payload::run_nonce();
        let channel = asked.zip(sha256).map(|(asked, sha256)| (asked, length, sha256));
        let word = payload::put_word(request.location, &nonce, request.kind.bin(), channel);
        (nonce, payload::input(&word, Some(&built.line)))
    };
    let watch = Watch::Put(attempt.limits_for("inputs"));
    let archive = Sent { file, length, sha256 };
    let called = attempt.inputs_call(&head, Some(archive), None, watch).map_err(Unsent::Ended)?;
    judged(attempt, &called, Step::Put)?;
    installed_all(&called.lines, count).map_err(Unsent::Refused)
}

/// Whether the put's `lines` say the builder installed each of the `sent` files, by its `installed <n>` line; else why
/// not.
fn installed_all(lines: &[String], sent: usize) -> Result<(), String> {
    let said = lines
        .iter()
        .find_map(|line| line.strip_prefix(INSTALLED))
        .ok_or_else(|| format!("the builder printed no '{INSTALLED}<n>' line"))?;
    match said.trim().parse::<usize>() {
        Ok(count) if count == sent => Ok(()),
        Ok(count) => Err(format!("the builder installed {count} of the {sent} inputs sent")),
        Err(_) => Err(format!("the builder printed '{}'", escaped(&format!("{INSTALLED}{said}")))),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::ops::Deref;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use gate_router::{BuilderOs, CARRIED_INPUT, PAIR_CAPS};
    use serde_json::Value;

    use super::{
        Asked, Built, Caps, End, Marking, PAST_THE_CAP, Pack, PairAsked, PairInputs, PairTree, Single, Unsent, built_pair, carried_form,
        gitmodules_at, installed_all, made_in_tmp, needed, packed, pair_declared, pair_inputs, relative, single,
    };
    use crate::outputs::hash::sha256_of;
    use crate::outputs::walk::Found;
    use crate::remote::git::GIT_BOUNDS;
    use crate::remote::outputs::Unbuilt;

    const KEY: &str = "app-1a2b3c4d";

    /// A test's scratch folder, removed when the test ends.
    struct Scratch(PathBuf);

    impl Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // A folder a test leaves is emptied by the next run of the test, which starts by removing it.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An empty scratch folder for one test, emptied first.
    fn scratch(name: &str) -> Scratch {
        let folder = crate::test_support::temp_dir().join(format!("gate-remote-inputs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        Scratch(folder)
    }

    /// A scratch worktree for one test, emptied first, made a git repository whose `.gitignore` is `ignore`.
    fn worktree(name: &str, ignore: &str) -> Scratch {
        let folder = scratch(name);
        git(&folder, &["init", "--quiet"]);
        put(&folder, ".gitignore", ignore);
        folder
    }

    #[expect(clippy::disallowed_methods, reason = "a test's git setup takes no spawn lock")]
    fn git(folder: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(folder).args(args).status().expect("git can be run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Writes `text` at the `/`-separated `path` below `root`, its folders made, last written an hour ago, so a build
    /// keeps its hash in the cache.
    fn put(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("the file has a folder")).expect("the folder can be made");
        fs::write(&full, text).expect("the file can be written");
        let file = fs::File::options().write(true).open(&full).expect("the file can be opened");
        file.set_modified(SystemTime::now() - Duration::from_hours(1))
            .expect("its last write can be set");
    }

    /// What a single repo's run of `globs` in `root` sends, its log at `log`, built under `caps`.
    fn single_as(root: &Path, globs: &[&str], log: &Path, caps: Caps) -> Result<Single, Unbuilt> {
        single_under(root, globs, log, caps, &|| true)
    }

    /// [`single_as`] with `go_on` asked as the run asks it.
    fn single_under(root: &Path, globs: &[&str], log: &Path, caps: Caps, go_on: &dyn Fn() -> bool) -> Result<Single, Unbuilt> {
        let globs: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
        let asked = Asked {
            root,
            globs: &globs,
            key: KEY,
            log,
            cache: &cache_of(root),
            caps,
            bounds: GIT_BOUNDS,
            interrupt: None,
        };
        single(&asked, go_on)
    }

    /// The inputs of `globs` in `root`, its log at `log`, built under `caps`; a run that sends none is a failure.
    fn build_as(root: &Path, globs: &[&str], log: &Path, caps: Caps) -> Result<Built, Unbuilt> {
        single_as(root, globs, log, caps).and_then(|single| match single {
            Single::Built(built) => Ok(built),
            Single::None(said) => Err(Unbuilt::Failed(format!("the run sends no inputs: {said:?}"))),
        })
    }

    /// The lines of a single repo's run of `globs` in `root` that sends no inputs; else what it sent, or why it failed.
    fn none_said(root: &Path, globs: &[&str]) -> Result<Vec<String>, String> {
        match single_as(root, globs, &root.join("logs").join("run.log"), Caps::FULL) {
            Ok(Single::None(said)) => Ok(said),
            Ok(Single::Built(built)) => Err(format!("the run sends inputs: {:?} {:?}", paths(&built), built.said)),
            Err(unbuilt) => Err(failure(Err(unbuilt))),
        }
    }

    fn build(root: &Path, globs: &[&str]) -> Result<Built, Unbuilt> {
        build_as(root, globs, &root.join("logs").join("run.log"), Caps::FULL)
    }

    fn cache_of(root: &Path) -> PathBuf {
        root.join("state").join("inputs-hashes")
    }

    fn paths(built: &Built) -> Vec<&str> {
        built.manifest.files().map(|(path, _)| path).collect()
    }

    /// Why `built` failed, which it must have.
    fn failure(built: Result<Built, Unbuilt>) -> String {
        match built {
            Err(Unbuilt::Failed(why)) => why,
            Err(Unbuilt::Interrupted) => "the build was interrupted".to_owned(),
            Ok(built) => format!("the build passed: {:?}", paths(&built)),
        }
    }

    /// The inputs of a sibling pair whose trees are `trees`, each its root and its repo's globs, each tree's folder
    /// name its prefix and its repo's name, built under `caps`; each file the build asks about before its read counted
    /// in `asked`.
    fn build_pair(trees: &[(&Path, &[&str])], caps: Caps, asked: &Cell<usize>) -> Result<Built, Unbuilt> {
        let globs: Vec<Vec<String>> = trees
            .iter()
            .map(|(_, globs)| globs.iter().map(|glob| (*glob).to_owned()).collect())
            .collect();
        let names: Vec<String> = trees
            .iter()
            .map(|(root, _)| root.file_name().expect("the tree has a name").to_string_lossy().into_owned())
            .collect();
        let pair: Vec<PairTree<'_>> = trees
            .iter()
            .zip(&globs)
            .zip(&names)
            .map(|(((root, _), globs), name)| PairTree {
                prefix: name,
                repo: name,
                root,
                globs,
                key: KEY.to_owned(),
                cache: cache_of(root),
            })
            .collect();
        let log = trees[0].0.join("logs").join("run.log");
        let pair_asked = PairAsked {
            layout: names.join("|"),
            log: &log,
            caps,
            bounds: GIT_BOUNDS,
            interrupt: None,
        };
        built_pair(&pair, &pair_asked, || {
            asked.set(asked.get() + 1);
            true
        })
    }

    #[test]
    fn a_pair_past_the_member_cap_together_is_refused_before_any_file_is_hashed() {
        let (app, lib) = (worktree("pair-cap-app", "secrets/\n"), worktree("pair-cap-lib", "secrets/\n"));
        for root in [&app, &lib] {
            put(root, "secrets/a", "a");
            put(root, "secrets/b", "b");
        }
        let asked = Cell::new(0);

        let refused = failure(build_pair(
            &[(&app, &["secrets/**"]), (&lib, &["secrets/**"])],
            Caps { members: 3, ..Caps::FULL },
            &asked,
        ));

        assert_eq!(
            refused, "4 files match the input globs, more than one put carries (3) (the pair's trees together)",
            "each tree's two stay under the cap, their four do not"
        );
        assert_eq!(asked.get(), 0, "no file was hashed once the trees together passed the cap");
    }

    #[test]
    fn a_pair_past_the_member_cap_with_submodule_files_says_so() {
        let (app, lib) = (worktree("pair-sub-cap-app", "secrets/\n"), worktree("pair-sub-cap-lib", "secrets/\n"));
        put(&app, "secrets/a", "a");
        with_submodule(&lib);
        put(&lib, "tools/shared/untracked.txt", "untracked");

        let refused = failure(build_pair(
            &[(&app, &["secrets/**"]), (&lib, &[])],
            Caps { members: 2, ..Caps::FULL },
            &Cell::new(0),
        ));

        assert!(
            refused.starts_with("3 files with the submodules' files, more than one put carries (2)"),
            "{refused}"
        );
        assert!(refused.ends_with(PAIR_CAPS), "{refused}");
    }

    #[test]
    fn a_tree_alone_past_a_cap_names_its_repo_and_not_the_pair() {
        let (app, lib) = (worktree("pair-alone-app", "secrets/\n"), worktree("pair-alone-lib", "secrets/\n"));
        put(&app, "secrets/a", "a");
        for path in ["secrets/a", "secrets/b", "secrets/c"] {
            put(&lib, path, "x");
        }
        let repo = lib.file_name().expect("the tree has a name").to_string_lossy().into_owned();
        let trees: [(&Path, &[&str]); 2] = [(&app, &["secrets/**"]), (&lib, &["secrets/**"])];

        let members = failure(build_pair(&trees, Caps { members: 2, ..Caps::FULL }, &Cell::new(0)));
        let manifest = failure(build_pair(
            &[(&app, &[]), (&lib, &["secrets/**"])],
            Caps { manifest: 64, ..Caps::FULL },
            &Cell::new(0),
        ));

        assert_eq!(
            members,
            format!("{repo}: 3 files match the input globs, more than one put carries (2)"),
            "the sibling alone passes the member cap"
        );
        assert_eq!(manifest, format!("{repo}: {PAST_THE_CAP}"), "the sibling alone passes the manifest's cap");
    }

    #[test]
    fn a_pair_with_inputs_on_a_macos_builder_falls_back_before_any_inputs_call() {
        assert_eq!(
            pair_inputs(BuilderOs::Macos, true),
            PairInputs::Refused("the macOS builder does not take a sibling pair's inputs yet")
        );
        assert_eq!(pair_inputs(BuilderOs::Windows, true), PairInputs::Sent, "a Windows builder takes them");
    }

    #[test]
    fn a_pair_tree_s_gitmodules_declares_inputs_as_its_globs_do() {
        let root = scratch("pair-declared");
        let globs = ["secrets/**".to_owned()];
        assert!(!pair_declared([(&*root, &[][..])]), "no globs and no .gitmodules declare nothing");
        assert!(pair_declared([(&*root, &globs[..])]), "globs declare inputs");
        put(&root, ".gitmodules", &gitmodules(&["tools/shared"]));
        assert!(pair_declared([(&*root, &[][..])]), "a .gitmodules may carry a submodule's files");
    }

    #[test]
    fn a_macos_pair_without_inputs_asks_for_no_inputs_call() {
        assert_eq!(
            pair_inputs(BuilderOs::Macos, false),
            PairInputs::None,
            "decided before any cache is looked at, so no sweep is sent"
        );
        assert_eq!(
            pair_inputs(BuilderOs::Windows, false),
            PairInputs::Sent,
            "a Windows builder's caches decide the sweep"
        );
    }

    #[test]
    fn a_pair_carries_each_tree_s_command_named_paths_in_that_tree_under_the_pair_marker() {
        let (app, lib) = (worktree("pair-carried-app", "probe/\n"), worktree("pair-carried-lib", ".tmp/\n"));
        put(&app, "probe/P.csproj", "x");
        put(&lib, ".tmp/probe.targets", "t");
        let (app_carried, lib_carried) = (format!("{CARRIED_INPUT}probe/P.csproj"), format!("{CARRIED_INPUT}.tmp/probe.targets"));
        let name = |root: &Path| root.file_name().expect("the tree has a name").to_string_lossy().into_owned();

        let built = build_pair(
            &[(&app, &[app_carried.as_str()]), (&lib, &[lib_carried.as_str()])],
            Caps::FULL,
            &Cell::new(0),
        )
        .map_err(|unbuilt| failure(Err(unbuilt)));

        let expected = vec![format!("{}/probe/P.csproj", name(&app)), format!("{}/.tmp/probe.targets", name(&lib))];
        assert_eq!(
            built
                .as_ref()
                .map(|built| paths(built).into_iter().map(str::to_owned).collect::<Vec<_>>()),
            Ok::<_, &String>(expected),
            "each carried path lands under its own tree's prefix"
        );
        assert!(
            built.is_ok_and(|built| built.marking == Marking::Leave),
            "a pair writes no carried marker: the pair inputs marker sweeps its copies"
        );
    }

    #[test]
    fn a_carried_path_the_exclusions_match_is_neither_sent_nor_missing() {
        let root = worktree("carried-excluded", "*.log\n*.err\n.tmp/\nprobe/\n");
        let stage = format!(".tmp/{KEY}-x");
        let carried = ["probe/P.csproj", "logs/run.log", "logs/run.log.err", stage.as_str()];
        for path in carried {
            put(&root, path, "x");
        }
        let globs: Vec<String> = carried.iter().map(|path| format!("{CARRIED_INPUT}{path}")).collect();

        let built = build(&root, &globs.iter().map(String::as_str).collect::<Vec<_>>()).map_err(|unbuilt| failure(Err(unbuilt)));

        assert_eq!(
            built.as_ref().map(paths),
            Ok::<_, &String>(vec!["probe/P.csproj"]),
            "neither the log, its .err nor a stage is sent, and none is missing"
        );
    }

    #[test]
    fn a_carried_folder_sends_its_gitignored_files_and_is_never_missing() {
        let root = worktree("carried-folder", ".tmp/\n");
        for path in [
            ".tmp/mocks/a-src/index.html",
            ".tmp/mocks/a-src/models/m.glb",
            ".tmp/mocks/b-src/index.html",
        ] {
            put(&root, path, "x");
        }
        let folder = format!("{CARRIED_INPUT}.tmp/mocks/a-src/**");
        let missing = format!("{CARRIED_INPUT}.tmp/mocks/gone-src/**");

        let built = build(&root, &[folder.as_str(), missing.as_str()]).map_err(|unbuilt| failure(Err(unbuilt)));

        assert_eq!(
            built.as_ref().map(paths),
            Ok::<_, &String>(vec![".tmp/mocks/a-src/index.html", ".tmp/mocks/a-src/models/m.glb"]),
            "the folder's files alone, and a folder that is not there is no failure"
        );
        let built = built.expect("built");
        assert_eq!(built.marking, Marking::Write, "a carried folder writes the carried marker");
        assert!(
            !built
                .said
                .iter()
                .any(|line| line.contains("could not be sent") || line.contains("matched nothing")),
            "{:?}",
            built.said
        );
    }

    /// Packs every file of `built` in `root` into a new archive in its `.tmp`, the cache at [`cache_of`] forgotten from,
    /// `go_on` asked.
    fn pack_all(root: &Path, built: &Built, go_on: &dyn Fn() -> bool) -> Result<(u64, Option<[u8; 32]>), Unsent> {
        fs::create_dir_all(root.join(".tmp")).expect("the .tmp folder can be made");
        let path = root.join(".tmp").join("pack.tar.zst");
        let archive = made_in_tmp(root, &path, "the inputs archive").expect("the archive can be made");
        let files: Vec<&Found> = built.found.iter().collect();
        let cache = cache_of(root);
        let packing = Pack {
            root,
            files: &files,
            manifest: &built.manifest,
            cache: &cache,
            trees: &[],
        };
        packed(&archive, &path, &packing, go_on, false)
    }

    #[test]
    fn a_put_that_installs_another_count_than_was_sent_failed() {
        let lines = |said: &[&str]| said.iter().map(|line| (*line).to_owned()).collect::<Vec<_>>();

        assert_eq!(installed_all(&lines(&["gate-remote-inputs: installed 2"]), 2), Ok(()));
        assert_eq!(
            installed_all(&lines(&["gate-remote-inputs: installed 1"]), 2),
            Err("the builder installed 1 of the 2 inputs sent".to_owned())
        );
        assert_eq!(
            installed_all(&lines(&["gate-remote-inputs: installed 3"]), 2),
            Err("the builder installed 3 of the 2 inputs sent".to_owned())
        );
        assert_eq!(
            installed_all(&lines(&["gate-remote-inputs: installed two"]), 2),
            Err("the builder printed 'gate-remote-inputs: installed two'".to_owned())
        );
        assert_eq!(
            installed_all(&lines(&["something else"]), 2),
            Err("the builder printed no 'gate-remote-inputs: installed <n>' line".to_owned())
        );
    }

    #[test]
    fn the_inputs_exclusions_leave_out_each_class_and_reach_the_tmp_folder() {
        let root = worktree("exclusions", ".tmp/\nlogs/\nx/\n*.env\n.env\n.env.*\n.envrc\n.lfsconfig\n");
        put(&root, ".tmp/helper/tool.dll", "helper");
        put(&root, &format!(".tmp/{KEY}-0123.tar.zst"), "an outputs archive");
        put(&root, &format!(".tmp/{KEY}-0123/staged.txt"), "an outputs stage");
        put(&root, ".tmp/other-key-0123.tar.zst", "another key's");
        put(&root, "secret.env", "secret");
        put(&root, ".env", "secret");
        put(&root, ".env.local", "secret");
        put(&root, ".env.production", "secret");
        put(&root, "x/.env.development.local", "secret");
        put(&root, ".ENV.Production", "secret");
        put(&root, ".envrc", "secret");
        put(&root, "x/.envrc", "secret");
        put(&root, "x/kept.txt", "kept");
        put(&root, ".lfsconfig", "lfs");
        put(&root, "logs/run.log", "the log");
        put(&root, "logs/run.log.err", "its err");
        put(&root, "logs/kept.txt", "kept");
        put(&root, "sub/.git/config", "a nested git folder");

        let built = build(&root, &["**"]).expect("the inputs are built");

        assert_eq!(
            paths(&built),
            [".tmp/helper/tool.dll", ".tmp/other-key-0123.tar.zst", "logs/kept.txt", "x/kept.txt"]
        );
    }

    #[test]
    fn a_match_git_does_not_ignore_is_skipped_with_its_line() {
        let root = worktree("snapshot", "keys.json\n");
        put(&root, "keys.json", "{}");
        put(&root, "tracked.json", "{}");

        let built = build(&root, &["*.json"]).expect("the inputs are built");

        assert_eq!(paths(&built), ["keys.json"]);
        assert_eq!(built.said, ["gate: inputs: tracked.json is in the snapshot; not sent as an input"]);
    }

    #[test]
    fn a_glob_matching_nothing_is_said() {
        let root = worktree("nothing", ".tmp/\n");
        put(&root, ".tmp/helper/a.txt", "a");

        let built = build(&root, &[".tmp/helper/**", "missing/**"]).expect("the inputs are built");

        assert_eq!(paths(&built), [".tmp/helper/a.txt"]);
        assert_eq!(built.said, ["gate: inputs: missing/** matched nothing"]);
    }

    #[test]
    fn a_folder_holding_its_own_git_is_left_out_once_with_its_line() {
        let root = worktree("nested", "vendor/\n");
        put(&root, "vendor/kept.txt", "kept");
        put(&root, "vendor/lib/.git/HEAD", "ref: refs/heads/main");
        put(&root, "vendor/lib/a.txt", "a");
        put(&root, "vendor/lib/deep/b.txt", "b");
        put(&root, "vendor/mod/.git", "gitdir: ../../.git/modules/mod");
        put(&root, "vendor/mod/c.txt", "c");

        let built = build(&root, &["vendor/**"]).expect("the inputs are built");

        assert_eq!(paths(&built), ["vendor/kept.txt"]);
        assert_eq!(
            built.said,
            [
                "gate: inputs: vendor/lib is another repository; not sent as an input",
                "gate: inputs: vendor/mod is another repository; not sent as an input",
            ]
        );
    }

    /// Runs git with `args` in `folder` as a committer of the test's own, with no hook and no signing.
    fn commit_git(folder: &Path, args: &[&str]) {
        let mut all = vec![
            "-c",
            "user.name=gate test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.no-hooks",
        ];
        all.extend_from_slice(args);
        git(folder, &all);
    }

    /// The `.gitmodules` text declaring a submodule at each of `paths`.
    fn gitmodules(paths: &[&str]) -> String {
        paths
            .iter()
            .map(|path| format!("[submodule \"{path}\"]\n\tpath = {path}\n\turl = ./shared\n"))
            .collect::<Vec<_>>()
            .concat()
    }

    /// Makes `tools/shared` in `root` an initialized submodule: a repository of its own holding a committed
    /// `tracked.txt`, declared in the root's `.gitmodules` and added to the root as a gitlink.
    fn with_submodule(root: &Path) {
        let shared = root.join("tools").join("shared");
        put(root, "tools/shared/tracked.txt", "tracked");
        git(&shared, &["init", "--quiet"]);
        git(&shared, &["add", "tracked.txt"]);
        commit_git(&shared, &["commit", "--quiet", "-m", "one"]);
        put(root, ".gitmodules", &gitmodules(&["tools/shared"]));
        commit_git(root, &["add", "tools/shared", ".gitmodules"]);
    }

    #[test]
    fn an_initialized_submodule_s_tracked_and_untracked_files_are_kept_and_its_ignored_ones_not() {
        let root = worktree("submodule", "logs/\nstate/\n");
        with_submodule(&root);
        put(&root, "tools/shared/.gitignore", "ignored.txt\n");
        put(&root, "tools/shared/untracked.txt", "untracked");
        put(&root, "tools/shared/ignored.txt", "ignored");

        let built = build(&root, &["**"]).expect("the inputs are built");

        assert_eq!(paths(&built), ["tools/shared/tracked.txt", "tools/shared/untracked.txt"]);
        // The `**` glob matched `ignored.txt` there too, which is not sent, so the folder's line stays.
        assert_eq!(
            built.said,
            [
                "gate: inputs: tools/shared is another repository; not sent as an input",
                "gate: inputs: submodule tools/shared: 2 files sent as inputs",
            ]
        );
        assert_eq!(built.marking, Marking::Write, "a later run with none removes the copies");
    }

    #[test]
    fn an_uninitialized_submodule_is_passed_over_saying_nothing() {
        let root = worktree("submodule-uninitialized", "logs/\nstate/\n");
        put(&root, ".gitmodules", &gitmodules(&["tools/shared"]));
        put(&root, "tools/shared/a.txt", "a folder with no .git");

        assert_eq!(none_said(&root, &[]).expect("the run sends no inputs"), Vec::<String>::new());
    }

    #[test]
    fn a_worktree_with_no_gitmodules_asks_git_nothing_about_submodules() {
        // Any git call fails in this folder, whose `.git` names no repository: a call would refuse the run.
        let root = scratch("submodule-no-gitmodules");
        put(&root, ".git", "gitdir: nowhere");
        put(&root, "tools/shared/.git", "gitdir: nowhere");
        put(&root, "tools/shared/a.txt", "a");

        assert_eq!(none_said(&root, &[]).expect("the run sends no inputs"), Vec::<String>::new());

        put(&root, ".gitmodules", &gitmodules(&["tools/shared"]));
        let why = none_said(&root, &[]).expect_err("a .gitmodules git cannot read keeps the run here");
        assert!(why.starts_with(".gitmodules cannot be read: "), "{why}");
    }

    #[test]
    fn a_gitmodules_git_cannot_read_keeps_the_run_here() {
        let root = worktree("submodule-unreadable", "logs/\nstate/\n");
        with_submodule(&root);
        put(&root, ".gitmodules", "[broken\n");

        let why = failure(build(&root, &["**"]));

        assert!(why.starts_with(".gitmodules cannot be read: status 128: "), "{why}");
    }

    #[test]
    fn a_submodule_git_cannot_list_keeps_the_run_here() {
        let root = worktree("submodule-unlisted", "logs/\nstate/\n");
        with_submodule(&root);
        fs::remove_dir_all(root.join("tools").join("shared").join(".git")).expect("the submodule's git folder can be removed");
        put(&root, "tools/shared/.git", "gitdir: nowhere");

        let why = none_said(&root, &[]).expect_err("a submodule git cannot list keeps the run here");

        assert!(why.starts_with("submodule tools/shared cannot be listed: "), "{why}");
    }

    #[test]
    fn a_gitmodules_listing_no_path_carries_nothing_and_says_nothing() {
        let root = worktree("submodule-no-paths", "logs/\nstate/\n");
        put(&root, ".gitmodules", "[core]\n\tbare = false\n");

        assert_eq!(none_said(&root, &[]).expect("the run sends no inputs"), Vec::<String>::new());
    }

    #[test]
    fn a_submodule_path_of_a_form_the_gate_does_not_carry_is_refused_with_its_line() {
        let root = worktree("submodule-refused", "logs/\nstate/\n");
        with_submodule(&root);
        put(
            &root,
            ".gitmodules",
            &gitmodules(&["../x", "a/.GitHub/b", "/abs", "vendor./lib", "TOOLS~1/x"]),
        );

        let said = none_said(&root, &[]).expect("the run sends no inputs");

        assert_eq!(
            said,
            [
                "gate: inputs: submodule path ../x is not one the gate carries; not sent",
                "gate: inputs: submodule path /abs is not one the gate carries; not sent",
                "gate: inputs: submodule path TOOLS~1/x is not one the gate carries; not sent",
                "gate: inputs: submodule path a/.GitHub/b is not one the gate carries; not sent",
                "gate: inputs: submodule path vendor./lib is not one the gate carries; not sent",
            ]
        );
    }

    #[test]
    fn a_submodule_path_is_carried_only_in_a_form_the_box_takes() {
        for path in ["tools/shared", "a/b/c", "a b/c"] {
            assert!(carried_form(path), "{path} is carried");
        }
        for path in [
            "../x",
            "a/../b",
            "./a",
            "a//b",
            ".git/x",
            "a/.GitHub/b",
            "/abs",
            "C:/x",
            r"a\b",
            "",
            "a/",
            "vendor./lib",
            "TOOLS~1/x",
        ] {
            assert!(!carried_form(path), "{path:?} is refused");
        }
    }

    #[test]
    fn a_gitmodules_path_with_no_gitlink_in_the_index_sends_nothing() {
        let root = worktree("submodule-no-gitlink", "logs/\nstate/\n");
        with_submodule(&root);
        commit_git(&root, &["rm", "--cached", "--quiet", "-f", "tools/shared"]);

        assert_eq!(none_said(&root, &[]).expect("the run sends no inputs"), Vec::<String>::new());
    }

    #[test]
    fn a_gitmodules_path_spelled_otherwise_than_the_index_s_gitlink_sends_nothing() {
        let root = worktree("submodule-spelled", "logs/\nstate/\n");
        with_submodule(&root);
        put(&root, ".gitmodules", &gitmodules(&["Tools/Shared"]));

        assert_eq!(none_said(&root, &[]).expect("the run sends no inputs"), Vec::<String>::new());
    }

    /// Makes `link` a junction to the folder `target`.
    #[expect(clippy::disallowed_methods, reason = "a test's junction setup takes no spawn lock")]
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    #[test]
    fn a_listed_submodule_file_under_a_junction_or_deleted_is_skipped() {
        let root = worktree("submodule-junction", "logs/\nstate/\n");
        with_submodule(&root);
        let shared = root.join("tools").join("shared");
        put(&root, "tools/shared/sub/x.txt", "x");
        git(&shared, &["add", "sub/x.txt"]);
        commit_git(&shared, &["commit", "--quiet", "-m", "two"]);
        fs::remove_dir_all(shared.join("sub")).expect("the folder can be removed");
        let elsewhere = scratch("submodule-junction-target");
        put(&elsewhere, "x.txt", "outside");
        junction(&shared.join("sub"), &elsewhere);
        put(&root, "tools/shared/untracked.txt", "untracked");
        fs::remove_file(shared.join("tracked.txt")).expect("the tracked file can be deleted");

        let built = build_as(&root, &[], &root.join("logs").join("run.log"), Caps::FULL).expect("the submodule's files are built");

        assert_eq!(paths(&built), ["tools/shared/untracked.txt"]);
        fs::remove_dir(shared.join("sub")).expect("the junction can be removed");
    }

    #[test]
    fn a_submodule_look_ends_interrupted_once_the_run_is() {
        let root = worktree("submodule-interrupted", "logs/\nstate/\n");
        with_submodule(&root);

        let interrupted = single_under(&root, &[], &root.join("logs").join("run.log"), Caps::FULL, &|| false);

        assert!(matches!(interrupted, Err(Unbuilt::Interrupted)));
    }

    #[test]
    fn a_folder_s_line_stays_while_a_globbed_file_there_is_not_sent_and_goes_once_every_one_is() {
        let root = worktree("submodule-nested-line", "logs/\nstate/\n");
        with_submodule(&root);
        put(&root, "tools/shared/.gitignore", "bin/\n");
        put(&root, "tools/shared/bin/out.dll", "built");

        let build_file = build(&root, &["tools/shared/bin/**"]).expect("the inputs are built");
        let carried_file = build(&root, &["tools/shared/tracked.txt"]).expect("the inputs are built");

        assert_eq!(
            build_file.said,
            [
                "gate: inputs: tools/shared is another repository; not sent as an input",
                "gate: inputs: submodule tools/shared: 1 file sent as inputs",
            ]
        );
        assert_eq!(carried_file.said, ["gate: inputs: submodule tools/shared: 1 file sent as inputs"]);
    }

    /// A worktree whose submodule holds a tracked and an untracked file, beside two ignored files `.tmp/**` matches.
    fn capped_worktree(name: &str) -> Scratch {
        let root = worktree(name, "logs/\nstate/\n.tmp/\n");
        with_submodule(&root);
        put(&root, "tools/shared/untracked.txt", "untracked");
        put(&root, ".tmp/a.txt", "a");
        put(&root, ".tmp/b.txt", "b");
        root
    }

    #[test]
    fn a_set_past_the_member_cap_is_blamed_on_the_globs_when_they_alone_pass_it_else_on_the_submodules() {
        let root = capped_worktree("submodule-caps");
        let log = root.join("logs").join("run.log");

        let globs = failure(build_as(&root, &[".tmp/**"], &log, Caps { members: 1, ..Caps::FULL }));
        let submodules = failure(build_as(&root, &[".tmp/**"], &log, Caps { members: 3, ..Caps::FULL }));
        let whole = build_as(&root, &[".tmp/**"], &log, Caps { members: 4, ..Caps::FULL }).expect("the inputs are built");

        assert_eq!(globs, "2 files match the input globs, more than one put carries (1)");
        assert_eq!(submodules, "4 files with the submodules' files, more than one put carries (3)");
        assert_eq!(
            paths(&whole),
            [".tmp/a.txt", ".tmp/b.txt", "tools/shared/tracked.txt", "tools/shared/untracked.txt"]
        );
    }

    #[test]
    fn a_set_past_the_manifest_cap_is_blamed_on_the_globs_when_they_alone_pass_it_else_on_the_submodules() {
        let root = capped_worktree("submodule-manifest-cap");
        let log = root.join("logs").join("run.log");
        let whole = build_as(&root, &[".tmp/**"], &log, Caps::FULL).expect("the inputs are built");
        let manifest = whole.line.len() - 1;

        let submodules = failure(build_as(&root, &[".tmp/**"], &log, Caps { manifest, ..Caps::FULL }));
        let globs = failure(build_as(&root, &[".tmp/**"], &log, Caps { manifest: 10, ..Caps::FULL }));

        assert_eq!(
            submodules,
            "the submodules' files would make the inputs manifest larger than the box takes (64 MiB)"
        );
        assert_eq!(globs, PAST_THE_CAP);
    }

    #[test]
    fn a_set_past_both_the_member_and_the_manifest_cap_is_refused_for_its_members() {
        let root = capped_worktree("submodule-both-caps");
        let log = root.join("logs").join("run.log");

        let refused = failure(build_as(&root, &[".tmp/**"], &log, Caps { members: 3, manifest: 10 }));

        assert_eq!(refused, "4 files with the submodules' files, more than one put carries (3)");
    }

    /// Why a single repo's run of `globs` in `root` under `caps`, `go_on` asked as the run asks it, is refused; a run
    /// that is not refused says so instead.
    fn refused_under(root: &Path, globs: &[&str], caps: Caps, go_on: &dyn Fn() -> bool) -> String {
        match single_under(root, globs, &root.join("logs").join("run.log"), caps, go_on) {
            Err(unbuilt) => failure(Err(unbuilt)),
            Ok(Single::Built(built)) => format!("the run sends inputs: {:?}", paths(&built)),
            Ok(Single::None(said)) => format!("the run sends no inputs: {said:?}"),
        }
    }

    #[test]
    fn a_run_with_no_globs_whose_submodule_files_pass_a_cap_is_refused_and_touches_no_hash_cache() {
        let root = capped_worktree("submodule-capped-alone");
        let caps = Caps { members: 1, ..Caps::FULL };

        let refused = refused_under(&root, &[], caps, &|| true);

        assert_eq!(refused, "2 files with the submodules' files, more than one put carries (1)");
        assert!(!cache_of(&root).exists(), "no hash cache is made");
        let held = b"a cache as an earlier run left it\n";
        fs::create_dir_all(cache_of(&root).parent().expect("the cache has a folder")).expect("the folder can be made");
        fs::write(cache_of(&root), held).expect("the cache can be written");
        assert_eq!(refused_under(&root, &[], caps, &|| true), refused);
        assert_eq!(
            fs::read(cache_of(&root)).expect("the cache is there"),
            held,
            "the cache is left as it was"
        );
    }

    #[test]
    fn a_submodule_inside_a_submodule_is_said_and_not_carried() {
        let root = worktree("submodule-nested", "logs/\nstate/\n");
        with_submodule(&root);
        let shared = root.join("tools").join("shared");
        let inner = shared.join("inner");
        put(&root, "tools/shared/inner/x.txt", "inner");
        git(&inner, &["init", "--quiet"]);
        git(&inner, &["add", "x.txt"]);
        commit_git(&inner, &["commit", "--quiet", "-m", "inner"]);
        commit_git(&shared, &["add", "inner"]);

        let built = build_as(&root, &[], &root.join("logs").join("run.log"), Caps::FULL).expect("the submodule's files are built");

        assert_eq!(paths(&built), ["tools/shared/tracked.txt"]);
        assert_eq!(
            built.said,
            [
                "gate: inputs: submodule tools/shared/inner is inside a submodule; not carried",
                "gate: inputs: submodule tools/shared: 1 file sent as inputs",
            ]
        );
    }

    #[test]
    fn a_gitmodules_that_cannot_be_read_is_told_from_one_that_is_not_there() {
        let root = scratch("gitmodules-unreadable");
        put(&root, ".gitmodules/held.txt", "a folder, not a file");

        assert_eq!(gitmodules_at(&root), Ok(false), "a folder is no .gitmodules");
        assert_eq!(gitmodules_at(&root.join("missing")), Ok(false), "nothing is there");
        // A name Windows refuses to open fails the read, but not as missing.
        let unreadable = gitmodules_at(&root.join("held<name"));
        assert!(unreadable.is_err(), "{unreadable:?}");
    }

    /// A name longer than any a Windows folder may have: reading what it names fails, but not as missing.
    fn unreadable_name() -> String {
        "l".repeat(300)
    }

    #[test]
    fn a_submodule_folder_that_cannot_be_read_keeps_the_run_here() {
        let root = worktree("submodule-folder-unreadable", "logs/\nstate/\n");
        put(&root, "tools/kept.txt", "kept");
        let path = format!("tools/{}", unreadable_name());
        put(&root, ".gitmodules", &gitmodules(&[&path]));
        let gitlink = format!("160000,{},{path}", "1".repeat(40));
        commit_git(&root, &["update-index", "--add", "--cacheinfo", &gitlink]);

        let why = none_said(&root, &[]).expect_err("a folder that cannot be read keeps the run here");

        assert!(why.starts_with(&format!("submodule {path} cannot be listed: ")), "{why}");
    }

    #[test]
    fn a_folder_above_a_listed_submodule_file_that_cannot_be_read_keeps_the_run_here() {
        let root = worktree("submodule-file-folder-unreadable", "logs/\nstate/\n");
        with_submodule(&root);
        let shared = root.join("tools").join("shared");
        put(&root, "tools/shared/empty.txt", "");
        git(&shared, &["add", "empty.txt"]);
        let listed = format!("100644,e69de29bb2d1d6434b8b29ae775ad8c2e48c5391,{}/x.txt", unreadable_name());
        git(&shared, &["update-index", "--add", "--cacheinfo", &listed]);

        let why = none_said(&root, &[]).expect_err("a folder that cannot be read keeps the run here");

        assert!(why.starts_with("submodule tools/shared cannot be listed: "), "{why}");
    }

    #[test]
    fn a_folder_s_line_spelled_in_another_case_than_the_submodule_s_path_still_goes() {
        let root = worktree("submodule-nested-case", "logs/\nstate/\n");
        with_submodule(&root);
        let tools = root.join("tools");
        fs::rename(tools.join("shared"), tools.join("moved")).expect("the folder can be moved");
        fs::rename(tools.join("moved"), tools.join("Shared")).expect("the folder can be renamed");

        let built = build(&root, &["tools/**"]).expect("the inputs are built");

        assert_eq!(paths(&built), ["tools/shared/tracked.txt"]);
        assert_eq!(built.said, ["gate: inputs: submodule tools/shared: 1 file sent as inputs"]);
    }

    #[test]
    fn a_run_with_no_globs_and_a_carried_submodule_builds_a_manifest_of_its_files() {
        let root = worktree("submodule-no-globs", "logs/\nstate/\n");
        with_submodule(&root);

        let built = build_as(&root, &[], &root.join("logs").join("run.log"), Caps::FULL).expect("the submodule's files are built");

        assert_eq!(paths(&built), ["tools/shared/tracked.txt"]);
        assert_eq!(built.said, ["gate: inputs: submodule tools/shared: 1 file sent as inputs"]);
        assert_eq!(built.marking, Marking::Write);
    }

    #[test]
    fn a_failed_check_ignore_says_git_s_error_and_its_plain_status() {
        let root = scratch("check-ignore-fails");
        put(&root, ".git", "gitdir: nowhere");
        put(&root, "a.txt", "a");

        let why = failure(build(&root, &["a.txt"]));

        assert!(why.starts_with("git check-ignore failed: status 128: fatal: "), "{why}");
        assert!(!why.contains("Some("), "{why}");
    }

    #[test]
    fn a_set_past_the_member_cap_or_the_manifest_cap_is_refused() {
        let root = worktree("caps", ".tmp/\n");
        put(&root, ".tmp/a.txt", "a");
        put(&root, ".tmp/b.txt", "b");
        let log = root.join("logs").join("run.log");

        let members = failure(build_as(&root, &[".tmp/**"], &log, Caps { members: 1, ..Caps::FULL }));
        let manifest = failure(build_as(&root, &[".tmp/**"], &log, Caps { manifest: 64, ..Caps::FULL }));

        assert_eq!(members, "2 files match the input globs, more than one put carries (1)");
        assert_eq!(manifest, PAST_THE_CAP);
        assert!(
            build_as(&root, &[".tmp/**"], &log, Caps { members: 2, ..Caps::FULL }).is_ok(),
            "a set at the cap is sent"
        );
    }

    #[test]
    fn the_run_s_log_spelled_another_way_is_still_left_out() {
        let root = worktree("log-spelled", "logs/\n");
        put(&root, "logs/run.log", "the log");
        put(&root, "logs/run.log.err", "its err");
        put(&root, "logs/kept.txt", "kept");
        let verbatim = PathBuf::from(format!(r"\\?\{}", root.join("logs").join("run.log").display()));
        let parent_hop = root.join("logs").join("..").join("logs").join("run.log");

        let by_verbatim = build_as(&root, &["logs/**"], &verbatim, Caps::FULL).expect("the inputs are built");
        let by_hop = build_as(&root, &["logs/**"], &parent_hop, Caps::FULL).expect("the inputs are built");

        assert_eq!(paths(&by_verbatim), ["logs/kept.txt"]);
        assert_eq!(paths(&by_hop), ["logs/kept.txt"]);
    }

    #[test]
    fn a_log_outside_the_worktree_is_not_below_it() {
        let root = worktree("log-outside", "logs/\n");
        put(&root, "logs/run.log", "a file of the worktree's own, not the run's log");
        let beside = scratch("log-outside2");
        put(&beside, "run.log", "the log");

        assert_eq!(
            relative(&root, &beside.join("run.log")),
            None,
            "a sibling folder sharing the root's name as a prefix"
        );
        assert_eq!(relative(&root, &root.join("logs").join("run.log")).as_deref(), Some("logs/run.log"));
        let built = build_as(&root, &["logs/**"], &beside.join("run.log"), Caps::FULL).expect("the inputs are built");
        assert_eq!(paths(&built), ["logs/run.log"], "the log is elsewhere, so the worktree's file is sent");
    }

    #[test]
    fn the_digest_is_the_sha256_of_the_manifest_line_without_its_newline() {
        let root = worktree("digest", ".tmp/\n");
        put(&root, ".tmp/a.txt", "x");

        let built = build(&root, &[".tmp/**"]).expect("the inputs are built");

        assert_eq!(built.line, built.manifest.line());
        let line = built.line.strip_suffix('\n').expect("the line ends with its newline");
        let (expected, _) = sha256_of(line.as_bytes()).expect("the line can be hashed");
        assert_eq!(built.digest, expected);
        let (with_newline, _) = sha256_of(built.line.as_bytes()).expect("the line can be hashed");
        assert_ne!(built.digest, with_newline, "the newline is not hashed");
        let x = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881";
        assert!(line.contains(&format!(r#""sha256":"{x}""#)) && line.contains(r#""size":1"#), "{line}");
    }

    /// Rewrites the inputs cache in `root` with the hash `sha256` for `path`.
    fn plant(root: &Path, path: &str, sha256: &str) {
        let cache = cache_of(root);
        let mut saved: Value = serde_json::from_str(&fs::read_to_string(&cache).expect("the cache was saved")).expect("JSON");
        saved["files"][path]["sha256"] = Value::from(sha256);
        fs::write(&cache, format!("{saved}\n")).expect("the cache can be rewritten");
    }

    #[test]
    fn the_inputs_cache_is_trusted_at_a_matching_stamp_and_not_past_it() {
        let root = worktree("cache", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        build(&root, &[".tmp/**"]).expect("the first build");
        let planted = "f".repeat(64);
        plant(&root, ".tmp/a.txt", &planted);

        let trusted = build(&root, &[".tmp/**"]).expect("the second build");
        put(&root, ".tmp/a.txt", "changed");
        let rehashed = build(&root, &[".tmp/**"]).expect("the third build");

        let sha = |built: &Built| built.manifest.get(".tmp/a.txt").expect("the file is listed").sha256.clone();
        assert_eq!(sha(&trusted), planted, "the stamp matched, so the cache's hash is taken");
        assert_ne!(sha(&rehashed), planted, "the size changed, so the file is hashed again");
    }

    #[test]
    fn a_wrong_cached_hash_found_at_the_pack_is_forgotten_so_the_next_build_heals() {
        let root = worktree("heal", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        let honest = build(&root, &[".tmp/**"]).expect("the first build");
        let truth = honest.manifest.get(".tmp/a.txt").expect("the file is listed").sha256.clone();
        plant(&root, ".tmp/a.txt", &"f".repeat(64));
        let misled = build(&root, &[".tmp/**"]).expect("the second build");

        let refused = pack_all(&root, &misled, &|| true);
        let healed = build(&root, &[".tmp/**"]).expect("the third build");

        assert!(
            matches!(&refused, Err(Unsent::Refused(why)) if why == ".tmp/a.txt changed after it was hashed"),
            "{refused:?}"
        );
        assert_eq!(healed.manifest.get(".tmp/a.txt").expect("the file is listed").sha256, truth);
    }

    #[test]
    fn a_file_changed_after_it_was_hashed_is_refused_at_the_pack() {
        let root = worktree("changed", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        let hashed = build(&root, &[".tmp/**"]).expect("the build");
        put(&root, ".tmp/a.txt", "other");

        let refused = pack_all(&root, &hashed, &|| true);
        let sent = pack_all(&root, &build(&root, &[".tmp/**"]).expect("the build again"), &|| true);

        assert!(
            matches!(&refused, Err(Unsent::Refused(why)) if why == ".tmp/a.txt changed after it was hashed"),
            "{refused:?}"
        );
        assert!(matches!(sent, Ok((compressed, _)) if compressed > 0), "{sent:?}");
    }

    #[test]
    fn packed_returns_the_sha256_of_the_archive_it_wrote() {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Seek, SeekFrom};

        let root = worktree("pack-sha256", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        put(&root, ".tmp/b.txt", "second");
        let built = build(&root, &[".tmp/**"]).expect("the build");
        fs::create_dir_all(root.join(".tmp")).expect("the .tmp folder can be made");
        let path = root.join(".tmp").join("pack.tar.zst");
        let mut archive = made_in_tmp(&root, &path, "the inputs archive").expect("the archive can be made");
        let files: Vec<&Found> = built.found.iter().collect();
        let cache = cache_of(&root);
        let packing = Pack {
            root: &root,
            files: &files,
            manifest: &built.manifest,
            cache: &cache,
            trees: &[],
        };

        let (length, sha256) = packed(&archive, &path, &packing, &|| true, true).expect("the pack passes");

        let mut written = Vec::new();
        archive.seek(SeekFrom::Start(0)).expect("the archive can be read from its start");
        archive.read_to_end(&mut written).expect("the archive can be read");
        assert_eq!(length, written.len() as u64, "the length is the file's");
        assert_eq!(sha256, Some(<[u8; 32]>::from(Sha256::digest(&written))), "the digest is the file's");
    }

    #[test]
    fn a_pack_ends_interrupted_once_the_run_is() {
        let root = worktree("pack-interrupted", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        let built = build(&root, &[".tmp/**"]).expect("the build");
        let asked = std::cell::Cell::new(0);
        let once = || {
            asked.set(asked.get() + 1);
            asked.get() == 1
        };

        let at_once = pack_all(&root, &built, &|| false);
        let while_writing = pack_all(&root, &built, &once);

        assert!(matches!(at_once, Err(Unsent::Ended(End::Interrupted))), "{at_once:?}");
        assert!(matches!(while_writing, Err(Unsent::Ended(End::Interrupted))), "{while_writing:?}");
    }

    #[test]
    fn the_archive_made_in_tmp_goes_with_its_last_handle() {
        let root = worktree("tmp-file", ".tmp/\n");
        fs::create_dir_all(root.join(".tmp")).expect("the .tmp folder can be made");
        let path = root.join(".tmp").join("made.tar.zst");

        let file = made_in_tmp(&root, &path, "the inputs archive").expect("the file is made");
        let again = made_in_tmp(&root, &path, "the inputs archive");
        let clone = file.try_clone().expect("the handle can be cloned");
        drop(file);
        let there_while_cloned = path.exists();
        drop(clone);

        assert!(matches!(again, Err(Unsent::Refused(_))), "made new, never over a file there");
        assert!(there_while_cloned, "a clone of the handle keeps it");
        assert!(!path.exists(), "the last handle closed removes it");
    }

    /// The manifest built of `a.txt` and `b.txt`, for the diff answers to name.
    fn two_files(name: &str) -> (Scratch, Built) {
        let root = worktree(name, "*.txt\n");
        put(&root, "a.txt", "a");
        put(&root, "b.txt", "b");
        let built = build(&root, &["*.txt"]).expect("the inputs are built");
        (root, built)
    }

    #[test]
    fn a_diff_answer_naming_a_path_twice_is_refused() {
        let (_root, built) = two_files("twice");

        let twice = needed(
            "gate-remote-inputs: need 2\ngate-remote-inputs: path a.txt\ngate-remote-inputs: path a.txt\n",
            &built,
        );

        assert_eq!(twice.map(|found| found.len()), Err("the box named a.txt twice".to_owned()));
    }

    #[test]
    fn a_malformed_diff_answer_is_refused() {
        let (_root, built) = two_files("malformed");
        let said = |text: &str| needed(text, &built).map(|found| found.iter().map(|file| file.path.clone()).collect::<Vec<_>>());

        assert_eq!(
            said("gate-remote-inputs: need 1\ngate-remote-inputs: path b.txt\n"),
            Ok(vec!["b.txt".to_owned()])
        );
        assert_eq!(
            said("gate-remote-inputs: path a.txt\n"),
            Err("the builder printed no 'gate-remote-inputs: need <n>' line".to_owned())
        );
        assert_eq!(said(""), Err("the builder printed no 'gate-remote-inputs: need <n>' line".to_owned()));
        assert_eq!(
            said("gate-remote-inputs: need 2\ngate-remote-inputs: path a.txt\n"),
            Err("the builder said it needs 2 file(s) and named 1".to_owned())
        );
        assert_eq!(
            said("gate-remote-inputs: need 1\ngate-remote-inputs: path c.txt\n"),
            Err("the builder named 'c.txt', which the manifest does not hold".to_owned())
        );
        assert_eq!(
            said("gate-remote-inputs: need 1\nsomething else\n"),
            Err("the builder printed 'something else'".to_owned())
        );
    }
}

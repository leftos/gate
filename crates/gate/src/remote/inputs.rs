//! A routed run's ignored inputs on the PC: the files under its repo's input globs that git reports ignored in the
//! worktree, hashed into the inputs manifest before anything is sent, each file whose stamp the inputs hash cache holds
//! taking its hash from it. The globs are relative to the worktree root, `/`-separated and matched without regard to
//! case, as output globs are, with the inputs' own exclusions: any `.git*` path, a `.lfsconfig`, a `.env`, `*.env`,
//! `.env.<name>` or `.envrc` file, the gate's own archives and stages under `.tmp` (named for the work folder's key) and
//! the run's own log and `.err` file are never sent. A match git does not report ignored is in the snapshot, and one
//! inside a folder holding a `.git` of its own is another repository's; each is skipped with a line.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_router::{CARRIED_INPUT, escaped};
use gate_state::{FallbackReason, v1};
use gate_win32::Interrupt;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_READ};

use super::channel;
use super::client::{Attempt, Called, End, RawSink, Sent, Watch};
use super::git::{Call, Git, GitBounds, GitError, git_failure};
use super::outputs::{Archive, DELETE, GENERIC_WRITE, Unbuilt, sha256_for};
use super::payload::{self, InputsStep, Location};
use super::watchdog::{self, Cut};
use crate::outputs::archive::{final_path, pack};
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{Chosen, HeldBy, MANIFEST_LIMIT, Manifest, build_manifest_with};
use crate::outputs::walk::{Found, in_context, walk};

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
}

/// What a landed inputs step does with its work folder's carried marker ([`v1::inputs_carried_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Marking {
    /// Nothing: the step carried no path a command named.
    Leave,
    /// Writes the marker: the step carried a path a command named.
    Write,
    /// Removes the marker and the inputs hash cache: the step was the empty manifest that removed the carried copies.
    Clear,
}

/// The inputs manifest `asked` names: the matched files git reports ignored, outside any folder holding a `.git` of its
/// own, each hashed once its stamp is read through its handle, or taking the hash the inputs hash cache holds at that
/// stamp; `go_on` is asked before each file and each read. The cache the build leaves is saved, unless it equals the
/// one it read. A set past `asked`'s caps is refused. [`Unbuilt::Interrupted`] once `go_on` says no or git was
/// interrupted; else why the globs, the walk, git or a file failed, or why the set is not sent.
pub(super) fn built(asked: &Asked<'_>, go_on: impl Fn() -> bool) -> Result<Built, Unbuilt> {
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
    said.extend(
        nested
            .iter()
            .map(|folder| format!("gate: inputs: {} is another repository; not sent as an input", escaped(folder))),
    );
    let ignored = ignored(asked, &found)?;
    let (kept, tracked): (Vec<Found>, Vec<Found>) = found.into_iter().partition(|file| ignored.contains(&file.path));
    said.extend(
        tracked
            .iter()
            .map(|file| format!("gate: inputs: {} is in the snapshot; not sent as an input", escaped(&file.path))),
    );
    if kept.len() > asked.caps.members {
        return Err(Unbuilt::Failed(format!(
            "{} files match the input globs, more than one put carries ({})",
            kept.len(),
            asked.caps.members
        )));
    }
    let manifest = hashed(asked, &kept, go_on)?;
    let line = manifest.line();
    if line.len() > asked.caps.manifest {
        return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned()));
    }
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    let marking = if asked_globs.iter().any(|glob| glob.starts_with(CARRIED_INPUT)) {
        Marking::Write
    } else {
        Marking::Leave
    };
    Ok(Built {
        line,
        digest,
        manifest,
        found: kept,
        said,
        marking,
    })
}

/// Why an inputs manifest past its cap is not sent.
const PAST_THE_CAP: &str = "the inputs manifest is larger than the box takes (64 MiB)";

/// The manifest of `files`, below `asked`'s root, each hashed as [`sha256_for`] hashes an output, the inputs hash cache
/// read before and saved after when it changed; else the interrupt, or why a file failed.
fn hashed(asked: &Asked<'_>, files: &[Found], go_on: impl Fn() -> bool) -> Result<Manifest, Unbuilt> {
    let previous = HashCache::load(asked.cache);
    let none = HashCache::default();
    let cache = previous.as_ref().unwrap_or(&none);
    let inside = fs::canonicalize(asked.root).map_err(|error| Unbuilt::Failed(in_context(asked.root, &error).to_string()))?;
    let started = file_time_now();
    let stopped = Cell::new(false);
    let mut next = HashCache::default();
    let built = build_manifest_with(files, asked.caps.manifest, |file| {
        let (sha256, stamp) = sha256_for(file, &inside, cache, &go_on, &stopped)?;
        next.keep(&file.path, stamp, &sha256, started);
        Ok((sha256, stamp))
    });
    if stopped.get() {
        return Err(Unbuilt::Interrupted);
    }
    let manifest = match built {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned())),
        Err(error) => return Err(Unbuilt::Failed(error.to_string())),
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
/// compared without regard to ASCII letter case: one gone since the decision, or one the exclusions drop.
fn unsent_carried<'a>(asked: &'a [String], found: &[Found]) -> Option<&'a str> {
    asked
        .iter()
        .filter_map(|glob| glob.strip_prefix(CARRIED_INPUT))
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
/// repository, which git cannot be asked about; and those folders, each the outermost such one, once each, in order.
fn outside_nested(root: &Path, found: Vec<Found>) -> (Vec<Found>, Vec<String>) {
    let mut known = HashMap::new();
    let mut nested = BTreeSet::new();
    let mut kept = Vec::new();
    for file in found {
        match nested_folder(root, &file.path, &mut known) {
            Some(folder) => {
                nested.insert(folder);
            }
            None => kept.push(file),
        }
    }
    (kept, nested.into_iter().collect())
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
fn ignored(asked: &Asked<'_>, found: &[Found]) -> Result<HashSet<String>, Unbuilt> {
    if found.is_empty() {
        return Ok(HashSet::new());
    }
    let git = Git::find(asked.root).map_err(Unbuilt::Failed)?;
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
/// Why a pair's run sends no inputs.
const PAIR: &str = "a sibling pair does not send inputs yet";
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

/// The inputs manifest of `attempt`'s run, its lines logged. A run with no input globs sends the empty manifest when
/// an earlier run's carried copies are to be removed ([`emptied`]), else none, as a pair's run with none does. A pair's
/// run with input globs, and a manifest that cannot be built or is past a cap, refuse the route `inputs` before
/// anything is sent.
pub(super) fn sent(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if call.inputs.is_empty() {
        return if call.siblings.is_empty() { emptied(attempt) } else { Ok(None) };
    }
    if !call.siblings.is_empty() {
        return Err(refused(attempt, PAIR));
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
    match built(&asked, || attempt.go_on().is_ok()) {
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

/// The empty inputs manifest of a run with no inputs whose work folder holds copies a command's carried paths put, its
/// carried marker there ([`v1::inputs_carried_path`]): the box's diff, needing nothing, deletes those copies, and
/// [`send`] removes the marker once it landed. `None` when there is no marker. A digest that cannot be made refuses the
/// route `inputs`.
fn emptied(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if !v1::inputs_carried_path(call.state_dir, call.builder.id.as_deref(), call.key).exists() {
        return Ok(None);
    }
    let manifest = Manifest::default();
    let line = manifest.line();
    match sha256_of(line.trim_end_matches('\n').as_bytes()) {
        Ok((digest, _)) => Ok(Some(Built {
            line,
            digest,
            manifest,
            found: Vec::new(),
            said: Vec::new(),
            marking: Marking::Clear,
        })),
        Err(error) => Err(refused(attempt, &error.to_string())),
    }
}

/// The log line of a run whose empty manifest removed the copies an earlier run carried.
const REMOVED: &str = "gate: inputs: removed the carried copies of an earlier run";

/// Does what the landed inputs step's `marking` says with `attempt`'s work folder's carried marker: writes it, empty,
/// its folder made; or removes it and the inputs hash cache, so the next run with no inputs sends none, and says
/// [`REMOVED`]. A marker that cannot be written, or a file that cannot be removed, is said in the log: a later run
/// then sends the empty manifest once fewer, or once more.
fn landed(attempt: &mut Attempt<'_, '_>, marking: Marking) {
    let call = attempt.call;
    let marker = v1::inputs_carried_path(call.state_dir, call.builder.id.as_deref(), call.key);
    match marking {
        Marking::Leave => {}
        Marking::Write => {
            let written = marker.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(&marker, ""));
            if let Err(error) = written {
                attempt.logged(&format!("gate: inputs: cannot write the carried marker {}: {error}", marker.display()));
            }
        }
        Marking::Clear => {
            let cache = v1::inputs_hashes_path(call.state_dir, call.builder.id.as_deref(), call.key);
            for path in [cache, marker] {
                match fs::remove_file(&path) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        attempt.logged(&format!("gate: inputs: cannot remove {}: {error}", path.display()));
                    }
                    _ => {}
                }
            }
            attempt.logged(REMOVED);
        }
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
/// file, the put of an archive of exactly those, timed in the run's split; once it landed, the carried marker is kept
/// as `built` says ([`landed`]). Whether it landed, so the run call carries its digest. A Ctrl+C ends the run. Any other
/// failure of the empty manifest that removes carried copies ([`Marking::Clear`]) is said in the log and the run goes
/// on without it, its marker kept for a later run; of any other manifest, it refuses the route `inputs`, its reason in
/// the log. An end no inputs call returns fails the run.
pub(super) fn send(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &Built) -> Result<bool, End> {
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
            landed(attempt, built.marking);
            Ok(true)
        }
        Err(Unsent::Ended(End::Interrupted)) => Err(End::Interrupted),
        Err(unsent) if built.marking == Marking::Clear => {
            let why = unsent_why(&unsent);
            attempt.logged(&format!("gate: inputs: the carried copies were not removed: {}", escaped(&why)));
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
    let manifest = hashed(&asked, &found, || attempt.go_on().is_ok())?;
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
    })
}

/// How an end no inputs or LFS call returns is said: the run call's own ends, and a pair's.
pub(super) fn unexpected(end: &End) -> &'static str {
    match end {
        End::Status(_) => "with a command's status",
        End::Killed => "killed",
        End::Lost(_) => "with its connection lost",
        End::NotRouted(_) => "not routed",
        End::Refused(_) => "refused",
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
        if built.marking != Marking::Clear {
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
    let chosen: Vec<Chosen<'_>> = packing.files.iter().map(|file| Chosen::from(*file)).collect();
    let sink = Asking {
        inner: archive,
        go_on,
        stopped: &stopped,
    };
    let result = pack(sink, packing.root, &chosen, &HeldBy::new());
    if stopped.get() {
        return Err(Unsent::Ended(End::Interrupted));
    }
    let packed = result.map_err(|error| Unsent::Refused(format!("cannot pack the inputs into {}: {error}", shown.display())))?;
    if let Some(entry) = packed
        .entries
        .iter()
        .find(|entry| packing.manifest.get(&entry.path).is_none_or(|held| held.sha256 != entry.sha256))
    {
        if let Some(mut cache) = HashCache::load(packing.cache) {
            cache.forget(&entry.path);
            cache.save(packing.cache);
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
    use std::fs;
    use std::ops::Deref;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use gate_router::CARRIED_INPUT;
    use serde_json::Value;

    use super::{Asked, Built, Caps, End, PAST_THE_CAP, Pack, Unsent, built, installed_all, made_in_tmp, needed, packed, relative};
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

    /// The inputs of `globs` in `root`, its log at `log`, built under `caps`.
    fn build_as(root: &Path, globs: &[&str], log: &Path, caps: Caps) -> Result<Built, Unbuilt> {
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
        built(&asked, || true)
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

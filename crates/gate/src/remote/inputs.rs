//! A routed run's ignored inputs on the PC: the files under its repo's input globs that git reports ignored in the
//! worktree, hashed into the inputs manifest before anything is sent, each file whose stamp the inputs hash cache holds
//! taking its hash from it. The globs are relative to the worktree root, `/`-separated and matched without regard to
//! case, as output globs are, with the inputs' own exclusions: any `.git*` path, a `.lfsconfig`, a `.env` or `*.env`
//! file, the gate's own archives and stages under `.tmp` (named for the work folder's key) and the run's own log and
//! `.err` file are never sent. A match git does not report ignored is in the snapshot, and is skipped with a line.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::{Duration, Instant};

use gate_state::{FallbackReason, v1};
use gate_win32::Interrupt;

use super::client::{self, Attempt, Called, End, RawSink, Watch};
use super::git::{Call, Git, GitError};
use super::outputs::{Unbuilt, sha256_for};
use super::payload::{self, InputsStep, Location};
use crate::boxside::payload::escaped;
use crate::outputs::archive::{final_path, pack};
use crate::outputs::cache::{HashCache, file_time_now};
use crate::outputs::glob::Globs;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{Chosen, HeldBy, MANIFEST_LIMIT, Manifest};
use crate::outputs::walk::{Found, in_context, walk};

/// The most files one put carries: the archive's member limit.
pub(super) const MEMBERS: usize = 50_000;
/// The paths inputs never carry, whatever the globs: anything under a `.git` folder, and a `.env` or `*.env` file.
/// Matched without regard to case; the gate's own files are added per run ([`exclusions`]).
const EXCLUDED: &str = r"(^|/)\.git/|\.env$";

/// What a run's inputs manifest is built from: the worktree `root`, the repo's input `globs`, the work folder's `key`,
/// which names the gate's own archives and stages under `.tmp`, the run's `log`, the inputs hash cache's file `cache`,
/// and how long the `git check-ignore` call may take, answering to the console `interrupt`.
pub(super) struct Asked<'a> {
    pub(super) root: &'a Path,
    pub(super) globs: &'a [String],
    pub(super) key: &'a str,
    pub(super) log: &'a Path,
    pub(super) cache: &'a Path,
    pub(super) limit: Duration,
    pub(super) interrupt: Option<Interrupt>,
}

/// A run's inputs manifest as built: its line, newline included, the SHA-256 of that line without its newline (the
/// run call's `InputsDigest`), the manifest it holds, the files it names as the walk found them, and the log-only lines
/// the build said: each glob that matched nothing and each match that is in the snapshot.
pub(super) struct Built {
    pub(super) line: String,
    pub(super) digest: String,
    pub(super) manifest: Manifest,
    pub(super) found: Vec<Found>,
    pub(super) said: Vec<String>,
}

/// The inputs manifest `asked` names: the matched files git reports ignored, each hashed once its stamp is read through
/// its handle, or taking the hash the inputs hash cache holds at that stamp; `go_on` is asked before each file and each
/// read. The cache the build leaves is saved, unless it equals the one it read. A set past [`MEMBERS`] files or a line
/// past [`MANIFEST_LIMIT`] bytes is refused. [`Unbuilt::Interrupted`] once `go_on` says no or git was interrupted;
/// else why the globs, the walk, git or a file failed, or why the set is not sent.
pub(super) fn built(asked: &Asked<'_>, go_on: impl Fn() -> bool) -> Result<Built, Unbuilt> {
    let excluded = exclusions(asked.root, asked.key, asked.log);
    let globs = Globs::inputs(asked.globs, &excluded).map_err(Unbuilt::Failed)?;
    let found = walk(asked.root, &globs).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    let mut said = unmatched(asked.globs, &excluded, &found)?;
    let ignored = ignored(asked, &found)?;
    let (kept, tracked): (Vec<Found>, Vec<Found>) = found.into_iter().partition(|file| ignored.contains(&file.path));
    said.extend(
        tracked
            .iter()
            .map(|file| format!("gate: inputs: {} is in the snapshot; not sent as an input", escaped(&file.path))),
    );
    if kept.len() > MEMBERS {
        return Err(Unbuilt::Failed(format!(
            "{} files match the input globs, more than one put carries ({MEMBERS})",
            kept.len()
        )));
    }
    let manifest = hashed(asked, &kept, go_on)?;
    let line = manifest.line();
    if line.len() > MANIFEST_LIMIT {
        return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned()));
    }
    let (digest, _) = sha256_of(line.trim_end_matches('\n').as_bytes()).map_err(|error| Unbuilt::Failed(error.to_string()))?;
    Ok(Built {
        line,
        digest,
        manifest,
        found: kept,
        said,
    })
}

/// Why an inputs manifest past [`MANIFEST_LIMIT`] is not sent.
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
    let built = Manifest::build_with(files, MANIFEST_LIMIT, |file| {
        let (sha256, stamp) = sha256_for(file, &inside, cache, &go_on, &stopped)?;
        next.keep(&file.path, stamp, &sha256, started);
        Ok((sha256, stamp))
    });
    if stopped.get() {
        return Err(Unbuilt::Interrupted);
    }
    let manifest = match built {
        Ok(manifest) => manifest,
        Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => return Err(Unbuilt::Failed(PAST_THE_CAP.to_owned())),
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

/// `path` as a `/`-separated path relative to `root`, compared without regard to case; `None` when it is not below it.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let spelled = |path: &Path| std::path::absolute(path).ok().map(|path| path.to_string_lossy().replace('\\', "/"));
    let (root, path) = (spelled(root)?, spelled(path)?);
    let root = root.trim_end_matches('/');
    let head = path.get(..root.len())?;
    let rest = path.get(root.len()..)?.strip_prefix('/')?;
    (head.eq_ignore_ascii_case(root) && !rest.is_empty()).then(|| rest.to_owned())
}

/// The line of each of `globs` that matches none of `found`, under the exclusions `excluded`; else why one cannot be
/// matched.
fn unmatched(globs: &[String], excluded: &str, found: &[Found]) -> Result<Vec<String>, Unbuilt> {
    let mut said = Vec::new();
    for glob in globs {
        let one = Globs::inputs(std::slice::from_ref(glob), excluded).map_err(Unbuilt::Failed)?;
        if !found.iter().any(|file| one.matches(&file.path)) {
            said.push(format!("gate: inputs: {} matched nothing", escaped(glob)));
        }
    }
    Ok(said)
}

/// The paths of `found` that `git check-ignore` reports ignored in `asked`'s worktree, the global excludes and
/// `.git/info/exclude` included and a tracked file never; else the interrupt, or why git failed.
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
        ..Call::plain(asked.root, &args, asked.limit)
    };
    let failed = |why: String| Unbuilt::Failed(format!("git check-ignore failed: {why}"));
    let ran = git.run(&call).map_err(|error| match error {
        GitError::Interrupted => Unbuilt::Interrupted,
        GitError::Failed(why) | GitError::OverCap(why) => failed(why),
    })?;
    // 1 says no path is ignored; anything but 0 and 1 is git's own failure.
    if !matches!(ran.status.code(), Some(0 | 1)) {
        return Err(failed(format!("status {:?}", ran.status.code())));
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
const MB: f64 = 1024.0 * 1024.0;
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

/// The inputs manifest of `attempt`'s run, its lines logged; `None` for a run with no input globs. A pair's run, and
/// a manifest that cannot be built or is past a cap, refuse the route `inputs` before anything is sent.
pub(super) fn sent(attempt: &mut Attempt<'_, '_>) -> Result<Option<Built>, End> {
    let call = attempt.call;
    if call.inputs.is_empty() {
        return Ok(None);
    }
    if !call.siblings.is_empty() {
        return Err(refused(attempt, PAIR));
    }
    let cache = v1::inputs_hashes_path(call.state_dir, call.key);
    let asked = Asked {
        root: Path::new(call.root),
        globs: call.inputs,
        key: call.key,
        log: call.log,
        cache: &cache,
        limit: attempt.limit,
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

/// Sends `built` to the work folder at `location` for the run of `nonce`: the diff call, then, when the box needs any
/// file, the put of an archive of exactly those, timed in the run's split. A Ctrl+C ends the run; any other failure of
/// either call refuses the route `inputs`, its reason in the log.
pub(super) fn send(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &Built) -> Result<(), End> {
    let started = Instant::now();
    let ended = sent_to(attempt, location, nonce, built);
    attempt.split.inputs = Some(started.elapsed());
    match ended {
        Ok(()) => Ok(()),
        Err(Unsent::Ended(End::Interrupted)) => Err(End::Interrupted),
        Err(Unsent::Ended(end)) => Err(refused(attempt, &ended_why(&end))),
        Err(Unsent::Refused(why)) => Err(refused(attempt, &why)),
    }
}

/// Why an inputs step was not done: a call that ended the attempt's way, or a reason of the step's own.
enum Unsent {
    Ended(End),
    Refused(String),
}

/// Why a call that ended as `end` sent no inputs.
fn ended_why(end: &End) -> String {
    match end {
        End::Failed(why) => why.clone(),
        End::Refused(reason) => format!("the inputs call ended {}", reason.as_str()),
        other => format!("the inputs call ended: {other:?}"),
    }
}

/// The diff, then the put of what it needs ([`send`]).
fn sent_to(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &Built) -> Result<(), Unsent> {
    let needed = diffed(attempt, location, nonce, built)?;
    if needed.is_empty() {
        attempt.logged(&format!("gate: inputs: {} file(s) unchanged", built.found.len()));
        return Ok(());
    }
    let bytes = needed.iter().fold(0_u64, |total, file| total.saturating_add(file.stamp.size));
    if bytes > attempt.inputs_budget {
        return Err(Unsent::Refused(format!(
            "the inputs the builder needs hold {bytes} bytes, more than one put carries ({} bytes)",
            attempt.inputs_budget
        )));
    }
    let root = Path::new(attempt.call.root);
    let archive = root.join(".tmp").join(format!("{}-{nonce}.inputs.tar.zst", attempt.call.key));
    let started = Instant::now();
    let put = packed(root, &archive, &needed, &built.manifest).and_then(|compressed| {
        put(attempt, location, nonce, built, (&archive, needed.len()))?;
        Ok(compressed)
    });
    if let Err(error) = fs::remove_file(&archive)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        attempt.logged(&format!("gate: cannot remove the inputs archive {}: {error}", archive.display()));
    }
    let compressed = put?;
    attempt.say(sent_line(needed.len(), bytes, compressed, started.elapsed()));
    Ok(())
}

/// The console line of a put that sent `count` files of `bytes` bytes, `compressed` as sent, in `took`.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn sent_line(count: usize, bytes: u64, compressed: u64, took: Duration) -> String {
    format!(
        "gate: inputs: {count} file(s), {:.1} MB ({:.1} MB compressed) in {:.1} s",
        bytes as f64 / MB,
        compressed as f64 / MB,
        took.as_secs_f64()
    )
}

/// The files of `built` the box's diff call says its work folder lacks or holds otherwise; else why not.
fn diffed<'b>(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &'b Built) -> Result<Vec<&'b Found>, Unsent> {
    let head = payload::input(&payload::inputs_word(location, nonce, InputsStep::Diff), Some(&built.line));
    let said = Path::new(attempt.call.root)
        .join(".tmp")
        .join(format!("{}-{nonce}.inputs-diff", attempt.call.key));
    let cap = NonZeroU64::new(2 * (built.line.len() as u64) + 1024 * 1024).unwrap_or(NonZeroU64::MIN);
    let make = || -> Result<RawSink, String> {
        if let Some(folder) = said.parent() {
            fs::create_dir_all(folder).map_err(|error| format!("cannot make {}: {error}", folder.display()))?;
        }
        let file = fs::File::create(&said).map_err(|error| format!("cannot write {}: {error}", said.display()))?;
        Ok(RawSink {
            file,
            shown: said.display().to_string(),
            cap,
        })
    };
    let watch = Watch::Setup(attempt.limits_for("inputs"));
    let called = attempt.inputs_call(&head, None, Some(&make), watch);
    let text = fs::read(&said).map_err(|error| format!("cannot read {}: {error}", said.display()));
    if let Err(error) = fs::remove_file(&said)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        attempt.logged(&format!("gate: cannot remove {}: {error}", said.display()));
    }
    let called = called.map_err(Unsent::Ended)?;
    judged(attempt, &called, "the inputs diff call")?;
    needed(&String::from_utf8_lossy(&text.map_err(Unsent::Refused)?), built).map_err(Unsent::Refused)
}

/// Why the inputs call `what` that ended as `called` failed: a cut, its line said, or a status other than 0, the box's
/// last error line when it said one.
fn judged(attempt: &mut Attempt<'_, '_>, called: &Called, what: &str) -> Result<(), Unsent> {
    if let Some(cut) = called.cut {
        if let Some(line) = client::watchdog_line(cut, "sending the inputs") {
            attempt.say(line);
        }
        attempt.noted(called);
        return Err(Unsent::Refused(cut.reason(what)));
    }
    if called.status != 0 {
        return Err(Unsent::Refused(match &called.last_error {
            Some(line) => line.clone(),
            None => format!("{what} failed (status {})", called.status),
        }));
    }
    Ok(())
}

/// The files of `built` the diff's standard output `said` names: `need <n>`, then `n` paths, each escaped as the box
/// escapes it and each a file of the manifest; else why the answer is not one.
fn needed<'b>(said: &str, built: &'b Built) -> Result<Vec<&'b Found>, String> {
    let mut lines = said.lines().filter(|line| !line.trim().is_empty());
    let count: usize = lines
        .next()
        .and_then(|line| line.strip_prefix(NEED))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| format!("the builder printed no '{NEED}<n>' line"))?;
    let by_escaped: HashMap<String, &Found> = built.found.iter().map(|file| (escaped(&file.path), file)).collect();
    let needed = lines
        .map(|line| {
            let path = line
                .strip_prefix(PATH)
                .ok_or_else(|| format!("the builder printed '{}'", escaped(line)))?;
            by_escaped
                .get(path)
                .copied()
                .ok_or_else(|| format!("the builder named '{}', which the manifest does not hold", escaped(path)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if needed.len() != count {
        return Err(format!("the builder said it needs {count} file(s) and named {}", needed.len()));
    }
    Ok(needed)
}

/// Packs `files` from `root` into a new `archive` in the worktree's `.tmp`, each checked to hold what `manifest` says it
/// held when it was hashed; the archive's compressed size, else why not.
fn packed(root: &Path, archive: &Path, files: &[&Found], manifest: &Manifest) -> Result<u64, Unsent> {
    let cannot = |error: std::io::Error| Unsent::Refused(format!("cannot pack the inputs into {}: {error}", archive.display()));
    if let Some(folder) = archive.parent() {
        fs::create_dir_all(folder).map_err(cannot)?;
    }
    let file = OpenOptions::new().write(true).create_new(true).open(archive).map_err(cannot)?;
    let inside = fs::canonicalize(root).map_err(cannot)?;
    if !final_path(&file).map_err(cannot)?.starts_with(&inside) {
        return Err(Unsent::Refused(format!(
            "the inputs archive {} is not inside the worktree",
            archive.display()
        )));
    }
    let chosen: Vec<Chosen<'_>> = files.iter().map(|file| Chosen::from(*file)).collect();
    let packed = pack(&file, root, &chosen, &HeldBy::new()).map_err(cannot)?;
    if let Some(entry) = packed
        .entries
        .iter()
        .find(|entry| manifest.get(&entry.path).is_none_or(|held| held.sha256 != entry.sha256))
    {
        return Err(Unsent::Refused(format!("{} changed after it was hashed", escaped(&entry.path))));
    }
    Ok(packed.compressed)
}

/// The put call of the `sent` files packed at `archive`: the request, the manifest and then the archive, watched as a
/// put; else why not, a builder that installed another count than was sent included.
fn put(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, built: &Built, sent: (&Path, usize)) -> Result<(), Unsent> {
    let (archive, count) = sent;
    let head = payload::input(&payload::inputs_word(location, nonce, InputsStep::Put), Some(&built.line));
    let watch = Watch::Put(attempt.limits_for("inputs"));
    let called = attempt.inputs_call(&head, Some(archive), None, watch).map_err(Unsent::Ended)?;
    judged(attempt, &called, "the inputs put call")?;
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
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use serde_json::Value;

    use super::{Asked, Built, built, installed_all};
    use crate::outputs::hash::sha256_of;
    use crate::remote::git::GIT_LIMIT;
    use crate::remote::outputs::Unbuilt;

    const KEY: &str = "app-1a2b3c4d";

    /// A scratch worktree for one test, emptied first, made a git repository whose `.gitignore` is `ignore`.
    fn worktree(name: &str, ignore: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-remote-inputs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
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

    fn build(root: &Path, globs: &[&str]) -> Result<Built, Unbuilt> {
        let globs: Vec<String> = globs.iter().map(|glob| (*glob).to_owned()).collect();
        let asked = Asked {
            root,
            globs: &globs,
            key: KEY,
            log: &root.join("logs").join("run.log"),
            cache: &root.join("state").join("inputs-hashes"),
            limit: GIT_LIMIT,
            interrupt: None,
        };
        built(&asked, || true)
    }

    fn paths(built: &Built) -> Vec<&str> {
        built.manifest.files().map(|(path, _)| path).collect()
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
        let root = worktree("exclusions", ".tmp/\nlogs/\n*.env\n.env\n.lfsconfig\n");
        put(&root, ".tmp/helper/tool.dll", "helper");
        put(&root, &format!(".tmp/{KEY}-0123.tar.zst"), "an outputs archive");
        put(&root, &format!(".tmp/{KEY}-0123/staged.txt"), "an outputs stage");
        put(&root, ".tmp/other-key-0123.tar.zst", "another key's");
        put(&root, "secret.env", "secret");
        put(&root, ".env", "secret");
        put(&root, ".lfsconfig", "lfs");
        put(&root, "logs/run.log", "the log");
        put(&root, "logs/run.log.err", "its err");
        put(&root, "logs/kept.txt", "kept");
        put(&root, "sub/.git/config", "a nested git folder");

        let built = build(&root, &["**"]).expect("the inputs are built");

        assert_eq!(paths(&built), [".tmp/helper/tool.dll", ".tmp/other-key-0123.tar.zst", "logs/kept.txt"]);
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

    #[test]
    fn the_inputs_cache_is_trusted_at_a_matching_stamp_and_not_past_it() {
        let root = worktree("cache", ".tmp/\nstate/\n");
        put(&root, ".tmp/a.txt", "first");
        build(&root, &[".tmp/**"]).expect("the first build");
        let cache = root.join("state").join("inputs-hashes");
        let mut saved: Value = serde_json::from_str(&fs::read_to_string(&cache).expect("the cache was saved")).expect("JSON");
        let planted = "f".repeat(64);
        saved["files"][".tmp/a.txt"]["sha256"] = Value::from(planted.as_str());
        fs::write(&cache, format!("{saved}\n")).expect("the cache can be rewritten");

        let trusted = build(&root, &[".tmp/**"]).expect("the second build");
        put(&root, ".tmp/a.txt", "changed");
        let rehashed = build(&root, &[".tmp/**"]).expect("the third build");

        let sha = |built: &Built| built.manifest.get(".tmp/a.txt").expect("the file is listed").sha256.clone();
        assert_eq!(sha(&trusted), planted, "the stamp matched, so the cache's hash is taken");
        assert_ne!(sha(&rehashed), planted, "the size changed, so the file is hashed again");
    }
}

//! The client's sync-back: after a run call that ended with the command's own status, the changes the command left on
//! the builder are fetched from `refs/gate-back/<key>` and applied to the worktree when it still matches the snapshot.
//! The builder is trusted less than this PC. A fetched commit that is not the one the builder advertised, or that was
//! not made from this snapshot, is ignored. One that touches a `.git*` path or makes a link or a submodule, one whose
//! files would write more than the apply cap into the worktree, or one the worktree changed under during the run, is
//! kept at `refs/gate-remote/<key>-back` and fails a run that had passed; one that a step before its apply could not
//! take further, as it failed or was interrupted, is kept there too, and the line that ends the run says where to see
//! its changes. A fetch whose job writes more than the fetch cap is stopped and fails a run that had passed, the
//! changes left on the builder and nothing applied.
//! The two git calls that reach the builder take the pre-banner retry and keep their lines out of the log; any other
//! git failure is the run's, status 1 and `gate: the remote run failed: ...`, with no fallback. Every name and git line
//! said here has its control characters escaped.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use super::client::{Attempt, Called, Echo, End, Watch};
use super::git::{self, Git, GitError};
use super::launch::KILL_WAIT;
use super::payload;
use super::retry::GIT_SSH_FAILED;
use super::snapshot::{self, BACK, REFS, Snapshot};
use super::watchdog::{self, Cut};
use crate::boxside::payload::escaped;

/// The patch an applied sync-back is written to in the git folder, named for the process and a nonce, and removed
/// whether or not it applied.
const PATCH: &str = "gate-remote-";
const PATCH_SUFFIX: &str = ".patch";
/// The modes of a symbolic link and of a submodule, which no entry of a back commit may take.
const LINK_MODES: [&str; 2] = ["120000", "160000"];
/// What the mode of a regular or executable file starts with.
const REGULAR_MODE: &str = "100";
/// What the name of a path's component starts with, its letter case aside, when git reads the file itself.
const GIT_NAME: &str = ".git";

/// Fetches and applies the builder's changes for `snapshot`, or ends the run as `status` when there is nothing to do,
/// the run's own when the changes were ignored, and 1 when a passing run's changes could not be applied.
pub(super) fn sync_back(attempt: &mut Attempt<'_, '_>, snapshot: &Snapshot, status: i64) -> Result<End, End> {
    attempt.go_on()?;
    let target = attempt.transport.push_target(attempt.call.host, attempt.call.repo);
    let pack = attempt.transport.upload_pack();
    let reference = format!("refs/gate-back/{}", attempt.call.key);
    let remote = Remote {
        url: &target.url,
        pack: pack.as_deref(),
        ssh: target.ssh_command.as_deref(),
        reference: &reference,
    };
    let git = Git::find(Path::new(attempt.call.root)).map_err(End::Failed)?;
    let Some(advertised) = has_back(attempt, &git, &remote)? else {
        return Ok(End::Status(status));
    };
    if let Some(stopped) = fetch(attempt, &git, &remote, status)? {
        return Ok(stopped);
    }
    let back = line(attempt, &git, &["rev-parse", "FETCH_HEAD"])?;
    if back != advertised || !made_from(attempt, &git, &back, &snapshot.commit)? {
        attempt.say(ignored_line(&back, &snapshot.commit));
        return Ok(End::Status(status));
    }
    receive(attempt, &git, snapshot, &back, status)
}

/// The back commit the builder advertises for this run, from `ls-remote` of its ref; `None` when it holds none.
fn has_back(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>) -> Result<Option<String>, End> {
    let ls_remote = Network {
        verb: "ls-remote",
        watch: Watch::Deadline(attempt.deadline_for("ls-remote", attempt.limit)),
    };
    let called = to_the_builder(attempt, git, remote, &ls_remote, &remote.args(&["ls-remote"]))?;
    Ok(advertised(&called.lines, remote.reference))
}

/// The words of the sync-back's fetch: every object it sends is checked, and the automatic housekeeping git would
/// otherwise start inside the fetch's own job — a `maintenance run --auto` or a `gc --auto`, whose lock a Ctrl+C
/// during the fetch can leave in the user's repository — is off. Only the fetch, which runs with the console
/// interrupt, needs this: the calls that hold a lock already run without it.
const FETCH_WORDS: [&str; 10] = [
    "-c",
    "fetch.fsckObjects=true",
    "-c",
    "maintenance.auto=false",
    "-c",
    "gc.auto=0",
    "fetch",
    "--progress",
    "--no-recurse-submodules",
    "--no-tags",
];

/// Fetches the back ref the builder holds into `FETCH_HEAD`, every object it sends checked, and no tag or submodule.
/// It says its progress, which a live fetch keeps sending on standard error, so only a fetch that went quiet stalls.
/// Its job may write at most the attempt's fetch cap: past it the fetch is stopped, and the run of the command that
/// ended `status` ends with the line saying the changes stay on the builder, a passing run as 1. A fetch that did not
/// exit cleanly has the parts of a pack it left in the repository's pack folder removed, however it ended. That end;
/// `None` when the fetch went through.
fn fetch(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, status: i64) -> Result<Option<End>, End> {
    let packs = pack_folder(attempt, git)?;
    let before = tmp_packs(&packs).map_err(|error| End::Failed(escaped(&format!("cannot list {}: {error}", packs.display()))))?;
    let fetch = Network {
        verb: "fetch",
        watch: Watch::Fetch(attempt.limits_for("fetch"), Some(attempt.fetch_cap)),
    };
    let args = remote.args(&FETCH_WORDS);
    let made = reached(attempt, git, remote, &fetch, &args);
    // Any end but a clean exit may leave part of a pack, which no later fetch removes: a cut, an interrupt, a git that
    // failed. The end itself stands as it would without the removal, and its lines come before the removal's.
    let clean = matches!(&made, Ok(called) if called.cut.is_none() && called.status == 0);
    let unremoved = if clean {
        Vec::new()
    } else {
        remove_new_tmp_packs(&packs, &before, KILL_WAIT, || {})
    };
    let end = made.and_then(|called| {
        if let Some(Cut::TooBig(cap)) = called.cut {
            attempt.noted(&called);
            attempt.say(stopped_line(cap, remote.reference));
            return Ok(Some(End::Status(if status == 0 { 1 } else { status })));
        }
        checked(attempt, &fetch, &args, called).map(|_| None)
    });
    for line in unremoved {
        attempt.say(line);
    }
    end
}

/// The pack folder of the worktree's repository, as `git rev-parse --git-path objects/pack` names it, asked with no
/// interrupt: a Ctrl+C meanwhile is left to the fetch that follows, which ends as the run's interrupt.
fn pack_folder(attempt: &Attempt<'_, '_>, git: &Git) -> Result<PathBuf, End> {
    let args = ["rev-parse", "--git-path", "objects/pack"];
    let printed = git
        .output(&git::Call::plain(Path::new(attempt.call.root), &args, attempt.limit))
        .map_err(escaped_end)?;
    let folder = printed
        .lines()
        .next()
        .map(str::trim_end)
        .filter(|folder| !folder.is_empty())
        .ok_or_else(|| End::Failed(format!("git {} printed nothing", args.join(" "))))?;
    Ok(Path::new(attempt.call.root).join(folder))
}

/// What a pack being written into the pack folder is named until git renames it: `tmp_pack_` and six random characters.
const TMP_PACK: &str = "tmp_pack_";
/// Windows' error for a file another process holds open, which refuses its removal.
const SHARING_VIOLATION: i32 = 32;
/// How long a removal waits before it tries again a file another process holds open.
const REMOVE_STEP: Duration = Duration::from_millis(20);

/// The names of the `tmp_pack_*` files in the pack folder `folder`; none when there is no such folder.
fn tmp_packs(folder: &Path) -> io::Result<Vec<OsString>> {
    let entries = match std::fs::read_dir(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        listed => listed?,
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with(TMP_PACK) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Removes the `tmp_pack_*` files in the pack folder `folder` that were not among `before`: the part of a pack a fetch
/// that did not end cleanly left. A file another process still holds open is tried again for up to `wait`, `held`
/// called each time before the removal waits for one; the lines saying each file that could not be removed even so. A
/// folder that cannot be listed leaves everything, unsaid.
fn remove_new_tmp_packs(folder: &Path, before: &[OsString], wait: Duration, mut held: impl FnMut()) -> Vec<String> {
    let Ok(now) = tmp_packs(folder) else {
        return Vec::new();
    };
    let deadline = Instant::now() + wait;
    now.iter()
        .filter(|name| !before.contains(name))
        .filter_map(|name| {
            let path = folder.join(name);
            removed(&path, deadline, &mut held)
                .err()
                .map(|error| format!("gate: remote could not remove {}: {error}", escaped(&path.display().to_string())))
        })
        .collect()
}

/// Removes the file at `path`, trying again every [`REMOVE_STEP`] until `deadline` while another process holds it
/// open, `held` called before each wait; a file already gone counts as removed.
fn removed(path: &Path, deadline: Instant, held: &mut impl FnMut()) -> io::Result<()> {
    loop {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) && Instant::now() < deadline => {
                held();
                thread::sleep(REMOVE_STEP);
            }
            done => return done,
        }
    }
}

/// The line said for a fetch stopped once its job wrote more than `cap` bytes, whose changes stay at `reference`.
fn stopped_line(cap: u64, reference: &str) -> String {
    format!(
        "gate: remote sync-back stopped: the fetch wrote more than {}; the run's changes stay on the builder ({reference})",
        watchdog::size(cap)
    )
}

/// One git call of the sync-back that reaches the builder: its verb, and how it is watched. `ls-remote` gets the git
/// limit; `fetch`, which may take long, ends once nothing came from it for the run's stall window, or at the run's
/// ceiling.
struct Network {
    verb: &'static str,
    watch: Watch,
}

/// git with `args`, the call `network` that reaches the builder: retried once when it failed before the banner, its
/// lines kept out of the log, and killed as its watch says or at a line longer than the cap. A failure is the run's,
/// or its interrupt when the console interrupt was raised during the call.
fn to_the_builder(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, network: &Network, args: &[String]) -> Result<Called, End> {
    let called = reached(attempt, git, remote, network, args)?;
    checked(attempt, network, args, called)
}

/// git with `args`, the call `network` that reaches the builder, made as [`to_the_builder`] makes it: the call that
/// counts, however it ended.
fn reached(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, network: &Network, args: &[String]) -> Result<Called, End> {
    let (called, _) = attempt.with_retry(GIT_SSH_FAILED, |attempt| {
        attempt.git(git, args, remote.ssh, Echo::Unlogged, network.watch)
    })?;
    Ok(called)
}

/// The call `network` with `args` that [`reached`] made, as [`to_the_builder`] ends it: a cut or a failure is the run's,
/// or its interrupt when the console interrupt was raised during the call.
fn checked(attempt: &mut Attempt<'_, '_>, network: &Network, args: &[String], called: Called) -> Result<Called, End> {
    if let Some(cut) = called.cut {
        attempt.noted(&called);
        return Err(End::Failed(escaped(&cut.reason(&format!("git {}", network.verb)))));
    }
    if called.status != 0 {
        attempt.go_on()?;
        return Err(git_failure(attempt.call.root, args, &called));
    }
    Ok(called)
}

/// Whether `back` was made from `snapshot`, as its `rev-list --parents` line says.
fn made_from(attempt: &Attempt<'_, '_>, git: &Git, back: &str, snapshot: &str) -> Result<bool, End> {
    let parents = line(attempt, git, &["rev-list", "--parents", "-n", "1", back])?;
    Ok(from_snapshot(&parents, snapshot))
}

/// Applies the back commit `back`, or keeps it when it touches what a sync-back never writes, its files would write
/// more than the apply cap into the worktree, or the worktree no longer matches the snapshot. A step that fails or is
/// interrupted before the patch applied keeps it as well, so its changes are not lost; a patch that applied but cannot
/// be removed then fails the run with nothing kept.
fn receive(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str, status: i64) -> Result<End, End> {
    let commit = snapshot.commit.as_str();
    let refused = refusal(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    if let Some(said) = refused {
        keep(attempt, git, back).map_err(End::Failed)?;
        attempt.say(said);
        return Ok(End::Status(if status == 0 { 1 } else { status }));
    }
    let (names, patch) = apply(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    remove(&patch)?;
    attempt.say(synced_line(&names));
    Ok(End::Status(status))
}

/// Why `back` is kept rather than applied: the line said for a back commit that touches what a sync-back never writes,
/// for one whose files would write more than the apply cap into the worktree, or for one the worktree changed under
/// during the run; `None` when it may be applied.
fn refusal(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<Option<String>, End> {
    let listing = git_call(attempt, git, &["diff-tree", "-r", "-z", "--raw", &snapshot.commit, back])?;
    if let Some(path) = forbidden(&listing) {
        return Ok(Some(touched_line(&path, &snapshot.commit, back)));
    }
    let size = written_size(attempt, git, &listing)?;
    if let Some(line) = written_line(size, attempt.apply_cap, &snapshot.commit, back) {
        return Ok(Some(line));
    }
    Ok(changed(attempt, git, snapshot)?.then(|| not_applied_line(&snapshot.commit, back)))
}

/// The bytes the files the back commit's `diff-tree -r -z --raw` `listing` writes total, each path counted: the sizes
/// of their blobs, asked of one `git cat-file --batch-check=%(objectsize)` fed their ids. An answer that is not a size
/// for each of them is the run's failure.
fn written_size(attempt: &Attempt<'_, '_>, git: &Git, listing: &str) -> Result<u64, End> {
    let blobs = written_blobs(listing);
    if blobs.is_empty() {
        return Ok(0);
    }
    let ids: String = blobs.iter().flat_map(|id| [*id, "\n"]).collect();
    let args = ["cat-file", "--batch-check=%(objectsize)"];
    let call = git::Call {
        interrupt: attempt.call.interrupt,
        stdin: Some(ids.as_bytes()),
        ..git::Call::plain(Path::new(attempt.call.root), &args, attempt.limit)
    };
    let answer = git.output(&call).map_err(escaped_end)?;
    total(&answer, &blobs).map_err(|why| escaped_end(GitError::Failed(format!("git {} failed in {}: {why}", args.join(" "), attempt.call.root))))
}

/// The blob ids of the files a back commit's `diff-tree -r -z --raw` `listing` writes, one per path in the listing's
/// order: each path it adds or changes to a regular or executable file, so a blob two paths share is written, and
/// counted, twice. A deletion, whose new id is all zeros, writes nothing.
fn written_blobs(listing: &str) -> Vec<&str> {
    entries(listing)
        .into_iter()
        .filter(|entry| entry.new_mode.starts_with(REGULAR_MODE) && !entry.new_id.bytes().all(|digit| digit == b'0'))
        .map(|entry| entry.new_id)
        .collect()
}

/// One entry of a `diff-tree -r -z --raw` listing: the mode and the blob id it gives its path, each empty when its
/// header lacks them, and the path it names, or a rename's or a copy's source and destination.
struct Entry<'a> {
    new_mode: &'a str,
    new_id: &'a str,
    paths: Vec<&'a str>,
}

/// The entries of a `diff-tree -r -z --raw` `listing`, in its order: each header field, `:` and its words, is followed
/// by as many path fields as its change names, which are read as paths whatever they start with.
fn entries(listing: &str) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    let mut fields = listing.split('\0');
    while let Some(field) = fields.next() {
        let Some(meta) = field.strip_prefix(':') else {
            continue;
        };
        let words: Vec<&str> = meta.split(' ').collect();
        // A rename or a copy names its source and its destination; a plain change names one path.
        let count = if words.get(4).is_some_and(|change| change.starts_with(['R', 'C'])) {
            2
        } else {
            1
        };
        entries.push(Entry {
            new_mode: words.get(1).copied().unwrap_or_default(),
            new_id: words.get(3).copied().unwrap_or_default(),
            paths: fields.by_ref().take(count).collect(),
        });
    }
    entries
}

/// The bytes the blobs `blobs` total, from `cat-file --batch-check=%(objectsize)`'s `answer`, one line for each in
/// their order, summed up to `u64::MAX` at most; else why the answer cannot be read: a line that is not a size, as
/// `<id> missing` is not, or a line count other than the blobs'.
fn total(answer: &str, blobs: &[&str]) -> Result<u64, String> {
    let lines: Vec<&str> = answer.lines().collect();
    if lines.len() != blobs.len() {
        return Err(format!("it answered {} line(s) for {} object(s)", lines.len(), blobs.len()));
    }
    lines.iter().zip(blobs).try_fold(0_u64, |sum, (line, id)| {
        line.parse::<u64>()
            .map(|size| sum.saturating_add(size))
            .map_err(|_| format!("it answered '{line}' for {id}"))
    })
}

/// The line said for a back commit whose files would write `size` bytes into the worktree, more than the `cap` a
/// sync-back writes; `None` when they write no more than it.
fn written_line(size: u64, cap: u64, snapshot: &str, back: &str) -> Option<String> {
    (size > cap).then(|| {
        format!(
            "gate: remote changes not applied: they would write {} into the worktree, more than {}; {}",
            watchdog::size(size),
            watchdog::size(cap),
            see_diff(snapshot, back)
        )
    })
}

/// How the run ends once `back`, which a step before its apply could not take past `failed`, is kept; what
/// [`after_keep`] decides, and its line said.
fn kept_after(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &str, back: &str, failed: End) -> End {
    let kept = keep(attempt, git, back);
    let (end, said) = after_keep(failed, kept, &see_diff(snapshot, back));
    if let Some(line) = said {
        attempt.say(line);
    }
    end
}

/// How a run that a step before the apply ended at `failed` ends once the back commit's keep came out as `kept`, and
/// the line it says then: a failure's reason says the changes are kept and where to `see` them, or that they could not
/// be kept and why; an interrupt whose changes are kept stays an interrupt and says so, and one whose changes could
/// not be kept is a failure, since they are lost. No reason is dropped: any other end stands, and a keep that failed
/// beside it is said.
fn after_keep(failed: End, kept: Result<(), String>, see: &str) -> (End, Option<String>) {
    match (failed, kept) {
        (End::Failed(why), Ok(())) => (End::Failed(format!("{why}; the changes are kept: {see}")), None),
        (End::Failed(why), Err(not_kept)) => (End::Failed(format!("{why}; the changes could not be kept either: {not_kept}")), None),
        (End::Interrupted, Ok(())) => (
            End::Interrupted,
            Some(format!("gate: the run was interrupted; the changes are kept: {see}")),
        ),
        (End::Interrupted, Err(not_kept)) => (
            End::Failed(format!("the run was interrupted, and the changes could not be kept: {not_kept}")),
            None,
        ),
        (other, Ok(())) => (other, None),
        (other, Err(not_kept)) => (other, Some(format!("gate: the changes could not be kept: {not_kept}"))),
    }
}

/// `see git diff <snapshot> <back>`, the two commits' short shas: where every line that keeps a back commit points.
fn see_diff(snapshot: &str, back: &str) -> String {
    format!("see git diff {} {}", short(snapshot), short(back))
}

/// Whether the worktree no longer matches the snapshot: the tree `git add -A` would write for it now is not the
/// snapshot's own.
fn changed(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot) -> Result<bool, End> {
    let now = snapshot::worktree(Path::new(attempt.call.root), attempt.limit, attempt.call.interrupt).map_err(escaped_end)?;
    let then = line(attempt, git, &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)])?;
    Ok(now != then)
}

/// Keeps `back` at the worktree's `refs/gate-remote/<key>-back`, for the changes a run could not apply; else why not,
/// escaped.
fn keep(attempt: &Attempt<'_, '_>, git: &Git, back: &str) -> Result<(), String> {
    let name = format!("{REFS}{}{BACK}", attempt.call.key);
    let args = ["update-ref", name.as_str(), back];
    let call = git::Call::locked(Path::new(attempt.call.root), &args, attempt.limit);
    git.output(&call).map(drop).map_err(|error| escaped(&String::from(error)))
}

/// Writes the back commit's changes as a patch into the git folder and applies it to the worktree; the names of the
/// files it changed, and the patch, which the caller removes. A patch that did not apply is removed here, and the
/// apply's failure is the one the run ends with.
fn apply(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<(Vec<String>, PathBuf), End> {
    let names = names(attempt, git, snapshot, back)?;
    let patch = patch_file(attempt, git)?;
    let shown = patch.display().to_string();
    let output = format!("--output={shown}");
    let diff = ["diff-tree", "-p", "--binary", "--full-index", "-r", &output, &snapshot.commit, back];
    let made = git_call(attempt, git, &diff);
    // NTFS protection refuses a path that names `.git` by its 8.3 short name or an alternate stream, whatever the
    // repository's own setting.
    let apply = ["-c", "core.protectNTFS=true", "apply", "--whitespace=nowarn", "--", &shown];
    match made.and_then(|_| locked_call(attempt, git, &apply)) {
        Ok(_) => Ok((names, patch)),
        Err(failed) => {
            // The apply's failure is the run's; a patch that cannot be removed after it is the lesser problem.
            drop(remove(&patch));
            Err(failed)
        }
    }
}

/// The names of the files the back commit `back` changes, against the snapshot, as git's plumbing lists them.
fn names(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &str) -> Result<Vec<String>, End> {
    let listed = git_call(attempt, git, &["diff-tree", "-r", "-z", "--name-only", &snapshot.commit, back])?;
    Ok(listed.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

/// A new, empty file in the git folder for this run's patch, named for the process and a nonce so no other process
/// can name it beforehand; one already there fails the run.
fn patch_file(attempt: &Attempt<'_, '_>, git: &Git) -> Result<PathBuf, End> {
    let name = format!("{PATCH}{}-{}{PATCH_SUFFIX}", std::process::id(), payload::job_nonce());
    let printed = line(attempt, git, &["rev-parse", "--git-path", &name])?;
    let path = Path::new(attempt.call.root).join(printed);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| End::Failed(format!("cannot make the patch file {}: {error}", path.display())))?;
    Ok(path)
}

/// Removes the patch; a patch that cannot be removed fails the run, as PowerShell's `Remove-Item` would.
fn remove(patch: &Path) -> Result<(), End> {
    match std::fs::remove_file(patch) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(End::Failed(format!("cannot remove {}: {error}", patch.display()))),
        _ => Ok(()),
    }
}

/// Runs git with `args` in the worktree and returns what it wrote on standard output; a failure is the run's.
fn git_call(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    let call = git::Call {
        interrupt: attempt.call.interrupt,
        ..git::Call::plain(Path::new(attempt.call.root), args, attempt.limit)
    };
    git.output(&call).map_err(escaped_end)
}

/// Runs git with `args` in the worktree as [`git_call`] does, but as a [`git::Call::locked`] call, which no Ctrl+C
/// ends while it holds a lock in the git folder.
fn locked_call(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    let call = git::Call::locked(Path::new(attempt.call.root), args, attempt.limit);
    git.output(&call).map_err(escaped_end)
}

/// A git failure as the attempt's end, its text escaped: an interrupt is the run's interrupt, any other a failure.
fn escaped_end(error: GitError) -> End {
    match error {
        GitError::Interrupted => End::Interrupted,
        GitError::Failed(why) | GitError::OverCap(why) => End::Failed(escaped(&why)),
    }
}

/// The first line of what git with `args` wrote on standard output: what every call that asks git for one value reads.
fn line(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str]) -> Result<String, End> {
    git_call(attempt, git, args)?
        .lines()
        .next()
        .map(|line| line.trim_end().to_owned())
        .ok_or_else(|| End::Failed(format!("git {} printed nothing", args.join(" "))))
}

/// The git call of the sync-back that failed, as the attempt's own failure: no fallback, the run's status 1.
fn git_failure(root: &str, args: &[String], called: &Called) -> End {
    End::Failed(escaped(&format!(
        "git {} failed in {root} (status {}): {}",
        args.join(" "),
        called.status,
        git::failure_text(&called.lines)
    )))
}

/// The hash `ls-remote` printed among its `lines` for exactly `reference`: a ref that only ends in it is another's.
fn advertised(lines: &[String], reference: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        let (hash, name) = line.split_once('\t')?;
        (name == reference).then(|| hash.to_owned())
    })
}

/// The first path the back commit's `diff-tree -r -z --raw` listing names that a sync-back never writes: one with a
/// component whose name starts with `.git`, its letter case aside, or one the back commit makes a symbolic link or a
/// submodule. `None` when every path may be written.
fn forbidden(listing: &str) -> Option<String> {
    entries(listing).into_iter().find_map(|entry| {
        let link = LINK_MODES.contains(&entry.new_mode);
        entry.paths.into_iter().find(|path| link || is_git_s(path)).map(str::to_owned)
    })
}

/// Whether `path` has a component whose name starts with `.git`, its letter case aside.
fn is_git_s(path: &str) -> bool {
    path.split('/')
        .any(|part| part.get(..GIT_NAME.len()).is_some_and(|start| start.eq_ignore_ascii_case(GIT_NAME)))
}

/// Whether the back commit whose `rev-list --parents -n 1` line is `parents` was made from `snapshot`: one parent, and
/// it is the snapshot itself.
pub(super) fn from_snapshot(parents: &str, snapshot: &str) -> bool {
    let line = parents.lines().next().unwrap_or_default();
    let mut words = line.split(' ').skip(1);
    matches!((words.next(), words.next()), (Some(parent), None) if parent == snapshot)
}

/// The line said for a back commit not made from this snapshot, or not the one the builder advertised, which is left
/// where it is.
pub(super) fn ignored_line(back: &str, snapshot: &str) -> String {
    format!(
        "gate: ignored stale remote changes {}: not made from snapshot {}",
        short(back),
        short(snapshot)
    )
}

/// The line said for a back commit the worktree changed under, named so the changes can be read with `git diff`.
pub(super) fn not_applied_line(snapshot: &str, back: &str) -> String {
    format!(
        "gate: remote changes not applied: the worktree changed during the run; {}",
        see_diff(snapshot, back)
    )
}

/// The line said for a back commit that touches `path`, which a sync-back never writes.
fn touched_line(path: &str, snapshot: &str, back: &str) -> String {
    format!(
        "gate: remote changes not applied: they touch {}, which a sync-back never writes; {}",
        escaped(path),
        see_diff(snapshot, back)
    )
}

/// The line said for the changes applied to the worktree, saying `0 file(s)` and nothing after the colon when the back
/// commit changed none, as PowerShell joins no names.
pub(super) fn synced_line(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().map(|name| escaped(name)).collect();
    format!("gate: synced back {} file(s): {}", names.len(), shown.join(", "))
}

/// The first eight hex digits of a commit sha, or the whole sha when it is shorter.
fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Where the sync-back's two calls that reach the builder go: the push's URL and ssh, the fetch's own pack command,
/// and the ref the back commit lives at.
struct Remote<'a> {
    url: &'a str,
    pack: Option<&'a str>,
    ssh: Option<&'a str>,
    reference: &'a str,
}

impl Remote<'_> {
    /// The words of a call: its `words`, the pack command when there is one, then `--`, the URL and the ref.
    fn args(&self, words: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        args.extend(self.pack.map(|pack| format!("--upload-pack={pack}")));
        args.extend(["--".to_owned(), self.url.to_owned(), self.reference.to_owned()]);
        args
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose interrupt, transport or git cannot be had must fail loudly")]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gate_state::StateRoot;
    use gate_win32::Interrupt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    use super::{
        FETCH_WORDS, Remote, advertised, after_keep, entries, fetch, forbidden, from_snapshot, has_back, ignored_line, not_applied_line,
        remove_new_tmp_packs, see_diff, synced_line, tmp_packs, total, touched_line, written_blobs, written_line,
    };
    use crate::boxside::payload::escaped;
    use crate::remote::RemoteCall;
    use crate::remote::client::{Attempt, End};
    use crate::remote::git::{self, Git};
    use crate::remote::transport::Local;

    const BACK: &str = "8e8777a0e71c530018dc15bdebd7288f4f432aa4";
    const SNAPSHOT: &str = "b8a4812193314ab4b2404479b93f2338078c69eb";
    /// Where the lines of a kept back commit point, for [`SNAPSHOT`] and [`BACK`].
    const SEE: &str = "see git diff b8a48121 8e8777a0";

    #[test]
    fn a_failure_whose_changes_are_kept_says_where_to_see_them() {
        assert_eq!(see_diff(SNAPSHOT, BACK), SEE);
        let (end, said) = after_keep(End::Failed("git apply failed".to_owned()), Ok(()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "git apply failed; the changes are kept: see git diff b8a48121 8e8777a0"),
            "{end:?}"
        );
        assert_eq!(said, None, "the failure's own line says it");
    }

    #[test]
    fn a_failure_whose_changes_could_not_be_kept_says_both_reasons() {
        let (end, said) = after_keep(End::Failed("git apply failed".to_owned()), Err("git update-ref failed".to_owned()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "git apply failed; the changes could not be kept either: git update-ref failed"),
            "{end:?}"
        );
        assert_eq!(said, None);
    }

    #[test]
    fn an_interrupt_whose_changes_are_kept_stays_an_interrupt_and_says_where_they_are() {
        let (end, said) = after_keep(End::Interrupted, Ok(()), SEE);
        assert!(matches!(end, End::Interrupted), "{end:?}");
        assert_eq!(
            said.as_deref(),
            Some("gate: the run was interrupted; the changes are kept: see git diff b8a48121 8e8777a0")
        );
    }

    #[test]
    fn an_interrupt_whose_changes_could_not_be_kept_fails_with_both() {
        let (end, said) = after_keep(End::Interrupted, Err("git update-ref failed".to_owned()), SEE);
        assert!(
            matches!(&end, End::Failed(why) if why == "the run was interrupted, and the changes could not be kept: git update-ref failed"),
            "{end:?}"
        );
        assert_eq!(said, None);
    }

    #[test]
    fn any_other_end_stands_and_a_failed_keep_beside_it_is_said() {
        let (end, said) = after_keep(End::Status(3), Err("git update-ref failed".to_owned()), SEE);
        assert!(matches!(end, End::Status(3)), "{end:?}");
        assert_eq!(said.as_deref(), Some("gate: the changes could not be kept: git update-ref failed"));
        let (end, said) = after_keep(End::Status(3), Ok(()), SEE);
        assert!(matches!(end, End::Status(3)), "{end:?}");
        assert_eq!(said, None);
    }

    /// git's call to the builder fails while the console interrupt is raised, as a Ctrl+C ends the git it reaches: the
    /// run is an interrupt, not the client's failure. The worktree is a repository of the test's own, so the fetch finds
    /// its pack folder and git's fetch itself runs.
    #[test]
    fn a_ctrl_c_during_a_sync_back_call_to_the_builder_is_an_interrupt() {
        let folder = std::env::temp_dir().join(format!("gate-sync-back-interrupt-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("the test's folder can be made");
        let root = folder.to_string_lossy().into_owned();
        let log = folder.join("out.log");
        let state = StateRoot::new(r"C:\gate-sync-back-tests", None);
        let interrupt = Interrupt::install().expect("the console handler is installed");
        interrupt.raise();
        let call = RemoteCall {
            host: "builder",
            key: "app-1a2b3c4d",
            root: &root,
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            log: &log,
            timeout: 60,
            stall: 60,
            tail: 0,
            no_markers: false,
            interrupt: Some(interrupt),
            state: &state,
            state_dir: Path::new(r"C:\gate-sync-back-tests"),
        };
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(&folder).expect("git is found");
        git.output(&git::Call::plain(&folder, &["init", "--quiet"], git::GIT_LIMIT))
            .expect("the test's folder is made a repository");
        let remote = Remote {
            url: r"C:\gate-sync-back-tests\no-such.git",
            pack: None,
            ssh: None,
            reference: "refs/gate-back/app-1a2b3c4d",
        };

        let listed = has_back(&mut attempt, &git, &remote);
        std::fs::remove_file(&log).expect("the ls-remote's call opened the log");
        let fetched = fetch(&mut attempt, &git, &remote, 0);

        assert!(matches!(listed, Err(End::Interrupted)), "the ls-remote: {listed:?}");
        assert!(matches!(fetched, Err(End::Interrupted)), "the fetch: {fetched:?}");
        assert!(log.exists(), "the fetch's own git call was started, which opens the log");
        std::fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    /// A pack folder holding `tmp_pack_*` files from before a fetch, and after it new ones, one read-only as index-pack
    /// makes them, a finished pack and a new `tmp_idx_*`: only the new `tmp_pack_*` files are removed.
    #[test]
    fn only_the_tmp_packs_new_since_the_listing_are_removed() {
        let folder = std::env::temp_dir().join(format!("gate-sync-back-tmp-packs-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the pack folder can be made");
        let make = |name: &str| std::fs::write(folder.join(name), b"pack").expect("a file can be made");
        make("tmp_pack_Earlr1");
        make("tmp_pack_Earlr2");
        let before = tmp_packs(&folder).expect("the folder can be listed");
        for name in ["tmp_pack_Newnw1", "tmp_pack_Newnw2", "pack-0123.pack", "tmp_idx_Newnw1"] {
            make(name);
        }
        let read_only = folder.join("tmp_pack_Newnw2");
        let mut permissions = std::fs::metadata(&read_only).expect("the file is there").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&read_only, permissions).expect("the file is made read-only");

        let unremoved = remove_new_tmp_packs(&folder, &before, HANG_GUARD, || {});

        assert_eq!(unremoved, Vec::<String>::new(), "every new tmp_pack_* was removed");
        let mut left: Vec<String> = std::fs::read_dir(&folder)
            .expect("the folder can be listed")
            .map(|entry| entry.expect("an entry can be read").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["pack-0123.pack", "tmp_idx_Newnw1", "tmp_pack_Earlr1", "tmp_pack_Earlr2"]);
        std::fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    /// The longest a removal test lets a held file be waited for: a guard against a hang, never a timing bound.
    const HANG_GUARD: Duration = Duration::from_secs(600);

    /// An empty pack folder of the test `name`'s own, holding the new file `tmp_pack_Heldh1` opened without delete
    /// sharing, as a live index-pack holds its pack: the folder, the file's path, and the handle.
    fn held_tmp_pack(name: &str) -> (PathBuf, PathBuf, File) {
        let folder = std::env::temp_dir().join(format!("gate-sync-back-{name}-{}", std::process::id()));
        if folder.exists() {
            std::fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        std::fs::create_dir_all(&folder).expect("the pack folder can be made");
        let path = folder.join("tmp_pack_Heldh1");
        std::fs::write(&path, b"pack").expect("the file can be made");
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .expect("the file can be held open");
        (folder, path, handle)
    }

    /// A new `tmp_pack_*` held open when the removal first tries it is let go at its first wait: it is removed then.
    #[test]
    fn a_tmp_pack_let_go_while_the_removal_waits_is_removed() {
        let (folder, path, handle) = held_tmp_pack("let-go");
        let mut held = Some(handle);
        let mut waits = 0;

        let unremoved = remove_new_tmp_packs(&folder, &[], HANG_GUARD, || {
            waits += 1;
            held.take();
        });

        assert_eq!(unremoved, Vec::<String>::new(), "the file let go was removed");
        assert!(!path.exists(), "the file is gone");
        assert!(waits >= 1, "the removal waited for the held file");
        std::fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    /// A new `tmp_pack_*` held open past the removal's wait stays, and the line saying so is given.
    #[test]
    fn a_tmp_pack_held_past_the_wait_stays_and_is_said() {
        let (folder, path, handle) = held_tmp_pack("held");

        let unremoved = remove_new_tmp_packs(&folder, &[], Duration::from_millis(50), || {});

        assert!(path.exists(), "the held file stays");
        let start = format!("gate: remote could not remove {}: ", escaped(&path.display().to_string()));
        assert_eq!(unremoved.len(), 1, "{unremoved:?}");
        assert!(unremoved[0].starts_with(&start), "{unremoved:?}");
        assert!(unremoved[0].ends_with("(os error 32)"), "{unremoved:?}");
        drop(handle);
        std::fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    #[test]
    fn a_back_commit_of_this_snapshot_alone_is_one_parent_and_it() {
        assert!(from_snapshot(&format!("{BACK} {SNAPSHOT}"), SNAPSHOT), "one parent, the snapshot");
        assert!(
            !from_snapshot(&format!("{BACK} 6a1c2f3d4e5f60718293a4b5c6d7e8f90a1b2c3d"), SNAPSHOT),
            "another parent"
        );
        assert!(!from_snapshot(BACK, SNAPSHOT), "no parent at all");
        assert!(!from_snapshot("", SNAPSHOT), "no line");
        assert!(
            !from_snapshot(&format!("{BACK} {SNAPSHOT} 6a1c2f3d4e5f60718293a4b5c6d7e8f90a1b2c3d"), SNAPSHOT),
            "two parents"
        );
    }

    #[test]
    fn only_the_ref_named_whole_is_the_advertised_back_commit() {
        let lines = |texts: &[&str]| texts.iter().map(|text| (*text).to_owned()).collect::<Vec<_>>();
        let reference = "refs/gate-back/app-1a2b3c4d";
        assert_eq!(advertised(&lines(&[&format!("{BACK}\t{reference}")]), reference), Some(BACK.to_owned()));
        assert_eq!(
            advertised(
                &lines(&[&format!("{SNAPSHOT}\trefs/gate-back/x/{reference}"), &format!("{BACK}\t{reference}")]),
                reference
            ),
            Some(BACK.to_owned()),
            "a ref that ends in the name is another's"
        );
        assert_eq!(
            advertised(&lines(&[&format!("{SNAPSHOT}\trefs/gate-back/x/{reference}")]), reference),
            None
        );
        assert_eq!(advertised(&lines(&["warning: something", ""]), reference), None, "no ref line");
    }

    /// One entry of `diff-tree -r -z --raw`: its modes, its change and its paths, each field ended by NUL.
    fn entry(old: &str, new: &str, change: &str, paths: &[&str]) -> String {
        let mut text = format!(":{old} {new} {SNAPSHOT} {BACK} {change}\0");
        for path in paths {
            text.push_str(path);
            text.push('\0');
        }
        text
    }

    #[test]
    fn a_back_commit_may_not_touch_git_s_own_files_or_make_a_link_or_a_submodule() {
        let plain = entry("100644", "100644", "M", &["src/a.txt"]);
        let added = entry("000000", "100755", "A", &["run.sh"]);
        assert_eq!(forbidden(&format!("{plain}{added}")), None, "files git does not read itself");
        assert_eq!(forbidden(""), None, "no change");
        for (listing, path) in [
            (entry("100644", "100644", "M", &[".gitignore"]), ".gitignore"),
            (entry("000000", "100644", "A", &["sub/.GitAttributes"]), "sub/.GitAttributes"),
            (entry("000000", "100644", "A", &[".gitmodules"]), ".gitmodules"),
            (
                entry("000000", "100644", "A", &["deep/.git/hooks/pre-commit"]),
                "deep/.git/hooks/pre-commit",
            ),
            (entry("000000", "120000", "A", &["link"]), "link"),
            (entry("100644", "120000", "T", &["was-a-file"]), "was-a-file"),
            (entry("000000", "160000", "A", &["vendor/lib"]), "vendor/lib"),
            (entry("100644", "100644", "R100", &["a.txt", ".gitignore"]), ".gitignore"),
        ] {
            assert_eq!(forbidden(&format!("{plain}{listing}")), Some(path.to_owned()), "{listing:?}");
        }
        let deleted_link = entry("120000", "000000", "D", &["old-link"]);
        assert_eq!(forbidden(&deleted_link), None, "a link the back commit removes");
        let first = format!(
            "{}{}",
            entry("000000", "120000", "A", &["one"]),
            entry("000000", "100644", "A", &[".gitignore"])
        );
        assert_eq!(forbidden(&first), Some("one".to_owned()), "the first path it names");
    }

    /// One entry of `diff-tree -r -z --raw` whose new blob is `new_id`: its modes, its change and its paths.
    fn written_entry(old: &str, new: &str, new_id: &str, change: &str, paths: &[&str]) -> String {
        let mut text = format!(":{old} {new} {SNAPSHOT} {new_id} {change}\0");
        for path in paths {
            text.push_str(path);
            text.push('\0');
        }
        text
    }

    #[test]
    fn a_back_commit_counts_each_path_it_writes_and_nothing_it_deletes() {
        let shared = "1111111111111111111111111111111111111111";
        let other = "2222222222222222222222222222222222222222";
        let zeros = "0000000000000000000000000000000000000000";
        let listing = [
            written_entry("000000", "100644", shared, "A", &["a.bin"]),
            written_entry("000000", "100755", shared, "A", &["run.sh"]),
            written_entry("100644", "100644", other, "M", &["c.txt"]),
            written_entry("100644", "000000", zeros, "D", &["gone.txt"]),
            written_entry("100644", "100644", other, "R100", &[":100644 100644 x y A", "renamed.txt"]),
        ]
        .concat();

        let blobs = written_blobs(&listing);

        assert_eq!(blobs, [shared, shared, other, other], "per path, the deletion left out");
        assert_eq!(total("5\n5\n7\n7\n", &blobs), Ok(24), "a blob two paths share counts twice");
        assert_eq!(written_blobs(""), Vec::<&str>::new(), "no change writes nothing");
        assert_eq!(total("", &[]), Ok(0));
        assert_eq!(
            total("18446744073709551615\n1\n", &[shared, other]),
            Ok(u64::MAX),
            "the sum stops at the most a u64 holds"
        );
    }

    /// A path field is read by its entry's count, so a name that starts with `:` and looks like a header is a path: the
    /// first entry writes that name, and the next real entry's link is still seen.
    #[test]
    fn a_path_that_looks_like_a_header_is_read_as_a_path() {
        let zeros = "0000000000000000000000000000000000000000";
        let first = "1111111111111111111111111111111111111111";
        let second = "2222222222222222222222222222222222222222";
        let crafted = ":000000 120000 aaaa bbbb A";
        let listing = format!(":000000 100644 {zeros} {first} A\0{crafted}\0:000000 120000 {zeros} {second} A\0evil\0");

        let read = entries(&listing);

        let paths: Vec<&[&str]> = read.iter().map(|entry| entry.paths.as_slice()).collect();
        assert_eq!(paths, [&[crafted][..], &["evil"][..]]);
        assert_eq!(forbidden(&listing), Some("evil".to_owned()), "the link the second entry makes");
        assert_eq!(written_blobs(&listing), [first], "the crafted name is a file the first entry writes");
    }

    #[test]
    fn an_answer_that_is_not_a_size_for_each_blob_is_never_a_size() {
        let id = "1111111111111111111111111111111111111111";
        assert_eq!(
            total(&format!("{id} missing\n"), &[id]),
            Err(format!("it answered '{id} missing' for {id}"))
        );
        assert_eq!(total("5\n", &[id, id]), Err("it answered 1 line(s) for 2 object(s)".to_owned()));
        assert_eq!(total("5\n5\n", &[id]), Err("it answered 2 line(s) for 1 object(s)".to_owned()));
        assert_eq!(total("-1\n", &[id]), Err(format!("it answered '-1' for {id}")));
    }

    #[test]
    fn a_back_commit_that_writes_one_byte_past_the_cap_is_refused() {
        assert_eq!(written_line(1_048_576, 1_048_576, SNAPSHOT, BACK), None, "the cap itself passes");
        assert_eq!(
            written_line(1_048_577, 1_048_576, SNAPSHOT, BACK).as_deref(),
            Some("gate: remote changes not applied: they would write 1.0 MiB into the worktree, more than 1.0 MiB; see git diff b8a48121 8e8777a0")
        );
        assert_eq!(
            written_line(8_388_608, 1_048_576, SNAPSHOT, BACK).as_deref(),
            Some("gate: remote changes not applied: they would write 8.0 MiB into the worktree, more than 1.0 MiB; see git diff b8a48121 8e8777a0")
        );
    }

    #[test]
    fn the_sync_back_lines_cut_the_commits_to_eight_hex_digits() {
        assert_eq!(
            ignored_line(BACK, SNAPSHOT),
            "gate: ignored stale remote changes 8e8777a0: not made from snapshot b8a48121"
        );
        assert_eq!(
            not_applied_line(SNAPSHOT, BACK),
            "gate: remote changes not applied: the worktree changed during the run; see git diff b8a48121 8e8777a0"
        );
        assert_eq!(
            ignored_line("abc", "de"),
            "gate: ignored stale remote changes abc: not made from snapshot de"
        );
        assert_eq!(
            touched_line(".gitignore", SNAPSHOT, BACK),
            "gate: remote changes not applied: they touch .gitignore, which a sync-back never writes; see git diff b8a48121 8e8777a0"
        );
        assert_eq!(
            touched_line("a\n.gitignore", SNAPSHOT, BACK),
            r"gate: remote changes not applied: they touch a\n.gitignore, which a sync-back never writes; see git diff b8a48121 8e8777a0",
            "a name from the builder cannot break the line"
        );
    }

    #[test]
    fn the_synced_back_line_names_the_files_and_says_zero_with_nothing_after_the_colon() {
        let names = ["made-on-box.txt".to_owned()];
        assert_eq!(synced_line(&names), "gate: synced back 1 file(s): made-on-box.txt");
        assert_eq!(synced_line(&[]), "gate: synced back 0 file(s): ", "as PowerShell joins no names");
        let two = ["a.txt".to_owned(), "b.txt".to_owned()];
        assert_eq!(synced_line(&two), "gate: synced back 2 file(s): a.txt, b.txt");
        let forged = ["a\r\ngate: forged".to_owned()];
        assert_eq!(
            synced_line(&forged),
            r"gate: synced back 1 file(s): a\r\ngate: forged",
            "a name from the builder cannot break the line"
        );
    }

    #[test]
    fn the_network_calls_carry_the_pack_then_dash_dash_the_url_and_the_ref() {
        let remote = Remote {
            url: "builder:D:/gate-remote/app.git",
            pack: Some("pwsh -NoProfile -NonInteractive -Command git-upload-pack"),
            ssh: Some("ssh -o BatchMode=yes"),
            reference: "refs/gate-back/app-1a2b3c4d",
        };
        assert_eq!(
            remote.args(&["ls-remote"]),
            [
                "ls-remote",
                "--upload-pack=pwsh -NoProfile -NonInteractive -Command git-upload-pack",
                "--",
                "builder:D:/gate-remote/app.git",
                "refs/gate-back/app-1a2b3c4d",
            ]
        );
        let local = Remote { pack: None, ..remote };
        assert_eq!(
            local.args(&["-c", "fetch.fsckObjects=true", "fetch", "-q"]),
            [
                "-c",
                "fetch.fsckObjects=true",
                "fetch",
                "-q",
                "--",
                "builder:D:/gate-remote/app.git",
                "refs/gate-back/app-1a2b3c4d"
            ]
        );
    }

    /// The fetch turns off git's automatic housekeeping, whose lock a Ctrl+C during the fetch can leave behind: the
    /// two config overrides it carries before the verb, as the `fetch.fsckObjects` one does.
    #[test]
    fn the_fetch_turns_off_auto_maintenance() {
        let remote = Remote {
            url: "builder:D:/gate-remote/app.git",
            pack: None,
            ssh: None,
            reference: "refs/gate-back/app-1a2b3c4d",
        };
        let args = remote.args(&FETCH_WORDS);
        let verb = args.iter().position(|word| word == "fetch").expect("the fetch verb is in the words");
        let pairs: Vec<(&str, &str)> = args[..verb].windows(2).map(|pair| (pair[0].as_str(), pair[1].as_str())).collect();
        assert!(pairs.contains(&("-c", "maintenance.auto=false")), "{args:?}");
        assert!(pairs.contains(&("-c", "gc.auto=0")), "{args:?}");
    }
}

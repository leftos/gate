//! The client's sync-back: after a run call that ended with the command's own status, the changes the command left on
//! the builder are fetched from `refs/gate-back/<key>` and applied to the worktree when it still matches the snapshot.
//! The builder is trusted less than this PC. A fetched commit that is not the one the builder advertised, or that was
//! not made from this snapshot, is ignored. One that touches a `.git*` path or a `.lfsconfig` or makes a link or a
//! submodule, one that writes or deletes a path the worktree's attributes give a checkout filter or `ident`, one whose
//! files would write more than the apply cap into the worktree (a file the checkout converts to CRLF counted twice, one
//! with a `working-tree-encoding` four times, and one with both eight), or one the worktree changed under during the
//! run, is kept at
//! `refs/gate-remote/<key>-back` and fails a run that had passed; one that a step before its apply could not
//! take further, as it failed or was interrupted, is kept there too, and the line that ends the run says where to see
//! its changes. A fetch whose job writes more than the fetch cap is stopped and fails a run that had passed, the
//! changes left on the builder and nothing applied.
//! The fetch writes its objects into a quarantine folder of its own inside the repository's object store, which git
//! reads beside the store; they are moved into the store only when the back commit is kept or applied, and the folder
//! is removed at the sync-back's end, unless a move that failed keeps it. The quarantines gates that ended left are
//! swept at the start of the next sync-back; a kept one stays for a week.
//! The two git calls that reach the builder take the pre-banner retry and keep their lines out of the log; any other
//! git failure is the run's, status 1 and `gate: the remote run failed: ...`, with no fallback. Every name and git line
//! said here has its control characters escaped.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::client::{Attempt, Called, Echo, End, Watch};
use super::git::{self, Git, GitError};
use super::launch::KILL_WAIT;
use super::payload;
use super::retry::GIT_SSH_FAILED;
use super::snapshot::{self, BACK, REFS, Snapshot};
use super::watchdog::{self, Cut};
use crate::boxside::payload::escaped;
use crate::outputs::names::{is_git_s, is_lfs_config};
use crate::slots::{self, Liveness};

/// The patch an applied sync-back is written to in the git folder, named for the process and a nonce, and removed
/// whether or not it applied.
const PATCH: &str = "gate-remote-";
const PATCH_SUFFIX: &str = ".patch";
/// The modes of a symbolic link and of a submodule, which no entry of a back commit may take.
const LINK_MODES: [&str; 2] = ["120000", "160000"];
/// What the mode of a regular or executable file starts with.
const REGULAR_MODE: &str = "100";

/// Fetches and applies the builder's changes for `snapshot`, or ends the run as `status` when there is nothing to do,
/// the run's own when the changes were ignored, and 1 when a passing run's changes could not be applied.
pub(super) fn sync_back(attempt: &mut Attempt<'_, '_>, snapshot: &Snapshot, status: i64) -> Result<End, End> {
    attempt.go_on()?;
    let target = attempt.transport.push_target(attempt.call.host, attempt.call.repo).map_err(End::Failed)?;
    let pack = attempt.transport.upload_pack();
    let reference = format!("refs/gate-back/{}", attempt.call.key);
    let remote = Remote {
        url: &target.url,
        pack: pack.as_deref(),
        ssh: target.ssh_command.as_deref(),
        reference: &reference,
    };
    let git = Git::find(Path::new(attempt.call.root)).map_err(End::Failed)?;
    let objects = objects_folder(attempt, &git)?;
    for line in swept(&objects, SystemTime::now(), slots::gate_liveness) {
        attempt.say(line);
    }
    let Some(advertised) = has_back(attempt, &git, &remote)? else {
        return Ok(End::Status(status));
    };
    let name = incoming_name(std::process::id(), own_start(attempt.own_start_fails)?, &payload::job_nonce());
    let quarantine = Quarantine::made(&objects, &name).map_err(End::Failed)?;
    let back = Back {
        commit: &advertised,
        quarantine: &quarantine,
    };
    let end = fetch(attempt, &git, &remote, &quarantine, status).and_then(|stopped| match stopped {
        Some(stopped) => Ok(stopped),
        None => taken(attempt, &git, snapshot, &back, status),
    });
    // However the sync-back ended, the quarantine goes, unless a move that failed kept it; the end stands as it would
    // without the removal, and its lines come before the removal's.
    if let Some(line) = quarantine.removed(KILL_WAIT, || {}) {
        attempt.say(line);
    }
    end
}

/// This gate's start in Unix epoch milliseconds, read as the gate reads it for its slots, with the
/// `--test-own-start-fails` seam `seam`, which names its quarantine beside its pid; else why it cannot be read.
fn own_start(seam: bool) -> Result<u64, End> {
    slots::own_start(seam)
        .map(slots::epoch_ms)
        .map_err(|error| End::Failed(format!("cannot read this gate's start: {error}")))
}

/// The back commit the fetch brought, applied or kept when it is the one the builder advertised, `back`, and was made
/// from the snapshot; else it is ignored, and the run ends as `status`.
fn taken(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &Back<'_>, status: i64) -> Result<End, End> {
    let fetched = line(attempt, git, &["rev-parse", "FETCH_HEAD"], &[])?;
    if fetched != back.commit || !made_from(attempt, git, back, &snapshot.commit)? {
        attempt.say(ignored_line(&fetched, &snapshot.commit));
        return Ok(End::Status(status));
    }
    receive(attempt, git, snapshot, back, status)
}

/// The back commit the builder advertised, and the quarantine its fetched objects are read from.
struct Back<'a> {
    commit: &'a str,
    quarantine: &'a Quarantine,
}

/// The back commit the builder advertises for this run, from `ls-remote` of its ref; `None` when it holds none.
fn has_back(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>) -> Result<Option<String>, End> {
    let ls_remote = Network {
        verb: "ls-remote",
        watch: Watch::Deadline(attempt.deadline_for("ls-remote", attempt.limit)),
        set: &[],
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

/// Fetches the back ref the builder holds into `FETCH_HEAD`, its objects written into `quarantine` alone, every object
/// it sends checked, and no tag or submodule. It says its progress, which a live fetch keeps sending on standard error,
/// so only a fetch that went quiet stalls. Its job may write at most the attempt's fetch cap: past it the fetch is
/// stopped, and the run of the command that ended `status` ends with the line saying the changes stay on the builder, a
/// passing run as 1. One still running at its transfer ceiling is stopped with the TRANSFER TIMED OUT line: a passing
/// run ends killed, and a failing one keeps its command's status. That end; `None` when the fetch went through.
fn fetch(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, quarantine: &Quarantine, status: i64) -> Result<Option<End>, End> {
    let set = quarantine.env();
    let fetch = Network {
        verb: "fetch",
        watch: Watch::Fetch(attempt.limits_for("fetch"), Some(attempt.fetch_cap)),
        set: &set,
    };
    let args = remote.args(&FETCH_WORDS);
    let begun = Instant::now();
    let called = reached(attempt, git, remote, &fetch, &args)?;
    attempt.split.sync_back = Some(begun.elapsed());
    match called.cut {
        Some(Cut::TooBig(cap)) => {
            attempt.noted(&called);
            attempt.say(stopped_line(cap, remote.reference));
            return Ok(Some(End::Status(not_passing(status))));
        }
        Some(Cut::Ceiling(ceiling)) => {
            attempt.say(watchdog::transfer_line(ceiling, "fetching the sync-back"));
            attempt.noted(&called);
            return if status == 0 { Err(End::Killed) } else { Ok(Some(End::Status(status))) };
        }
        _ => {}
    }
    checked(attempt, &fetch, &args, called).map(|_| None)
}

/// `status`, but 1 for a passing run: how a run whose changes were not applied ends.
fn not_passing(status: i64) -> i64 {
    if status == 0 { 1 } else { status }
}

/// The object store of the worktree's repository, as `git rev-parse --git-path objects` names it, asked with no
/// interrupt: a Ctrl+C meanwhile is left to the call that follows, which ends as the run's interrupt.
fn objects_folder(attempt: &Attempt<'_, '_>, git: &Git) -> Result<PathBuf, End> {
    let args = ["rev-parse", "--git-path", "objects"];
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

/// The real object store `objects` as `GIT_ALTERNATE_OBJECT_DIRECTORIES` names it: as it is, or C-quoted, its `\` and
/// `"` escaped, when it holds the list's separator `;` or starts with the `"` git reads a quoted entry by.
fn alternates(objects: &Path) -> OsString {
    let bytes = objects.as_os_str().as_encoded_bytes();
    if !bytes.contains(&b';') && bytes.first() != Some(&b'"') {
        return objects.as_os_str().to_owned();
    }
    let mut quoted = String::from('"');
    for character in objects.to_string_lossy().chars() {
        if matches!(character, '\\' | '"') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    OsString::from(quoted)
}

/// What the folder a sync-back's fetch writes its objects into is named, in the repository's object store:
/// `incoming-gate-<pid>-<start>-<nonce>`, for the gate's pid, its start in Unix epoch milliseconds and a nonce.
const INCOMING: &str = "incoming-gate-";
/// The file a quarantine whose objects could not be moved in holds, so that no sweep removes it at once.
const KEPT_MARK: &str = "gate-kept";
/// What the reason a move into the store failed for starts with.
const UNMOVED: &str = "the fetched objects could not be moved into the repository";
/// What the name of a pack's own file starts with: `pack-<hash>.<kind>`.
const PACK_FILE: &str = "pack-";
/// Windows' error for a file another process holds open, which refuses its move or its removal.
const SHARING_VIOLATION: i32 = 32;
/// How long a move or a removal waits before it tries again a file another process holds open.
const RETRY_STEP: Duration = Duration::from_millis(20);

/// The name of the quarantine of the gate `pid` started at `start`, in Unix epoch milliseconds, for the nonce `nonce`.
fn incoming_name(pid: u32, start: u64, nonce: &str) -> String {
    format!("{INCOMING}{pid}-{start}-{nonce}")
}

/// The pid and the start of the gate the quarantine `name` was made for; `None` for a name that does not read as one.
fn incoming_owner(name: &str) -> Option<(u32, u64)> {
    let mut parts = name.strip_prefix(INCOMING)?.splitn(3, '-');
    let pid = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    parts.next().filter(|nonce| !nonce.is_empty())?;
    Some((pid, start))
}

/// The folder a sync-back's fetch writes its objects into, inside the repository's object store `objects` so that each
/// move into the store is a rename. git is given it as its object store and the real one as its alternate, so nothing
/// reaches the store before the back commit is kept or applied; it is removed at the sync-back's end, unless a move
/// that failed kept it.
struct Quarantine {
    folder: PathBuf,
    objects: PathBuf,
    /// The store as the alternates variable names it.
    alternates: OsString,
    /// Whether a move that failed kept the folder.
    kept: Cell<bool>,
    /// How long the sync-back's move waits for a file another process holds open: [`KILL_WAIT`], or a test's own.
    wait: Duration,
}

impl Quarantine {
    /// The new folder `name` made in the object store `objects`; else why it could not be made, escaped.
    fn made(objects: &Path, name: &str) -> Result<Quarantine, String> {
        let folder = objects.join(name);
        std::fs::create_dir(&folder).map_err(|error| escaped(&format!("cannot make {}: {error}", folder.display())))?;
        Ok(Quarantine {
            folder,
            objects: objects.to_path_buf(),
            alternates: alternates(objects),
            kept: Cell::new(false),
            wait: KILL_WAIT,
        })
    }

    /// The variables that point git at the quarantine, the store its alternate: what the fetch and every call that reads
    /// the objects it brought are given.
    fn env(&self) -> [(&'static str, &OsStr); 2] {
        [
            ("GIT_OBJECT_DIRECTORY", self.folder.as_os_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", self.alternates.as_os_str()),
        ]
    }

    /// Moves the fetched objects into the store as [`move_plan`] orders them, a file the store holds already left where
    /// it is; a file another process holds open is tried again for up to `wait`, `held` called before each wait. A move
    /// that fails even so, or a quarantine that cannot be listed, stops there: every file the move put into the store
    /// is returned to the quarantine, and the quarantine is kept, marked: why, and where the objects are kept. A
    /// quarantine that is gone keeps nothing, and the reason says so.
    fn moved_in(&self, wait: Duration, mut held: impl FnMut()) -> Result<(), String> {
        let deadline = Instant::now() + wait;
        let plan = match move_plan(&self.folder, &self.objects) {
            Ok(plan) => plan,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(format!("{UNMOVED}: the quarantine folder is gone")),
            Err(error) => return Err(self.kept_for(&format!("cannot list the quarantine: {error}"))),
        };
        let mut done = Vec::new();
        for (source, target) in &plan {
            match moved(source, target, deadline, &mut held) {
                Ok(true) => done.push((source, target)),
                Ok(false) => {}
                Err(error) => {
                    let shown = source.strip_prefix(&self.folder).unwrap_or(source).display();
                    let unreturned = returned(&done, wait, &mut held);
                    return Err(self.kept_for(&format!("{shown}: {error}{unreturned}")));
                }
            }
        }
        Ok(())
    }

    /// Keeps the quarantine after a move that failed for `detail`, marked with [`KEPT_MARK`]: the reason, escaped, which
    /// says where the objects are kept, and when the mark could not be written, that a later sweep may remove them.
    fn kept_for(&self, detail: &str) -> String {
        self.kept.set(true);
        let unmarked = match std::fs::write(self.folder.join(KEPT_MARK), "") {
            Ok(()) => String::new(),
            Err(error) => format!("; it could not be marked kept, so the next sync-back may remove it: {error}"),
        };
        escaped(&format!("{UNMOVED}: {detail}; they are kept in {}{unmarked}", self.folder.display()))
    }

    /// Removes the quarantine whole, unless a move that failed kept it; a file another process holds open is tried
    /// again for up to `wait`, `held` called before each wait. The line saying it could not be removed even so.
    fn removed(&self, wait: Duration, mut held: impl FnMut()) -> Option<String> {
        if self.kept.get() {
            return None;
        }
        tree_removed(&self.folder, Instant::now() + wait, &mut held)
            .err()
            .map(|error| unremoved_line(&self.folder, &error))
    }
}

/// The moves that bring the quarantine `folder`'s objects into the store `objects`, in order, each a file and where it
/// goes: the packs' own files, every `.idx` after all the others, so that git sees no pack before its data is in;
/// then the loose objects of each two-hex-digit folder. A `.keep`, or any other file, is left to the quarantine's
/// removal, and a `pack` folder the quarantine lacks holds nothing; a quarantine `folder` that is gone is a `NotFound`
/// error.
fn move_plan(folder: &Path, objects: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut loose = listed(folder)?;
    let mut packs = names_in(&folder.join("pack"))?;
    packs.retain(|name| name.starts_with(PACK_FILE) && !is_kind(name, "keep"));
    packs.sort();
    packs.sort_by_key(|name| is_kind(name, "idx"));
    let mut plan: Vec<(PathBuf, PathBuf)> = packs
        .iter()
        .map(|name| (folder.join("pack").join(name), objects.join("pack").join(name)))
        .collect();
    loose.retain(|name| name.len() == 2 && is_hex(name));
    loose.sort();
    for part in loose {
        let mut files = names_in(&folder.join(&part))?;
        files.retain(|name| is_hex(name));
        files.sort();
        plan.extend(files.iter().map(|name| (folder.join(&part).join(name), objects.join(&part).join(name))));
    }
    Ok(plan)
}

/// Whether the file `name`'s extension is `kind`, its letter case aside.
fn is_kind(name: &str, kind: &str) -> bool {
    Path::new(name).extension().is_some_and(|extension| extension.eq_ignore_ascii_case(kind))
}

/// Whether `name` is hex digits alone, as the folders and the files of loose objects are named.
fn is_hex(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The names in `folder` that are text, as git names every file it writes; a folder that cannot be listed, or is not
/// there, is an error.
fn listed(folder: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        names.extend(entry?.file_name().into_string().ok());
    }
    Ok(names)
}

/// The names [`listed`] reads in `folder`; none when there is no such folder.
fn names_in(folder: &Path) -> io::Result<Vec<String>> {
    match listed(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        names => names,
    }
}

/// Moves the file `source` to `target`, its folder made when missing, never replacing a file `target` names, which
/// has the same content under the same name: whether it moved, `false` when `target` was there already. Tried again
/// every [`RETRY_STEP`] until `deadline` while another process holds it open, `held` called before each wait.
fn moved(source: &Path, target: &Path, deadline: Instant, held: &mut impl FnMut()) -> io::Result<bool> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match retried(deadline, held, || gate_win32::move_no_replace(source, target)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns each file a move that failed put into the store, `done` as its source in the quarantine and its target in
/// the store, to the quarantine, the last moved first; a file another process holds open is tried again for up to
/// `wait`, `held` called before each wait. What the kept line adds: nothing when every file went back, else how many
/// could not and where the first of them is.
fn returned(done: &[(&PathBuf, &PathBuf)], wait: Duration, held: &mut impl FnMut()) -> String {
    let deadline = Instant::now() + wait;
    let stuck: Vec<&PathBuf> = done
        .iter()
        .rev()
        .filter(|(source, target)| retried(deadline, &mut *held, || gate_win32::move_no_replace(target, source)).is_err())
        .map(|(_, target)| *target)
        .collect();
    match stuck.first() {
        None => String::new(),
        Some(first) => format!("; {} moved files could not be returned: {}", stuck.len(), first.display()),
    }
}

/// Removes the folder `path` whole, tried again every [`RETRY_STEP`] until `deadline` while another process holds a
/// file of it open, `held` called before each wait; a folder already gone counts as removed.
fn tree_removed(path: &Path, deadline: Instant, held: &mut impl FnMut()) -> io::Result<()> {
    retried(deadline, held, || match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        done => done,
    })
}

/// What `act` ends with, tried again every [`RETRY_STEP`] until `deadline` while it fails as another process holds its
/// file open, `held` called before each wait.
fn retried(deadline: Instant, held: &mut impl FnMut(), mut act: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match act() {
            Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) && Instant::now() < deadline => {
                held();
                thread::sleep(RETRY_STEP);
            }
            done => return done,
        }
    }
}

/// The line said for the folder `path`, which could not be removed for `error`.
fn unremoved_line(path: &Path, error: &io::Error) -> String {
    format!("gate: remote could not remove {}: {error}", escaped(&path.display().to_string()))
}

/// The line said for the store `objects`, which could not be listed for `error` to sweep the quarantines left in it.
fn unlisted_line(objects: &Path, error: &io::Error) -> String {
    escaped(&format!(
        "gate: remote could not list {} for leftover sync-backs: {error}",
        objects.display()
    ))
}

/// The line said for a back commit whose fetched objects could not be moved into the repository, for `reason`.
fn unmoved_line(reason: &str) -> String {
    format!("gate: remote changes not applied: {reason}")
}

/// How long a quarantine marked kept stays, from its mark's last write, before a sweep removes it.
const KEPT_FOR: Duration = Duration::from_hours(7 * 24);

/// The most quarantines marked kept, of gates that ended, a sweep leaves in an object store.
const KEPT_MOST: usize = 3;

/// Removes the quarantines in the store `objects` that gates which ended left, tried once each: one whose gate is gone
/// by `liveness` at once, unless it is marked kept, and a kept one once it is [`KEPT_FOR`] old at `now`, or once
/// [`KEPT_MOST`] kept ones are newer. A live gate's own, one whose gate's liveness cannot be read, and a folder whose
/// name does not read as a quarantine's are left, and not counted. Nothing is said of a removal: the lines saying each
/// quarantine that could not be removed, or the store that could not be listed.
fn swept(objects: &Path, now: SystemTime, liveness: impl Fn(u32, u64) -> Liveness) -> Vec<String> {
    let names = match names_in(objects) {
        Ok(names) => names,
        Err(error) => return vec![unlisted_line(objects, &error)],
    };
    let mut ended = Vec::new();
    let mut kept = Vec::new();
    for folder in names.iter().filter_map(|name| ended_quarantine(objects, name, &liveness)) {
        match kept_since(&folder, now) {
            Some(since) if now.duration_since(since).is_ok_and(|age| age < KEPT_FOR) => kept.push((since, folder)),
            _ => ended.push(folder),
        }
    }
    // The oldest go first, so the newest kept ones stay.
    kept.sort();
    let extra = kept.len().saturating_sub(KEPT_MOST);
    ended.extend(kept.into_iter().take(extra).map(|(_, folder)| folder));
    ended
        .iter()
        .filter_map(|folder| {
            tree_removed(folder, Instant::now(), &mut || {})
                .err()
                .map(|error| unremoved_line(folder, &error))
        })
        .collect()
}

/// The quarantine `name` in the store `objects` when it is a folder whose gate is gone by `liveness`; `None` for a live
/// gate's, one whose gate's liveness cannot be read, and a name that does not read as a quarantine's.
fn ended_quarantine(objects: &Path, name: &str, liveness: &impl Fn(u32, u64) -> Liveness) -> Option<PathBuf> {
    let (pid, start) = incoming_owner(name)?;
    let folder = objects.join(name);
    (folder.is_dir() && matches!(liveness(pid, start), Liveness::Gone)).then_some(folder)
}

/// When the quarantine `folder` was marked kept, as a sweep ages it: its mark's last write; when that cannot be read or
/// lies past `now`, the folder's creation; when that cannot be read or lies past `now` either, `now`, so it counts as
/// newly kept and [`KEPT_MOST`] still bounds it. `None` when it is not marked kept.
fn kept_since(folder: &Path, now: SystemTime) -> Option<SystemTime> {
    let mark = match std::fs::metadata(folder.join(KEPT_MARK)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        mark => mark,
    };
    let past = |time: io::Result<SystemTime>| time.ok().filter(|time| *time <= now);
    let since = past(mark.and_then(|mark| mark.modified())).or_else(|| past(std::fs::metadata(folder).and_then(|folder| folder.created())));
    Some(since.unwrap_or(now))
}

/// The line said for a fetch stopped once its job wrote more than `cap` bytes, whose changes stay at `reference`.
fn stopped_line(cap: u64, reference: &str) -> String {
    format!(
        "gate: remote sync-back stopped: the fetch wrote more than {}; the run's changes stay on the builder ({reference})",
        watchdog::size(cap)
    )
}

/// One git call of the sync-back that reaches the builder: its verb, and how it is watched. `ls-remote` gets the git
/// limit; `fetch`, which may take long, ends once nothing came from it for the run's stall window, or at its transfer
/// ceiling, counted from its own start.
struct Network<'a> {
    verb: &'static str,
    watch: Watch,
    /// The variables the call is given beside the ssh it runs: the quarantine's, for the fetch.
    set: &'a [(&'a str, &'a OsStr)],
}

/// git with `args`, the call `network` that reaches the builder: retried once when it failed before the banner, its
/// lines kept out of the log, and killed as its watch says or at a line longer than the cap. A failure is the run's,
/// or its interrupt when the console interrupt was raised during the call.
fn to_the_builder(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, network: &Network<'_>, args: &[String]) -> Result<Called, End> {
    let called = reached(attempt, git, remote, network, args)?;
    checked(attempt, network, args, called)
}

/// git with `args`, the call `network` that reaches the builder, made as [`to_the_builder`] makes it: the call that
/// counts, however it ended. A retried call is given the same variables, so a retried fetch writes into the same
/// quarantine.
fn reached(attempt: &mut Attempt<'_, '_>, git: &Git, remote: &Remote<'_>, network: &Network<'_>, args: &[String]) -> Result<Called, End> {
    let mut set = network.set.to_vec();
    set.extend(remote.ssh.map(|ssh| ("GIT_SSH_COMMAND", OsStr::new(ssh))));
    let (called, _) = attempt.with_retry(GIT_SSH_FAILED, |attempt| attempt.git(git, args, &set, Echo::Unlogged, network.watch))?;
    Ok(called)
}

/// The call `network` with `args` that [`reached`] made, as [`to_the_builder`] ends it: a cut or a failure is the run's,
/// or its interrupt when the console interrupt was raised during the call.
fn checked(attempt: &mut Attempt<'_, '_>, network: &Network<'_>, args: &[String], called: Called) -> Result<Called, End> {
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

/// Whether `back` was made from `snapshot`, as its `rev-list --parents` line says, read through its quarantine.
fn made_from(attempt: &Attempt<'_, '_>, git: &Git, back: &Back<'_>, snapshot: &str) -> Result<bool, End> {
    let parents = line(attempt, git, &["rev-list", "--parents", "-n", "1", back.commit], &back.quarantine.env())?;
    Ok(from_snapshot(&parents, snapshot))
}

/// Applies the back commit `back`, or keeps it when it touches what a sync-back never writes, its files would write
/// more than the apply cap into the worktree, or the worktree no longer matches the snapshot. A step that fails or is
/// interrupted before the patch applied keeps it as well, so its changes are not lost; a patch that applied but cannot
/// be removed then fails the run with nothing kept. Its fetched objects are moved into the store before it is kept or
/// applied; when they cannot be, nothing is kept or applied, the line says where they are kept, and a passing run is 1.
fn receive(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &Back<'_>, status: i64) -> Result<End, End> {
    let commit = snapshot.commit.as_str();
    let refused = refusal(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    if let Some(said) = refused {
        return refused_end(attempt, git, back, said, status);
    }
    let (names, patch) = written_patch(attempt, git, snapshot, back).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    if let Err(unmoved) = back.quarantine.moved_in(back.quarantine.wait, || {}) {
        attempt.say(unmoved_line(&unmoved));
        remove(&patch)?;
        return Ok(End::Status(not_passing(status)));
    }
    applied(attempt, git, &patch).map_err(|failed| kept_after(attempt, git, commit, back, failed))?;
    remove(&patch)?;
    attempt.say(synced_line(&names));
    Ok(End::Status(status))
}

/// How the run of the command that ended `status` ends once its back commit `back` is refused for the line `said`: its
/// objects moved in, the commit kept and the line said, a passing run as 1. Objects that cannot be moved in keep
/// nothing, and the line saying where they are kept is said instead.
fn refused_end(attempt: &mut Attempt<'_, '_>, git: &Git, back: &Back<'_>, said: String, status: i64) -> Result<End, End> {
    if let Err(unmoved) = back.quarantine.moved_in(back.quarantine.wait, || {}) {
        attempt.say(unmoved_line(&unmoved));
        return Ok(End::Status(not_passing(status)));
    }
    keep(attempt, git, back.commit).map_err(End::Failed)?;
    attempt.say(said);
    Ok(End::Status(not_passing(status)))
}

/// Why `back` is kept rather than applied: the line said for a back commit that touches what a sync-back never writes,
/// for one that writes, or deletes, a path through a checkout filter or `ident`, for one whose files would write more
/// than the apply cap into the worktree, or for one the worktree changed under during the run; `None` when it may be
/// applied.
fn refusal(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &Back<'_>) -> Result<Option<String>, End> {
    let set = back.quarantine.env();
    let listing = git_call(attempt, git, &["diff-tree", "-r", "-z", "--raw", &snapshot.commit, back.commit], &set)?;
    if let Some(path) = forbidden(&listing) {
        return Ok(Some(touched_line(&path, &snapshot.commit, back.commit)));
    }
    let paths = listed_paths(&listing);
    let attributes = attributes(attempt, git, &paths)?;
    let config = if paths.is_empty() {
        String::new()
    } else {
        git_call(attempt, git, &["config", "-z", "--list"], &[])?
    };
    if let Some((path, attribute)) = filtered(&paths, &attributes, names_odd_driver(&config)) {
        return Ok(Some(filtered_line(path, &attribute, &snapshot.commit, back.commit)));
    }
    let written = written_blobs(&listing);
    let weights = weights(&written, &attributes, LineEnds::of(&config));
    let size = written_size(attempt, git, &written, &weights, &set)?;
    if let Some(line) = written_line(size, attempt.apply_cap, &snapshot.commit, back.commit) {
        return Ok(Some(line));
    }
    Ok(changed(attempt, git, snapshot)?.then(|| not_applied_line(&snapshot.commit, back.commit)))
}

/// The bytes the files `written` write into the worktree total, each path counted: the sizes of their blobs, asked of
/// one `git cat-file --batch-check=%(objectsize)` given the variables `set` and fed their ids, each times its weight in
/// `weights`. An answer that is not a size for each of them is the run's failure.
fn written_size(attempt: &Attempt<'_, '_>, git: &Git, written: &[Written<'_>], weights: &[u64], set: &[(&str, &OsStr)]) -> Result<u64, End> {
    if written.is_empty() {
        return Ok(0);
    }
    let blobs: Vec<&str> = written.iter().map(|file| file.id).collect();
    let ids: String = blobs.iter().flat_map(|id| [*id, "\n"]).collect();
    let args = ["cat-file", "--batch-check=%(objectsize)"];
    let call = git::Call {
        interrupt: attempt.call.interrupt,
        stdin: Some(ids.as_bytes()),
        set,
        ..git::Call::plain(Path::new(attempt.call.root), &args, attempt.limit)
    };
    let answer = git.output(&call).map_err(escaped_end)?;
    total(&answer, &blobs, weights).map_err(|why| unreadable(attempt, &args, &why))
}

/// The run's failure for git with `args`, whose answer could not be read for `why`.
fn unreadable(attempt: &Attempt<'_, '_>, args: &[&str], why: &str) -> End {
    escaped_end(GitError::Failed(format!("git {} failed in {}: {why}", args.join(" "), attempt.call.root)))
}

/// One file a back commit writes: the id of the blob written there, and where its path stands among every path the
/// listing names.
struct Written<'a> {
    id: &'a str,
    at: usize,
}

/// The files a back commit's `diff-tree -r -z --raw` `listing` writes, one per path in the listing's order: each path
/// it adds or changes to a regular or executable file, a rename's or a copy's destination, so a blob two paths share is
/// written, and counted, twice. A deletion, whose new id is all zeros, writes nothing.
fn written_blobs(listing: &str) -> Vec<Written<'_>> {
    let mut written = Vec::new();
    let mut at = 0;
    for entry in entries(listing) {
        let writes = entry.new_mode.starts_with(REGULAR_MODE) && !entry.new_id.bytes().all(|digit| digit == b'0');
        if writes && !entry.paths.is_empty() {
            written.push(Written {
                id: entry.new_id,
                at: at + entry.paths.len() - 1,
            });
        }
        at += entry.paths.len();
    }
    written
}

/// Every path a back commit's `diff-tree -r -z --raw` `listing` names, in its order: what it writes, what it deletes,
/// and a rename's or a copy's source, each of which `git apply` reads from the worktree through its clean filter.
fn listed_paths(listing: &str) -> Vec<&str> {
    entries(listing).into_iter().flat_map(|entry| entry.paths).collect()
}

/// The attributes `git check-attr` is asked of each listed path, in the order it answers them.
const ATTRIBUTES: [&str; 6] = ["filter", "ident", "working-tree-encoding", "text", "eol", "crlf"];
/// The bytes `git check-attr -z` is allowed per attribute of a path beside the path itself: two NULs, the attribute's
/// name and its value, with room to spare, so that each slice of paths it is fed answers well under the runner's cap.
/// An answer that is larger even so fails closed: the runner's over-cap failure ends the run, the back commit kept.
const ATTRIBUTE_ROOM: usize = 256;
/// The bytes the answer of each slice of paths fed to `git check-attr` is planned within: the runner's cap.
#[cfg(not(feature = "test-seams"))]
const SLICE_CAP: usize = git::OUTPUT_CAP;
/// The bytes the answer of each slice of paths fed to `git check-attr` is planned within, in a `test-seams` build: room
/// for two paths of up to 32 bytes, so that a suite case's few files span slices.
#[cfg(feature = "test-seams")]
const SLICE_CAP: usize = 2 * ATTRIBUTES.len() * (32 + ATTRIBUTE_ROOM);
/// What `git check-attr` says of an attribute no file sets, and of one a file unsets.
const NOT_GIVEN: [&str; 2] = ["unspecified", "unset"];
/// What the config keys of a filter driver named as `git check-attr` says of an attribute not given start with: a
/// `filter` that reads so may name that driver.
const ODD_DRIVERS: [&str; 2] = ["filter.unspecified.", "filter.unset."];
/// How many times a path the checkout converts to CRLF is counted: each LF may become two bytes.
const CRLF_WEIGHT: u64 = 2;
/// How many times a path with a `working-tree-encoding` is counted: UTF-32 writes four bytes for each one.
const ENCODED_WEIGHT: u64 = 4;

/// The checkout attributes of one listed path, as `git check-attr` gives them: `unspecified`, `set`, `unset` or the
/// value.
#[derive(Debug, PartialEq, Eq)]
struct Attributes {
    filter: String,
    ident: String,
    encoding: String,
    text: String,
    eol: String,
    crlf: String,
}

/// The worktree's attributes of each of `paths`, in its order, read by `git check-attr -z --stdin` in the worktree, so
/// its own `.git/info/attributes` counts as the apply would read it, fed the paths in slices whose answers stay under
/// the runner's cap. An answer that does not give each attribute of each path is the run's failure.
fn attributes(attempt: &Attempt<'_, '_>, git: &Git, paths: &[&str]) -> Result<Vec<Attributes>, End> {
    let args = [
        "check-attr",
        "-z",
        "--stdin",
        ATTRIBUTES[0],
        ATTRIBUTES[1],
        ATTRIBUTES[2],
        ATTRIBUTES[3],
        ATTRIBUTES[4],
        ATTRIBUTES[5],
    ];
    let mut read = Vec::with_capacity(paths.len());
    for slice in attribute_slices(paths, SLICE_CAP) {
        let fed: String = slice.iter().flat_map(|path| [*path, "\0"]).collect();
        let call = git::Call {
            interrupt: attempt.call.interrupt,
            stdin: Some(fed.as_bytes()),
            ..git::Call::plain(Path::new(attempt.call.root), &args, attempt.limit)
        };
        let answer = git.output(&call).map_err(escaped_end)?;
        read.extend(read_attributes(&answer, slice).map_err(|why| unreadable(attempt, &args, &why))?);
    }
    Ok(read)
}

/// `paths` cut into slices, in order, each holding as many paths as `git check-attr` can answer within `cap` bytes,
/// [`ATTRIBUTE_ROOM`] allowed for each attribute beside the path; a path that alone needs more is a slice of its own.
fn attribute_slices<'a>(paths: &'a [&'a str], cap: usize) -> Vec<&'a [&'a str]> {
    let mut slices = Vec::new();
    let mut rest = paths;
    while !rest.is_empty() {
        let mut used = 0_usize;
        let count = rest
            .iter()
            .take_while(|path| {
                used = used.saturating_add(ATTRIBUTES.len() * (path.len() + ATTRIBUTE_ROOM));
                used <= cap
            })
            .count()
            .max(1);
        let (slice, after) = rest.split_at(count);
        slices.push(slice);
        rest = after;
    }
    slices
}

/// The attributes of each of `paths` from `git check-attr -z`'s `answer`: a `path`, attribute, value triple for each
/// attribute of [`ATTRIBUTES`] of each path, in their order; else why the answer cannot be read.
fn read_attributes(answer: &str, paths: &[&str]) -> Result<Vec<Attributes>, String> {
    let mut fields = answer.split('\0');
    let mut read = Vec::with_capacity(paths.len());
    for path in paths {
        let mut values = [""; ATTRIBUTES.len()];
        for (value, name) in values.iter_mut().zip(ATTRIBUTES) {
            match (fields.next(), fields.next(), fields.next()) {
                (Some(said), Some(attribute), Some(given)) if said == *path && attribute == name => *value = given,
                _ => return Err(format!("it did not answer {name} for {path}")),
            }
        }
        let [filter, ident, encoding, text, eol, crlf] = values.map(str::to_owned);
        read.push(Attributes {
            filter,
            ident,
            encoding,
            text,
            eol,
            crlf,
        });
    }
    if fields.any(|field| !field.is_empty()) {
        return Err(format!("it answered more than the {} path(s) it was asked", paths.len()));
    }
    Ok(read)
}

/// Whether an attribute `git check-attr` gave as `value` is set to anything: neither unspecified nor unset.
fn is_given(value: &str) -> bool {
    !NOT_GIVEN.contains(&value)
}

/// Whether the config `git config -z --list` gave as `listed` defines a filter driver named `unspecified` or `unset`,
/// which a path's `filter` names in the words `git check-attr` says of an attribute not given.
fn names_odd_driver(listed: &str) -> bool {
    listed.split('\0').any(|entry| ODD_DRIVERS.iter().any(|start| entry.starts_with(start)))
}

/// The first of `paths`, in its order, that the apply would read or write through a checkout filter, with the
/// attribute `attributes` gives it; `None` when no path is. `odd_driver` says whether the config defines a driver named
/// as an attribute not given reads.
fn filtered<'a>(paths: &[&'a str], attributes: &[Attributes], odd_driver: bool) -> Option<(&'a str, String)> {
    paths
        .iter()
        .zip(attributes)
        .find_map(|(path, given)| checkout_filter(given, odd_driver).map(|attribute| (*path, attribute)))
}

/// The checkout filter the attributes `given` set, as the refusal names it: `filter=<driver>`, `filter (no driver)` for
/// a `filter` set with no value, or `ident` for any `ident` given; a `filter` not given counts as `filter=<what it
/// reads>` when `odd_driver` says the config defines a driver of that name. `None` for no checkout filter.
fn checkout_filter(given: &Attributes, odd_driver: bool) -> Option<String> {
    match given.filter.as_str() {
        "set" => return Some("filter (no driver)".to_owned()),
        driver if is_given(driver) || odd_driver => return Some(format!("filter={driver}")),
        _ => {}
    }
    is_given(&given.ident).then(|| "ident".to_owned())
}

/// How `core.autocrlf` is set: `true`, `input`, or false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoCrlf {
    True,
    Input,
    False,
}

/// The worktree's line-end config, as the checkout reads it: its `core.autocrlf`, and whether its `core.eol` writes
/// CRLF, as `crlf`, `native` and no setting do in Git for Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineEnds {
    autocrlf: AutoCrlf,
    eol_crlf: bool,
}

impl LineEnds {
    /// The line ends `git config -z --list`'s `listed` sets, the last of each key winning as it does for git: a
    /// `core.autocrlf` is read by [`auto_crlf`], and a `core.eol` that is not `lf` is taken to write CRLF.
    fn of(listed: &str) -> LineEnds {
        let mut autocrlf = AutoCrlf::False;
        let mut eol_crlf = true;
        for entry in listed.split('\0') {
            let (key, value) = entry.split_once('\n').map_or((entry, None), |(key, value)| (key, Some(value)));
            match key {
                "core.autocrlf" => autocrlf = auto_crlf(value),
                "core.eol" => eol_crlf = !value.is_some_and(|value| value.eq_ignore_ascii_case("lf")),
                _ => {}
            }
        }
        LineEnds { autocrlf, eol_crlf }
    }

    /// Whether a path the checkout takes as text is written with CRLF.
    fn text_is_crlf(self) -> bool {
        self.autocrlf == AutoCrlf::True || (self.autocrlf != AutoCrlf::Input && self.eol_crlf)
    }
}

/// How `core.autocrlf`'s `value` sets it, failing toward true: `false`, `no`, `off`, an empty value or a number that
/// is 0 is false, `input` is input, its letter case aside, and anything else, no value included, is true.
fn auto_crlf(value: Option<&str>) -> AutoCrlf {
    let Some(value) = value else {
        return AutoCrlf::True;
    };
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "input" => AutoCrlf::Input,
        "false" | "no" | "off" | "" => AutoCrlf::False,
        number if number.parse::<i64>().is_ok_and(|number| number == 0) => AutoCrlf::False,
        _ => AutoCrlf::True,
    }
}

/// How many times each of `written` is counted toward the apply cap, in its order, by the attributes `attributes`
/// gives every listed path, under the line-end config `ends`. A file whose attributes are missing counts as much as
/// any can.
fn weights(written: &[Written<'_>], attributes: &[Attributes], ends: LineEnds) -> Vec<u64> {
    written
        .iter()
        .map(|file| attributes.get(file.at).map_or(ENCODED_WEIGHT * CRLF_WEIGHT, |given| weight(given, ends)))
        .collect()
}

/// How many times a path with the attributes `given` is counted under the line-end config `ends`: four times with a
/// `working-tree-encoding`, twice when the checkout converts it to CRLF, and both multiplied when it does both.
fn weight(given: &Attributes, ends: LineEnds) -> u64 {
    let encoded = if is_given(&given.encoding) { ENCODED_WEIGHT } else { 1 };
    let crlf = if converts_to_crlf(given, ends) { CRLF_WEIGHT } else { 1 };
    encoded * crlf
}

/// What a path's `text` attribute, or its legacy `crlf` attribute when `text` is not given, makes of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    /// Set: text.
    Text,
    /// Unset: binary, never converted.
    Binary,
    /// `input`: text written with LF.
    Input,
    /// `auto`: text unless git detects binary, which is not done here.
    Auto,
    /// Neither, nor another value: `core.autocrlf` decides.
    Undecided,
}

/// What the attribute value `value` makes of a path, as git reads `text` and `crlf`.
fn text_of(value: &str) -> Reading {
    match value {
        "set" => Reading::Text,
        "unset" => Reading::Binary,
        "input" => Reading::Input,
        "auto" => Reading::Auto,
        _ => Reading::Undecided,
    }
}

/// Whether the checkout may write a path with the attributes `given` with CRLF under the line-end config `ends`: its
/// `eol` is `crlf`; else not when it is binary, `input` or its `eol` is `lf`; else, as text or `auto`, when the config
/// writes text with CRLF, and when its text is undecided, when `core.autocrlf` is true.
fn converts_to_crlf(given: &Attributes, ends: LineEnds) -> bool {
    if given.eol == "crlf" {
        return true;
    }
    let text = match text_of(&given.text) {
        Reading::Undecided => text_of(&given.crlf),
        text => text,
    };
    match text {
        Reading::Binary | Reading::Input => false,
        _ if given.eol == "lf" => false,
        Reading::Text | Reading::Auto => ends.text_is_crlf(),
        Reading::Undecided => ends.autocrlf == AutoCrlf::True,
    }
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
/// their order, each size times its weight in `weights`, summed up to `u64::MAX` at most; else why the answer cannot be
/// read: a line that is not a size, as `<id> missing` is not, or a line count other than the blobs'.
fn total(answer: &str, blobs: &[&str], weights: &[u64]) -> Result<u64, String> {
    let lines: Vec<&str> = answer.lines().collect();
    if lines.len() != blobs.len() {
        return Err(format!("it answered {} line(s) for {} object(s)", lines.len(), blobs.len()));
    }
    if weights.len() != blobs.len() {
        return Err(format!("{} weight(s) were given for {} object(s)", weights.len(), blobs.len()));
    }
    lines.iter().zip(blobs).zip(weights).try_fold(0_u64, |sum, ((line, id), weight)| {
        line.parse::<u64>()
            .map(|size| sum.saturating_add(size.saturating_mul(*weight)))
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

/// What a run whose back commit's objects could not be moved in says it could not keep: the line said before it says
/// why.
const SEE_ABOVE: &str = "see the line above";

/// How the run ends once `back`, which a step before its apply could not take past `failed`, has its objects moved in
/// and is kept; what [`after_keep`] decides, and its line said. Objects that cannot be moved in are not kept: the line
/// saying where they are kept is said first, and the run's ending says the changes could not be kept, see above.
fn kept_after(attempt: &mut Attempt<'_, '_>, git: &Git, snapshot: &str, back: &Back<'_>, failed: End) -> End {
    let kept = match back.quarantine.moved_in(back.quarantine.wait, || {}) {
        Ok(()) => keep(attempt, git, back.commit),
        Err(unmoved) => {
            attempt.say(unmoved_line(&unmoved));
            Err(SEE_ABOVE.to_owned())
        }
    };
    let (end, said) = after_keep(failed, kept, &see_diff(snapshot, back.commit));
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
    let then = line(attempt, git, &["rev-parse", &format!("{}^{{tree}}", snapshot.commit)], &[])?;
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

/// Writes the back commit's changes as a patch into the git folder, read through its quarantine: the names of the
/// files it changes, and the patch, which the caller applies and removes. A patch that could not be written is removed
/// here, and the diff's failure is the one the run ends with.
fn written_patch(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &Back<'_>) -> Result<(Vec<String>, PathBuf), End> {
    let set = back.quarantine.env();
    let names = names(attempt, git, snapshot, back)?;
    let patch = patch_file(attempt, git)?;
    let output = format!("--output={}", patch.display());
    let diff = [
        "diff-tree",
        "-p",
        "--binary",
        "--full-index",
        "-r",
        &output,
        &snapshot.commit,
        back.commit,
    ];
    if let Err(failed) = git_call(attempt, git, &diff, &set) {
        // The diff's failure is the run's; a patch that cannot be removed after it is the lesser problem.
        drop(remove(&patch));
        return Err(failed);
    }
    Ok((names, patch))
}

/// Applies the patch `patch` to the worktree. A patch that did not apply is removed here, and the apply's failure is
/// the one the run ends with.
fn applied(attempt: &Attempt<'_, '_>, git: &Git, patch: &Path) -> Result<(), End> {
    let shown = patch.display().to_string();
    // NTFS protection refuses a path that names `.git` by its 8.3 short name or an alternate stream, whatever the
    // repository's own setting.
    let apply = ["-c", "core.protectNTFS=true", "apply", "--whitespace=nowarn", "--", &shown];
    locked_call(attempt, git, &apply).map(drop).inspect_err(|_| {
        // The apply's failure is the run's; a patch that cannot be removed after it is the lesser problem.
        drop(remove(patch));
    })
}

/// The names of the files the back commit `back` changes, against the snapshot, as git's plumbing lists them.
fn names(attempt: &Attempt<'_, '_>, git: &Git, snapshot: &Snapshot, back: &Back<'_>) -> Result<Vec<String>, End> {
    let args = ["diff-tree", "-r", "-z", "--name-only", &snapshot.commit, back.commit];
    let listed = git_call(attempt, git, &args, &back.quarantine.env())?;
    Ok(listed.split('\0').filter(|name| !name.is_empty()).map(str::to_owned).collect())
}

/// A new, empty file in the git folder for this run's patch, named for the process and a nonce so no other process
/// can name it beforehand; one already there fails the run.
fn patch_file(attempt: &Attempt<'_, '_>, git: &Git) -> Result<PathBuf, End> {
    let name = format!("{PATCH}{}-{}{PATCH_SUFFIX}", std::process::id(), payload::job_nonce());
    let printed = line(attempt, git, &["rev-parse", "--git-path", &name], &[])?;
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

/// Runs git with `args` in the worktree, given the variables `set`, and returns what it wrote on standard output; a
/// failure is the run's.
fn git_call(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str], set: &[(&str, &OsStr)]) -> Result<String, End> {
    let call = git::Call {
        interrupt: attempt.call.interrupt,
        set,
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

/// The first line of what git with `args`, given the variables `set`, wrote on standard output: what every call that
/// asks git for one value reads.
fn line(attempt: &Attempt<'_, '_>, git: &Git, args: &[&str], set: &[(&str, &OsStr)]) -> Result<String, End> {
    git_call(attempt, git, args, set)?
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
/// component whose name starts with `.git`, its letter case aside, a `.lfsconfig` at any depth, which git-lfs reads
/// from the worktree, either named as Windows opens it or by a name shaped as its 8.3 alias, or one the back commit
/// makes a symbolic link or a submodule. `None` when every path may be written.
fn forbidden(listing: &str) -> Option<String> {
    entries(listing).into_iter().find_map(|entry| {
        let link = LINK_MODES.contains(&entry.new_mode);
        entry
            .paths
            .into_iter()
            .find(|path| link || is_git_s(path) || is_lfs_config(path))
            .map(str::to_owned)
    })
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

/// The line said for a back commit that writes `path` through the checkout filter `attribute`, `filter=<driver>` or
/// `ident`, which a sync-back does not take.
fn filtered_line(path: &str, attribute: &str, snapshot: &str, back: &str) -> String {
    format!(
        "gate: remote changes not applied: {} is written through a checkout filter ({}), which a sync-back does not take; {}",
        escaped(path),
        escaped(attribute),
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

/// The `check-attr` slicing at the runner's own cap, which a `test-seams` build never slices with, run by a test build
/// with or without the feature.
#[cfg(test)]
mod output_cap_slices {
    use std::collections::HashSet;

    #[cfg(not(feature = "test-seams"))]
    use super::SLICE_CAP;
    use super::{ATTRIBUTE_ROOM, ATTRIBUTES, attribute_slices, git};

    /// The UTF-16 units of the longest path Windows opens, each at most three bytes in UTF-8: no worktree path is longer.
    const LONGEST_PATH_UNITS: usize = 32_767;

    /// The bytes `git check-attr` is planned to answer for `paths`, by the figure the slicing plans each path with.
    fn planned(paths: &[&str]) -> usize {
        paths.iter().map(|path| ATTRIBUTES.len() * (path.len() + ATTRIBUTE_ROOM)).sum()
    }

    /// Short ASCII paths, short non-ASCII ones and non-ASCII ones near the longest a worktree path can be, all distinct.
    fn mixed_paths() -> Vec<String> {
        (0..180)
            .map(|number| match number % 3 {
                0 => format!("src/file-{number}.rs"),
                1 => format!("données/été-{number}.txt"),
                _ => format!("{number:04}/{}", "日".repeat(LONGEST_PATH_UNITS - 64)),
            })
            .collect()
    }

    #[test]
    fn every_path_lands_in_exactly_one_slice_in_order_at_the_output_cap() {
        let owned = mixed_paths();
        let paths: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert_eq!(paths.iter().collect::<HashSet<_>>().len(), paths.len(), "the paths are distinct");

        let slices = attribute_slices(&paths, git::OUTPUT_CAP);

        let joined = slices.concat();
        assert!(
            joined == paths,
            "the slices join back to {} path(s) of {}, not each once in order",
            joined.len(),
            paths.len()
        );
    }

    #[test]
    fn paths_whose_answers_pass_the_output_cap_take_more_than_one_slice() {
        let owned = mixed_paths();
        let paths: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert!(
            planned(&paths) > git::OUTPUT_CAP,
            "the paths plan {} bytes, within the cap",
            planned(&paths)
        );

        let slices = attribute_slices(&paths, git::OUTPUT_CAP);

        assert!(slices.len() >= 2, "{} slice(s)", slices.len());
    }

    #[test]
    fn each_slice_s_planned_answer_stays_within_the_output_cap() {
        let owned = mixed_paths();
        let paths: Vec<&str> = owned.iter().map(String::as_str).collect();

        for (at, slice) in attribute_slices(&paths, git::OUTPUT_CAP).iter().enumerate() {
            assert!(planned(slice) <= git::OUTPUT_CAP, "slice {at} plans {} bytes", planned(slice));
        }
    }

    #[test]
    fn a_path_whose_answer_alone_passes_the_output_cap_is_a_slice_of_its_own() {
        let huge = "日".repeat(git::OUTPUT_CAP / 3);
        let paths = ["a.txt", huge.as_str(), "été.txt"];
        assert!(planned(&paths[1..2]) > git::OUTPUT_CAP, "the huge path plans within the cap");

        let slices = attribute_slices(&paths, git::OUTPUT_CAP);

        let sizes: Vec<usize> = slices.iter().map(|slice| slice.len()).collect();
        assert!(
            slices == [&paths[..1], &paths[1..2], &paths[2..]],
            "the huge path is not kept alone between its neighbours: slices of {sizes:?} path(s)"
        );
    }

    #[cfg(not(feature = "test-seams"))]
    #[test]
    fn a_build_without_test_seams_slices_at_the_output_cap() {
        assert_eq!(SLICE_CAP, git::OUTPUT_CAP);
    }
}

#[cfg(all(test, feature = "test-seams"))]
#[expect(clippy::expect_used, reason = "a test whose interrupt, transport or git cannot be had must fail loudly")]
mod tests {
    use std::ffi::OsStr;
    use std::fs::{File, FileTimes, OpenOptions};
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use gate_state::StateRoot;
    use gate_win32::Interrupt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES};

    use super::{
        ATTRIBUTE_ROOM, ATTRIBUTES, Attributes, AutoCrlf, Back, FETCH_WORDS, LineEnds, Quarantine, Remote, advertised, after_keep, alternates,
        attribute_slices, auto_crlf, converts_to_crlf, entries, fetch, filtered, forbidden, from_snapshot, has_back, ignored_line, incoming_name,
        incoming_owner, listed_paths, move_plan, names_odd_driver, not_applied_line, read_attributes, receive, refused_end, see_diff, swept,
        synced_line, total, touched_line, unmoved_line, weight, written_blobs, written_line,
    };
    use crate::boxside::payload::escaped;
    use crate::remote::RemoteCall;
    use crate::remote::client::{Attempt, End};
    use crate::remote::git::{self, Git};
    use crate::remote::snapshot::{Snapshot, repository};
    use crate::remote::transport::Local;
    use crate::slots;

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
            outputs_withheld: false,
            log: &log,
            timeout: 60,
            usual: None,
            stall: 60,
            tail: 0,
            no_markers: false,
            bring_tracked: &[],
            interrupt: Some(interrupt),
            state: &state,
            state_dir: Path::new(r"C:\gate-sync-back-tests"),
        };
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(&folder).expect("git is found");
        git.output(&git::Call::plain(&folder, &["init", "--quiet"], git::GIT_LIMIT))
            .expect("the test's folder is made a repository");
        let objects = folder.join(".git").join("objects");
        let quarantine = Quarantine::made(&objects, &incoming_name(std::process::id(), 1, NONCE)).expect("the quarantine is made");
        let remote = Remote {
            url: r"C:\gate-sync-back-tests\no-such.git",
            pack: None,
            ssh: None,
            reference: "refs/gate-back/app-1a2b3c4d",
        };

        let listed = has_back(&mut attempt, &git, &remote);
        std::fs::remove_file(&log).expect("the ls-remote's call opened the log");
        let fetched = fetch(&mut attempt, &git, &remote, &quarantine, 0);

        assert!(matches!(listed, Err(End::Interrupted)), "the ls-remote: {listed:?}");
        assert!(matches!(fetched, Err(End::Interrupted)), "the fetch: {fetched:?}");
        assert!(log.exists(), "the fetch's own git call was started, which opens the log");
        std::fs::remove_dir_all(&folder).expect("the test's folder can be removed");
    }

    /// The longest a move or removal test lets a held file be waited for: a guard against a hang, never a timing bound.
    const HANG_GUARD: Duration = Duration::from_secs(600);
    /// The nonce of every test's quarantine.
    const NONCE: &str = "0123456789ab";

    /// A new, empty object store of the test `name`'s own, holding only its `pack` folder.
    fn store(name: &str) -> PathBuf {
        let objects = std::env::temp_dir().join(format!("gate-quarantine-{name}-{}", std::process::id()));
        if objects.exists() {
            std::fs::remove_dir_all(&objects).expect("an earlier run's store is removed");
        }
        std::fs::create_dir_all(objects.join("pack")).expect("the store can be made");
        objects
    }

    /// The store of the test `name`'s own and this process's quarantine in it, started at 1.
    fn quarantined(name: &str) -> (PathBuf, Quarantine) {
        let objects = store(name);
        let quarantine = Quarantine::made(&objects, &incoming_name(std::process::id(), 1, NONCE)).expect("the quarantine is made");
        (objects, quarantine)
    }

    /// Writes `text` to the file `path`, making its folder.
    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("a file has a folder")).expect("the file's folder can be made");
        std::fs::write(path, text).expect("the file can be written");
    }

    /// The file `path` opened without delete sharing, as a scanner or a live git holds it, which refuses its move.
    fn held_open(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .expect("the file can be held open")
    }

    #[test]
    fn the_quarantine_is_named_for_the_gate_s_pid_start_and_nonce() {
        assert_eq!(
            incoming_name(4242, 1_760_000_000_123, NONCE),
            "incoming-gate-4242-1760000000123-0123456789ab"
        );
        assert_eq!(
            incoming_owner("incoming-gate-4242-1760000000123-0123456789ab"),
            Some((4242, 1_760_000_000_123))
        );
        for name in [
            "incoming-gate-x-1-ab",
            "incoming-gate-4242-1760000000123",
            "incoming-gate-4242-1760000000123-",
            "incoming-gate-4242--ab",
            "incoming-gate-",
            "incoming-1-2-ab",
        ] {
            assert_eq!(incoming_owner(name), None, "{name}");
        }
        let (objects, quarantine) = quarantined("name");
        let folder = objects.join(format!("incoming-gate-{}-1-{NONCE}", std::process::id()));
        assert_eq!(quarantine.folder, folder);
        assert!(folder.is_dir(), "the folder is made");
        assert_eq!(
            quarantine.env(),
            [
                ("GIT_OBJECT_DIRECTORY", folder.as_os_str()),
                ("GIT_ALTERNATE_OBJECT_DIRECTORIES", objects.as_os_str())
            ]
        );
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// The packs go in first, every `.idx` after every other file of the packs and a `.keep` or a `tmp_*` never, then
    /// the loose objects, into a two-digit folder made when missing; a file the store already holds is not replaced,
    /// and its copy stays in the quarantine until the quarantine is removed.
    #[test]
    fn the_move_puts_the_indexes_last_never_a_keep_and_replaces_nothing() {
        let (objects, quarantine) = quarantined("order");
        let folder = quarantine.folder.clone();
        let pack = folder.join("pack");
        for name in [
            "pack-aa.idx",
            "pack-aa.pack",
            "pack-aa.rev",
            "pack-aa.keep",
            "pack-bb.idx",
            "pack-bb.pack",
            "tmp_pack_x1",
        ] {
            put(&pack.join(name), name);
        }
        put(&folder.join("ab").join("cdef"), "new loose");
        put(&folder.join("12").join("3456"), "loose copy");
        put(&objects.join("12").join("3456"), "loose held");
        put(&objects.join("pack").join("pack-bb.pack"), "pack held");

        let plan: Vec<PathBuf> = move_plan(&folder, &objects)
            .expect("the quarantine can be listed")
            .into_iter()
            .map(|(source, _)| source)
            .collect();
        let moved = quarantine.moved_in(HANG_GUARD, || {});

        let sources = ["pack-aa.pack", "pack-aa.rev", "pack-bb.pack", "pack-aa.idx", "pack-bb.idx"].map(|name| pack.join(name));
        let loose = [folder.join("12").join("3456"), folder.join("ab").join("cdef")];
        assert_eq!(plan, [&sources[..], &loose[..]].concat());
        assert_eq!(moved, Ok(()));
        let read = |path: PathBuf| std::fs::read_to_string(path).expect("the file is there");
        for name in ["pack-aa.pack", "pack-aa.rev", "pack-aa.idx", "pack-bb.idx"] {
            assert_eq!(read(objects.join("pack").join(name)), name, "{name} went in");
        }
        assert!(!objects.join("pack").join("pack-aa.keep").exists(), "a .keep never goes in");
        assert!(pack.join("pack-aa.keep").exists(), "the .keep stays for the quarantine's removal");
        assert_eq!(
            read(objects.join("pack").join("pack-bb.pack")),
            "pack held",
            "a pack the store holds is not replaced"
        );
        assert_eq!(
            read(objects.join("ab").join("cdef")),
            "new loose",
            "a loose object went in, its folder made"
        );
        assert_eq!(
            read(objects.join("12").join("3456")),
            "loose held",
            "a loose object the store holds is not replaced"
        );
        assert!(folder.join("12").join("3456").exists(), "its copy is left to the quarantine's removal");
        assert_eq!(quarantine.removed(HANG_GUARD, || {}), None);
        assert!(!folder.exists(), "the quarantine is removed whole");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A file held open when the move first tries it is let go at its first wait: it is moved then.
    #[test]
    fn a_file_let_go_while_the_move_waits_is_moved() {
        let (objects, quarantine) = quarantined("let-go");
        let source = quarantine.folder.join("pack").join("pack-cc.pack");
        put(&source, "pack");
        let mut held = Some(held_open(&source));
        let mut waits = 0;

        let moved = quarantine.moved_in(HANG_GUARD, || {
            waits += 1;
            held.take();
        });

        assert_eq!(moved, Ok(()));
        assert!(objects.join("pack").join("pack-cc.pack").exists(), "the pack went in");
        assert!(waits >= 1, "the move waited for the held file");
        assert_eq!(quarantine.removed(HANG_GUARD, || {}), None);
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A pack held open past the move's wait stops the move before its index goes in: the quarantine is marked kept and
    /// is not removed, and the reason names the file and the folder it is kept in.
    #[test]
    fn a_file_held_past_the_wait_keeps_the_quarantine_and_says_where() {
        let (objects, quarantine) = quarantined("held");
        let folder = quarantine.folder.clone();
        let source = folder.join("pack").join("pack-dd.pack");
        put(&source, "pack");
        put(&folder.join("pack").join("pack-dd.idx"), "idx");
        let handle = held_open(&source);

        let moved = quarantine.moved_in(Duration::from_millis(50), || {});

        let why = moved.expect_err("a file held past the wait cannot move");
        let start = format!(
            "the fetched objects could not be moved into the repository: {}: ",
            escaped(&Path::new("pack").join("pack-dd.pack").display().to_string())
        );
        assert!(why.starts_with(&start), "{why}");
        let end = format!("(os error 32); they are kept in {}", escaped(&folder.display().to_string()));
        assert!(why.ends_with(&end), "{why}");
        assert_eq!(unmoved_line(&why), format!("gate: remote changes not applied: {why}"));
        assert!(
            !objects.join("pack").join("pack-dd.idx").exists(),
            "the index never goes in before its pack"
        );
        assert!(folder.join("gate-kept").exists(), "the quarantine is marked kept");
        drop(handle);
        assert_eq!(
            quarantine.removed(HANG_GUARD, || {}),
            None,
            "a kept quarantine is not removed, and nothing is said"
        );
        assert!(source.exists(), "the kept quarantine holds the pack");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A pid no process has: Windows pids are multiples of 4.
    const DEAD_PID: u32 = u32::MAX - 1;
    /// A day.
    const DAY: Duration = Duration::from_hours(24);

    /// A folder `name` in the store `objects` holding a pack: the folder.
    fn leftover(objects: &Path, name: &str) -> PathBuf {
        let folder = objects.join(name);
        put(&folder.join("pack").join("pack-ee.pack"), "pack");
        folder
    }

    /// Marks the quarantine `folder` kept, its mark last written at `at`.
    fn marked(folder: &Path, at: SystemTime) {
        let mark = folder.join("gate-kept");
        std::fs::write(&mark, "").expect("the mark can be written");
        let file = OpenOptions::new().write(true).open(&mark).expect("the mark can be opened");
        file.set_modified(at).expect("the mark's last write can be set");
    }

    /// The sweep removes the quarantine of a gate whose pid no process has, and of one whose pid a process started at
    /// another time has, and a kept one whose mark is over 7 days old; it leaves this live gate's own, a kept one whose
    /// mark is younger, and every folder whose name does not read as a quarantine's, and says nothing.
    #[test]
    fn the_sweep_removes_the_quarantines_of_gates_that_ended_and_leaves_the_rest() {
        let objects = store("sweep");
        let pid = std::process::id();
        let start = slots::epoch_ms(gate_win32::process_start().expect("this process's start can be read"));
        let now = SystemTime::now();
        let dead = leftover(&objects, &incoming_name(DEAD_PID, 1, NONCE));
        let reused = leftover(&objects, &incoming_name(pid, start + 1, NONCE));
        let live = leftover(&objects, &incoming_name(pid, start, NONCE));
        let young = leftover(&objects, &incoming_name(DEAD_PID, 2, NONCE));
        marked(&young, now - 6 * DAY);
        let old = leftover(&objects, &incoming_name(DEAD_PID, 3, NONCE));
        marked(&old, now - 8 * DAY);
        let unreadable = leftover(&objects, &format!("incoming-gate-x-1-{NONCE}"));
        let other = leftover(&objects, "info");

        let said = swept(&objects, now, slots::gate_liveness);

        assert_eq!(said, Vec::<String>::new(), "a removal says nothing");
        assert!(!dead.exists(), "a dead pid's quarantine is removed");
        assert!(!reused.exists(), "a quarantine whose pid another process now has is removed");
        assert!(live.exists(), "a live gate's quarantine is left");
        assert!(young.exists(), "a kept quarantine under 7 days old is left");
        assert!(!old.exists(), "a kept quarantine over 7 days old is removed");
        assert!(unreadable.exists(), "a name that does not read as a quarantine's is left");
        assert!(other.exists(), "a folder that is no quarantine is left");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A dead gate's quarantine holding a file another process holds open cannot be removed: one line says so.
    #[test]
    fn a_quarantine_the_sweep_cannot_remove_is_said_once() {
        let objects = store("sweep-held");
        let folder = leftover(&objects, &incoming_name(DEAD_PID, 1, NONCE));
        put(&folder.join("pack").join("pack-ee.idx"), "idx");
        let handle = held_open(&folder.join("pack").join("pack-ee.pack"));

        let said = swept(&objects, SystemTime::now(), slots::gate_liveness);

        let start = format!("gate: remote could not remove {}: ", escaped(&folder.display().to_string()));
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with(&start), "{said:?}");
        assert!(folder.exists(), "the folder stays");
        drop(handle);
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A quarantine whose `pack` is a file cannot be listed: the move fails as any move that fails does, the folder
    /// kept and marked, and the reason says the listing failed.
    #[test]
    fn a_quarantine_that_cannot_be_listed_is_kept_as_a_failed_move() {
        let (objects, quarantine) = quarantined("unlisted");
        let folder = quarantine.folder.clone();
        put(&folder.join("pack"), "not a folder");

        let moved = quarantine.moved_in(HANG_GUARD, || {});

        let why = moved.expect_err("a quarantine that cannot be listed cannot be moved");
        let start = "the fetched objects could not be moved into the repository: cannot list the quarantine: ";
        assert!(why.starts_with(start), "{why}");
        let end = format!("; they are kept in {}", escaped(&folder.display().to_string()));
        assert!(why.ends_with(&end), "{why}");
        assert!(folder.join("gate-kept").exists(), "the quarantine is marked kept");
        assert_eq!(quarantine.removed(HANG_GUARD, || {}), None);
        assert!(folder.exists(), "a kept quarantine is not removed");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A pack's index held open past the wait stops the move after its other files went in: they are returned to the
    /// quarantine, so the store holds no file of the pack, and the quarantine is kept whole.
    #[test]
    fn a_move_that_fails_part_way_returns_what_it_moved() {
        let (objects, quarantine) = quarantined("rollback");
        let folder = quarantine.folder.clone();
        let pack = folder.join("pack");
        let names = ["pack-ff.pack", "pack-ff.rev", "pack-ff.idx"];
        for name in names {
            put(&pack.join(name), name);
        }
        put(&folder.join("ab").join("cdef"), "loose");
        let handle = held_open(&pack.join("pack-ff.idx"));

        let moved = quarantine.moved_in(Duration::from_millis(50), || {});

        drop(handle);
        let why = moved.expect_err("an index held past the wait cannot move");
        assert!(why.contains("pack-ff.idx"), "{why}");
        assert!(!why.contains("could not be returned"), "{why}");
        let store: Vec<_> = std::fs::read_dir(objects.join("pack"))
            .expect("the store's pack folder can be listed")
            .collect();
        assert!(store.is_empty(), "the store holds no file of the pack: {store:?}");
        for name in names {
            assert!(pack.join(name).exists(), "{name} is in the quarantine");
        }
        assert!(!objects.join("ab").join("cdef").exists(), "no loose object went in");
        assert!(folder.join("gate-kept").exists(), "the quarantine is marked kept");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// A quarantine folder that is gone by the move fails it: nothing is kept, and the reason says the folder is gone.
    #[test]
    fn a_quarantine_that_is_gone_fails_the_move() {
        let (objects, quarantine) = quarantined("gone");
        std::fs::remove_dir(&quarantine.folder).expect("the quarantine can be removed");

        let moved = quarantine.moved_in(HANG_GUARD, || {});

        assert_eq!(
            moved,
            Err("the fetched objects could not be moved into the repository: the quarantine folder is gone".to_owned())
        );
        assert_eq!(quarantine.removed(HANG_GUARD, || {}), None, "a folder already gone counts as removed");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// Sets the creation time of the folder `folder` to `at`.
    fn created(folder: &Path, at: SystemTime) {
        let handle = OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(folder)
            .expect("the folder can be opened");
        handle
            .set_times(FileTimes::new().set_created(at))
            .expect("the folder's creation time can be set");
    }

    /// A kept quarantine whose mark was last written in the future is aged by its folder's creation instead: one made 8
    /// days ago is removed, one made now is left.
    #[test]
    fn a_kept_quarantine_whose_mark_lies_in_the_future_is_aged_by_its_folder() {
        let objects = store("sweep-future");
        let now = SystemTime::now();
        let old = leftover(&objects, &incoming_name(DEAD_PID, 1, NONCE));
        marked(&old, now + DAY);
        created(&old, now - 8 * DAY);
        let young = leftover(&objects, &incoming_name(DEAD_PID, 2, NONCE));
        marked(&young, now + DAY);

        let said = swept(&objects, now, slots::gate_liveness);

        assert_eq!(said, Vec::<String>::new());
        assert!(!old.exists(), "a folder made 8 days ago is removed");
        assert!(young.exists(), "a folder made now is left");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
    }

    /// The sweep leaves at most 3 kept quarantines of gates that ended, the newest, whatever their age; a live gate's
    /// kept quarantine is neither touched nor counted.
    #[test]
    fn the_sweep_leaves_the_three_newest_kept_quarantines() {
        let objects = store("sweep-most");
        let now = SystemTime::now();
        let kept: Vec<PathBuf> = (1..=5_u64)
            .map(|days| {
                let folder = leftover(&objects, &incoming_name(DEAD_PID, days, NONCE));
                marked(&folder, now - u32::try_from(days).expect("a few days") * DAY);
                folder
            })
            .collect();
        let start = slots::epoch_ms(gate_win32::process_start().expect("this process's start can be read"));
        let live = leftover(&objects, &incoming_name(std::process::id(), start, NONCE));
        marked(&live, now);

        let said = swept(&objects, now, slots::gate_liveness);

        assert_eq!(said, Vec::<String>::new());
        let left: Vec<bool> = kept.iter().map(|folder| folder.exists()).collect();
        assert_eq!(left, [true, true, true, false, false], "the two oldest are removed");
        assert!(live.exists(), "a live gate's quarantine is left");
        std::fs::remove_dir_all(&objects).expect("the test's store can be removed");
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

    /// The blob ids of the files the `diff-tree -r -z --raw` `listing` writes, in its order.
    fn ids(listing: &str) -> Vec<&str> {
        written_blobs(listing).iter().map(|file| file.id).collect()
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

        let blobs = ids(&listing);

        assert_eq!(blobs, [shared, shared, other, other], "per path, the deletion left out");
        assert_eq!(total("5\n5\n7\n7\n", &blobs, &[1; 4]), Ok(24), "a blob two paths share counts twice");
        assert_eq!(ids(""), Vec::<&str>::new(), "no change writes nothing");
        assert_eq!(total("", &[], &[]), Ok(0));
        assert_eq!(
            total("18446744073709551615\n1\n", &[shared, other], &[1, 1]),
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
        assert_eq!(ids(&listing), [first], "the crafted name is a file the first entry writes");
    }

    #[test]
    fn an_answer_that_is_not_a_size_for_each_blob_is_never_a_size() {
        let id = "1111111111111111111111111111111111111111";
        assert_eq!(
            total(&format!("{id} missing\n"), &[id], &[1]),
            Err(format!("it answered '{id} missing' for {id}"))
        );
        assert_eq!(total("5\n", &[id, id], &[1, 1]), Err("it answered 1 line(s) for 2 object(s)".to_owned()));
        assert_eq!(total("5\n5\n", &[id], &[1]), Err("it answered 2 line(s) for 1 object(s)".to_owned()));
        assert_eq!(total("-1\n", &[id], &[1]), Err(format!("it answered '-1' for {id}")));
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

    /// The `-c` words that let a test's repository commit whatever the user's own configuration says.
    const COMMITTER: [&str; 6] = [
        "-c",
        "user.name=gate",
        "-c",
        "user.email=gate@example.invalid",
        "-c",
        "commit.gpgsign=false",
    ];

    /// git with `args` in `root`, given the variables `set`: what it printed, or why it failed.
    fn asked(root: &Path, set: &[(&str, &OsStr)], args: &[&str]) -> Result<String, git::GitError> {
        let git = Git::find(root).expect("git is found");
        git.output(&git::Call {
            set,
            ..git::Call::plain(root, args, git::GIT_LIMIT)
        })
    }

    /// Every file under `objects` but those in the folder `left_out`, sorted.
    fn stored(objects: &Path, left_out: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut folders = vec![objects.to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).expect("a folder of the store can be listed") {
                let path = entry.expect("an entry can be read").path();
                if path == left_out {
                    continue;
                }
                if path.is_dir() {
                    folders.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    /// A fetch run with the quarantine's two variables writes nothing into the real store, and the commit it brought
    /// is read through them alone: `FETCH_HEAD` names it, the real store lacks it, and the quarantine holds it.
    #[test]
    fn a_fetch_into_the_quarantine_leaves_the_real_store_alone_and_is_read_through_it() {
        let builder = repository("quarantine-builder");
        asked(&builder, &[], &["add", "-A"]).expect("the builder's file is added");
        asked(&builder, &[], &[&COMMITTER[..], &["commit", "--quiet", "-m", "back"]].concat()).expect("the builder commits");
        let client = repository("quarantine-client");
        let objects = client.join(".git").join("objects");
        let folder = objects.join("incoming-gate-test");
        std::fs::create_dir(&folder).expect("the quarantine can be made");
        let before = stored(&objects, &folder);
        let real = alternates(&objects);
        let set = [
            ("GIT_OBJECT_DIRECTORY", folder.as_os_str()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", real.as_os_str()),
        ];
        let url = builder.to_string_lossy();

        asked(&client, &set, &["fetch", "--quiet", "--no-tags", "--", &url, "HEAD"]).expect("the fetch goes through");

        assert_eq!(stored(&objects, &folder), before, "the real store is as it was");
        let back = asked(&client, &[], &["rev-parse", "FETCH_HEAD"]).expect("FETCH_HEAD names the commit");
        let back = back.trim_end();
        assert!(asked(&client, &[], &["cat-file", "-e", back]).is_err(), "the real store lacks {back}");
        assert_eq!(
            asked(&client, &set, &["cat-file", "-e", back]),
            Ok(String::new()),
            "the quarantine holds {back}"
        );
        std::fs::remove_dir_all(&builder).expect("the builder can be removed");
        std::fs::remove_dir_all(&client).expect("the client can be removed");
    }

    /// git reads a C-quoted entry of `GIT_ALTERNATE_OBJECT_DIRECTORIES` whole, so a real store whose path holds the
    /// list's separator `;` is still read through it; unquoted, the path splits there and the object is not found.
    #[test]
    fn an_alternate_quoted_for_a_path_holding_a_semicolon_is_honoured() {
        let root = repository("alternates;semicolon");
        let objects = root.join(".git").join("objects");
        let id = asked(&root, &[], &["hash-object", "-w", "a.txt"]).expect("the file is stored");
        let id = id.trim_end();
        let folder = objects.join("incoming-gate-test");
        std::fs::create_dir(&folder).expect("the quarantine can be made");
        let quoted = alternates(&objects);
        let through = |alternate: &OsStr| {
            let set = [
                ("GIT_OBJECT_DIRECTORY", folder.as_os_str()),
                ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate),
            ];
            asked(&root, &set, &["cat-file", "-e", id])
        };

        assert!(quoted.to_string_lossy().starts_with('"'), "a path holding ';' is quoted: {quoted:?}");
        assert_eq!(through(&quoted), Ok(String::new()), "the quoted store is read");
        assert!(through(objects.as_os_str()).is_err(), "unquoted, the path splits at its ';'");
        assert_eq!(
            alternates(Path::new(r"C:\work\app\.git\objects")),
            OsStr::new(r"C:\work\app\.git\objects"),
            "no ';', no quotes"
        );
        std::fs::remove_dir_all(&root).expect("the repository can be removed");
    }

    /// The work-folder key of every wiring test's call.
    const KEY: &str = "app-1a2b3c4d";

    /// The call of a wiring test in the repository `root`, its lines written to `log`.
    fn call_in<'a>(root: &'a str, log: &'a Path, state: &'a StateRoot) -> RemoteCall<'a> {
        RemoteCall {
            host: "builder",
            key: KEY,
            root,
            repo: "app",
            subfolder: "",
            words: &[],
            outputs: &[],
            outputs_withheld: false,
            log,
            timeout: 60,
            usual: None,
            stall: 60,
            tail: 0,
            no_markers: false,
            bring_tracked: &[],
            interrupt: None,
            state,
            state_dir: Path::new(r"C:\gate-sync-back-tests"),
        }
    }

    /// The log of the wiring test `name`, outside its repository, and none there yet.
    fn test_log(name: &str) -> PathBuf {
        let log = std::env::temp_dir().join(format!("gate-sync-back-{name}-{}.log", std::process::id()));
        if log.exists() {
            std::fs::remove_file(&log).expect("an earlier run's log is removed");
        }
        log
    }

    /// The repository of the test `name`'s own whose one commit is the snapshot, and this process's quarantine in it,
    /// which waits 50 ms for a held file and holds a back commit of the snapshot's own tree: the repository, the
    /// snapshot, the back commit and the quarantine.
    fn backed(name: &str) -> (PathBuf, Snapshot, String, Quarantine) {
        let root = repository(name);
        asked(&root, &[], &["add", "-A"]).expect("the file is added");
        asked(&root, &[], &[&COMMITTER[..], &["commit", "--quiet", "-m", "snapshot"]].concat()).expect("the snapshot is committed");
        let snapshot = asked(&root, &[], &["rev-parse", "HEAD"]).expect("HEAD is read").trim_end().to_owned();
        let objects = root.join(".git").join("objects");
        let mut quarantine = Quarantine::made(&objects, &incoming_name(std::process::id(), 1, NONCE)).expect("the quarantine is made");
        quarantine.wait = Duration::from_millis(50);
        let tree = format!("{snapshot}^{{tree}}");
        let words = [&COMMITTER[..], &["commit-tree", "-p", &snapshot, "-m", "back", &tree]].concat();
        let back = asked(&root, &quarantine.env(), &words).expect("the back commit is made");
        let snapshot = Snapshot {
            commit: snapshot,
            bridge: None,
        };
        (root, snapshot, back.trim_end().to_owned(), quarantine)
    }

    /// How a sync-back whose move failed ends: `ended` is a passing run's 1, the repository `root` holds no kept ref, the
    /// quarantine stays marked after the sync-back's removal, and the log `log` says one line of where it is kept.
    fn assert_unmoved(ended: &Result<End, End>, root: &Path, quarantine: &Quarantine, log: &Path) {
        assert!(matches!(ended, Ok(End::Status(1))), "{ended:?}");
        let kept = format!("refs/gate-remote/{KEY}-back");
        assert!(asked(root, &[], &["rev-parse", "--verify", "--quiet", &kept]).is_err(), "no kept ref");
        assert_eq!(quarantine.removed(HANG_GUARD, || {}), None, "the sync-back's removal says nothing");
        assert!(quarantine.folder.join("gate-kept").exists(), "the quarantine stays, marked kept");
        let said = std::fs::read_to_string(log).expect("the log was written");
        let owner = "gate: remote changes not applied: the fetched objects could not be moved into the repository: ";
        assert_eq!(said.lines().filter(|line| line.starts_with(owner)).count(), 1, "{said}");
    }

    /// The back commit `back`'s loose object in the quarantine `folder`.
    fn loose_object(folder: &Path, back: &str) -> PathBuf {
        folder.join(&back[..2]).join(&back[2..])
    }

    /// A back commit that may be applied, whose objects cannot be moved in as one is held open past the wait, is
    /// neither applied nor kept.
    #[test]
    fn a_back_commit_whose_objects_cannot_be_moved_in_is_not_applied() {
        let (root, snapshot, back, quarantine) = backed("unmoved-receive");
        let shown = root.to_string_lossy().into_owned();
        let log = test_log("unmoved-receive");
        let state = StateRoot::new(r"C:\gate-sync-back-tests", None);
        let call = call_in(&shown, &log, &state);
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(&root).expect("git is found");
        let handle = held_open(&loose_object(&quarantine.folder, &back));

        let back_of = Back {
            commit: &back,
            quarantine: &quarantine,
        };
        let ended = receive(&mut attempt, &git, &snapshot, &back_of, 0);

        drop(handle);
        assert_unmoved(&ended, &root, &quarantine, &log);
        assert!(
            !std::fs::read_to_string(&log).expect("the log").contains("synced back"),
            "nothing is applied"
        );
        std::fs::remove_dir_all(&root).expect("the repository can be removed");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    /// A refused back commit whose objects cannot be moved in, as one is held open past the wait, is not kept, and the
    /// refusal's own line is not said.
    #[test]
    fn a_refused_back_commit_whose_objects_cannot_be_moved_in_is_not_kept() {
        let (root, _, back, quarantine) = backed("unmoved-refused");
        let shown = root.to_string_lossy().into_owned();
        let log = test_log("unmoved-refused");
        let state = StateRoot::new(r"C:\gate-sync-back-tests", None);
        let call = call_in(&shown, &log, &state);
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(&root).expect("git is found");
        let handle = held_open(&loose_object(&quarantine.folder, &back));

        let back_of = Back {
            commit: &back,
            quarantine: &quarantine,
        };
        let ended = refused_end(&mut attempt, &git, &back_of, "gate: the refusal's own line".to_owned(), 0);

        drop(handle);
        assert_unmoved(&ended, &root, &quarantine, &log);
        assert!(
            !std::fs::read_to_string(&log).expect("the log").contains("the refusal's own line"),
            "the refusal is not said"
        );
        std::fs::remove_dir_all(&root).expect("the repository can be removed");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    /// A fetch whose first try fails before the banner is tried again with the same variables, so the retry writes into
    /// the same quarantine: the commit it brought is read through it, and the real store lacks it. The first try's
    /// pack command fails as ssh does when the builder closes the connection before its banner.
    #[test]
    fn a_fetch_retried_before_the_banner_writes_into_the_same_quarantine() {
        let builder = repository("retry-builder");
        asked(&builder, &[], &["add", "-A"]).expect("the builder's file is added");
        asked(&builder, &[], &[&COMMITTER[..], &["commit", "--quiet", "-m", "back"]].concat()).expect("the builder commits");
        let client = repository("retry-client");
        let objects = client.join(".git").join("objects");
        let quarantine = Quarantine::made(&objects, &incoming_name(std::process::id(), 1, NONCE)).expect("the quarantine is made");
        let marker = std::env::temp_dir().join(format!("gate-sync-back-retry-{}", std::process::id()));
        if marker.exists() {
            std::fs::remove_file(&marker).expect("an earlier run's marker is removed");
        }
        let at = marker.to_string_lossy().replace('\\', "/");
        let pack = format!(
            "f() {{ if [ -e '{at}' ]; then git-upload-pack \"$@\"; else : > '{at}'; \
             echo 'kex_exchange_identification: read: Connection reset by peer' >&2; exit 128; fi; }}; f"
        );
        let url = builder.to_string_lossy().into_owned();
        let remote = Remote {
            url: &url,
            pack: Some(&pack),
            ssh: None,
            reference: "HEAD",
        };
        let shown = client.to_string_lossy().into_owned();
        let log = test_log("retry");
        let state = StateRoot::new(r"C:\gate-sync-back-tests", None);
        let call = call_in(&shown, &log, &state);
        let transport = Local::new(r"local:C:\gate-sync-back-tests", None).expect("a local transport");
        let mut attempt = Attempt::for_tests(&call, &transport);
        let git = Git::find(&client).expect("git is found");

        let fetched = fetch(&mut attempt, &git, &remote, &quarantine, 0);

        assert!(matches!(fetched, Ok(None)), "{fetched:?}");
        let said = std::fs::read_to_string(&log).expect("the log was written");
        assert!(said.contains("gate: builder did not answer the ssh banner; trying once more"), "{said}");
        let back = asked(&client, &[], &["rev-parse", "FETCH_HEAD"]).expect("FETCH_HEAD names the commit");
        let back = back.trim_end();
        assert!(asked(&client, &[], &["cat-file", "-e", back]).is_err(), "the real store lacks {back}");
        assert_eq!(
            asked(&client, &quarantine.env(), &["cat-file", "-e", back]),
            Ok(String::new()),
            "the quarantine holds {back}"
        );
        let quarantines = std::fs::read_dir(&objects)
            .expect("the store can be listed")
            .filter(|entry| {
                let entry = entry.as_ref().expect("an entry can be read");
                entry.file_name().to_string_lossy().starts_with("incoming-gate-")
            })
            .count();
        assert_eq!(quarantines, 1, "no other quarantine was made");
        std::fs::remove_dir_all(&builder).expect("the builder can be removed");
        std::fs::remove_dir_all(&client).expect("the client can be removed");
        std::fs::remove_file(&marker).expect("the marker can be removed");
        std::fs::remove_file(&log).expect("the log can be removed");
    }

    /// Every attribute of a path not given: what `git check-attr` says of a path no attribute file names.
    const PLAIN: [&str; 6] = ["unspecified"; 6];

    /// The attributes `values` give, in [`ATTRIBUTES`]' order.
    fn given(values: [&str; 6]) -> Attributes {
        let [filter, ident, encoding, text, eol, crlf] = values.map(str::to_owned);
        Attributes {
            filter,
            ident,
            encoding,
            text,
            eol,
            crlf,
        }
    }

    /// What `git check-attr -z` answers for each path and its values, in [`ATTRIBUTES`]' order.
    fn answer(paths: &[(&str, [&str; 6])]) -> String {
        let mut text = String::new();
        for (path, values) in paths {
            for (name, value) in ATTRIBUTES.iter().zip(values) {
                text.extend([*path, "\0", name, "\0", value, "\0"]);
            }
        }
        text
    }

    #[test]
    fn check_attr_s_answer_is_read_path_by_path_and_attribute_by_attribute() {
        let lfs = ["lfs", "unspecified", "unspecified", "unset", "unspecified", "unspecified"];
        let whole = answer(&[("a.txt", PLAIN), ("b.lfs", lfs)]);

        assert_eq!(read_attributes(&whole, &["a.txt", "b.lfs"]), Ok(vec![given(PLAIN), given(lfs)]));
        assert_eq!(read_attributes("", &[]), Ok(vec![]), "no path, no answer");
        assert_eq!(
            read_attributes(&answer(&[("c.txt", lfs)]), &["c.txt"]),
            Ok(vec![given(lfs)]),
            "a second slice is read on its own"
        );
        assert_eq!(
            read_attributes(&whole, &["a.txt", "other"]),
            Err("it did not answer filter for other".to_owned()),
            "the wrong path"
        );
        assert_eq!(
            read_attributes(&whole.replacen("\0ident\0", "\0idents\0", 1), &["a.txt", "b.lfs"]),
            Err("it did not answer ident for a.txt".to_owned()),
            "the wrong attribute name"
        );
        assert_eq!(
            read_attributes(&answer(&[("a.txt", PLAIN)]), &["a.txt", "b.lfs"]),
            Err("it did not answer filter for b.lfs".to_owned()),
            "a short answer"
        );
        let cut = whole
            .strip_suffix("b.lfs\0crlf\0unspecified\0")
            .expect("the answer ends with b.lfs's crlf");
        assert_eq!(
            read_attributes(cut, &["a.txt", "b.lfs"]),
            Err("it did not answer crlf for b.lfs".to_owned()),
            "an answer cut short"
        );
        assert_eq!(
            read_attributes(&whole, &["a.txt"]),
            Err("it answered more than the 1 path(s) it was asked".to_owned()),
            "a surplus path"
        );
    }

    #[test]
    fn the_paths_fed_to_check_attr_are_cut_into_slices_that_fit_the_cap() {
        let cost = ATTRIBUTES.len() * (1 + ATTRIBUTE_ROOM);
        let paths = ["a", "b", "c"];

        assert_eq!(attribute_slices(&[], cost), Vec::<&[&str]>::new(), "no path, no slice");
        assert_eq!(attribute_slices(&paths, 3 * cost), [&paths[..]], "exactly at the cap: one slice");
        assert_eq!(
            attribute_slices(&paths, 3 * cost - 1),
            [&paths[..2], &paths[2..]],
            "a byte past the cap: the last path goes on"
        );
        let long = "x".repeat(10 * cost);
        let mixed = ["a", long.as_str(), "b"];
        assert_eq!(
            attribute_slices(&mixed, 2 * cost),
            [&mixed[..1], &mixed[1..2], &mixed[2..]],
            "a path over the cap alone is a slice of its own"
        );
        let many: Vec<String> = (0..50).map(|number| format!("path-{number}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let slices = attribute_slices(&many, 4 * cost);
        assert!(slices.len() > 1, "{slices:?}");
        assert_eq!(slices.concat(), many, "the slices join back in order");
    }

    #[test]
    fn the_line_end_config_takes_each_key_s_last_value() {
        let native = LineEnds {
            autocrlf: AutoCrlf::False,
            eol_crlf: true,
        };
        assert_eq!(LineEnds::of(""), native, "nothing set: no autocrlf, native CRLF");
        assert_eq!(
            LineEnds::of("core.autocrlf\ntrue\0core.eol\nlf\0core.autocrlf\nfalse\0core.eol\ncrlf\0"),
            native,
            "the last of each key wins"
        );
        assert_eq!(
            LineEnds::of("core.autocrlf\0core.eol\0"),
            LineEnds {
                autocrlf: AutoCrlf::True,
                eol_crlf: true
            },
            "valueless keys"
        );
        assert_eq!(
            LineEnds::of("core.eol\nLF\0core.autocrlf\nINPUT\0"),
            LineEnds {
                autocrlf: AutoCrlf::Input,
                eol_crlf: false
            },
            "values in any letter case"
        );
        assert_eq!(LineEnds::of("user.name\nlf\0core.eolx\nlf\0core.autocrlfx\ntrue\0"), native, "other keys");
    }

    #[test]
    fn core_autocrlf_fails_toward_true() {
        for (value, read) in [
            (None, AutoCrlf::True),
            (Some("yes"), AutoCrlf::True),
            (Some("on"), AutoCrlf::True),
            (Some("1"), AutoCrlf::True),
            (Some("-1"), AutoCrlf::True),
            (Some("0x1"), AutoCrlf::True),
            (Some("1k"), AutoCrlf::True),
            (Some("0"), AutoCrlf::False),
            (Some(""), AutoCrlf::False),
            (Some("FALSE"), AutoCrlf::False),
            (Some("no"), AutoCrlf::False),
            (Some("off"), AutoCrlf::False),
            (Some("INPUT"), AutoCrlf::Input),
        ] {
            assert_eq!(auto_crlf(value), read, "{value:?}");
        }
    }

    #[test]
    fn a_path_counts_as_its_checkout_converts_it() {
        let ends = |autocrlf, eol_crlf| LineEnds { autocrlf, eol_crlf };
        let (auto, lf, native, input) = (
            ends(AutoCrlf::True, false),
            ends(AutoCrlf::False, false),
            ends(AutoCrlf::False, true),
            ends(AutoCrlf::Input, true),
        );
        let u = "unspecified";
        for (values, line_ends, counted) in [
            (PLAIN, lf, 1),
            (PLAIN, auto, 2),
            (PLAIN, native, 1),
            ([u, u, u, "set", u, u], native, 2),
            ([u, u, u, "set", u, u], lf, 1),
            ([u, u, u, "set", u, u], input, 1),
            ([u, u, u, "auto", u, u], native, 2),
            ([u, u, u, "unset", u, u], auto, 1),
            ([u, u, u, "input", u, u], auto, 1),
            ([u, u, u, "set", "lf", u], auto, 1),
            ([u, u, u, u, "lf", u], auto, 1),
            ([u, u, u, u, "crlf", u], lf, 2),
            ([u, u, u, "unset", "crlf", u], lf, 2),
            ([u, u, u, u, u, "set"], native, 2),
            ([u, u, u, u, u, "unset"], auto, 1),
            ([u, u, u, u, u, "input"], auto, 1),
            ([u, u, u, "set", u, "unset"], native, 2),
            ([u, u, "UTF-16", u, u, u], lf, 4),
            ([u, u, "UTF-16", "set", u, u], native, 8),
            ([u, u, "UTF-16", u, "crlf", u], lf, 8),
        ] {
            let attributes = given(values);
            assert_eq!(weight(&attributes, line_ends), counted, "{values:?} under {line_ends:?}");
            assert_eq!(
                converts_to_crlf(&attributes, line_ends),
                matches!(counted, 2 | 8),
                "{values:?} under {line_ends:?}"
            );
        }
    }

    #[test]
    fn a_filter_any_driver_or_none_or_ident_refuses_the_first_path_it_is_on() {
        let u = "unspecified";
        let paths = ["a", "b", "c"];
        let refused = |values: [&str; 6], odd: bool| filtered(&paths, &[given(PLAIN), given(values), given(PLAIN)], odd);

        assert_eq!(refused(PLAIN, false), None, "no filter");
        assert_eq!(refused(["foo", u, u, u, u, u], false), Some(("b", "filter=foo".to_owned())));
        assert_eq!(refused(["set", u, u, u, u, u], false), Some(("b", "filter (no driver)".to_owned())));
        assert_eq!(refused([u, "set", u, u, u, u], false), Some(("b", "ident".to_owned())));
        assert_eq!(
            refused([u, "foo", u, u, u, u], false),
            Some(("b", "ident".to_owned())),
            "an ident with a value"
        );
        assert_eq!(refused(["unset", u, u, u, u, u], false), None, "a filter unset");
        assert_eq!(
            refused(["unset", u, u, u, u, u], true),
            Some(("a", "filter=unspecified".to_owned())),
            "a driver named as a filter not given may be any path's"
        );
        assert!(names_odd_driver("core.eol\nlf\0filter.unset.clean\nx\0"));
        assert!(names_odd_driver("filter.unspecified.smudge\nx\0"));
        assert!(!names_odd_driver("filter.unsets.clean\nx\0filter.lfs.smudge\nx\0"));
        assert!(!names_odd_driver(""));
    }

    #[test]
    fn every_listed_path_is_checked_and_only_the_written_ones_are_counted() {
        let zeros = "0000000000000000000000000000000000000000";
        let new = "1111111111111111111111111111111111111111";
        let listing = [
            written_entry("100644", "000000", zeros, "D", &["gone.x"]),
            written_entry("100644", "100644", new, "R100", &["old.txt", "new.txt"]),
            written_entry("000000", "100644", new, "A", &["added.txt"]),
        ]
        .concat();

        let listed = listed_paths(&listing);
        assert_eq!(listed, ["gone.x", "old.txt", "new.txt", "added.txt"]);
        let written: Vec<&str> = written_blobs(&listing).iter().map(|file| listed[file.at]).collect();
        assert_eq!(written, ["new.txt", "added.txt"], "each written file's place among the listed paths");
    }

    #[test]
    fn a_back_commit_may_not_write_an_lfsconfig_or_any_spelling_of_a_refused_name() {
        for path in [
            ".lfsconfig",
            "sub/.lfsconfig",
            ".LfsConfig",
            ".lfsconfig.",
            ".lfsconfig::$DATA",
            ".lfsconfig. . ",
            r"sub\.gitattributes",
            ".gitignore:stream",
            "GITHUB~1/workflows/x.yml",
            "LFSCON~1",
            "sub/gitatt~1",
            "GIT~1/config",
            "G~1",
        ] {
            let listing = entry("000000", "100644", "A", &[path]);
            assert_eq!(forbidden(&listing), Some(path.to_owned()), "{path}");
        }
        for path in [".lfsconfig/x", "x.lfsconfig", "A~1.txt", "notes~1.txt", "sub/lfs~x", "agit~1"] {
            let listing = entry("000000", "100644", "A", &[path]);
            assert_eq!(forbidden(&listing), None, "{path}");
        }
    }

    #[test]
    fn each_blob_counts_its_weight_and_every_blob_needs_one() {
        let (one, two) = ("1111111111111111111111111111111111111111", "2222222222222222222222222222222222222222");
        assert_eq!(total("5\n7\n", &[one, two], &[2, 8]), Ok(66));
        assert_eq!(total("5\n", &[one], &[]), Err("0 weight(s) were given for 1 object(s)".to_owned()));
        assert_eq!(
            total("18446744073709551615\n", &[one], &[8]),
            Ok(u64::MAX),
            "a weighted size stops at the most a u64 holds"
        );
    }
}

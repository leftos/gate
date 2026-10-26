//! `gate-box box prune --root <folder>`: the age prune, which a scheduled job on the builder runs every few hours. It
//! removes the work folders no run has used for over a day: a folder's last use is the newest last write of the folder,
//! each tree's `.git/FETCH_HEAD` (every run fetches into each tree it checks out) and the key's records. A key's work
//! folder `<root>/work/<key>` goes only with its lock `<root>/work/<key>.lock` held, so no run is using it and none can
//! start in it meanwhile (one that tries is refused busy, status 75, and its PC runs the command itself); a key whose
//! lock is held is kept, and so is one whose recorded process group, a git step's `<key>.pgid`, a tool restore's
//! `<key>.tools.pgid` or a package restore's `<key>.packages.pgid`, is still the one its run made, as a run refuses
//! `busy` for it.
//! The folder's layout record is marked changing and its other records removed first, then the folder is renamed into
//! `<root>/pruned` under the lock, so no run ever sees it half removed, and its layout and index records go once it is
//! there; a rename that fails leaves the layout marked changing, so the next run clears the folder whole. The folder is
//! removed from `pruned` after the lock is let go; what a removal leaves there the next sweep takes. A key with no
//! folder has its records removed. The lock stays. The age prune never deletes a ref.
//! `gate-box box prune --root <folder> --keys <key>...`, the keyed prune the PC sends for worktrees that are gone,
//! removes those keys whatever their last use, a lock it made with them, and their refs in each bare repository.
//! After its age pass, the age prune keeps the root's volume from filling: while under 100 GiB is free, it removes the
//! work folders no run holds, oldest use first, until 150 GiB is. Then it removes the temp folders runs ended hard left
//! in `/tmp`, idle for over a day.
//! The sweep works only in a root private to the box user: the box's own folder, writable by no one but the builder, as
//! [`records_folder`] checks it before anything beneath it is looked at, so only that user can swap what the sweep
//! looks at between its look and its act. A root the check refuses is left whole: the age prune ends there, and the
//! keyed prune fails each key with the check's reason, writing nothing and running no git in it. Its git runs with
//! no hooks and no file-system monitor. The sweep never follows a link: every name is looked at as itself, a link
//! where `work`, `pruned` or a work folder goes is refused, a renamed folder is checked to be the one looked at, and a
//! link inside a folder is removed as itself.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, DirBuilder, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_wire::{escaped, is_key};

use crate::console::Console;
use crate::files::{self, HeldLock, Locked};
use crate::git::Git;
use crate::lines::{FAILED_STATUS, LAYOUT_CHANGING, OwnerFlaw, USAGE_STATUS, WRITABLE_BY_OTHERS, owner_flaw, recorded_group, signal_guard};
use crate::stop::{Record, own_group, record_current, records_folder, unusable_refusal};

/// How long a work folder goes unused before a sweep removes it.
const IDLE_AGE: Duration = Duration::from_hours(24);
/// The extensions of the files beside a work folder `<key>` named `<key>.<extension>`: its lock and its records.
const KEY_FILES: [&str; 13] = [
    "lock",
    "warm",
    "layout",
    "inputs",
    "sent-build",
    "tools",
    "packages",
    "index",
    "command",
    "pgid",
    "tools.pgid",
    "packages.pgid",
    "index.lock",
];
/// The records naming a run's process group: its git step's, its tool restore's and its package restore's.
const GROUP_RECORDS: [&str; 3] = ["pgid", "tools.pgid", "packages.pgid"];
/// The records a sweep removes before it renames the work folder aside, in the order the box clears them, then the
/// group records; `command` is the interrupted-command record, cleared like the others.
const RECORDS_BEFORE: [&str; 9] = [
    "warm",
    "packages",
    "tools",
    "inputs",
    "sent-build",
    "command",
    "pgid",
    "tools.pgid",
    "packages.pgid",
];
/// The records a sweep removes once the work folder is renamed aside: the layout, marked changing until then, the
/// index and git's lock on it. The lock stays.
const RECORDS_AFTER: [&str; 3] = ["layout", "index", "index.lock"];
/// What the name of a record's temp file holds after the record's name ([`files::replace_plain`]).
const TEMP_MARK: &str = ".gate-new-";
/// The seconds in an hour, as the idle time is said.
const HOUR_SECONDS: u64 = 3600;
/// The kinds of a key's refs in a bare repository, each `refs/<kind>/<key>`: the PC's snapshot, its bridge, and the
/// box's back commit.
const REF_KINDS: [&str; 3] = ["gate", "gate-bridge", "gate-back"];
/// The git step that deletes a key's ref in a bare repository, as its lines name it.
const REFS_STEP: &str = "update-ref";

/// Prunes the root `root`, after what an earlier sweep left in `<root>/pruned`: without `keys`, the work folders no run
/// has used for over [`IDLE_AGE`] before `now` ([`aged`]); with them, those keys alone ([`named`]). The exit status:
/// [`USAGE_STATUS`] for a root that is not an absolute path or a key that is no key, before anything is opened;
/// [`FAILED_STATUS`] when the root, the work folders or the `pruned` folder could not be read or listed, or `work` or
/// `pruned` is not a plain folder, and for an age prune of a root [`records_folder`] refuses; else each mode's own.
pub fn prune(root: &Path, keys: &[String], now: SystemTime, console: &mut Console) -> u8 {
    let tmp = tmp_root();
    let seams = Seams {
        before_rename: &no_rename_seen,
        after_rename: &no_rename_seen,
        volumes: Path::new(VOLUMES),
        free: &free_read,
        floor_low: FLOOR_LOW,
        floor_high: FLOOR_HIGH,
        tmp: &tmp,
    };
    pruned_with(root, keys, now, &seams, console)
}

/// The folder macOS mounts each volume but the startup one in, as `/Volumes/<name>`.
const VOLUMES: &str = "/Volumes";
/// The folder a run makes its own temp folder in, `gate.` and eight letters or digits ([`is_run_temp_name`]): `/tmp`
/// itself, whatever the sweep's own `TMPDIR`.
const TMP: &str = "/tmp";
/// A gibibyte, as the free space is said.
const GIB: u64 = 1 << 30;
/// The free space under which the age prune goes on to remove work folders however recent ([`Sweep::floor_removed`]).
const FLOOR_LOW: u64 = 100 * GIB;
/// The free space at which that removal stops.
const FLOOR_HIGH: u64 = 150 * GIB;

/// What a prune is given besides its words, which a test replaces: what it calls with each work folder just before
/// renaming it aside and with its new name just after, the folder the volumes are mounted in, the reader of the free
/// bytes of the volume holding a folder, the free-space floors, and the folder runs make their temp folders in.
struct Seams<'a> {
    before_rename: &'a dyn Fn(&Path),
    after_rename: &'a dyn Fn(&Path),
    volumes: &'a Path,
    free: &'a dyn Fn(&Path) -> io::Result<u64>,
    floor_low: u64,
    floor_high: u64,
    tmp: &'a Path,
}

/// Nothing: what a prune calls before and after each rename outside the tests.
fn no_rename_seen(_folder: &Path) {}

/// The folder runs make their temp folders in: [`TMP`], or in a build with the test seams `GATE_BOX_TEST_TMP_ROOT`
/// when set.
fn tmp_root() -> PathBuf {
    // Set by tests/run.rs `an_age_prune_removes_an_idle_work_folder_and_a_leftover_run_folder`.
    #[cfg(feature = "test-seams")]
    if let Some(root) = std::env::var_os("GATE_BOX_TEST_TMP_ROOT") {
        return PathBuf::from(root);
    }
    PathBuf::from(TMP)
}

/// The free bytes of the volume holding `path` ([`free_bytes`]), or in a build with the test seams
/// `GATE_BOX_TEST_FREE_BYTES` when set.
fn free_read(path: &Path) -> io::Result<u64> {
    // Set by tests/run.rs `an_age_prune_removes_an_idle_work_folder_and_a_leftover_run_folder`.
    #[cfg(feature = "test-seams")]
    if let Some(bytes) = std::env::var_os("GATE_BOX_TEST_FREE_BYTES") {
        return bytes
            .to_str()
            .and_then(|bytes| bytes.parse().ok())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "GATE_BOX_TEST_FREE_BYTES is not a count of bytes"));
    }
    free_bytes(path)
}

/// The bytes of the volume holding `path` that the box's user may still write, by `statfs(2)`.
fn free_bytes(path: &Path) -> io::Result<u64> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `statfs` is plain data, for which all zeroes is a valid value, filled in by the call below.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a NUL-terminated path and `stat` a whole `statfs`, both living across the call, which only
    // reads the one and writes the other.
    if unsafe { libc::statfs(name.as_ptr(), &raw mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(statfs_count(stat.f_bavail)?.saturating_mul(statfs_count(stat.f_bsize)?))
}

/// A count `statfs` gave, whose type is unsigned on macOS and may be signed elsewhere (Linux's block size); else that
/// it was negative.
fn statfs_count(count: impl TryInto<u64>) -> io::Result<u64> {
    count
        .try_into()
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "statfs gave a negative count"))
}

/// [`prune`], with `seams`.
fn pruned_with(root: &Path, keys: &[String], now: SystemTime, seams: &Seams<'_>, console: &mut Console) -> u8 {
    // The parse takes keys alone; this guards the names the sweep would look at beneath `work` all the same.
    if let Some(word) = keys.iter().find(|key| !is_key(key)) {
        console.err(&format!("gate: box prune --keys holds a word that is not a key: {word:?}"));
        return USAGE_STATUS;
    }
    let Some(text) = root.to_str().filter(|text| text.starts_with('/')) else {
        console.err(&format!("gate: box prune --root is not an absolute path: {:?}", root.to_string_lossy()));
        return USAGE_STATUS;
    };
    let trimmed = text.trim_end_matches('/');
    let root = if trimmed.is_empty() { "/" } else { trimmed };
    let swept = if keys.is_empty() {
        aged(root, now, seams, console)
    } else {
        named(root, keys, now, seams, console)
    };
    match swept {
        Ok(status) => status,
        Err(line) => {
            console.err(&line);
            FAILED_STATUS
        }
    }
}

/// The age prune of the root `root`, given without a trailing separator: removes the work folders idle at `now`, and
/// the records of a key whose folder is gone; says each folder removed and how many, with a line on stderr for each
/// key kept for a failure, and gives 0. Else the line saying why the sweep ended. A root or work folder that is not
/// there holds nothing to remove.
fn aged(root: &str, now: SystemTime, seams: &Seams<'_>, console: &mut Console) -> Result<u8, String> {
    let (removed, folders) = match Sweep::opened(root, now, seams, false, console)? {
        Some(sweep) => {
            let listed = listed(&sweep.work).map_err(|error| cannot_list(&sweep.work, &error))?;
            let mut aged = 0;
            // The floor pass takes the keys the age pass kept; one it failed has said its line once.
            let mut left = Vec::new();
            for key in &listed.keys {
                match sweep.aged_key(key, console) {
                    Pruned::Removed(_) => aged += 1,
                    Pruned::Failed => {}
                    Pruned::Held | Pruned::Kept => left.push(key),
                }
            }
            let floored = sweep.floor_removed(&left, seams, console);
            (aged + floored, listed.folders)
        }
        None => (0, 0),
    };
    leftover_run_folders_removed(seams.tmp, now, console);
    console.out(&format!("gate: remote prune removed {removed} of {folders} work folders"));
    Ok(0)
}

/// The keyed prune of the root `root`, given without a trailing separator: removes each key of `keys`, in the order
/// given, whatever its last use, and no other, saying for each that it was pruned, kept as in use, or could not be
/// pruned. Gives [`FAILED_STATUS`] when a key could not be pruned, else 0; else the line saying why the sweep ended. A
/// root that is not there holds nothing of any key, unless it is not known to be gone ([`root_found`]), which says
/// nothing of them. A root [`records_folder`] refuses is left whole, each key failing with the check's reason.
fn named(root: &str, keys: &[String], now: SystemTime, seams: &Seams<'_>, console: &mut Console) -> Result<u8, String> {
    let every_key_failed = |why: &str, console: &mut Console| {
        for key in keys {
            console.err(&could_not_prune(key, why));
        }
        Ok(FAILED_STATUS)
    };
    let sweep = match root_found(Path::new(root), seams.volumes) {
        RootFound::Unknown(why) => return every_key_failed(&why, console),
        RootFound::Gone => None,
        // The root alone is judged here, as its own parent, before anything in it is made, removed or run.
        RootFound::There => match records_folder(root, Path::new(root)) {
            Ok(true) => Sweep::opened(root, now, seams, true, console)?,
            Ok(false) => None,
            // The line's own `gate: ` goes, as the key's line that carries it says it once.
            Err((line, _)) => return every_key_failed(line.strip_prefix("gate: ").unwrap_or(&line), console),
        },
    };
    let Some(sweep) = sweep else {
        for key in keys {
            console.out(&gone_line(key));
        }
        return Ok(0);
    };
    let mut failed = false;
    for key in keys {
        match sweep.pruned_key(key, Mode::Named, console) {
            Pruned::Removed(_) => console.out(&gone_line(key)),
            Pruned::Held => console.out(&format!("gate: remote kept {}: in use", escaped(key))),
            // The keyed prune judges no age, so it keeps a key only for a held lock or a failure.
            Pruned::Kept => {}
            Pruned::Failed => failed = true,
        }
    }
    Ok(if failed { FAILED_STATUS } else { 0 })
}

/// The line saying the keyed prune removed what was left of the key `key`.
fn gone_line(key: &str) -> String {
    format!("gate: remote pruned {} (worktree gone)", escaped(key))
}

/// What the keyed prune knows of its root.
#[derive(Debug, PartialEq, Eq)]
enum RootFound {
    /// Something is at its name.
    There,
    /// Nothing is at its name, and its parent folder can be read, so it is gone.
    Gone,
    /// It could not be looked at, or it is missing with a parent that cannot be read or a volume not mounted: why.
    Unknown(String),
}

/// The reason a missing root whose volume or parent cannot be seen says nothing of its keys.
const VOLUME_NOT_THERE: &str = "the root's volume is not there";

/// What is known of the root `root`, an absolute path, whose volumes other than the startup one are mounted in
/// `volumes`: a root beneath `<volumes>/<name>` is unknown while that volume is not there; a missing root is gone only
/// when its parent folder can be read; any other failure to look at it is unknown, saying the failure.
fn root_found(root: &Path, volumes: &Path) -> RootFound {
    let volume = root.strip_prefix(volumes).ok().and_then(|rest| rest.components().next());
    if let Some(volume) = volume
        && fs::metadata(volumes.join(volume)).is_err()
    {
        return RootFound::Unknown(VOLUME_NOT_THERE.to_owned());
    }
    match fs::symlink_metadata(root) {
        Ok(_) => RootFound::There,
        Err(error) if error.kind() == ErrorKind::NotFound => match root.parent() {
            Some(parent) if fs::read_dir(parent).is_ok() => RootFound::Gone,
            _ => RootFound::Unknown(VOLUME_NOT_THERE.to_owned()),
        },
        Err(error) => RootFound::Unknown(format!("the root cannot be looked at: {error}")),
    }
}

/// How a sweep prunes a key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The age prune: an idle folder alone, and the lock kept.
    Aged,
    /// The age prune's floor pass: a folder whatever its last use, and the lock kept.
    Floor,
    /// The keyed prune: whatever the folder's last use, the lock it made removed, and the key's refs deleted.
    Named,
}

/// One sweep: the root's work folder and `pruned` folder, the time it judges by, and what it calls with each work
/// folder just before renaming it aside and with its new name just after.
struct Sweep<'a> {
    root: String,
    work: PathBuf,
    pruned: PathBuf,
    now: SystemTime,
    before_rename: &'a dyn Fn(&Path),
    after_rename: &'a dyn Fn(&Path),
}

/// What became of one key of a sweep.
enum Pruned {
    /// Its lock taken, its work folder and records removed, and in the keyed prune its refs: the folder renamed aside,
    /// idle as long as given in the age prune, or there was none.
    Removed(Option<Duration>),
    /// A run or another sweep holds its lock, or its recorded process group is still its run's.
    Held,
    /// The age prune keeps it: its folder is not idle, or it has none, its records alone removed.
    Kept,
    /// It could not be pruned, as a line on stderr said.
    Failed,
}

/// What a sweep found of a key's work folder, its lock held.
enum Found {
    /// No folder: its records are removed.
    Missing,
    /// A folder that is not idle, left as it is.
    Fresh,
    /// The folder, renamed aside.
    Aside(Aside),
}

/// What the work folder holds: each key, named by a folder or by a file beside one, sorted without regard to case; and
/// how many of the keys' entries are plain folders.
struct Listed {
    keys: Vec<String>,
    folders: usize,
}

/// A work folder renamed aside: where it now is, how long it had been idle, which the age prune alone judges, and the
/// line saying a record that could not be removed once it was aside.
struct Aside {
    path: PathBuf,
    idle: Option<Duration>,
    left: Option<String>,
}

impl<'a> Sweep<'a> {
    /// The sweep of the root `root`, given without a trailing separator, at `now`, keyed as `named` says: the root
    /// checked as [`records_folder`] checks it, `pruned` made when missing and what an earlier sweep left in it removed,
    /// and `work` looked at; the keyed prune makes a missing `work`, so each key's lock is taken there as a run takes
    /// it. `None` when the root, or for the age prune `work`, is not there. Else the line saying why the sweep ended, a
    /// root the check refuses among them.
    fn opened(root: &str, now: SystemTime, seams: &Seams<'a>, named: bool, console: &mut Console) -> Result<Option<Sweep<'a>>, String> {
        if named && !plain_folder_there(Path::new(root))? {
            return Ok(None);
        }
        // The root alone is judged here, as its own parent; `work` is looked at as itself once `pruned` is done.
        match records_folder(root, Path::new(root)) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err((line, _)) => return Err(line),
        }
        let pruned = Path::new(root).join("pruned");
        folder_made(&pruned)?;
        leftovers_removed(&pruned, console)?;
        let work = Path::new(root).join("work");
        if named {
            folder_made(&work)?;
        } else if !plain_folder_there(&work)? {
            return Ok(None);
        }
        if let Some(unusable) = files::unusable_folder(&work, true) {
            return Err(unusable_refusal(unusable, true).0);
        }
        Ok(Some(Sweep {
            root: root.to_owned(),
            work,
            pruned,
            now,
            before_rename: seams.before_rename,
            after_rename: seams.after_rename,
        }))
    }

    /// The age prune of the key `key` ([`Sweep::pruned_key`]), saying the folder removed and how long it was idle;
    /// what became of the key.
    fn aged_key(&self, key: &str, console: &mut Console) -> Pruned {
        let pruned = self.pruned_key(key, Mode::Aged, console);
        if let Pruned::Removed(Some(idle)) = pruned {
            let hours = idle.as_secs() / HOUR_SECONDS;
            console.out(&format!("gate: remote pruned {}: idle {hours} h", escaped(key)));
        }
        pruned
    }

    /// The age prune's floor pass over the keys `left` after its age pass: while the free space of the root's volume
    /// ([`Seams::free`]) is under the low floor, removes their work folders, those no run holds and no recorded process
    /// group of which is still its run's, in the floor's order ([`Sweep::floor_order`]), one at a time and the free
    /// space read again after each, until it is at the high floor. Says each folder removed and the free space before
    /// it, and, on stderr, a free space that cannot be read, or a floor not reached once no folder is left or a removal
    /// freed nothing. How many it removed.
    fn floor_removed(&self, left: &[&String], seams: &Seams<'_>, console: &mut Console) -> usize {
        let root = Path::new(&self.root);
        let read = |console: &mut Console| {
            (seams.free)(root)
                .inspect_err(|error| {
                    console.err(&format!(
                        "gate: remote could not read the free space of {}: {}",
                        escaped(&self.root),
                        escaped(&error.to_string())
                    ));
                })
                .ok()
        };
        let Some(mut free) = read(console) else {
            return 0;
        };
        if free >= seams.floor_low {
            return 0;
        }
        let mut removed = 0;
        for key in self.floor_order(left) {
            if free >= seams.floor_high {
                break;
            }
            if !matches!(self.pruned_key(key, Mode::Floor, console), Pruned::Removed(_)) {
                continue;
            }
            removed += 1;
            console.out(&format!("gate: remote pruned {}: {} GiB free", escaped(key), free / GIB));
            let before = free;
            let Some(now_free) = read(console) else {
                return removed;
            };
            free = now_free;
            if free <= before {
                break;
            }
        }
        if free < seams.floor_high {
            console.err(&format!(
                "gate: remote prune could not reach the free-space floor: {} GiB free",
                free / GIB
            ));
        }
        removed
    }

    /// The keys of `keys` with a work folder whose last use can be read, oldest use first; a last use after the sweep's
    /// time is the newest.
    fn floor_order<'k>(&self, keys: &[&'k String]) -> Vec<&'k str> {
        let mut dated: Vec<(SystemTime, &str)> = Vec::new();
        for key in keys {
            let folder = self.work.join(key);
            let Ok(looked) = fs::symlink_metadata(&folder) else {
                continue;
            };
            // A folder whose last use cannot be read is one the age pass said it could not prune.
            if let Some(last_use) = looked.is_dir().then(|| self.last_use(key, &folder, &looked).ok()).flatten() {
                dated.push((last_use, key.as_str()));
            }
        }
        dated.sort();
        dated.into_iter().map(|(_, key)| key).collect()
    }

    /// Prunes the key `key` as `mode` says when no run holds its lock and no recorded process group of it is still its
    /// run's: its folder renamed aside, its records removed, in the keyed prune its refs too, and the folder removed
    /// once the lock is let go. A failure is a line on stderr and keeps what is left.
    fn pruned_key(&self, key: &str, mode: Mode, console: &mut Console) -> Pruned {
        let lock_path = self.work.join(format!("{key}.lock"));
        let lock = match files::take_lock(&lock_path) {
            Ok(Locked::Held(lock)) => lock,
            Ok(Locked::Busy) => return Pruned::Held,
            Err(error) if mode == Mode::Named => {
                console.err(&could_not_prune(key, &error.to_string()));
                return Pruned::Failed;
            }
            Err(error) => {
                console.err(&format!(
                    "gate: remote could not open the work folder lock {}: {}",
                    escaped(&lock_path.display().to_string()),
                    escaped(&error.to_string())
                ));
                return Pruned::Failed;
            }
        };
        let found = match self.group_live(key) {
            Ok(true) => {
                let_go(lock, mode, console);
                return Pruned::Held;
            }
            Ok(false) => self.set_aside(key, mode, &lock),
            Err(line) => Err(line),
        };
        let aside = match found {
            Ok(Found::Aside(aside)) => Some(aside),
            Ok(Found::Missing) if mode == Mode::Named => None,
            Ok(Found::Missing | Found::Fresh) => {
                let_go(lock, mode, console);
                return Pruned::Kept;
            }
            Err(line) => {
                let_go(lock, mode, console);
                console.err(&line);
                return Pruned::Failed;
            }
        };
        self.removed(key, mode, lock, aside, console)
    }

    /// With the key's lock `lock` held and its work folder `aside` renamed aside, or none there: in the keyed prune its
    /// refs deleted, then the lock let go and the folder removed. A folder that cannot be removed is a line on stderr,
    /// which the next sweep takes. A record left once the folder was aside is a line on stderr, and fails the key in
    /// the keyed prune; refs that cannot be deleted are the key's line saying why.
    fn removed(&self, key: &str, mode: Mode, lock: HeldLock, aside: Option<Aside>, console: &mut Console) -> Pruned {
        // The age prune leaves every ref: a worktree still alive may have pushed one just before the lock was taken, and
        // its run's fetch would fail where a rebuilt folder alone does no harm. A gone worktree pushes nothing.
        // Under the lock and with the folder out of `work`, so a run that comes for the key waits, then finds neither.
        let refs = if mode == Mode::Named {
            self.refs_removed(key, lock.path())
        } else {
            Ok(())
        };
        let_go(lock, mode, console);
        let (idle, left) = match aside {
            Some(aside) => {
                // A folder not there was taken by another sweep emptying `pruned`, as this one would have.
                if let Err(error) = fs::remove_dir_all(&aside.path)
                    && error.kind() != ErrorKind::NotFound
                {
                    console.err(&could_not_remove_pruned(&aside.path, &error));
                }
                (aside.idle, aside.left)
            }
            None => (None, None),
        };
        if let Some(line) = left {
            console.err(&line);
            if mode == Mode::Named {
                return Pruned::Failed;
            }
        }
        match refs {
            Ok(()) => Pruned::Removed(idle),
            Err(why) => {
                console.err(&could_not_prune(key, &format!("its refs could not be removed: {why}")));
                Pruned::Failed
            }
        }
    }

    /// Deletes the key's refs of [`REF_KINDS`] in each bare repository of the root ([`whole_bare`]), every one tried
    /// whatever one before it did, with the key's lock file at `lock`; else why the first could not be. Git runs as a
    /// run's git does: its hooks read from `<lock>/hooks`, which cannot exist as the lock is a file, and no file-system
    /// monitor, so nothing in a bare repository runs.
    fn refs_removed(&self, key: &str, lock: &Path) -> Result<(), String> {
        let hooks = format!("core.hooksPath={}/hooks", lock.display());
        let safety = ["-c", hooks.as_str(), "-c", "core.fsmonitor=false"];
        let root = Path::new(&self.root);
        let mut names = Vec::new();
        for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
            let name = entry.map_err(|error| error.to_string())?.file_name();
            if let Some(name) = name.to_str().filter(|name| is_bare_name(name)) {
                names.push(name.to_owned());
            }
        }
        names.sort();
        let git = Git::of(&self.root, &self.work);
        let mut first = None;
        for name in names {
            let bare = root.join(&name);
            match whole_bare(&bare) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    first.get_or_insert(error.to_string());
                    continue;
                }
            }
            // A bare another account may write in is left whole, its refs and all, and named.
            let flaw = files::facts(&bare).map(|facts| {
                let shown = bare.display().to_string();
                bare_flaw(&shown, facts.owner, facts.mode, facts.euid, &files::user_name(facts.euid))
            });
            match flaw {
                Ok(None) => {}
                Ok(Some(why)) => {
                    first.get_or_insert(why);
                    continue;
                }
                Err(error) => {
                    first.get_or_insert(error.to_string());
                    continue;
                }
            }
            let git_dir = format!("--git-dir={}", bare.display());
            for kind in REF_KINDS {
                if let Err(why) = ref_deleted(&git, &safety, &git_dir, &format!("refs/{kind}/{key}")) {
                    first.get_or_insert(why);
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Whether a group record of the key `key` ([`GROUP_RECORDS`]) names a group that is still its run's
    /// ([`record_current`], unknown counting as current); one naming no group a run makes, or a group no longer its
    /// run's, goes with the key. Else the line saying a record could not be read, a link or a folder at its name.
    fn group_live(&self, key: &str) -> Result<bool, String> {
        for extension in GROUP_RECORDS {
            let path = self.work.join(format!("{key}.{extension}"));
            let (bytes, written) = match files::read_plain_dated(&path, files::RECORD_LIMIT) {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(could_not_prune(key, &format!("cannot read {}: {error}", path.display()))),
                Ok(read) => read,
            };
            if let Some(recorded) = recorded_group(&bytes)
                && signal_guard(recorded.group, own_group()).is_ok()
                && record_current(recorded, written) == Record::Current
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// With the key's lock `lock` held: its layout marked changing, its other records removed and its work folder
    /// renamed aside when the folder is idle, then its layout and index records and its records' temp files removed;
    /// the records alone removed when it has no folder; a folder not idle left. Else the line saying why the key is
    /// kept. A record that cannot be removed once the folder is aside is the aside folder's line saying so
    /// ([`Aside::left`]).
    fn set_aside(&self, key: &str, mode: Mode, lock: &HeldLock) -> Result<Found, String> {
        let keep = |why: &dyn ToString| could_not_prune(key, &why.to_string());
        let folder = self.work.join(key);
        let looked = match fs::symlink_metadata(&folder) {
            Ok(looked) => looked,
            Err(error) if error.kind() == ErrorKind::NotFound => return self.records_alone_removed(key, lock),
            Err(error) => return Err(keep(&error)),
        };
        if !looked.is_dir() {
            return Err(keep(&format!("{} is not a plain folder", folder.display())));
        }
        let idle = if mode == Mode::Aged {
            let last_use = self.last_use(key, &folder, &looked).map_err(|error| keep(&error))?;
            let Some(idle) = idle_for(last_use, self.now) else {
                return Ok(Found::Fresh);
            };
            Some(idle)
        } else {
            None
        };
        still_held(key, lock)?;
        files::replace_plain(&self.work.join(format!("{key}.layout")), LAYOUT_CHANGING.as_bytes()).map_err(|why| keep(&why))?;
        self.records_removed(key, &RECORDS_BEFORE)?;
        // The nonce only keeps two names apart within a sweep, so a clock before 1970 may give 0.
        let nanos = self.now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let aside = self.pruned.join(format!("{key}.{}.{nanos}", std::process::id()));
        (self.before_rename)(&folder);
        fs::rename(&folder, &aside).map_err(|error| keep(&error))?;
        (self.after_rename)(&aside);
        let moved = match fs::symlink_metadata(&aside) {
            Ok(moved) => Some(moved),
            // Another sweep emptying `pruned` took the folder: it is gone, as this sweep would have made it.
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(keep(&error)),
        };
        if moved.is_some_and(|moved| (moved.dev(), moved.ino()) != (looked.dev(), looked.ino())) {
            // What was renamed stays in `pruned`, which the next sweep empties.
            return Err(keep(&"its work folder changed during the sweep"));
        }
        let left = self.records_removed(key, &RECORDS_AFTER).and_then(|()| self.temps_removed(key)).err();
        Ok(Found::Aside(Aside { path: aside, idle, left }))
    }

    /// With the key's lock `lock` held and no work folder of it: its records and their temp files removed. Else the
    /// line saying why the key is kept.
    fn records_alone_removed(&self, key: &str, lock: &HeldLock) -> Result<Found, String> {
        still_held(key, lock)?;
        self.records_removed(key, &RECORDS_BEFORE)?;
        self.records_removed(key, &RECORDS_AFTER)?;
        self.temps_removed(key)?;
        Ok(Found::Missing)
    }

    /// The newest last write among the work folder `folder`, looked at as `looked`, each tree's `.git/FETCH_HEAD` in it
    /// or in a folder of its own directly in it, and the key's records beside it, its lock left out (no run writes the
    /// lock, so its last write is when it was made); each looked at as itself, never through a link, and one that is
    /// not there skipped.
    fn last_use(&self, key: &str, folder: &Path, looked: &Metadata) -> io::Result<SystemTime> {
        let mut newest = looked.modified()?;
        let mut stamps = vec![fetched(folder)?];
        for entry in fs::read_dir(folder)? {
            let tree = entry?.path();
            if plain_folder_named(&tree)? {
                stamps.push(fetched(&tree)?);
            }
        }
        for extension in KEY_FILES.iter().filter(|extension| **extension != "lock") {
            stamps.push(written(&self.work.join(format!("{key}.{extension}")))?);
        }
        for stamp in stamps.into_iter().flatten() {
            newest = newest.max(stamp);
        }
        Ok(newest)
    }

    /// Removes the key's records `extensions`, in order, each as itself; else the line saying the first that could not
    /// be removed.
    fn records_removed(&self, key: &str, extensions: &[&str]) -> Result<(), String> {
        for extension in extensions {
            files::remove_if_there(&self.work.join(format!("{key}.{extension}"))).map_err(|error| could_not_prune(key, &error.to_string()))?;
        }
        Ok(())
    }

    /// Removes the temp files of the key's records beside the work folder, `<key>.<name>.gate-new-<pid>-<n>`, which a
    /// box ended mid-write leaves; else the line saying the first that could not be removed.
    fn temps_removed(&self, key: &str) -> Result<(), String> {
        let cannot = |error: io::Error| could_not_prune(key, &error.to_string());
        for entry in fs::read_dir(&self.work).map_err(cannot)? {
            let entry = entry.map_err(cannot)?;
            let name = entry.file_name();
            let is_temp = name
                .to_str()
                .and_then(|name| name.split_once('.'))
                .is_some_and(|(stem, rest)| stem.eq_ignore_ascii_case(key) && rest.contains(TEMP_MARK));
            if is_temp {
                files::remove_if_there(&entry.path()).map_err(cannot)?;
            }
        }
        Ok(())
    }
}

/// Whether the name of the key's lock, held as `lock`, still names the file the sweep locked; else the line keeping the
/// key `key`.
fn still_held(key: &str, lock: &HeldLock) -> Result<(), String> {
    match lock.still_held() {
        Ok(true) => Ok(()),
        Ok(false) => Err(could_not_prune(
            key,
            &format!("its lock {} is no longer the file the sweep locked", lock.path().display()),
        )),
        Err(error) => Err(could_not_prune(key, &error.to_string())),
    }
}

/// Removes each leftover run folder in `tmp` ([`leftover_idle`]) idle at `now`, saying each removed and how long it was
/// idle; a folder that cannot be listed or removed, or `tmp` that cannot be listed, is a line on stderr.
fn leftover_run_folders_removed(tmp: &Path, now: SystemTime, console: &mut Console) {
    let entries = match fs::read_dir(tmp) {
        Ok(entries) => entries,
        Err(error) => return console.err(&cannot_list(tmp, &error)),
    };
    let euid = files::effective_uid();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return console.err(&cannot_list(tmp, &error)),
        };
        if !entry.file_name().to_str().is_some_and(is_run_temp_name) {
            continue;
        }
        let path = entry.path();
        let shown = escaped(&path.display().to_string());
        let idle = match leftover_idle(&path, euid, now) {
            Ok(Some(idle)) => idle,
            Ok(None) => continue,
            Err(error) => {
                console.err(&cannot_list(&path, &error));
                continue;
            }
        };
        match fs::remove_dir_all(&path) {
            Ok(()) => console.out(&format!(
                "gate: remote removed the leftover run folder {shown}: idle {} h",
                idle.as_secs() / HOUR_SECONDS
            )),
            Err(error) => console.err(&format!(
                "gate: remote could not remove the leftover run folder {shown}: {}",
                escaped(&error.to_string())
            )),
        }
    }
}

/// How long the folder `path` has been idle at `now` when it is a leftover run folder: a folder looked at as itself,
/// not a link, owned by the user `euid`, the newest last write of it and of each entry directly in it over
/// [`IDLE_AGE`] before `now`. `None` when it is not, or is gone. Else why it could not be looked at or listed.
fn leftover_idle(path: &Path, euid: u32, now: SystemTime) -> io::Result<Option<Duration>> {
    let looked = match fs::symlink_metadata(path) {
        Ok(looked) => looked,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !looked.is_dir() || looked.uid() != euid {
        return Ok(None);
    }
    let mut newest = looked.modified()?;
    for entry in fs::read_dir(path)? {
        if let Some(stamp) = written(&entry?.path())? {
            newest = newest.max(stamp);
        }
    }
    Ok(idle_for(newest, now))
}

/// Whether `name` is a run's temp folder's: `gate.` and eight letters or digits, as `mkdtemp(3)` fills them in.
fn is_run_temp_name(name: &str) -> bool {
    name.strip_prefix("gate.")
        .is_some_and(|rest| rest.len() == 8 && rest.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

/// Deletes the ref `reference`, not a ref it points at, in the bare repository `git_dir` names, through `git` given the
/// options `safety` first; a ref that is not there is no failure, as git deletes it without a word. Else git's first
/// line on stderr, or why it did not run to its end.
fn ref_deleted(git: &Git<'_>, safety: &[&str], git_dir: &str, reference: &str) -> Result<(), String> {
    let args: Vec<&str> = safety
        .iter()
        .copied()
        .chain([git_dir, "update-ref", "-d", "--no-deref", reference])
        .collect();
    let ran = match git.run(REFS_STEP, &args) {
        Ok(ran) => ran,
        Err(failed) => {
            let line = failed.lines.first().map_or("", String::as_str);
            return Err(line.strip_prefix("gate: ").unwrap_or(line).to_owned());
        }
    };
    if ran.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&ran.stderr);
    if let Some(said) = stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        return Err(said.to_owned());
    }
    Err(match ran.status.code() {
        Some(code) => format!("git exited with status {code}"),
        None => format!("git ended with {}", ran.status),
    })
}

/// Whether `path` is a whole bare repository, looked at as itself: a folder of its own, not a link, holding `HEAD`,
/// `objects` and `refs`. `false` when it is not there.
fn whole_bare(path: &Path) -> io::Result<bool> {
    if !plain_folder_named(path)? {
        return Ok(false);
    }
    Ok(["HEAD", "objects", "refs"].iter().all(|part| files::exists(&path.join(part))))
}

/// Why the keyed prune leaves the whole bare repository `bare`, owned by `owner` with mode `mode`, as it is, the box's
/// effective user `euid` named `user`: `<bare> is not owned by <user>` or `<bare> is writable by others`
/// ([`owner_flaw`]); `None` when its refs may be removed.
fn bare_flaw(bare: &str, owner: u32, mode: u32, euid: u32, user: &str) -> Option<String> {
    match owner_flaw(owner, mode, euid, WRITABLE_BY_OTHERS)? {
        OwnerFlaw::Owner => Some(format!("{bare} is not owned by {user}")),
        OwnerFlaw::Open => Some(format!("{bare} is writable by others")),
    }
}

/// Whether `name` names a bare repository of the root: `<name>.git`, the extension in any case.
fn is_bare_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, extension)| !stem.is_empty() && extension.eq_ignore_ascii_case("git"))
}

/// Lets the key's lock `lock` go; in the keyed prune, a lock its take made is removed first, while it is still held
/// ([`HeldLock::still_held`]), so a key the builder never had leaves nothing behind and a run that opened the file
/// meanwhile locks a new one. A lock that cannot be removed is a line on stderr.
fn let_go(lock: HeldLock, mode: Mode, console: &mut Console) {
    if mode == Mode::Named && lock.made() {
        let removed = match lock.still_held() {
            Ok(true) => fs::remove_file(lock.path()),
            // Another file is at the name: it is not this sweep's to remove.
            Ok(false) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            console.err(&format!(
                "gate: remote could not remove the work folder lock {}: {}",
                escaped(&lock.path().display().to_string()),
                escaped(&error.to_string())
            ));
        }
    }
    drop(lock);
}

/// Makes the sweep's folder `path`, mode 0700, when missing; else the line saying it cannot be made or looked at, or is
/// a link or not a folder.
fn folder_made(path: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if plain_folder_there(path)? {
                Ok(())
            } else {
                Err(cannot_list(path, &io::Error::from(ErrorKind::NotFound)))
            }
        }
        Err(error) => Err(cannot_list(path, &error)),
    }
}

/// Whether the folder `path` of the sweep is there: `false` when it is missing. Else the line saying it cannot be
/// looked at, or is a link or not a folder, which ends the sweep.
fn plain_folder_there(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(not_plain(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(cannot_list(path, &error)),
    }
}

/// Removes each entry an earlier sweep left in the `pruned` folder `pruned`, each as itself, a line on stderr for each
/// that cannot be removed; one another sweep is removing meanwhile says nothing. Else the line saying the folder cannot
/// be listed, which ends the sweep.
fn leftovers_removed(pruned: &Path, console: &mut Console) -> Result<(), String> {
    for entry in fs::read_dir(pruned).map_err(|error| cannot_list(pruned, &error))? {
        let path = entry.map_err(|error| cannot_list(pruned, &error))?.path();
        if let Err(error) = entry_removed(&path)
            && !matches!(error.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty)
        {
            console.err(&could_not_remove_pruned(&path, &error));
        }
    }
    Ok(())
}

/// Removes the entry `path` as itself: a folder with everything in it, a link in it removed as itself; anything else,
/// a link included, as a file.
fn entry_removed(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// The keys in the work folder `work` and how many of their entries are plain folders: each name that is a key, and
/// each `<key>.<extension>` of [`KEY_FILES`]; other names are left out. Else why it cannot be listed.
fn listed(work: &Path) -> io::Result<Listed> {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut folders = 0;
    for entry in fs::read_dir(work)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stem = if is_key(name) {
            // An entry that cannot be looked at is no plain folder seen; the key's own sweep says why.
            if fs::symlink_metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir()) {
                folders += 1;
            }
            name
        } else if let Some(stem) = key_file_stem(name) {
            stem
        } else {
            continue;
        };
        keys.entry(stem.to_ascii_lowercase()).or_insert_with(|| stem.to_owned());
    }
    Ok(Listed {
        keys: keys.into_values().collect(),
        folders,
    })
}

/// The key of `name` when it is `<key>.<extension>` for one of [`KEY_FILES`], the extension in any case: the key is
/// the part before the first `.`, since a key holds none.
fn key_file_stem(name: &str) -> Option<&str> {
    let (stem, extension) = name.split_once('.')?;
    let known = KEY_FILES.iter().any(|known| known.eq_ignore_ascii_case(extension));
    (known && is_key(stem)).then_some(stem)
}

/// Whether `path` is a folder, as itself; `false` when it is not there, a link or anything else.
fn plain_folder_named(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The last write of the tree `tree`'s `.git/FETCH_HEAD`, read only through a `.git` that is a folder of its own;
/// `None` when either is not there or `.git` is a link.
fn fetched(tree: &Path) -> io::Result<Option<SystemTime>> {
    let git = tree.join(".git");
    if plain_folder_named(&git)? {
        written(&git.join("FETCH_HEAD"))
    } else {
        Ok(None)
    }
}

/// The last write of `path`, looked at as itself; `None` when nothing has the name.
fn written(path: &Path) -> io::Result<Option<SystemTime>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.modified().map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// How long a folder last used at `last_use` has been idle at `now`, when that is over [`IDLE_AGE`]; `None` while it
/// is not, a last use after `now` included.
fn idle_for(last_use: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(last_use).ok().filter(|idle| *idle > IDLE_AGE)
}

/// The line saying the folder `path` is a link or not a folder, which ends the sweep.
fn not_plain(path: &Path) -> String {
    format!(
        "gate: remote could not prune: {} is not a plain folder",
        escaped(&path.display().to_string())
    )
}

/// The line saying the key `key` is kept, for `why`.
fn could_not_prune(key: &str, why: &str) -> String {
    format!("gate: remote could not prune {}: {}", escaped(key), escaped(why))
}

/// The line saying the folder `path`, renamed aside, could not be removed, for `error`.
fn could_not_remove_pruned(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote could not remove the pruned folder {}: {}",
        escaped(&path.display().to_string()),
        escaped(&error.to_string())
    )
}

/// The line saying the folder `path` cannot be read or listed, for `error`.
fn cannot_list(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote cannot list {}: {}",
        escaped(&path.display().to_string()),
        escaped(&error.to_string())
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folders cannot be made or looked at must fail loudly")]
mod tests {
    use std::cell::RefCell;
    use std::fs::{self, DirBuilder, File, FileTimes};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::time::{Duration, SystemTime};

    use gate_wire::ROOT_UNSAFE;

    use super::{GIB, IDLE_AGE, Seams, free_bytes, idle_for, key_file_stem, no_rename_seen, prune, pruned_with};
    use crate::console::Console;
    use crate::files::held_for_test;

    use crate::lines::{FAILED_STATUS, USAGE_STATUS, job_record_text};
    use crate::pid_info::start_of;

    const KEY: &str = "repo-0123abcd";
    const TWO_DAYS: Duration = Duration::from_hours(48);

    /// A root private to this process and the case `name`, with its `work` folder, removed when the case ends.
    struct Case {
        base: PathBuf,
        root: PathBuf,
        tmp: PathBuf,
        now: SystemTime,
    }

    /// Free space no floor pass acts on.
    const PLENTY: u64 = 1000 * GIB;

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-prune-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            fs::create_dir_all(&base).expect("the case folder is made");
            let root = base.join("root");
            DirBuilder::new().mode(0o700).create(&root).expect("the root is made");
            DirBuilder::new().mode(0o700).create(root.join("work")).expect("the work folder is made");
            let tmp = base.join("tmp");
            fs::create_dir(&tmp).expect("the case's temp folder is made");
            Case {
                base,
                root,
                tmp,
                now: SystemTime::now(),
            }
        }

        fn work(&self) -> PathBuf {
            self.root.join("work")
        }

        fn pruned(&self) -> PathBuf {
            self.root.join("pruned")
        }

        /// The key's work folder, holding a file, last written `age` before the case's time.
        fn folder(&self, age: Duration) -> PathBuf {
            self.folder_of(KEY, age)
        }

        /// The work folder of `key`, holding a file, last written `age` before the case's time.
        fn folder_of(&self, key: &str, age: Duration) -> PathBuf {
            let folder = self.work().join(key);
            fs::create_dir(&folder).expect("the work folder is made");
            fs::write(folder.join("file"), "x").expect("a file is written in the work folder");
            dated(&folder, self.now - age);
            folder
        }

        /// The key's record `<key>.<extension>` holding `text`, last written `age` before the case's time.
        fn record(&self, extension: &str, text: &str, age: Duration) -> PathBuf {
            let path = self.work().join(format!("{KEY}.{extension}"));
            fs::write(&path, text).expect("the record is written");
            dated(&path, self.now - age);
            path
        }

        fn sweep(&self) -> (u8, Console) {
            self.sweep_seeing(&|_| {})
        }

        fn sweep_seeing(&self, before_rename: &dyn Fn(&Path)) -> (u8, Console) {
            let seams = self.seams(before_rename, &|_| Ok(PLENTY), Path::new("/Volumes"));
            self.ran(&self.root, &[], &seams)
        }

        /// The age prune, the free space read by `free`.
        fn sweep_freeing(&self, free: &dyn Fn(&Path) -> std::io::Result<u64>) -> (u8, Console) {
            let seams = self.seams(&|_| {}, free, Path::new("/Volumes"));
            self.ran(&self.root, &[], &seams)
        }

        /// The case's seams: floors of 100 and 150 GiB, and its own temp folder.
        fn seams<'a>(&'a self, before_rename: &'a dyn Fn(&Path), free: &'a dyn Fn(&Path) -> std::io::Result<u64>, volumes: &'a Path) -> Seams<'a> {
            Seams {
                before_rename,
                after_rename: &no_rename_seen,
                volumes,
                free,
                floor_low: 100 * GIB,
                floor_high: 150 * GIB,
                tmp: &self.tmp,
            }
        }

        /// The keyed prune of the case's root for `keys`.
        fn keyed(&self, keys: &[&str]) -> (u8, Console) {
            self.keyed_in(&self.root, keys, Path::new("/Volumes"))
        }

        /// The keyed prune of the root `root` for `keys`, its volumes mounted in `volumes`.
        fn keyed_in(&self, root: &Path, keys: &[&str], volumes: &Path) -> (u8, Console) {
            let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
            let seams = self.seams(&|_| {}, &|_| Ok(PLENTY), volumes);
            self.ran(root, &keys, &seams)
        }

        fn ran(&self, root: &Path, keys: &[String], seams: &Seams<'_>) -> (u8, Console) {
            let mut console = Console::captured();
            let status = pruned_with(root, keys, self.now, seams, &mut console);
            (status, console)
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::set_permissions(self.pruned(), fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Sets the last write of `path`, a file or a folder, to `when`.
    fn dated(path: &Path, when: SystemTime) {
        File::open(path)
            .and_then(|file| file.set_times(FileTimes::new().set_modified(when)))
            .expect("the last write is set");
    }

    fn summary(removed: usize, folders: usize) -> String {
        format!("gate: remote prune removed {removed} of {folders} work folders")
    }

    fn names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .expect("the folder is listed")
            .map(|entry| entry.expect("the entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_idle_folder_goes_with_its_records_and_the_sweep_says_so() {
        let case = Case::new("idle");
        case.folder(TWO_DAYS);
        for (extension, text) in [("warm", "w"), ("layout", "single"), ("index", "i"), ("index.lock", "l"), ("command", "c")] {
            case.record(extension, text, TWO_DAYS);
        }
        case.record("layout.gate-new-1-0", "single", TWO_DAYS);

        let (status, console) = case.sweep();

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote pruned {KEY}: idle 48 h"), summary(1, 1)]);
        assert_eq!(names(&case.work()), [format!("{KEY}.lock")], "the lock alone stays");
        assert!(names(&case.pruned()).is_empty(), "the folder set aside is removed");
    }

    #[test]
    fn a_folder_last_written_in_the_future_is_kept() {
        let case = Case::new("future");
        let folder = case.work().join(KEY);
        fs::create_dir(&folder).expect("the work folder is made");
        dated(&folder, case.now + TWO_DAYS);

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]), "{:?}", console.err);
        assert!(folder.is_dir());
    }

    #[test]
    fn a_held_lock_keeps_the_key() {
        let case = Case::new("held");
        let folder = case.folder(TWO_DAYS);
        let lock = held_for_test(&case.work().join(format!("{KEY}.lock")));

        let (status, console) = case.sweep();
        drop(lock);

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]));
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert!(folder.is_dir(), "a held key's folder stays");
    }

    /// A `sleep 600` leading a process group of its own, as a run's command does.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for a run"
    )]
    fn sleeping_group() -> Child {
        use std::os::unix::process::CommandExt;

        Command::new("sleep").arg("600").process_group(0).spawn().expect("sleep starts")
    }

    /// The id of a process group that is gone: a `true` that ran and was reaped.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for a run"
    )]
    fn ended_group() -> u32 {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("true").process_group(0).spawn().expect("true starts");
        child.wait().expect("true is reaped");
        child.id()
    }

    /// This process's session.
    fn own_session() -> i32 {
        // SAFETY: `getsid(0)` asks for the caller's own session and touches no memory.
        unsafe { libc::getsid(0) }
    }

    /// The text of a record naming the group of the live process `pid`, made in this process's session, with that
    /// process's start, as a run writes it.
    fn live_record_text(pid: u32) -> String {
        let pid = i32::try_from(pid).expect("a pid is a pid_t");
        job_record_text("job", pid, own_session(), start_of(pid).expect("the process's start is read"))
    }

    #[test]
    fn a_group_record_still_its_run_s_keeps_the_key_s_records() {
        let case = Case::new("live");
        let mut child = sleeping_group();
        let record = case.work().join(format!("{KEY}.pgid"));
        fs::write(&record, live_record_text(child.id())).expect("the record is written");
        let warm = case.record("warm", "w", TWO_DAYS);

        let (status, console) = case.sweep();
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!((status, console.out), (0, vec![summary(0, 0)]));
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert!(record.is_file() && warm.is_file(), "a live group's key keeps its records");
    }

    #[test]
    fn a_stale_group_record_goes_with_the_key() {
        let case = Case::new("stale");
        case.folder(TWO_DAYS);
        let group = i32::try_from(ended_group()).expect("a pid is a pid_t");
        let record = case.record("pgid", &job_record_text("job", group, own_session(), (1, 0)), TWO_DAYS);
        let tools = case.record("tools.pgid", "not a record", TWO_DAYS);
        let packages = case.record("packages.pgid", "not a record", TWO_DAYS);

        let (status, console) = case.sweep();

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote pruned {KEY}: idle 48 h"), summary(1, 1)]);
        assert!(
            !record.exists() && !tools.exists() && !packages.exists(),
            "a stale and a malformed record go with the key"
        );
    }

    #[test]
    fn a_fresh_fetch_in_a_pair_s_tree_keeps_the_key() {
        let case = Case::new("pair-fetch");
        let folder = case.work().join(KEY);
        let git = folder.join("leaf").join(".git");
        fs::create_dir_all(&git).expect("the tree's .git is made");
        fs::write(git.join("FETCH_HEAD"), "x").expect("FETCH_HEAD is written");
        dated(&git.join("FETCH_HEAD"), case.now - Duration::from_hours(1));
        dated(&folder.join("leaf"), case.now - TWO_DAYS);
        dated(&folder, case.now - TWO_DAYS);

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]), "{:?}", console.err);
        assert!(folder.is_dir());
    }

    #[test]
    fn the_layout_says_changing_and_the_records_are_gone_while_the_folder_is_still_in_work() {
        let case = Case::new("order");
        let folder = case.folder(TWO_DAYS);
        let layout = case.record("layout", "single", TWO_DAYS);
        let warm = case.record("warm", "w", TWO_DAYS);
        let seen = RefCell::new(None);

        let (status, console) = case.sweep_seeing(&|renamed| {
            let text = fs::read_to_string(&layout).expect("the layout is read");
            seen.replace(Some((renamed.to_owned(), text, folder.is_dir(), warm.exists())));
        });

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(seen.into_inner(), Some((folder.clone(), "changing".to_owned(), true, false)));
        assert!(!layout.exists() && !folder.exists(), "the layout goes once the folder is aside");
    }

    #[test]
    fn a_rename_that_fails_leaves_the_layout_changing() {
        let case = Case::new("rename-fails");
        let folder = case.folder(TWO_DAYS);
        let layout = case.record("layout", "single", TWO_DAYS);
        let warm = case.record("warm", "w", TWO_DAYS);
        DirBuilder::new()
            .mode(0o500)
            .create(case.pruned())
            .expect("an unwritable pruned folder is made");

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]));
        assert_eq!(console.err.len(), 1, "{:?}", console.err);
        assert!(
            console.err[0].starts_with(&format!("gate: remote could not prune {KEY}: ")),
            "{:?}",
            console.err
        );
        assert_eq!(fs::read_to_string(&layout).expect("the layout is read"), "changing");
        assert!(
            folder.is_dir() && !warm.exists(),
            "the folder stays in work, its records before the rename gone"
        );
    }

    #[test]
    fn a_folder_another_sweep_took_from_pruned_counts_as_removed() {
        let case = Case::new("aside-taken");
        case.folder(TWO_DAYS);
        let layout = case.record("layout", "single", TWO_DAYS);
        let taken = |aside: &Path| fs::remove_dir_all(aside).expect("the aside folder is taken");
        let seams = Seams {
            after_rename: &taken,
            ..case.seams(&|_| {}, &|_| Ok(PLENTY), Path::new("/Volumes"))
        };

        let (status, console) = case.ran(&case.root, &[], &seams);

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote pruned {KEY}: idle 48 h"), summary(1, 1)]);
        assert!(!layout.exists(), "the records after the rename go as usual");
        assert_eq!(names(&case.work()), [format!("{KEY}.lock")], "the lock alone stays");
    }

    #[test]
    fn a_link_where_the_work_folder_goes_is_refused() {
        let case = Case::new("link");
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the link's target is made");
        fs::write(elsewhere.join("file"), "x").expect("a file is written in the target");
        dated(&elsewhere, case.now - TWO_DAYS);
        let link = case.work().join(KEY);
        symlink(&elsewhere, &link).expect("the link is made");

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 0)]));
        assert_eq!(
            console.err,
            [format!("gate: remote could not prune {KEY}: {} is not a plain folder", link.display())]
        );
        assert!(
            elsewhere.join("file").is_file() && link.is_symlink(),
            "neither the link nor its target is touched"
        );
    }

    #[test]
    fn a_key_with_records_alone_has_them_removed_and_counts_no_folder() {
        let case = Case::new("records-alone");
        for (extension, text) in [
            ("warm", "w"),
            ("command", "c"),
            ("layout", "single"),
            ("tools.pgid", "x"),
            ("packages.pgid", "x"),
            ("index.lock", "l"),
        ] {
            case.record(extension, text, Duration::ZERO);
        }
        case.record("tools.gate-new-1-0", "t", Duration::ZERO);

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 0)]), "{:?}", console.err);
        assert_eq!(names(&case.work()), [format!("{KEY}.lock")], "the lock alone stays");
    }

    #[test]
    fn what_an_earlier_sweep_left_in_pruned_is_removed_first() {
        let case = Case::new("leftovers");
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the link's target is made");
        fs::write(elsewhere.join("file"), "x").expect("a file is written in the target");
        let left = case.pruned().join(format!("{KEY}.1.2"));
        fs::create_dir_all(left.join("sub")).expect("a leftover folder is made");
        fs::write(left.join("sub").join("file"), "x").expect("a leftover file is written");
        symlink(&elsewhere, left.join("link")).expect("a link in the leftover is made");
        fs::write(case.pruned().join("stray"), "x").expect("a stray file is written");
        symlink(&elsewhere, case.pruned().join("link")).expect("a link in pruned is made");

        let (status, console) = case.sweep();

        assert_eq!((status, console.out), (0, vec![summary(0, 0)]), "{:?}", console.err);
        assert!(names(&case.pruned()).is_empty(), "every leftover is removed");
        assert!(elsewhere.join("file").is_file(), "no link is followed");
    }

    #[test]
    fn a_root_that_is_not_absolute_or_a_word_that_is_not_a_key_is_refused_before_anything_is_opened() {
        let case = Case::new("usage");
        let mut console = Console::captured();

        let relative = prune(Path::new("gate-remote"), &[], case.now, &mut console);
        let not_a_key = prune(&case.root, &[KEY.to_owned(), "../x".to_owned()], case.now, &mut console);

        assert_eq!((relative, not_a_key), (USAGE_STATUS, USAGE_STATUS));
        assert_eq!(
            console.err,
            [
                r#"gate: box prune --root is not an absolute path: "gate-remote""#.to_owned(),
                r#"gate: box prune --keys holds a word that is not a key: "../x""#.to_owned(),
            ]
        );
        assert!(!case.pruned().exists(), "nothing is made beneath the root");
    }

    const HELD: &str = "held-0123abcd";
    const BROKEN: &str = "broken-0123abcd";

    fn gone(key: &str) -> String {
        format!("gate: remote pruned {key} (worktree gone)")
    }

    #[test]
    fn keys_pruned_held_and_failed_say_a_line_each_in_order_and_fail_the_prune() {
        let case = Case::new("keyed-three");
        let folder = case.folder(Duration::ZERO);
        let warm = case.record("warm", "w", Duration::ZERO);
        let held_folder = case.work().join(HELD);
        fs::create_dir(&held_folder).expect("the held key's folder is made");
        let lock = held_for_test(&case.work().join(format!("{HELD}.lock")));
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the link's target is made");
        let link = case.work().join(BROKEN);
        symlink(&elsewhere, &link).expect("the link is made");

        let (status, console) = case.keyed(&[KEY, HELD, BROKEN]);
        drop(lock);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(console.out, [gone(KEY), format!("gate: remote kept {HELD}: in use")]);
        assert_eq!(
            console.err,
            [format!("gate: remote could not prune {BROKEN}: {} is not a plain folder", link.display())]
        );
        assert!(!folder.exists() && !warm.exists(), "the fresh folder and its records go");
        assert!(held_folder.is_dir() && elsewhere.is_dir() && link.is_symlink());
        assert!(names(&case.pruned()).is_empty(), "the folder set aside is removed");
    }

    #[test]
    fn a_held_key_alone_is_kept_and_the_prune_passes() {
        let case = Case::new("keyed-held");
        let folder = case.folder(TWO_DAYS);
        let lock = held_for_test(&case.work().join(format!("{KEY}.lock")));

        let (status, console) = case.keyed(&[KEY]);
        drop(lock);

        assert_eq!((status, console.out), (0, vec![format!("gate: remote kept {KEY}: in use")]));
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert!(folder.is_dir());
    }

    #[test]
    fn a_key_the_builder_never_had_leaves_no_lock() {
        let case = Case::new("keyed-never");
        fs::remove_dir(case.work()).expect("the work folder is removed");

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert!(names(&case.work()).is_empty(), "the work folder is made, and no lock is left in it");
    }

    #[test]
    fn a_lock_that_was_there_before_the_keyed_prune_stays() {
        let case = Case::new("keyed-lock-stays");
        case.folder(Duration::ZERO);
        let lock = case.record("lock", "", Duration::ZERO);

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert_eq!(names(&case.work()), [format!("{KEY}.lock")], "the lock alone stays");
        assert!(lock.is_file());
    }

    /// Runs git with `args`; its standard output, trimmed. Fails the case unless git exits 0.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this sets a case's repository up"
    )]
    fn git(args: &[&str]) -> String {
        let ran = Command::new("git")
            .args(["-c", "user.name=gate", "-c", "user.email=gate@example.com"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("git starts");
        assert!(ran.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ran.stderr));
        String::from_utf8_lossy(&ran.stdout).trim().to_owned()
    }

    const OTHER: &str = "other-0123abcd";

    /// A bare repository at `path` holding a commit, with the key's three refs and another key's pointed at it.
    fn bare_with_refs(path: &Path) -> String {
        let bare = path.to_str().expect("the temp folder is UTF-8").to_owned();
        git(&["init", "--bare", "-q", &bare]);
        let git_dir = format!("--git-dir={bare}");
        let tree = git(&[&git_dir, "mktree", "--missing"]);
        let commit = git(&[&git_dir, "commit-tree", &tree, "-m", "snapshot"]);
        for reference in ["gate", "gate-bridge", "gate-back"].map(|kind| format!("refs/{kind}/{KEY}")) {
            git(&[&git_dir, "update-ref", &reference, &commit]);
        }
        git(&[&git_dir, "update-ref", &format!("refs/gate/{OTHER}"), &commit]);
        git_dir
    }

    /// The refs of the bare repository `git_dir` names, sorted.
    fn refs(git_dir: &str) -> Vec<String> {
        let listed = git(&[git_dir, "for-each-ref", "--format=%(refname)"]);
        listed.lines().map(str::to_owned).collect()
    }

    fn all_refs() -> Vec<String> {
        vec![
            format!("refs/gate-back/{KEY}"),
            format!("refs/gate-bridge/{KEY}"),
            format!("refs/gate/{OTHER}"),
            format!("refs/gate/{KEY}"),
        ]
    }

    #[test]
    fn the_keyed_prune_deletes_the_key_s_refs_in_every_bare_packed_or_loose() {
        let case = Case::new("refs-every-bare");
        case.folder(Duration::ZERO);
        let loose = bare_with_refs(&case.root.join("a.git"));
        let packed = bare_with_refs(&case.root.join("b.git"));
        git(&[&packed, "pack-refs", "--all"]);

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert_eq!(refs(&loose), [format!("refs/gate/{OTHER}")]);
        assert_eq!(refs(&packed), [format!("refs/gate/{OTHER}")]);
    }

    #[test]
    fn a_symbolic_ref_of_the_key_is_deleted_and_not_what_it_points_at() {
        let case = Case::new("refs-symbolic");
        let bare = bare_with_refs(&case.root.join("a.git"));
        let commit = git(&[&bare, "rev-parse", &format!("refs/gate/{KEY}")]);
        git(&[&bare, "update-ref", "refs/heads/main", &commit]);
        git(&[&bare, "symbolic-ref", &format!("refs/gate/{KEY}"), "refs/heads/main"]);

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert_eq!(refs(&bare), [format!("refs/gate/{OTHER}"), "refs/heads/main".to_owned()]);
    }

    #[test]
    fn a_key_whose_rename_fails_keeps_its_refs() {
        let case = Case::new("refs-rename-fails");
        let folder = case.folder(Duration::ZERO);
        let bare = bare_with_refs(&case.root.join("a.git"));
        DirBuilder::new()
            .mode(0o500)
            .create(case.pruned())
            .expect("an unwritable pruned folder is made");

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(console.err.len(), 1, "{:?}", console.err);
        assert!(console.err[0].starts_with(&format!("gate: remote could not prune {KEY}: ")));
        assert!(folder.is_dir());
        assert_eq!(refs(&bare), all_refs());
    }

    #[test]
    fn a_bare_others_may_write_keeps_its_refs_and_is_named_while_the_others_are_pruned() {
        let case = Case::new("refs-open-bare");
        case.folder(Duration::ZERO);
        let path = case.root.join("a.git");
        let open = bare_with_refs(&path);
        let other = bare_with_refs(&case.root.join("b.git"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o775)).expect("the bare is opened to the group");

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let shown = path.display();
        let line = format!("gate: remote could not prune {KEY}: its refs could not be removed: {shown} is writable by others");
        assert_eq!(console.err, [line]);
        assert_eq!(refs(&open), all_refs(), "the open bare is untouched");
        assert_eq!(refs(&other), [format!("refs/gate/{OTHER}")]);
    }

    #[test]
    fn a_bare_another_user_owns_or_others_may_write_is_named_raw_in_the_key_s_line() {
        use crate::prune::{bare_flaw, could_not_prune};
        let why = bare_flaw("/r/a.git", 502, 0o755, 501, "b").expect("another user's bare is left");
        assert_eq!(
            could_not_prune(KEY, &format!("its refs could not be removed: {why}")),
            format!("gate: remote could not prune {KEY}: its refs could not be removed: /r/a.git is not owned by b")
        );
        assert_eq!(
            bare_flaw("/r/a.git", 501, 0o775, 501, "b"),
            Some("/r/a.git is writable by others".to_owned())
        );
        assert_eq!(bare_flaw("/r/a.git", 501, 0o755, 501, "b"), None);
    }

    #[test]
    fn a_bare_name_that_is_a_link_or_no_whole_bare_is_skipped() {
        let case = Case::new("refs-skipped");
        let target = bare_with_refs(&case.base.join("elsewhere.git"));
        symlink(case.base.join("elsewhere.git"), case.root.join("c.git")).expect("the link is made");
        let partial = case.root.join("d.git");
        fs::create_dir_all(partial.join("refs")).expect("a bare without objects is made");
        fs::write(partial.join("HEAD"), "ref: refs/heads/main\n").expect("its HEAD is written");

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert_eq!(refs(&target), all_refs(), "no git ran through the link");
    }

    /// The keyed prune of the case `name`'s root set to the mode `mode`, which the root check refuses: every key fails
    /// with the check's reason, and nothing in the root is touched.
    fn a_refused_root_is_left_whole(name: &str, mode: u32) {
        let case = Case::new(name);
        let folder = case.folder(Duration::ZERO);
        let warm = case.record("warm", "w", Duration::ZERO);
        let bare = bare_with_refs(&case.root.join("a.git"));
        let left = case.pruned().join(format!("{KEY}.1.2"));
        fs::create_dir_all(&left).expect("a leftover in pruned is made");
        fs::set_permissions(&case.root, fs::Permissions::from_mode(mode)).expect("the root is opened up");

        let (status, console) = case.keyed(&[KEY, HELD]);

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        let refused = &ROOT_UNSAFE["gate: ".len()..];
        assert_eq!(console.err.len(), 2, "{:?}", console.err);
        for (line, key) in console.err.iter().zip([KEY, HELD]) {
            assert!(line.starts_with(&format!("gate: remote could not prune {key}: {refused}")), "{line}");
        }
        assert!(folder.is_dir() && warm.is_file() && left.is_dir(), "nothing in the root is touched");
        assert_eq!(names(&case.work()), [KEY.to_owned(), format!("{KEY}.warm")], "no lock is made");
        assert_eq!(refs(&bare), all_refs());
    }

    #[test]
    fn a_root_anyone_may_write_in_is_left_whole_and_fails_each_key() {
        a_refused_root_is_left_whole("refused-0777", 0o777);
    }

    #[test]
    fn a_root_its_group_may_write_in_is_left_whole_and_fails_each_key() {
        a_refused_root_is_left_whole("refused-0775", 0o775);
    }

    #[test]
    fn the_keyed_prune_runs_no_hook_of_a_bare() {
        let case = Case::new("refs-no-hook");
        let bare_path = case.root.join("a.git");
        let bare = bare_with_refs(&bare_path);
        let marker = case.base.join("hook-ran");
        let hooks = bare_path.join("hooks");
        fs::create_dir_all(&hooks).expect("the hooks folder is made");
        let hook = hooks.join("reference-transaction");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("the hook is written");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).expect("the hook is made runnable");

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!((status, console.out), (0, vec![gone(KEY)]), "{:?}", console.err);
        assert_eq!(refs(&bare), [format!("refs/gate/{OTHER}")]);
        assert!(!marker.exists(), "no hook ran");
    }

    #[test]
    fn the_age_prune_deletes_no_ref() {
        let case = Case::new("refs-aged");
        case.folder(TWO_DAYS);
        let bare = bare_with_refs(&case.root.join("a.git"));

        let (status, console) = case.sweep();

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, [format!("gate: remote pruned {KEY}: idle 48 h"), summary(1, 1)]);
        assert_eq!(refs(&bare), all_refs());
    }

    #[test]
    fn a_failing_update_ref_fails_the_key_and_the_next_bare_is_still_tried() {
        let case = Case::new("refs-fail");
        let failing = bare_with_refs(&case.root.join("a.git"));
        let next = bare_with_refs(&case.root.join("b.git"));
        let shim = case.base.join("git-shim");
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*\"a.git update-ref \"*) echo 'fatal: refs locked' >&2; exit 128;;\nesac\nexec '{}' \"$@\"\n",
            crate::git::test_program::real_git().display()
        );
        fs::write(&shim, script).expect("the shim is written");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).expect("the shim is made runnable");
        crate::git::test_program::set(&shim);

        let (status, console) = case.keyed(&[KEY]);

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(
            console.err,
            [format!(
                "gate: remote could not prune {KEY}: its refs could not be removed: fatal: refs locked"
            )]
        );
        assert_eq!(refs(&failing), all_refs());
        assert_eq!(refs(&next), [format!("refs/gate/{OTHER}")]);
    }

    const OLDEST: &str = "oldest-0123abcd";
    const OLDER: &str = "older-0123abcd";
    const RECENT: &str = "recent-0123abcd";
    const NEWEST: &str = "newest-0123abcd";
    const LOCKED: &str = "locked-0123abcd";
    const LIVE: &str = "live-0123abcd";

    /// A free-space reader: `start` GiB, and `each` more for every key of `keys` whose work folder is gone from `work`.
    fn freeing(work: &Path, keys: &'static [&'static str], start: u64, each: u64) -> impl Fn(&Path) -> std::io::Result<u64> + use<> {
        let work = work.to_owned();
        move |_| {
            let gone = u64::try_from(keys.iter().filter(|key| !work.join(key).exists()).count()).expect("a count fits");
            Ok((start + each * gone) * GIB)
        }
    }

    #[test]
    fn under_the_floor_the_oldest_free_keys_go_until_the_high_floor_and_a_recent_one_last() {
        let case = Case::new("floor");
        case.folder_of(OLDEST, Duration::from_hours(20));
        case.folder_of(OLDER, Duration::from_hours(10));
        case.folder_of(RECENT, Duration::from_mins(10));
        case.folder_of(NEWEST, Duration::from_mins(5));
        let locked = case.folder_of(LOCKED, Duration::from_hours(22));
        let lock = held_for_test(&case.work().join(format!("{LOCKED}.lock")));
        let live = case.folder_of(LIVE, Duration::from_hours(21));
        let mut child = sleeping_group();
        fs::write(case.work().join(format!("{LIVE}.pgid")), live_record_text(child.id())).expect("the record is written");
        let free = freeing(&case.work(), &[OLDEST, OLDER, RECENT, NEWEST], 90, 25);

        let (status, console) = case.sweep_freeing(&free);
        drop(lock);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert_eq!(
            console.out,
            [
                format!("gate: remote pruned {OLDEST}: 90 GiB free"),
                format!("gate: remote pruned {OLDER}: 115 GiB free"),
                format!("gate: remote pruned {RECENT}: 140 GiB free"),
                summary(3, 6),
            ]
        );
        assert!(case.work().join(NEWEST).is_dir() && locked.is_dir() && live.is_dir());
    }

    #[test]
    fn a_key_the_age_pass_could_not_prune_says_so_once_under_the_floor() {
        let case = Case::new("floor-failed");
        let folder = case.folder(TWO_DAYS);
        fs::create_dir(case.work().join(format!("{KEY}.pgid"))).expect("the record's name is taken by a folder");

        let (status, console) = case.sweep_freeing(&|_| Ok(90 * GIB));

        assert_eq!(status, 0, "{:?}", console.err);
        let said = format!("gate: remote could not prune {KEY}: ");
        assert_eq!(console.err.iter().filter(|line| line.starts_with(&said)).count(), 1, "{:?}", console.err);
        assert!(folder.is_dir(), "a key that could not be pruned keeps its folder");
    }

    #[test]
    fn over_the_low_floor_nothing_goes() {
        let case = Case::new("floor-over");
        let folder = case.folder(Duration::from_hours(20));

        let (status, console) = case.sweep_freeing(&|_| Ok(120 * GIB));

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]), "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert!(folder.is_dir());
    }

    #[test]
    fn a_removal_that_frees_nothing_ends_the_floor_pass() {
        let case = Case::new("floor-stuck");
        case.folder_of(OLDEST, Duration::from_hours(20));
        let older = case.folder_of(OLDER, Duration::from_hours(10));

        let (status, console) = case.sweep_freeing(&|_| Ok(90 * GIB));

        assert_eq!(status, 0);
        assert_eq!(console.out, [format!("gate: remote pruned {OLDEST}: 90 GiB free"), summary(1, 2)]);
        assert_eq!(console.err, ["gate: remote prune could not reach the free-space floor: 90 GiB free"]);
        assert!(older.is_dir());
    }

    #[test]
    fn a_free_space_that_cannot_be_read_says_so_and_the_sweep_passes() {
        let case = Case::new("floor-unread");
        let folder = case.folder(Duration::from_hours(20));

        let (status, console) = case.sweep_freeing(&|_| Err(std::io::Error::other("statfs failed")));

        assert_eq!((status, console.out), (0, vec![summary(0, 1)]));
        assert_eq!(
            console.err,
            [format!(
                "gate: remote could not read the free space of {}: statfs failed",
                case.root.display()
            )]
        );
        assert!(folder.is_dir());
    }

    #[test]
    fn the_free_space_of_a_real_folder_is_read() {
        let case = Case::new("statfs");

        let free = free_bytes(&case.root).expect("the free space is read");

        assert!(free > 0);
        assert!(free_bytes(&case.base.join("none")).is_err());
    }

    /// A folder at `path` holding a file, both last written `age` before `now`.
    fn run_folder(path: &Path, now: SystemTime, age: Duration) {
        fs::create_dir(path).expect("the run folder is made");
        fs::write(path.join("file"), "x").expect("a file is written in it");
        dated(&path.join("file"), now - age);
        dated(path, now - age);
    }

    #[test]
    fn the_sweep_removes_old_leftover_run_folders_alone() {
        let case = Case::new("tmp");
        let tmp = case.tmp.clone();
        run_folder(&tmp.join("gate.Ab3dE6g8"), case.now, TWO_DAYS);
        run_folder(&tmp.join("gate.young123"), case.now, Duration::from_hours(1));
        run_folder(&tmp.join("gate.abc"), case.now, TWO_DAYS);
        run_folder(&tmp.join("gate.ABCDEFGHI"), case.now, TWO_DAYS);
        run_folder(&tmp.join("gate.inner123"), case.now, TWO_DAYS);
        fs::write(tmp.join("gate.inner123").join("new"), "x").expect("a young entry is written");
        dated(&tmp.join("gate.inner123"), case.now - TWO_DAYS);
        fs::write(tmp.join("gate.file1234"), "x").expect("a file is written");
        dated(&tmp.join("gate.file1234"), case.now - TWO_DAYS);
        let elsewhere = case.base.join("elsewhere");
        run_folder(&elsewhere, case.now, TWO_DAYS);
        symlink(&elsewhere, tmp.join("gate.link1234")).expect("the link is made");

        let (status, console) = case.sweep();

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(console.err.is_empty(), "{:?}", console.err);
        assert_eq!(
            console.out,
            [
                format!(
                    "gate: remote removed the leftover run folder {}: idle 48 h",
                    tmp.join("gate.Ab3dE6g8").display()
                ),
                summary(0, 0),
            ]
        );
        assert_eq!(
            names(&tmp),
            [
                "gate.ABCDEFGHI",
                "gate.abc",
                "gate.file1234",
                "gate.inner123",
                "gate.link1234",
                "gate.young123"
            ]
        );
        assert!(elsewhere.join("file").is_file(), "the link's target is left");
    }

    #[test]
    fn a_missing_root_whose_parent_can_be_read_has_every_key_gone() {
        let case = Case::new("keyed-gone-root");
        let root = case.base.join("none");

        let (status, console) = case.keyed_in(&root, &[KEY, HELD], Path::new("/Volumes"));

        assert_eq!((status, console.out), (0, vec![gone(KEY), gone(HELD)]), "{:?}", console.err);
        assert!(!root.exists(), "the prune makes no root");
    }

    fn volume_unknown(key: &str) -> String {
        format!("gate: remote could not prune {key}: the root's volume is not there")
    }

    #[test]
    fn a_missing_root_whose_parent_is_missing_says_nothing_of_its_keys() {
        let case = Case::new("keyed-no-parent");
        let root = case.base.join("none").join("root");

        let (status, console) = case.keyed_in(&root, &[KEY, HELD], Path::new("/Volumes"));

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(console.err, [volume_unknown(KEY), volume_unknown(HELD)]);
    }

    #[test]
    fn a_root_on_a_volume_not_mounted_says_nothing_of_its_keys() {
        let case = Case::new("keyed-no-volume");
        let volumes = case.base.join("Volumes");
        fs::create_dir(&volumes).expect("the volumes folder is made");
        let root = volumes.join("Builds");

        let (status, console) = case.keyed_in(&root, &[KEY], &volumes);

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(console.err, [volume_unknown(KEY)]);
    }

    #[test]
    fn a_root_that_is_not_there_holds_nothing() {
        let case = Case::new("missing-root");
        let root = case.base.join("none");

        let (status, console) = case.ran(&root, &[], &case.seams(&|_| {}, &|_| Ok(PLENTY), Path::new("/Volumes")));

        assert_eq!((status, console.out), (0, vec![summary(0, 0)]), "{:?}", console.err);
        assert!(!root.exists(), "the prune makes no root");
    }

    #[test]
    fn a_root_others_may_write_in_is_refused() {
        let case = Case::new("open-root");
        fs::set_permissions(&case.root, fs::Permissions::from_mode(0o777)).expect("the root is opened up");

        let (status, console) = case.sweep();

        assert_eq!(status, FAILED_STATUS);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(console.err.len(), 1, "{:?}", console.err);
        assert!(console.err[0].starts_with(ROOT_UNSAFE), "{:?}", console.err);
        assert!(!case.pruned().exists(), "nothing is made beneath the root");
    }

    #[test]
    fn a_key_s_records_are_named_by_the_part_before_the_first_dot() {
        assert_eq!(key_file_stem("k.tools.pgid"), Some("k"));
        assert_eq!(key_file_stem("k.packages.pgid"), Some("k"));
        assert_eq!(key_file_stem("k.INDEX.lock"), Some("k"));
        assert_eq!(key_file_stem("k.layout.gate-new-1-0"), None, "a temp file names no key");
        assert_eq!(key_file_stem("k.nope"), None);
    }

    #[test]
    fn a_folder_last_used_exactly_a_day_ago_is_not_idle() {
        let now = SystemTime::now();

        assert_eq!(idle_for(now - IDLE_AGE, now), None);
    }

    #[test]
    fn a_folder_last_used_a_day_and_a_second_ago_is_idle() {
        let now = SystemTime::now();
        let idle = IDLE_AGE + Duration::from_secs(1);

        assert_eq!(idle_for(now - idle, now), Some(idle));
    }

    #[test]
    fn a_folder_last_used_in_the_future_is_not_idle() {
        let now = SystemTime::now();

        assert_eq!(idle_for(now + TWO_DAYS, now), None);
    }
}

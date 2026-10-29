//! `gate box prune --root <folder> [--keys <key>...]`: removes keys' work folders from a builder root, in one of two
//! modes. The age prune, without `--keys`, which a scheduled task on the builder runs every few hours, removes the work
//! folders no run has used for over a day: a folder's last use is the newest last write of the folder, each tree's
//! `.git\FETCH_HEAD` (every run fetches into each tree it checks out) and the key's records. The keyed prune removes
//! the keys it is given, whatever their last use, and no other: the PC names the keys of worktrees it found gone.
//! Either way a key's work folder `<root>\work\<key>` goes only with its lock `<root>\work\<key>.lock` held, so no run
//! is using it and none can start in it meanwhile (one that tries is refused busy, status 75, and its PC runs the
//! command itself); a key whose lock is held is kept.
//! The folder's layout record is marked changing and its other records removed first, then the folder is renamed into
//! `<root>\pruned` under the lock, so no run ever sees it half removed, and its layout and index records go once it is
//! there; a rename that fails leaves the layout marked changing, so the next run clears the folder whole. The folder is
//! removed from `pruned` after the lock is let go; what a removal leaves there the next sweep takes. A key with no
//! folder has its records removed.
//! The keyed prune also deletes the key's refs `refs/gate/<key>`, `refs/gate-bridge/<key>` and `refs/gate-back/<key>`,
//! themselves and never what a symbolic one points to, in every bare repository `<root>\<name>.git`: still under the
//! lock, once the folder is out of `work` or found absent. A key whose folder could not be renamed aside keeps them, and
//! so does every key of a root another account may write in ([`gate_win32::check_work_root`]), where no git runs. The
//! age prune never deletes a ref: a worktree still alive may have pushed one just before the lock was taken. A lock the
//! keyed prune made for a key is deleted as it is let go, so a key the builder never had leaves nothing behind.
//! Nothing after the root is opened is looked up by path but a bare repository, which git is given by its path: the
//! root, `work` and `pruned` are opened as themselves and held for the whole sweep, and every name in them is opened
//! beneath its folder's handle as itself, never followed. A bare repository is opened as itself for reading and listing,
//! sharing no deletion, and held while git deletes its refs, so no one can rename it and swap a link in where it was. A
//! link where the root, `work`, `pruned` or a work folder goes is refused; a link named as a bare repository, or a
//! `.git` folder without `HEAD`, `objects` and `refs`, is skipped; and a link inside a folder is removed as itself.
//! Both modes first read the builder's drain barrier, read only ([`drain::barrier_stands`]), and the keyed prune reads
//! it again before each key: while it stands, the age prune touches nothing and the keyed prune keeps each key left. A
//! barrier that cannot be read fails the prune. Once its sweep is done, the age prune runs `git gc --auto` in each bare
//! repository, one at a time in name order, since the bares' pushes run no gc of their own: only in a root
//! [`gate_win32::check_work_root`] passes, and each only while the barrier, read again before it, does not stand. A gc
//! that fails is said and the next goes on.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_args::BoxSeams;
use gate_router::escaped;
use gate_win32::{Make, Share};
use gate_wire::{LAYOUT_CHANGING, is_drive_path, is_key};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::forward::{ClientGone, OutputLock};
use crate::boxside::preflight::{Git, GitClass, GitFailure, GitOutput, GitStep, Pipe, Shared, Streams, is_folder_itself, written_beneath};
use crate::boxside::run::{FILE_FLAG_OPEN_REPARSE_POINT, LOCK_VIOLATION, SHARING_VIOLATION};
use crate::boxside::{drain, stamping};
use crate::console::Console;

/// How long a work folder goes unused before a sweep removes it.
const IDLE_AGE: Duration = Duration::from_hours(24);
/// The extensions of the files beside a work folder `<key>` named `<key>.<extension>`: its lock and its records.
const KEY_FILES: [&str; 10] = [
    "lock",
    "warm",
    "layout",
    "inputs",
    "sent-build",
    "tools",
    "packages",
    "index",
    "command",
    "ran",
];
/// The records a sweep removes before it renames the work folder aside, in the order the box clears them; `command`
/// is the interrupted-command record, cleared like the others, and `ran` the record of a command started there.
const RECORDS_BEFORE: [&str; 7] = ["warm", "ran", "packages", "tools", "inputs", "sent-build", "command"];
/// The access a sweep looks at a work folder with before it knows the folder is idle: its attributes, and its listing
/// for the trees in it. No open's sharing refuses the first, and any open that shares reading allows the second, so a
/// folder in use, held without delete sharing, is looked at without a failure.
const LOOK_ACCESS: u32 = FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY;
/// The records a sweep removes once the work folder is renamed aside: the layout, marked changing until then, and the
/// index. The lock stays.
const RECORDS_AFTER: [&str; 2] = ["layout", "index"];
/// The seconds in an hour, as the idle time is said.
const HOUR_SECONDS: u64 = 3600;
/// The access a sweep holds a key's lock with: enough to keep every run, which opens it for reading and writing sharing
/// nothing, and every other sweep, which asks for `DELETE` sharing no deletion, out while it is held.
const LOCK_ACCESS: u32 = FILE_GENERIC_READ | DELETE;
/// The kinds of a key's refs in a bare repository, each `refs/<kind>/<key>`: the PC's snapshot, its bridge, and the
/// box's back commit.
const REF_KINDS: [&str; 3] = ["gate", "gate-bridge", "gate-back"];
/// The git step that deletes a key's refs in one bare repository, as its lines and `--test-box-git-limit` name it.
const REFS_STEP: &str = "update-ref";
/// The git step that collects one bare repository after the age prune's sweep, as `--test-box-git-limit` names it.
const GC_STEP: &str = "gc";

/// Prunes the root `root`, after what an earlier sweep left in `<root>\pruned`: without `keys`, the work folders no
/// run has used for over [`IDLE_AGE`] before `now` ([`aged`]); with them, those keys alone ([`named`]). The exit
/// status: [`USAGE_STATUS`] for a root that is not an absolute path on a drive or a key that is no key, before anything
/// is opened; [`FAILED_STATUS`] when the root, the work folders or the `pruned` folder could not be opened or listed,
/// or one of them is not a plain folder, or the drain barrier could not be read; else each mode's own. The barrier is
/// judged at the time `seams` give, and the keyed prune's check of the root walks the folders above it up to the
/// suite's walk stop, when `seams` give one.
pub(crate) fn prune(root: &Path, keys: &[String], seams: &BoxSeams, now: SystemTime, console: &mut Console) -> u32 {
    // The parse takes keys alone; this guards the names the sweep opens beneath `work` all the same.
    if let Some(word) = keys.iter().find(|key| !is_key(key)) {
        stamping::err(console, &format!("gate: box prune --keys holds a word that is not a key: {word:?}"));
        return u32::from(USAGE_STATUS);
    }
    let Some(text) = root.to_str().filter(|text| is_drive_path(text)) else {
        stamping::err(
            console,
            &format!("gate: box prune --root is not an absolute path on a drive: {:?}", root.to_string_lossy()),
        );
        return u32::from(USAGE_STATUS);
    };
    let root = text.trim_end_matches(['\\', '/']);
    let swept = if keys.is_empty() {
        aged(root, now, seams, console)
    } else {
        named(root, keys, seams, now, console)
    };
    match swept {
        Ok(status) => status,
        Err(line) => {
            stamping::err(console, &line);
            u32::from(FAILED_STATUS)
        }
    }
}

/// The age prune of the root `root`, given without a trailing separator: removes the work folders idle at `now`, and
/// the records of a key whose folder is gone; says each folder removed and how many, with a line on stderr for each
/// key kept for a failure, then runs gc in each bare repository ([`Gc::bares_collected`]), and gives 0. Under a
/// standing drain barrier, judged at the time `seams` give, it says so and touches nothing. Else the line saying why
/// the sweep ended, or the barrier could not be read. A root or work folder that is not there holds nothing to remove.
fn aged(root: &str, now: SystemTime, seams: &BoxSeams, console: &mut Console) -> Result<u32, String> {
    if drained(root, seams)? {
        stamping::out(console, "gate: remote prune skipped: the builder is drained");
        return Ok(0);
    }
    let Some(sweep) = Sweep::opened(root, now, false, console)? else {
        stamping::out(console, "gate: remote prune removed 0 of 0 work folders");
        return Ok(0);
    };
    let listed = listed(&sweep.work).map_err(|error| cannot_list(Path::new(&sweep.work_path), &error))?;
    let removed = listed.keys.iter().filter(|key| sweep.aged_key(key, console)).count();
    stamping::out(
        console,
        &format!("gate: remote prune removed {removed} of {} work folders", listed.folders),
    );
    // The gc holds only the root: `work` and `pruned` are let go once the sweep is done.
    let Sweep {
        root: swept,
        root_folder,
        work,
        pruned,
        git,
        ..
    } = sweep;
    drop((work, pruned));
    let gc = Gc {
        root: &swept,
        root_folder: &root_folder,
        git: &git,
        seams,
    };
    gc.bares_collected(console)?;
    Ok(0)
}

/// The age prune's gc of the bare repositories of a root, once its sweep is done: the root, without a trailing
/// separator, and its handle, held for the whole gc; git; and the seams the drain barrier and the root's check take.
struct Gc<'a> {
    root: &'a str,
    root_folder: &'a File,
    git: &'a Git,
    seams: &'a BoxSeams,
}

/// How one bare repository's gc ended.
enum Collected {
    /// It ran to its end with status 0, or the bare was skipped.
    Done,
    /// It failed, for this reason.
    Failed(String),
    /// The client is gone: no further gc starts.
    ClientGone,
}

impl Gc<'_> {
    /// Runs `git gc --auto` in each bare repository of the root ([`held_bare`]), one at a time in name order, each held
    /// while its gc runs ([`Gc::collected`]), once the root passes [`gate_win32::check_work_root`], whose hold lasts the
    /// whole gc, and each only while the drain barrier does not stand. A gc that fails, a bare that cannot be opened, a
    /// root that cannot be listed and a root the check refuses are each a line on stdout, and the next bare goes on; a
    /// standing barrier is a line that ends the gc, and so is a gone client, with none. Whether a standing barrier ended
    /// it; else the line saying the barrier could not be read.
    fn bares_collected(&self, console: &mut Console) -> Result<bool, String> {
        let names = match self.bare_names() {
            Ok(names) if names.is_empty() => return Ok(false),
            Ok(names) => names,
            Err(error) => {
                stamping::out(console, &gc_failed(self.root, &error.to_string()));
                return Ok(false);
            }
        };
        let checked = gate_win32::check_work_root(Path::new(&root_name(self.root)), self.seams.root_walk_stop.as_deref());
        if let Err(why) = &checked {
            stamping::out(console, &gc_failed(self.root, &why.to_string()));
            return Ok(false);
        }
        for name in names {
            if drained(self.root, self.seams)? {
                stamping::out(console, "gate: remote prune gc stopped: the builder is drained");
                return Ok(true);
            }
            match self.collected(&name, console) {
                Collected::Done => {}
                Collected::Failed(why) => stamping::out(console, &gc_failed(&name, &why)),
                Collected::ClientGone => break,
            }
        }
        drop(checked);
        Ok(false)
    }

    /// The names in the root that name a bare repository ([`is_bare_name`]), in name order; else why the root cannot be
    /// listed.
    fn bare_names(&self) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = gate_win32::names_beneath(self.root_folder)?
            .into_iter()
            .filter_map(|name| name.into_string().ok())
            .filter(|name| is_bare_name(name))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Runs `git gc --auto` in the bare repository `<root>\<name>` when it is a whole one, held while it runs, and never
    /// detached, as a long write of the shared bare repository a gone client does not cut short
    /// ([`GitClass::LongBare`]). A failure names git's last line on stderr, which names its failure, or why it did not
    /// run to its end.
    fn collected(&self, name: &str, console: &mut Console) -> Collected {
        let held = match held_bare(self.root_folder, name) {
            Ok(Some(held)) => held,
            Ok(None) => return Collected::Done,
            Err(error) => return Collected::Failed(error.to_string()),
        };
        let git_dir = format!(r"--git-dir={}\{name}", self.root);
        let streams = Streams {
            stdout: Pipe::Discard,
            stderr: Pipe::Read,
            feed: None,
        };
        let step = GitStep {
            name: GC_STEP,
            class: GitClass::LongBare,
        };
        let (lock, gone) = (OutputLock::default(), ClientGone::default());
        let args = [git_dir.as_str(), "-c", "gc.autoDetach=false", "gc", "--auto"];
        let ran = self.git.run(&args, step, &streams, Shared::alone(&lock, &gone), console);
        drop(held);
        match ran {
            Ok(GitOutput { code: 0, .. }) => Collected::Done,
            Ok(ran) => {
                let stderr = String::from_utf8_lossy(&ran.stderr);
                let said = stderr.lines().map(str::trim).rfind(|line| !line.is_empty()).unwrap_or_default();
                Collected::Failed(if said.is_empty() {
                    format!("git exited with status {}", ran.code)
                } else {
                    said.to_owned()
                })
            }
            Err(GitFailure::NotStarted(why) | GitFailure::Failed(why)) => Collected::Failed(why),
            Err(GitFailure::ClientGone) => Collected::ClientGone,
        }
    }
}

/// Whether the drain barrier stands under the root `root`, given without a trailing separator, at the time `seams`
/// give ([`drain::barrier_stands`]); else the line saying the prune failed for why it could not be read.
fn drained(root: &str, seams: &BoxSeams) -> Result<bool, String> {
    drain::barrier_stands(Path::new(&root_name(root)), seams).map_err(|why| format!("gate: remote prune failed: {why}"))
}

/// Says each key of `keys` kept, the builder being drained.
fn kept_drained(keys: &[String], console: &mut Console) {
    for key in keys {
        stamping::out(console, &format!("gate: remote kept {}: the builder is drained", escaped(key)));
    }
}

/// The keyed prune of the root `root`, given without a trailing separator: removes each key of `keys` whatever its
/// last use, and no other, saying for each that it was pruned, kept as in use, or could not be pruned. Gives
/// [`FAILED_STATUS`] when a key could not be pruned, else 0; else the line saying why the sweep ended. A root that is
/// not there holds nothing of any key, unless its drive is not there either, which says nothing of them. A root
/// [`gate_win32::check_work_root`] refuses, with its walk above ending at the walk stop `seams` give, has its keys'
/// folders and records removed, but no git runs in it, so each key keeps its refs and could not be pruned. Under a
/// standing drain barrier, read before the sweep and before each key, each key left is kept.
fn named(root: &str, keys: &[String], seams: &BoxSeams, now: SystemTime, console: &mut Console) -> Result<u32, String> {
    if !drive_is_there(root) {
        for key in keys {
            stamping::err(console, &could_not_prune(key, "the root's drive is not there"));
        }
        return Ok(u32::from(FAILED_STATUS));
    }
    if drained(root, seams)? {
        kept_drained(keys, console);
        return Ok(0);
    }
    let Some(mut sweep) = Sweep::opened(root, now, true, console)? else {
        for key in keys {
            stamping::out(console, &gone_line(key));
        }
        return Ok(0);
    };
    // Held, when it passes, until every key is done, as the preflight holds the root it checked.
    let checked = gate_win32::check_work_root(Path::new(&root_name(root)), seams.root_walk_stop.as_deref());
    sweep.refused = checked.as_ref().err().map(ToString::to_string);
    let failed = sweep.named_keys(keys, seams, console)?;
    drop(checked);
    Ok(if failed { u32::from(FAILED_STATUS) } else { 0 })
}

/// The line saying the keyed prune removed what was left of the key `key`.
fn gone_line(key: &str) -> String {
    format!("gate: remote pruned {} (worktree gone)", escaped(key))
}

/// The name the system is given for the root `root`, given without a trailing separator: a drive's own root, such as
/// `C:`, with its separator, since `C:` alone names the current folder on that drive; any other root as it is.
fn root_name(root: &str) -> String {
    if root.ends_with(':') { format!(r"{root}\") } else { root.to_owned() }
}

/// The root `root` opened as itself, held so no one can rename or remove it; `None` when it is not there. Else the line
/// saying it cannot be opened, or is a link, a junction or not a folder.
fn opened_root(root: &str) -> Result<Option<File>, String> {
    let named = root_name(root);
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&named);
    let path = Path::new(&named);
    match opened {
        Ok(folder) if is_folder_itself(&folder).map_err(|error| cannot_list(path, &error))? => Ok(Some(folder)),
        Ok(_) => Err(not_plain(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(cannot_list(path, &error)),
    }
}

/// The folder `name` of the held root `root`, at `path`, opened as itself (or made, as `make` says) and held so no one
/// can rename or remove it; `None` when it is not there. Else the line saying it cannot be opened, or is a link, a
/// junction or not a folder.
fn folder_beneath(root: &File, name: &str, path: &Path, make: Make) -> Result<Option<File>, String> {
    match gate_win32::open_beneath_as(root, name, FILE_GENERIC_READ, Share::NoDelete, make) {
        Ok(folder) if is_folder_itself(&folder).map_err(|error| cannot_list(path, &error))? => Ok(Some(folder)),
        Ok(_) => Err(not_plain(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(cannot_list(path, &error)),
    }
}

/// Removes each entry an earlier sweep left in the held `pruned` folder at `path`, each opened beneath it as itself and
/// removed through its handle, a line on stderr for each that cannot be removed; else the line saying the folder cannot
/// be listed, which ends the sweep.
fn leftovers_removed(pruned: &File, path: &Path, console: &mut Console) -> Result<(), String> {
    let names = gate_win32::names_beneath(pruned).map_err(|error| cannot_list(path, &error))?;
    for name in names {
        let shown = path.join(&name);
        let Some(name) = name.to_str() else {
            let error = io::Error::new(ErrorKind::InvalidData, "its name is not valid Unicode");
            stamping::err(console, &could_not_remove_pruned(&shown, &error));
            continue;
        };
        if let Err(error) = removed_beneath(pruned, name) {
            stamping::err(console, &could_not_remove_pruned(&shown, &error));
        }
    }
    Ok(())
}

/// Removes the entry `name` of the held folder `folder`, opened beneath it as itself, and a folder of its own with
/// everything in it, through its handle ([`gate_win32::delete_tree_opened`]); nothing when nothing has the name.
fn removed_beneath(folder: &File, name: &str) -> io::Result<()> {
    match gate_win32::open_beneath_as(folder, name, DELETE | FILE_READ_ATTRIBUTES, Share::NoDelete, Make::Open) {
        Ok(entry) => gate_win32::delete_tree_opened(&entry),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// One sweep: the root, its path without a trailing separator, and its work folder and `pruned` folder, with their
/// paths for its lines, each held until the sweep is done so no one can rename or remove it meanwhile; the time it
/// judges by; whether it is the keyed prune, which judges no age and deletes refs; why the root may not have git run
/// in it, when the keyed prune's check of it refused it; and git, for the refs.
struct Sweep {
    root: String,
    root_folder: File,
    work: File,
    work_path: String,
    pruned: File,
    pruned_path: PathBuf,
    now: SystemTime,
    named: bool,
    refused: Option<String>,
    git: Git,
}

/// A key's lock as a sweep holds it, and its last write from before the sweep took it, `None` for one the sweep made.
struct KeyLock {
    file: File,
    written: Option<SystemTime>,
}

/// What became of one key of a sweep.
enum Pruned {
    /// Its lock taken, its work folder and records removed, and in the keyed prune its refs: the folder renamed aside,
    /// idle as long as given in the age prune, or there was none.
    Removed(Option<Duration>),
    /// A run or another sweep holds its lock.
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
    /// A folder the age prune finds not idle, left as it is.
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

/// A work folder renamed aside: its handle, where it now is, how long it had been idle, which the keyed prune does not
/// judge, and the line saying a record that could not be removed once it was aside.
struct Aside {
    folder: File,
    path: PathBuf,
    idle: Option<Duration>,
    left: Option<String>,
}

impl Sweep {
    /// The sweep of the root `root`, given without a trailing separator, at `now`, keyed as `named` says: the root,
    /// `pruned` (made when missing, and what an earlier sweep left in it removed) and `work` held; the keyed prune
    /// makes a missing `work`, so each key's lock is taken there as a run takes it. `None` when the root, or for the age
    /// prune `work`, is not there. Else the line saying why the sweep ended.
    fn opened(root: &str, now: SystemTime, named: bool, console: &mut Console) -> Result<Option<Sweep>, String> {
        let Some(root_folder) = opened_root(root)? else {
            return Ok(None);
        };
        let pruned_path = PathBuf::from(format!(r"{root}\pruned"));
        let pruned = folder_beneath(&root_folder, "pruned", &pruned_path, Make::Folder)?
            .ok_or_else(|| cannot_list(&pruned_path, &io::Error::from(ErrorKind::NotFound)))?;
        leftovers_removed(&pruned, &pruned_path, console)?;
        let work_path = format!(r"{root}\work");
        let make = if named { Make::Folder } else { Make::Open };
        let Some(work) = folder_beneath(&root_folder, "work", Path::new(&work_path), make)? else {
            return Ok(None);
        };
        Ok(Some(Sweep {
            root: root.to_owned(),
            root_folder,
            work,
            work_path,
            pruned,
            pruned_path,
            now,
            named,
            refused: None,
            git: Git::find(),
        }))
    }

    /// The age prune of the key `key` ([`Sweep::pruned_key`]), saying the folder removed and how long it was idle:
    /// `true` once it is removed.
    fn aged_key(&self, key: &str, console: &mut Console) -> bool {
        let Pruned::Removed(Some(idle)) = self.pruned_key(key, console) else {
            return false;
        };
        let hours = idle.as_secs() / HOUR_SECONDS;
        stamping::out(console, &format!("gate: remote pruned {}: idle {hours} h", escaped(key)));
        true
    }

    /// The keyed prune of each key of `keys` in turn ([`Sweep::pruned_key`]), each said as it ends, the drain barrier
    /// read before each at the time `seams` give: once it stands, each key left is kept. Whether a key could not be
    /// pruned; else the line saying the barrier could not be read.
    fn named_keys(&self, keys: &[String], seams: &BoxSeams, console: &mut Console) -> Result<bool, String> {
        let mut failed = false;
        for (at, key) in keys.iter().enumerate() {
            if drained(&self.root, seams)? {
                kept_drained(keys.get(at..).unwrap_or_default(), console);
                break;
            }
            match self.pruned_key(key, console) {
                Pruned::Removed(_) => stamping::out(console, &gone_line(key)),
                Pruned::Held => stamping::out(console, &format!("gate: remote kept {}: in use", escaped(key))),
                // The keyed prune judges no age, so it keeps a key only for a held lock or a failure.
                Pruned::Kept => {}
                Pruned::Failed => failed = true,
            }
        }
        Ok(failed)
    }

    /// Prunes the key `key` when no run holds its lock and, in the age prune, its work folder is idle: its folder
    /// renamed aside, its records removed, in the keyed prune its refs too, and the folder removed once the lock is let
    /// go. A failure is a line on stderr and keeps what is left.
    fn pruned_key(&self, key: &str, console: &mut Console) -> Pruned {
        let lock = match self.lock(key) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Pruned::Held,
            Err(line) => {
                stamping::err(console, &line);
                return Pruned::Failed;
            }
        };
        let aside = match self.set_aside(key, lock.written) {
            Ok(Found::Aside(aside)) => Some(aside),
            Ok(Found::Missing) if self.named => None,
            Ok(Found::Missing | Found::Fresh) => {
                self.let_go(key, lock, console);
                return Pruned::Kept;
            }
            Err(line) => {
                self.let_go(key, lock, console);
                stamping::err(console, &line);
                return Pruned::Failed;
            }
        };
        self.removed(key, lock, aside, console)
    }

    /// With the key's lock `lock` held and its work folder `aside` renamed aside, or none there: in the keyed prune its
    /// refs deleted, then the lock let go and the folder removed. A folder that cannot be removed is a line on stderr,
    /// which the next sweep takes. A record left once the folder was aside is a line on stderr, and fails the key in the
    /// keyed prune; refs that cannot be deleted are the key's line saying why.
    fn removed(&self, key: &str, lock: KeyLock, aside: Option<Aside>, console: &mut Console) -> Pruned {
        // The age prune leaves every ref: a worktree still alive may have pushed one just before the lock was taken, and
        // its run's fetch would fail where a rebuilt folder alone does no harm. A gone worktree pushes nothing.
        // Under the lock and with the folder out of `work`, so a run that comes for the key waits, then finds neither.
        let refs = if self.named { self.refs_removed(key, console) } else { Ok(()) };
        self.let_go(key, lock, console);
        let (idle, left) = match aside {
            Some(aside) => {
                if let Err(error) = gate_win32::delete_tree_opened(&aside.folder) {
                    stamping::err(console, &could_not_remove_pruned(&aside.path, &error));
                }
                (aside.idle, aside.left)
            }
            None => (None, None),
        };
        if let Some(line) = left {
            stamping::err(console, &line);
            if self.named {
                return Pruned::Failed;
            }
        }
        match refs {
            Ok(()) => Pruned::Removed(idle),
            Err(why) => {
                stamping::err(console, &could_not_prune(key, &format!("its refs could not be removed: {why}")));
                Pruned::Failed
            }
        }
    }

    /// Lets the key's lock `lock` go; in the keyed prune, one the sweep made is deleted first, through its handle, so a
    /// key the builder never had leaves nothing behind. A lock that cannot be deleted is a line on stderr.
    fn let_go(&self, key: &str, lock: KeyLock, console: &mut Console) {
        if self.named
            && lock.written.is_none()
            && let Err(error) = gate_win32::delete_opened(&lock.file)
        {
            stamping::err(
                console,
                &format!(
                    "gate: remote could not remove the work folder lock {}: {}",
                    escaped(&format!(r"{}\{key}.lock", self.work_path)),
                    escaped(&error.to_string())
                ),
            );
        }
        drop(lock);
    }

    /// Deletes the key's refs of [`REF_KINDS`] in each bare repository of the root ([`held_bare`]), every one tried
    /// whatever one before it did; else why the first could not be, or why the root's check refused git in it.
    fn refs_removed(&self, key: &str, console: &mut Console) -> Result<(), String> {
        if let Some(why) = &self.refused {
            return Err(why.clone());
        }
        let input = REF_KINDS.map(|kind| format!("delete refs/{kind}/{key}\n")).concat();
        let names = gate_win32::names_beneath(&self.root_folder).map_err(|error| error.to_string())?;
        let mut first = None;
        for name in names {
            let Some(name) = name.to_str().filter(|name| is_bare_name(name)) else {
                continue;
            };
            let held = match held_bare(&self.root_folder, name) {
                Ok(Some(held)) => held,
                Ok(None) => continue,
                Err(error) => {
                    first.get_or_insert(error.to_string());
                    continue;
                }
            };
            if let Err(why) = self.refs_deleted(name, &input, console) {
                first.get_or_insert(why);
            }
            drop(held);
        }
        first.map_or(Ok(()), Err)
    }

    /// Runs `git update-ref --no-deref --stdin` in the bare repository `<root>\<name>`, fed `input`, as a write of the shared bare
    /// repository a gone client does not cut short ([`GitClass::ShortBare`]); else git's first line on stderr, or why
    /// it did not run to its end.
    fn refs_deleted(&self, name: &str, input: &str, console: &mut Console) -> Result<(), String> {
        let git_dir = format!(r"--git-dir={}\{name}", self.root);
        let streams = Streams {
            stdout: Pipe::Discard,
            stderr: Pipe::Read,
            feed: Some(input.as_bytes()),
        };
        let step = GitStep {
            name: REFS_STEP,
            class: GitClass::ShortBare,
        };
        let (lock, gone) = (OutputLock::default(), ClientGone::default());
        match self.git.run(
            &[&git_dir, "update-ref", "--no-deref", "--stdin"],
            step,
            &streams,
            Shared::alone(&lock, &gone),
            console,
        ) {
            Ok(GitOutput { code: 0, .. }) => Ok(()),
            Ok(ran) => {
                let said = String::from_utf8_lossy(&ran.stderr).lines().next().unwrap_or_default().to_owned();
                Err(if said.is_empty() {
                    format!("git exited with status {}", ran.code)
                } else {
                    said
                })
            }
            Err(GitFailure::NotStarted(why) | GitFailure::Failed(why)) => Err(why),
            Err(GitFailure::ClientGone) => Err("the client is gone".to_owned()),
        }
    }

    /// The key's lock opened beneath the work folder as itself, made when missing, and held as [`LOCK_ACCESS`] says,
    /// with its last write from before this sweep took it (`None` for a lock the sweep made); `None` when a run or
    /// another sweep holds it, or made it meanwhile. Else the line saying it cannot be opened or is not a plain file:
    /// in the keyed prune, the key's own line.
    fn lock(&self, key: &str) -> Result<Option<KeyLock>, String> {
        let name = format!("{key}.lock");
        let cannot = |error: io::Error| {
            if self.named {
                return could_not_prune(key, &error.to_string());
            }
            format!(
                "gate: remote could not open the work folder lock {}: {}",
                escaped(&format!(r"{}\{name}", self.work_path)),
                escaped(&error.to_string())
            )
        };
        let open = |make| gate_win32::open_beneath_as(&self.work, &name, LOCK_ACCESS, Share::NoDelete, make);
        let opened = match open(Make::Open) {
            Err(error) if error.kind() == ErrorKind::NotFound => open(Make::NewFile).map(|lock| (lock, false)),
            opened => opened.map(|lock| (lock, true)),
        };
        match opened {
            Ok((lock, existed)) => {
                gate_win32::check_plain_file(&lock).map_err(cannot)?;
                let written = if existed {
                    Some(lock.metadata().and_then(|metadata| metadata.modified()).map_err(cannot)?)
                } else {
                    None
                };
                Ok(Some(KeyLock { file: lock, written }))
            }
            Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(None),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(cannot(error)),
        }
    }

    /// With the key's lock held, last written at `lock_written`: its layout marked changing, its other records removed
    /// and its work folder renamed aside when the folder is idle, or whatever its last use in the keyed prune, then its
    /// layout and index records removed; the records alone removed when it has no folder; a folder not idle left. Else
    /// the line saying why the key is kept. A record that cannot be removed once the folder is aside is the aside
    /// folder's line saying so ([`Aside::left`]).
    fn set_aside(&self, key: &str, lock_written: Option<SystemTime>) -> Result<Found, String> {
        let keep = |why: &dyn ToString| could_not_prune(key, &why.to_string());
        let looked = match gate_win32::open_beneath_as(&self.work, key, LOOK_ACCESS, Share::All, Make::Open) {
            Ok(looked) => looked,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.records_removed(key, &RECORDS_BEFORE)?;
                return self.records_removed(key, &RECORDS_AFTER).map(|()| Found::Missing);
            }
            Err(error) => return Err(keep(&error)),
        };
        if !is_folder_itself(&looked).map_err(|error| keep(&error))? {
            return Err(keep(&"its work folder is not a plain folder"));
        }
        let idle = if self.named {
            None
        } else {
            let last_use = self.last_use(key, &looked, lock_written).map_err(|error| keep(&error))?;
            let Some(idle) = idle_for(last_use, self.now) else {
                return Ok(Found::Fresh);
            };
            Some(idle)
        };
        let folder =
            gate_win32::open_beneath_as(&self.work, key, FILE_GENERIC_READ | DELETE, Share::NoDelete, Make::Open).map_err(|error| keep(&error))?;
        if !gate_win32::same_file(&looked, &folder).map_err(|error| keep(&error))? {
            return Err(keep(&"its work folder changed during the sweep"));
        }
        drop(looked);
        written_beneath(&self.work, &format!("{key}.layout"), LAYOUT_CHANGING).map_err(|why| keep(&why))?;
        self.records_removed(key, &RECORDS_BEFORE)?;
        // The nonce only keeps two names apart within a sweep, so a clock before 1970 may give 0.
        let nanos = self.now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let name = format!("{key}-{}-{nanos}", std::process::id());
        gate_win32::rename_beneath_no_replace(&folder, &self.pruned, &name).map_err(|error| keep(&error))?;
        Ok(Found::Aside(Aside {
            folder,
            path: self.pruned_path.join(name),
            idle,
            left: self.records_removed(key, &RECORDS_AFTER).err(),
        }))
    }

    /// The newest last write among the held work folder `folder`, each tree's `.git\FETCH_HEAD` in it, the key's records
    /// beside it and its lock, last written at `lock_written`; each read through a handle opened as itself, never
    /// through a link, and one that is not there skipped.
    fn last_use(&self, key: &str, folder: &File, lock_written: Option<SystemTime>) -> io::Result<SystemTime> {
        let mut newest = folder.metadata()?.modified()?;
        let mut stamps = vec![lock_written, fetched(folder)?];
        for name in gate_win32::names_beneath(folder)? {
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(tree) = opened_folder(folder, name)? {
                stamps.push(fetched(&tree)?);
            }
        }
        for extension in KEY_FILES.iter().filter(|extension| **extension != "lock") {
            stamps.push(written(&self.work, &format!("{key}.{extension}"))?);
        }
        for stamp in stamps.into_iter().flatten() {
            newest = newest.max(stamp);
        }
        Ok(newest)
    }

    /// Removes the key's records `extensions`, in order, each opened beneath the work folder as itself and removed
    /// through its handle; else the line saying the first that could not be removed.
    fn records_removed(&self, key: &str, extensions: &[&str]) -> Result<(), String> {
        for extension in extensions {
            removed_beneath(&self.work, &format!("{key}.{extension}")).map_err(|error| could_not_prune(key, &error.to_string()))?;
        }
        Ok(())
    }
}

/// The keys in the held work folder `work` and how many of their entries are plain folders: each name that is a key,
/// and each `<key>.<extension>` of [`KEY_FILES`]; other names are left out. Else why it cannot be listed.
fn listed(work: &File) -> io::Result<Listed> {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut folders = 0;
    for name in gate_win32::names_beneath(work)? {
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_key(name) {
            // An entry that cannot be opened is no plain folder seen; the key's own sweep says why.
            if opened_folder(work, name).is_ok_and(|folder| folder.is_some()) {
                folders += 1;
            }
            keys.insert(name.to_ascii_lowercase(), name.to_owned());
        } else if let Some(stem) = key_file_stem(name) {
            keys.entry(stem.to_ascii_lowercase()).or_insert_with(|| stem.to_owned());
        }
    }
    Ok(Listed {
        keys: keys.into_values().collect(),
        folders,
    })
}

/// The key of `name` when it is `<key>.<extension>` for one of [`KEY_FILES`], the extension in any case.
fn key_file_stem(name: &str) -> Option<&str> {
    let (stem, extension) = name.rsplit_once('.')?;
    let known = KEY_FILES.iter().any(|known| known.eq_ignore_ascii_case(extension));
    (known && is_key(stem)).then_some(stem)
}

/// The entry `name` of the held root `root` opened as itself for reading and listing, sharing no deletion, so while it
/// is held no one can rename it, and so swap a link in where it was, nor remove it; when it is a whole bare repository,
/// a folder of its own holding `HEAD`, `objects` and `refs`, each looked up beneath its handle. `None` when it is not
/// there, is a link, a junction or a file, or is not a whole bare repository.
fn held_bare(root: &File, name: &str) -> io::Result<Option<File>> {
    let bare = match gate_win32::open_beneath_as(root, name, FILE_GENERIC_READ, Share::NoDelete, Make::Open) {
        Ok(bare) => bare,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !is_folder_itself(&bare)? {
        return Ok(None);
    }
    for part in ["HEAD", "objects", "refs"] {
        if opened_beneath(&bare, part)?.is_none() {
            return Ok(None);
        }
    }
    Ok(Some(bare))
}

/// Whether the drive of the root `root`, an absolute path on a drive, is there: its own root can be read. A drive that
/// cannot be read for any reason counts as not there, so a key on it is never taken for gone.
fn drive_is_there(root: &str) -> bool {
    root.get(..2).is_some_and(|drive| std::fs::metadata(format!(r"{drive}\")).is_ok())
}

/// Whether `name` names a bare repository of the root: `<name>.git`, the extension in any case.
fn is_bare_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, extension)| !stem.is_empty() && extension.eq_ignore_ascii_case("git"))
}

/// The entry `name` of the open folder `folder`, opened as itself for its attributes alone, so no other open's sharing
/// refuses it; `None` when nothing has the name.
fn opened_beneath(folder: &File, name: &str) -> io::Result<Option<File>> {
    match gate_win32::open_beneath_as(folder, name, FILE_READ_ATTRIBUTES, Share::All, Make::Open) {
        Ok(entry) => Ok(Some(entry)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The entry `name` of the open folder `folder` when it is a folder of its own, opened as [`opened_beneath`] opens it;
/// `None` when it is not there, or is a link, a junction or a file.
fn opened_folder(folder: &File, name: &str) -> io::Result<Option<File>> {
    match opened_beneath(folder, name)? {
        Some(entry) if is_folder_itself(&entry)? => Ok(Some(entry)),
        _ => Ok(None),
    }
}

/// The last write of the tree `tree`'s `.git\FETCH_HEAD`, read only through a `.git` that is a folder of its own;
/// `None` when either is not there or `.git` is a link.
fn fetched(tree: &File) -> io::Result<Option<SystemTime>> {
    match opened_folder(tree, ".git")? {
        Some(git) => written(&git, "FETCH_HEAD"),
        None => Ok(None),
    }
}

/// The last write of the entry `name` of the open folder `folder`, read through its handle as itself; `None` when
/// nothing has the name.
fn written(folder: &File, name: &str) -> io::Result<Option<SystemTime>> {
    opened_beneath(folder, name)?
        .map(|entry| entry.metadata().and_then(|metadata| metadata.modified()))
        .transpose()
}

/// How long a folder last used at `last_use` has been idle at `now`, when that is over [`IDLE_AGE`]; `None` while it
/// is not, a last use after `now` included.
fn idle_for(last_use: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(last_use).ok().filter(|idle| *idle > IDLE_AGE)
}

/// The line saying the folder `path` is a link, a junction or not a folder, which ends the sweep.
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

/// The line saying `git gc --auto` failed in the bare repository `name`, or could not run in the root `name`, for `why`.
fn gc_failed(name: &str, why: &str) -> String {
    format!("gate: remote prune gc failed: {}: {}", escaped(name), escaped(why))
}

/// The line saying the folder `path`, renamed aside, could not be removed, for `error`.
fn could_not_remove_pruned(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote could not remove the pruned folder {}: {}",
        escaped(&path.display().to_string()),
        escaped(&error.to_string())
    )
}

/// The line saying the folder `path` cannot be opened or listed, for `error`.
fn cannot_list(path: &Path, error: &io::Error) -> String {
    format!(
        "gate: remote cannot list {}: {}",
        escaped(&path.display().to_string()),
        escaped(&error.to_string())
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use gate_args::BoxSeams;

    use super::{Gc, IDLE_AGE, held_bare, idle_for, opened_root, prune, root_name};
    use crate::boxside::preflight::Git;

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make or remove its root must fail loudly")]
    fn the_gc_reads_the_barrier_before_each_bare_and_stops_under_a_standing_one() {
        let root = crate::test_support::temp_dir().join(format!("gate-prune-gc-drained-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("an earlier root can be removed");
        }
        let bare = root.join("a.git");
        fs::create_dir_all(bare.join("objects")).expect("the objects folder can be made");
        fs::create_dir_all(bare.join("refs")).expect("the refs folder can be made");
        // No repository git can read: a gc that started would fail, and say so.
        fs::write(bare.join("HEAD"), "not a ref\n").expect("HEAD can be written");
        fs::create_dir_all(root.join("drain")).expect("the drain folder can be made");
        fs::write(root.join(r"drain\barrier"), r#"{"Raised":1000,"Lapses":2800,"Generation":1}"#).expect("the barrier can be written");
        let text = root.to_str().expect("the root is Unicode").to_owned();
        let root_folder = opened_root(&text).expect("the root opens").expect("the root is there");
        let (git, walk_stop) = (Git::find(), crate::test_support::temp_dir().to_path_buf());
        let at = |now| BoxSeams {
            now: Some(now),
            root_walk_stop: Some(walk_stop.clone()),
            ..BoxSeams::default()
        };
        let (standing, lapsed) = (at(2000), at(2800));
        let gc = |seams| {
            Gc {
                root: &text,
                root_folder: &root_folder,
                git: &git,
                seams,
            }
            .bares_collected(&mut Console::new())
        };

        let under_barrier = gc(&standing);
        let after_lapse = gc(&lapsed);

        drop(root_folder);
        fs::remove_dir_all(&root).expect("the root can be removed");
        assert_eq!(under_barrier, Ok(true), "a standing barrier stops the gc before the bare");
        assert_eq!(after_lapse, Ok(false), "a lapsed one lets it run");
    }

    #[test]
    fn a_drive_s_own_root_is_named_with_its_separator_and_any_other_root_as_given() {
        assert_eq!(root_name("C:"), r"C:\", "`C:` alone is the current folder on C, not its root");
        assert_eq!(root_name(r"D:\gate-remote"), r"D:\gate-remote");
    }
    use crate::answer::USAGE_STATUS;
    use crate::console::Console;

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make, hold or remove its folders must fail loudly")]
    fn a_held_bare_repository_cannot_be_renamed_and_a_stray_one_is_not_held() {
        let root = std::env::temp_dir().join(format!("gate-prune-held-bare-{}", std::process::id()));
        let bare = root.join("a.git");
        fs::create_dir_all(bare.join("objects")).expect("the objects folder can be made");
        fs::create_dir_all(bare.join("refs")).expect("the refs folder can be made");
        fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD can be written");
        fs::create_dir_all(root.join("x.git")).expect("a stray folder can be made");
        let root_folder = opened_root(root.to_str().expect("the root is Unicode"))
            .expect("the root opens")
            .expect("the root is there");

        let held = held_bare(&root_folder, "a.git").expect("the bare opens").expect("a whole bare is held");
        let renamed = fs::rename(&bare, root.join("b.git"));
        let stray = held_bare(&root_folder, "x.git").expect("the stray folder opens");
        drop((held, root_folder));

        let left = fs::remove_dir_all(&root);
        assert!(renamed.is_err(), "a held bare repository cannot be renamed");
        assert!(stray.is_none(), "a folder that is no whole bare repository is not held");
        left.expect("the test's folders can be removed");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make or remove its root must fail loudly")]
    fn a_word_that_is_not_a_key_is_refused_before_the_root_is_opened() {
        let root = std::env::temp_dir().join(format!("gate-prune-not-a-key-{}", std::process::id()));
        fs::create_dir_all(&root).expect("the root can be made");
        let keys = ["k".to_owned(), r"..\x".to_owned()];

        let status = prune(&root, &keys, &BoxSeams::default(), SystemTime::now(), &mut Console::new());

        let made = root.join("pruned").exists() || root.join("work").exists();
        fs::remove_dir(&root).expect("the root is left empty and can be removed");
        assert_eq!(status, u32::from(USAGE_STATUS));
        assert!(!made, "nothing is opened or made beneath the root");
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

        assert_eq!(idle_for(now + Duration::from_hours(48), now), None);
    }
}

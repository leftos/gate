//! `gate box prune --root <folder>`: removes the builder's work folders no run has used for over a day, which a
//! scheduled task on the builder runs every few hours. A key's work folder `<root>\work\<key>` goes only with its lock
//! `<root>\work\<key>.lock` held, so no run is using it and none can start in it meanwhile (one that tries is refused
//! busy, status 75, and its PC runs the command itself). Its last use is the newest last write of the folder, each
//! tree's `.git\FETCH_HEAD` (every run fetches into each tree it checks out) and the key's records.
//! An idle folder's layout record is marked changing and its other records removed first, then the folder is renamed
//! into `<root>\pruned` under the lock, so no run ever sees it half removed, and its layout and index records go once it
//! is there; a rename that fails leaves the layout marked changing, so the next run clears the folder whole. The folder
//! is removed there after the lock is let go; what a removal leaves there the next sweep takes.
//! Nothing after the root is opened is looked up by path: the root, `work` and `pruned` are opened as themselves and
//! held for the whole sweep, and every name in them is opened beneath its folder's handle as itself, never followed. A
//! link where the root, `work`, `pruned` or a work folder goes is refused, and a link inside a folder is removed as
//! itself.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_router::escaped;
use gate_win32::{Make, Share};
use gate_wire::{is_drive_path, is_key};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES};

use crate::answer::{FAILED_STATUS, USAGE_STATUS};
use crate::boxside::preflight::{is_folder_itself, written_beneath};
use crate::boxside::run::{FILE_FLAG_OPEN_REPARSE_POINT, LAYOUT_CHANGING, LOCK_VIOLATION, SHARING_VIOLATION};
use crate::console::Console;

/// How long a work folder goes unused before a sweep removes it.
const IDLE_AGE: Duration = Duration::from_hours(24);
/// The extensions of the files beside a work folder `<key>` named `<key>.<extension>`: its lock and its records.
const KEY_FILES: [&str; 9] = ["lock", "warm", "layout", "inputs", "sent-build", "tools", "packages", "index", "command"];
/// The records a sweep removes before it renames the work folder aside, in the order the box clears them; `command`
/// is the interrupted-command record, cleared like the others.
const RECORDS_BEFORE: [&str; 6] = ["warm", "packages", "tools", "inputs", "sent-build", "command"];
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

/// Removes the work folders under `root` no run has used for over [`IDLE_AGE`] before `now`, after what an earlier
/// sweep left in `<root>\pruned`, and the records of a key whose folder is gone; says each folder removed and how many,
/// with a line on stderr for each key kept for a failure. The exit status: 0 whenever the work folders could be listed,
/// [`FAILED_STATUS`] when the root, the work folders or the `pruned` folder could not be opened or listed, or one of
/// them is not a plain folder, and [`USAGE_STATUS`] for a root that is not an absolute path on a drive. A root or work
/// folder that is not there holds nothing to remove.
pub(crate) fn prune(root: &Path, now: SystemTime, console: &mut Console) -> u32 {
    let Some(text) = root.to_str().filter(|text| is_drive_path(text)) else {
        console.err(&format!(
            "gate: box prune --root is not an absolute path on a drive: {:?}",
            root.to_string_lossy()
        ));
        return u32::from(USAGE_STATUS);
    };
    match swept(text.trim_end_matches(['\\', '/']), now, console) {
        Ok((removed, folders)) => {
            console.out(&format!("gate: remote prune removed {removed} of {folders} work folders"));
            0
        }
        Err(line) => {
            console.err(&line);
            u32::from(FAILED_STATUS)
        }
    }
}

/// Sweeps the root `root`, given without a trailing separator: how many work folders it removed, and how many plain
/// work folders it saw. Else the line saying why the sweep ended.
fn swept(root: &str, now: SystemTime, console: &mut Console) -> Result<(usize, usize), String> {
    let Some(root_folder) = opened_root(root)? else {
        return Ok((0, 0));
    };
    let pruned_path = PathBuf::from(format!(r"{root}\pruned"));
    let pruned = folder_beneath(&root_folder, "pruned", &pruned_path, Make::Folder)?
        .ok_or_else(|| cannot_list(&pruned_path, &io::Error::from(ErrorKind::NotFound)))?;
    leftovers_removed(&pruned, &pruned_path, console)?;
    let work_path = format!(r"{root}\work");
    let Some(work) = folder_beneath(&root_folder, "work", Path::new(&work_path), Make::Open)? else {
        return Ok((0, 0));
    };
    let listed = listed(&work).map_err(|error| cannot_list(Path::new(&work_path), &error))?;
    let sweep = Sweep {
        work,
        work_path,
        pruned,
        pruned_path,
        now,
    };
    let removed = listed.keys.iter().filter(|key| sweep.pruned_key(key, console)).count();
    // The root is held until the sweep is done, so no one can rename or remove it meanwhile; `work` and `pruned` are
    // pinned the same way by their own handles.
    drop(root_folder);
    Ok((removed, listed.folders))
}

/// The root `root` opened as itself, held so no one can rename or remove it; `None` when it is not there. Else the line
/// saying it cannot be opened, or is a link, a junction or not a folder.
fn opened_root(root: &str) -> Result<Option<File>, String> {
    // A drive's own root is named with its separator, which the trimmed text lacks.
    let named = if root.ends_with(':') { format!(r"{root}\") } else { root.to_owned() };
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
            console.err(&could_not_remove_pruned(&shown, &error));
            continue;
        };
        if let Err(error) = removed_beneath(pruned, name) {
            console.err(&could_not_remove_pruned(&shown, &error));
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

/// One sweep: the held work folder and `pruned` folder, with their paths for its lines, and the time it judges by.
struct Sweep {
    work: File,
    work_path: String,
    pruned: File,
    pruned_path: PathBuf,
    now: SystemTime,
}

/// What the work folder holds: each key, named by a folder or by a file beside one, sorted without regard to case; and
/// how many of the keys' entries are plain folders.
struct Listed {
    keys: Vec<String>,
    folders: usize,
}

/// A work folder renamed aside: its handle, where it now is, and how long it had been idle.
struct Aside {
    folder: File,
    path: PathBuf,
    idle: Duration,
}

impl Sweep {
    /// Prunes the key `key` when no run holds its lock and its work folder is idle: `true` once the folder is renamed
    /// aside. A failure is a line on stderr and keeps what is left.
    fn pruned_key(&self, key: &str, console: &mut Console) -> bool {
        let (lock, lock_written) = match self.lock(key) {
            Ok(Some(lock)) => lock,
            Ok(None) => return false,
            Err(line) => {
                console.err(&line);
                return false;
            }
        };
        let aside = match self.set_aside(key, lock_written, console) {
            Ok(Some(aside)) => aside,
            Ok(None) => return false,
            Err(line) => {
                console.err(&line);
                return false;
            }
        };
        drop(lock);
        if let Err(error) = gate_win32::delete_tree_opened(&aside.folder) {
            console.err(&could_not_remove_pruned(&aside.path, &error));
        }
        let hours = aside.idle.as_secs() / HOUR_SECONDS;
        console.out(&format!("gate: remote pruned {}: idle {hours} h", escaped(key)));
        true
    }

    /// The key's lock opened beneath the work folder as itself, made when missing, and held as [`LOCK_ACCESS`] says,
    /// with its last write from before this sweep took it (`None` for a lock the sweep made); `None` when a run or
    /// another sweep holds it, or made it meanwhile. Else the line saying it cannot be opened or is not a plain file.
    fn lock(&self, key: &str) -> Result<Option<(File, Option<SystemTime>)>, String> {
        let name = format!("{key}.lock");
        let cannot = |error: io::Error| {
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
                Ok(Some((lock, written)))
            }
            Err(error) if matches!(error.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) => Ok(None),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(cannot(error)),
        }
    }

    /// With the key's lock held, last written at `lock_written`: its layout marked changing, its other records removed
    /// and its work folder renamed aside when the folder is idle, then its layout and index records removed; the
    /// records alone removed, and `None`, when it has no folder; `None` when it is in use. Else the line saying why the
    /// key is kept. A record that cannot be removed once the folder is aside is a line on stderr.
    fn set_aside(&self, key: &str, lock_written: Option<SystemTime>, console: &mut Console) -> Result<Option<Aside>, String> {
        let keep = |why: &dyn ToString| could_not_prune(key, &why.to_string());
        let looked = match gate_win32::open_beneath_as(&self.work, key, LOOK_ACCESS, Share::All, Make::Open) {
            Ok(looked) => looked,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.records_removed(key, &RECORDS_BEFORE)?;
                return self.records_removed(key, &RECORDS_AFTER).map(|()| None);
            }
            Err(error) => return Err(keep(&error)),
        };
        if !is_folder_itself(&looked).map_err(|error| keep(&error))? {
            return Err(keep(&"its work folder is not a plain folder"));
        }
        let last_use = self.last_use(key, &looked, lock_written).map_err(|error| keep(&error))?;
        let Some(idle) = idle_for(last_use, self.now) else {
            return Ok(None);
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
        if let Err(line) = self.records_removed(key, &RECORDS_AFTER) {
            console.err(&line);
        }
        Ok(Some(Aside {
            folder,
            path: self.pruned_path.join(name),
            idle,
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
    use std::time::{Duration, SystemTime};

    use super::{IDLE_AGE, idle_for};

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

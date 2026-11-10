//! `gate-box box lfs`: a routed run's Git LFS objects, sent from the PC before the run into the store beside the repo's
//! bare repository, `{Bare}/lfs/objects/{oid[0..2]}/{oid[2..4]}/{oid}`, answered as the Windows box answers them, line
//! for line. Its request is the `inputs` request's ([`Asked::read`]) with an optional `LfsRepo` naming one of the
//! pair's siblings, whose store, beside the bare repository the box derives for it, the call is for. `diff` reads the
//! PC's manifest line of `<oid> <size>` entries and says which objects the store lacks; `put` reads the manifest and
//! then an archive of those objects, each member named by its oid, in the outputs archive's format, to the end of
//! stdin, and installs them. This builder has no data channel: a request asking for one is refused `channel`.
//!
//! The store holds an object when its name is a plain file, opened beneath its folders' descriptors with no link
//! followed, the box user's own, of mode 0600 and of the manifest's size. Every folder of the store (`lfs`, `objects`
//! and the two below it) is made 0700, or adopted only when it is the box user's own and open to no other account;
//! one that is not is refused, never moved aside. Both steps take the store's lock, `{Bare}/lfs.lock`, a plain file
//! made 0600 beneath the bare repository, shared (`flock(LOCK_SH)`, waiting), for their whole body, so a prune that
//! takes it alone waits for them all. The diff holds the work folder's lock for its whole body; the put readies the
//! store and unpacks its archive into a stage of its own, `{root}/lfs-stage/{key}-{nonce}`, before it takes the work
//! folder's lock, refuses the whole archive when a member is not an object the manifest names, once, hashing to its
//! name and of its size, and installs each object under the work folder's lock by a hard link that never replaces:
//! an object already in the store, another put's among them, is kept and its staged copy removed, and any other file
//! at its name refuses the put. The stage is removed on every way out.
//!
//! The routed command runs as the box's own user and so could write the store; that is accepted, as the builder is
//! its owner's and the command is the routed repo's own, so the Windows box's elevation rule has no twin here.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gate_args::BoxSeams;
use gate_inputs::archive::{Entry, Limits, MEMBER_LIMIT, extract};
use gate_inputs::lfs::{LFS_BUDGET, Wanted, checked, parsed_manifest};
use gate_inputs::manifest::{HeldBy, MANIFEST_LIMIT};
use gate_wire::{LFS_INSTALLED, LFS_NEED, LFS_OID, Payload, Refusal, ordinal_upper, refused_line};

use crate::beneath::{self, Folder};
use crate::console::Console;
use crate::drain;
use crate::files;
use crate::inputs::{Asked, Facts, Stage, Staging, Step, locked, made_stage_in, root_checked};
use crate::lines::{FAILED_STATUS, OPEN_TO_OTHERS, USAGE_STATUS, channel_refusal, lfs_failed_line, lfs_not_the_object, lfs_store_flaw, owner_flaw};
use crate::request::{Unread, bounded_line};
use crate::run::REFUSED_STATUS;

/// The folder below the box's root that holds each put's stage and its lock file, `{root}/lfs-stage/{key}-{nonce}`.
const STAGES: &str = "lfs-stage";
/// The store's lock beside the store, `{Bare}/lfs.lock`.
const STORE_LOCK: &str = "lfs.lock";
/// The permission bits of every object the store holds.
const OBJECT_MODE: u32 = 0o600;

/// What an `lfs` request reads of its payload: the `inputs` request's, and the bare repository whose store it is for,
/// the sibling's `LfsRepo` names ([`Payload::lfs_repo`]), else the main repo's.
struct Request<'a> {
    asked: Asked<'a>,
    store: String,
}

impl<'a> Request<'a> {
    /// The request `payload` holds; else the usage line of the first key that fails.
    fn read(payload: &'a Payload) -> Result<Request<'a>, String> {
        let asked = Asked::read(payload)?;
        let store = payload.lfs_repo(asked.pair.as_ref())?.unwrap_or_else(|| asked.bare.to_owned());
        Ok(Request { asked, store })
    }
}

/// A put's request, its manifest and its readied store, as the install under the lock takes them.
#[derive(Clone, Copy)]
struct Job<'j> {
    request: &'j Request<'j>,
    wanted: &'j [Wanted],
    store: &'j Store,
}

/// Answers `gate-box box lfs`: the request read, a payload refused a usage error (2); one asking for a data channel
/// refused `channel` (75) ([`channel_refusal`]); the work root and every bare repository judged as the `inputs` verb
/// judges them ([`root_checked`]); the call passed through the drain ([`drain::passed`]) or refused `drain` (75); then
/// the manifest read from stdin before any lock is taken, so a client that stalls holds nothing; then the step. A held
/// work folder lock is refused `busy` (75); any other failure is refused `lfs` and fails the step (1).
#[must_use]
pub fn lfs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u8 {
    answered(payload, seams, &mut io::stdin().lock(), console)
}

/// [`lfs`], the manifest and a put's archive read from `input`.
fn answered(payload: &Payload, seams: &BoxSeams, input: &mut impl BufRead, console: &mut Console) -> u8 {
    let request = match Request::read(payload) {
        Ok(request) => request,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    if let Some(status) = channel_refused(payload, console) {
        return status;
    }
    if let Err((line, status)) = root_checked(&request.asked) {
        console.err(&line);
        return status;
    }
    // Held until the step returns, so a drain counts it as going; refused before the manifest is read.
    let _admitted = match drain::passed(&request.asked.root, payload, None, seams, console) {
        Ok(admitted) => admitted,
        Err(unpassed) => return unpassed.said(payload, console),
    };
    let wanted = match read_manifest(input) {
        Ok(wanted) => wanted,
        Err(why) => return refused(&request.asked, &why, console),
    };
    match request.asked.step {
        Step::Diff => diff(&request, &wanted, console),
        Step::Put => put(&request, &wanted, seams, input, console),
    }
}

/// The status of a request asking for a data channel, its lines said ([`channel_refusal`]); `None` for one without.
fn channel_refused(payload: &Payload, console: &mut Console) -> Option<u8> {
    match channel_refusal(payload) {
        Ok(None) => None,
        Ok(Some(lines)) => {
            for line in &lines {
                console.err(line);
            }
            Some(REFUSED_STATUS)
        }
        Err(line) => {
            console.err(&line);
            Some(USAGE_STATUS)
        }
    }
}

/// Says the step of `asked` is refused `lfs` and failed for `why`: the tag line, then the step's failure line; 1.
fn refused(asked: &Asked<'_>, why: &str, console: &mut Console) -> u8 {
    console.err(&refused_line(asked.nonce, Refusal::Lfs));
    console.err(&lfs_failed_line(asked.step.word(), why));
    FAILED_STATUS
}

/// The status of a step whose work folder could not be locked or laid out ([`locked`]), its own lines said: a refusal
/// of its own (busy, or its layout refused) as it is; any other failure then refused `lfs` as the step's failure.
fn unlocked(asked: &Asked<'_>, status: u8, console: &mut Console) -> u8 {
    if status != REFUSED_STATUS {
        refused(asked, "the work folder could not be locked or laid out", console);
    }
    status
}

/// The manifest line read from `input`, up to its first newline or the input's end and never more than
/// [`MANIFEST_LIMIT`] bytes, without its `\n` or `\r\n` ([`parsed_manifest`]); else why it is not one.
fn read_manifest(input: &mut impl BufRead) -> Result<Vec<Wanted>, String> {
    let mut line = bounded_line(&mut *input, MANIFEST_LIMIT as u64).map_err(|unread| match unread {
        Unread::OverLimit => "the LFS manifest reaches 64 MiB without a newline".to_owned(),
        Unread::Failed(error) => format!("the LFS manifest could not be read: {error}"),
    })?;
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let text = String::from_utf8(line).map_err(|_| "the LFS manifest is not UTF-8".to_owned())?;
    parsed_manifest(&text)
}

/// A repo's store, readied and held: its lock taken shared, held until this is dropped, and `lfs/objects` held by its
/// descriptor, with its path.
struct Store {
    _lock: File,
    objects: Folder,
    path: PathBuf,
}

impl Store {
    /// The store of the bare repository `bare`, `<root>/<name>.git`, reached beneath the judged work root `root`: its
    /// lock taken shared ([`store_lock`]), then `lfs` and `lfs/objects` each made or adopted ([`store_folder`]). Else
    /// why not.
    fn opened(root: &str, bare: &str) -> Result<Store, String> {
        let bare_path = Path::new(bare);
        let name = bare_path.file_name().ok_or_else(|| format!("{bare} names no folder"))?;
        let held = Folder::open_root(Path::new(root))
            .and_then(|root| root.open_child(name))
            .map_err(|error| format!("cannot open {bare}: {error}"))?;
        let lock = store_lock(&held, &bare_path.join(STORE_LOCK))?;
        let lfs_path = bare_path.join("lfs");
        let lfs = store_folder(&held, &lfs_path)?;
        let path = lfs_path.join("objects");
        let objects = store_folder(&lfs, &path)?;
        Ok(Store { _lock: lock, objects, path })
    }

    /// The folder `{oid[0..2]}/{oid[2..4]}` the object `oid` lives in, each made or adopted ([`store_folder`]); else
    /// why not.
    fn folder_of(&self, oid: &str) -> Result<Folder, String> {
        let (first, second) = (oid.get(..2).unwrap_or_default(), oid.get(2..4).unwrap_or_default());
        let first_path = self.path.join(first);
        let outer = store_folder(&self.objects, &first_path)?;
        store_folder(&outer, &first_path.join(second))
    }

    /// The oids of the objects of `wanted` the store does not hold ([`holds`]), in their order; else why not.
    fn lacking<'w>(&self, wanted: &'w [Wanted]) -> Result<Vec<&'w str>, String> {
        let mut lacking = Vec::new();
        for object in wanted {
            if !holds(&self.folder_of(&object.oid)?, object)? {
                lacking.push(object.oid.as_str());
            }
        }
        Ok(lacking)
    }
}

/// Why the file or folder `stat` says, at `path`, is not the box user's own and open to no other account
/// ([`owner_flaw`], [`lfs_store_flaw`]); `Ok` when it is.
fn owned_private(stat: &libc::stat, path: &Path) -> Result<(), String> {
    let euid = files::effective_uid();
    owner_flaw(stat.st_uid, u32::from(stat.st_mode), euid, OPEN_TO_OTHERS).map_or(Ok(()), |flaw| {
        Err(lfs_store_flaw(flaw, &path.display().to_string(), &files::user_name(euid)))
    })
}

/// The store folder at `path`, named by its last part in the held folder `parent`: made 0700 ([`Folder::make_child`]),
/// or, when there, opened as itself, a link never followed, and adopted only when it is the box user's own and open to
/// no other account ([`owned_private`]). Else why not.
fn store_folder(parent: &Folder, path: &Path) -> Result<Folder, String> {
    let name = path.file_name().unwrap_or_default();
    let cannot = |error: io::Error| format!("cannot ready {}: {error}", path.display());
    let opened = match parent.make_child(name) {
        Ok(made) => return Ok(made),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => parent.open_child(name).map_err(cannot)?,
        Err(error) => return Err(cannot(error)),
    };
    owned_private(&opened.stat().map_err(cannot)?, path)?;
    Ok(opened)
}

/// The store's lock at `path`, `{bare}/lfs.lock`, beneath the held bare repository `bare`: made new, 0600, else the
/// plain file there opened as itself, a link never followed, and adopted only when it is the box user's own and open
/// to no other account; then locked shared ([`files::flock_shared`]), waiting while a prune holds it alone. Else why not.
fn store_lock(bare: &Folder, path: &Path) -> Result<File, String> {
    let cannot = |error: io::Error| format!("cannot open the LFS store lock {}: {error}", path.display());
    let name = OsStr::new(STORE_LOCK);
    let lock = match bare.create_file(name) {
        Ok(made) => made,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let opened = bare.open_file(name).map_err(cannot)?;
            owned_private(&beneath::file_stat(&opened).map_err(cannot)?, path)?;
            opened
        }
        Err(error) => return Err(cannot(error)),
    };
    files::flock_shared(&lock).map_err(|error| format!("cannot lock the LFS store lock {}: {error}", path.display()))?;
    Ok(lock)
}

/// Whether the held store folder `folder` holds `wanted`: its oid names, looked at as itself beneath the folder with no
/// link followed, a plain file of the box user's own, of mode 0600 exactly and of the manifest's size ([`is_object`]).
/// Anything else at the name, a link or a file the box user cannot read among them, or nothing, is not the object. Else
/// why it cannot be looked at.
fn holds(folder: &Folder, wanted: &Wanted) -> Result<bool, String> {
    match folder.stat_child(OsStr::new(&wanted.oid)) {
        Ok(stat) => Ok(is_object(&stat, wanted.size)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("cannot read the LFS object {}: {error}", wanted.oid)),
    }
}

/// Whether the file `stat` says is an object of `size` bytes as the store keeps one: a plain file, the box user's own,
/// of mode [`OBJECT_MODE`] exactly.
fn is_object(stat: &libc::stat, size: u64) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
        && stat.st_uid == files::effective_uid()
        && u32::from(stat.st_mode) & 0o7777 == OBJECT_MODE
        && u64::try_from(stat.st_size).ok() == Some(size)
}

/// The diff, under the work folder's lock and the store's: `gate-remote-lfs: need <n>`, then `gate-remote-lfs: oid
/// <oid>` on stdout for each object of the manifest `wanted` the store lacks, in the manifest's order; 0. Else the
/// status, its lines said.
fn diff(request: &Request<'_>, wanted: &[Wanted], console: &mut Console) -> u8 {
    let asked = &request.asked;
    let lacking = locked(asked, console, |_, _, _| {
        let store = Store::opened(&asked.root, &request.store)?;
        store.lacking(wanted)
    });
    match lacking {
        Ok(Ok(lacking)) => {
            console.out(&format!("{LFS_NEED}{}", lacking.len()));
            for oid in lacking {
                console.out(&format!("{LFS_OID}{oid}"));
            }
            0
        }
        Ok(Err(why)) => refused(asked, &why, console),
        Err(status) => unlocked(asked, status, console),
    }
}

/// The put of `request` and its manifest `wanted`: the store readied under its lock, held to the end, then its stage
/// made ([`made_stage_in`]), the archive on `input` unpacked into it and checked against the manifest before the work
/// folder's lock is taken, then under that lock each object installed ([`installed`]); the stage and its lock file
/// removed on every way out once made, and `gate-remote-lfs: installed <n>` said; 0. Else the status, its lines said.
fn put(request: &Request<'_>, wanted: &[Wanted], seams: &BoxSeams, input: &mut impl BufRead, console: &mut Console) -> u8 {
    let asked = &request.asked;
    let store = match Store::opened(&asked.root, &request.store) {
        Ok(store) => store,
        Err(why) => return refused(asked, &why, console),
    };
    let stage = match made_stage_in(asked, STAGES, SystemTime::now(), console) {
        Ok(made) => made,
        Err(why) => return refused(asked, &why, console),
    };
    let done = staged(
        Job {
            request,
            wanted,
            store: &store,
        },
        seams,
        input,
        &stage,
        console,
    );
    let removal = stage.removed();
    drop(stage);
    put_answered(asked, done, removal, console)
}

/// Says how the put of `asked` ended: `done` as [`staged`] gives it, after the stage's `removal`; the status.
fn put_answered(asked: &Asked<'_>, done: Result<Result<usize, String>, u8>, removal: Result<(), String>, console: &mut Console) -> u8 {
    match (done, removal) {
        (Ok(Ok(count)), Ok(())) => {
            console.out(&format!("{LFS_INSTALLED}{count}"));
            0
        }
        (Err(status), Ok(())) => unlocked(asked, status, console),
        (Err(status), Err(why)) => {
            let status = unlocked(asked, status, console);
            console.err(&lfs_failed_line(asked.step.word(), &why));
            status
        }
        (Ok(Err(why)), Ok(())) | (Ok(Ok(_)), Err(why)) => refused(asked, &why, console),
        (Ok(Err(why)), Err(removal)) => refused(asked, &format!("{why}; and {removal}"), console),
    }
}

/// The archive on `input` unpacked into the held stage `stage` within the budget, each member named by its oid
/// ([`Staging`]), checked against the manifest ([`checked`]), then installed under the work folder's lock ([`locked`],
/// [`installed`]): the count installed or kept, or why not; `Err` with the status when the install never ran, its
/// lines said.
fn staged(job: Job<'_>, seams: &BoxSeams, input: &mut impl BufRead, stage: &Stage, console: &mut Console) -> Result<Result<usize, String>, u8> {
    let limits = Limits {
        bytes: seams.lfs_budget_bytes.unwrap_or(LFS_BUDGET),
        files: MEMBER_LIMIT,
    };
    let mut place = Staging::new(&stage.folder, &stage.path);
    // Copies come from the archive's own members only: no file of the box's is a copy's source.
    let entries: Vec<Entry> = match extract(&mut *input, limits, &HeldBy::new(), &|| true, &mut place) {
        Ok(items) => items.into_iter().map(|item| item.entry).collect(),
        Err(error) => return Ok(Err(error.to_string())),
    };
    if let Err(why) = checked(&entries, job.wanted) {
        return Ok(Err(why));
    }
    let left = &place.left;
    locked(&job.request.asked, console, |_, _, _| installed(job.store, &stage.folder, left, &entries))
}

/// Installs each object of `entries`, checked and unpacked into the held stage `files`, which the unpack left as
/// `left` says, into `store` ([`installed_one`]). The count installed or kept; else why not.
fn installed(store: &Store, files: &Folder, left: &BTreeMap<String, Facts>, entries: &[Entry]) -> Result<usize, String> {
    for entry in entries {
        let wanted = Wanted {
            oid: entry.path.clone(),
            size: entry.size,
        };
        installed_one(store, files, left, &wanted)?;
    }
    Ok(entries.len())
}

/// Whether the staged name `oid` in the held stage `files`, looked at as itself, is still the plain file of one name
/// the unpack wrote there, as `left` says (identity, size, mtime).
fn unchanged(files: &Folder, left: &BTreeMap<String, Facts>, oid: &str) -> bool {
    left.get(&ordinal_upper(oid))
        .filter(|left| left.is_plain())
        .is_some_and(|left| files.stat_child(OsStr::new(oid)).is_ok_and(|now| Facts::of(&now) == *left))
}

/// Installs the staged object `wanted` from the held stage `files` into its folder of `store`, once its staged name is
/// shown still the file the unpack wrote ([`unchanged`]), as `left` says: a hard link made beneath both folders, which
/// never replaces anything at the name; a name already there that holds the object ([`holds`]) is kept, and any other
/// file there refuses the put ([`lfs_not_the_object`]). The staged name is then removed. Else why not.
fn installed_one(store: &Store, files: &Folder, left: &BTreeMap<String, Facts>, wanted: &Wanted) -> Result<(), String> {
    let name = OsStr::new(&wanted.oid);
    if !unchanged(files, left, &wanted.oid) {
        return Err(format!("cannot install '{}': its staged copy changed after the unpack", wanted.oid));
    }
    let folder = store.folder_of(&wanted.oid)?;
    match files.link_child(name, &folder, name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !holds(&folder, wanted)? {
                return Err(lfs_not_the_object(&wanted.oid));
            }
        }
        Err(error) => return Err(format!("cannot install the LFS object {}: {error}", wanted.oid)),
    }
    files
        .remove_child(name, false)
        .map_err(|error| format!("cannot remove the staged copy of the LFS object {}: {error}", wanted.oid))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose folders, archives or payloads cannot be made must fail loudly"
)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_args::BoxSeams;
    use gate_inputs::hash::sha256_of;
    use gate_inputs::lfs::Wanted;
    use gate_inputs::manifest::Wrote;
    use gate_inputs::pack::Packer;
    use gate_wire::{PathForm, Payload, Refusal, busy_line, ordinal_upper, refused_line};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    use super::{Store, answered, installed_one};
    use crate::beneath::Folder;
    use crate::console::Console;
    use crate::files;
    use crate::inputs::Facts;
    use crate::lines::{FAILED_STATUS, USAGE_STATUS};
    use crate::run::REFUSED_STATUS;

    const KEY: &str = "repo-0123abcd";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    /// The last write every packed object carries, in seconds since 1970.
    const WRITTEN: u64 = 1_700_000_000;
    /// The Unix second the drain case raises its barrier at.
    const DRAIN_NOW: u64 = 1_800_000_000;

    const FIRST: &[u8] = b"first object\n";
    const SECOND: &[u8] = b"second object\n";
    const THIRD: &[u8] = b"third\n";
    const FOURTH: &[u8] = b"fourth, unreadable\n";

    /// A work root `<base>/gate-remote`, private to the box user, with its `<root>/work` and the bare repository
    /// `<root>/repo.git` (and a pair's sibling's `<root>/<sibling>.git`), in a temp folder of the case `name`, private to
    /// this process and removed when the case ends.
    struct Case {
        base: PathBuf,
        sibling: Option<&'static str>,
    }

    impl Case {
        fn new(name: &str) -> Case {
            Case::made(name, None)
        }

        fn pair(name: &str, sibling: &'static str) -> Case {
            Case::made(name, Some(sibling))
        }

        fn made(name: &str, sibling: Option<&'static str>) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-lfs-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let case = Case { base, sibling };
            folder(&case.root().join("work"));
            folder(Path::new(&case.bare()));
            if let Some(sibling) = sibling {
                folder(&case.root().join(format!("{sibling}.git")));
            }
            case
        }

        fn root(&self) -> PathBuf {
            self.base.join("gate-remote")
        }

        fn root_text(&self) -> String {
            self.root().to_str().expect("the temp folder is UTF-8").to_owned()
        }

        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.root_text())
        }

        fn lock(&self) -> String {
            format!("{}.lock", self.work())
        }

        fn bare(&self) -> String {
            format!("{}/repo.git", self.root_text())
        }

        /// The lfs request of this case's work folder for `step`.
        fn json(&self, step: &str) -> Value {
            let root = self.root_text();
            let work = self.work();
            let mut json = json!({
                "Key": KEY,
                "Bare": format!("{root}/repo.git"),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Nonce": NONCE,
                "Step": step,
            });
            if let Some(sibling) = self.sibling {
                json["Pair"] = json!({ "Leaf": "repo", "Siblings": [sibling] });
            }
            json
        }

        /// The diff of the manifest of `objects`: its status and what it said.
        fn diff(&self, objects: &[&[u8]]) -> (u8, Console) {
            answer(&self.json("diff"), manifest_line(objects).as_bytes(), &BoxSeams::default())
        }

        /// The put of the manifest of `objects` with an archive of `members`, each `(name, bytes)`.
        fn put(&self, objects: &[&[u8]], members: &[(&str, &[u8])]) -> (u8, Console) {
            put_with(&self.json("put"), objects, members, &BoxSeams::default())
        }

        /// The folder `lfs-stage` below the root: the names in it, none when it is missing.
        fn staged(&self) -> Vec<String> {
            let stages = self.root().join("lfs-stage");
            if stages.exists() { names_in(&stages) } else { Vec::new() }
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// The folder `path` and those above it, made 0700 whatever the umask.
    fn folder(path: &Path) {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path).expect("a folder is made");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("its mode is set");
    }

    fn oid(bytes: &[u8]) -> String {
        sha256_of(bytes).expect("bytes hash").0
    }

    /// The manifest line of `objects`, each `<oid> <size>`, with its newline.
    fn manifest_line(objects: &[&[u8]]) -> String {
        let entries: Vec<String> = objects.iter().map(|bytes| format!("{} {}", oid(bytes), bytes.len())).collect();
        format!("{}\n", entries.join(" "))
    }

    /// An archive of `members`, each `(name, bytes)`, in the outputs archive's format.
    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut sink = Vec::new();
        let mut packer = Packer::new(&mut sink, 20).expect("a packer is made");
        for (name, bytes) in members {
            packer
                .add(name, bytes.len() as u64, WRITTEN, *bytes, Wrote::ThisRun)
                .expect("a member is packed");
        }
        packer.finish().expect("the archive ends");
        sink
    }

    fn decoded(json: &Value) -> Payload {
        Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
    }

    /// The answer to the request `json` with `input` on its stdin under `seams`: the status and what it said.
    fn answer(json: &Value, input: &[u8], seams: &BoxSeams) -> (u8, Console) {
        let mut console = Console::captured();
        let mut input = input;
        let status = answered(&decoded(json), seams, &mut input, &mut console);
        (status, console)
    }

    fn put_with(json: &Value, objects: &[&[u8]], members: &[(&str, &[u8])], seams: &BoxSeams) -> (u8, Console) {
        let mut input = manifest_line(objects).into_bytes();
        input.extend(archive(members));
        answer(json, &input, seams)
    }

    /// The store's path of the object `oid` of the bare repository `bare`.
    fn object_path(bare: &str, oid: &str) -> PathBuf {
        Path::new(bare).join("lfs/objects").join(&oid[..2]).join(&oid[2..4]).join(oid)
    }

    /// Puts `bytes` at the store's name of the object `oid` of `bare`, mode `mode`, its folders made 0700.
    fn placed(bare: &str, oid: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = object_path(bare, oid);
        for folder_path in [
            Path::new(bare).join("lfs"),
            Path::new(bare).join("lfs/objects"),
            path.parent().and_then(Path::parent).expect("the object has folders").to_owned(),
            path.parent().expect("the object has a folder").to_owned(),
        ] {
            folder(&folder_path);
        }
        fs::write(&path, bytes).expect("the object is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("its mode is set");
        path
    }

    /// The permission bits of `path`, looked at as itself.
    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("the path is looked at").permissions().mode() & 0o7777
    }

    fn names_in(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(path)
            .expect("the folder is listed")
            .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn need(oids: &[&[u8]]) -> Vec<String> {
        std::iter::once(format!("gate-remote-lfs: need {}", oids.len()))
            .chain(oids.iter().map(|bytes| format!("gate-remote-lfs: oid {}", oid(bytes))))
            .collect()
    }

    fn lfs_tag() -> String {
        refused_line(NONCE, Refusal::Lfs)
    }

    #[test]
    fn a_diff_of_an_empty_store_names_every_object_in_manifest_order() {
        let case = Case::new("empty-store");

        let (status, console) = case.diff(&[SECOND, FIRST, THIRD]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[SECOND, FIRST, THIRD]));
        assert!(console.err.is_empty(), "{:?}", console.err);
        let bare = Path::new(&case.bare()).to_owned();
        assert_eq!(mode_of(&bare.join("lfs")), 0o700, "the store's folders are the box's alone");
        assert_eq!(mode_of(&bare.join("lfs/objects")), 0o700);
        assert_eq!(mode_of(&bare.join("lfs.lock")), 0o600, "the store lock is made 0600");
    }

    #[test]
    fn a_diff_names_none_of_the_objects_the_store_holds() {
        let case = Case::new("held");
        placed(&case.bare(), &oid(FIRST), FIRST, 0o600);
        placed(&case.bare(), &oid(THIRD), THIRD, 0o600);

        let (status, console) = case.diff(&[FIRST, SECOND, THIRD]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[SECOND]));
    }

    #[test]
    fn a_diff_counts_a_wrong_size_or_wrong_mode_object_as_lacking() {
        let case = Case::new("wrong-held");
        placed(&case.bare(), &oid(FIRST), b"not the first object's size\n", 0o600);
        placed(&case.bare(), &oid(SECOND), SECOND, 0o644);
        placed(&case.bare(), &oid(THIRD), THIRD, 0o600);
        placed(&case.bare(), &oid(FOURTH), FOURTH, 0o000);

        let (status, console) = case.diff(&[FIRST, SECOND, THIRD, FOURTH]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.out,
            need(&[FIRST, SECOND, FOURTH]),
            "an object the box cannot read is lacking too"
        );
    }

    #[test]
    fn a_diff_does_not_follow_a_link_at_an_object_name() {
        let case = Case::new("link");
        let outside = case.base.join("outside");
        fs::write(&outside, FIRST).expect("the outside file is written");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).expect("its mode is set");
        let path = placed(&case.bare(), &oid(FIRST), FIRST, 0o600);
        fs::remove_file(&path)
            .and_then(|()| symlink(&outside, &path))
            .expect("a link takes the object's name");

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[FIRST]), "a link is no object");
        assert_eq!(fs::read_link(&path).ok(), Some(outside), "the link is left as it is");
    }

    #[test]
    fn a_put_installs_each_object_at_0600_and_says_the_count() {
        let case = Case::new("put");
        let (first, second) = (oid(FIRST), oid(SECOND));

        let (status, console) = case.put(&[FIRST, SECOND], &[(&first, FIRST), (&second, SECOND)]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-lfs: installed 2"]);
        assert!(console.err.is_empty(), "{:?}", console.err);
        for (name, bytes) in [(&first, FIRST), (&second, SECOND)] {
            let path = object_path(&case.bare(), name);
            assert_eq!(fs::read(&path).expect("the object is read"), bytes);
            assert_eq!(mode_of(&path), 0o600, "{name}");
            assert_eq!(mode_of(path.parent().expect("a folder")), 0o700, "{name}");
        }
        let (status, console) = case.diff(&[FIRST, SECOND]);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[]), "a diff after the put needs nothing");
    }

    #[test]
    fn a_second_put_of_held_objects_keeps_them_and_says_the_count() {
        let case = Case::new("put-twice");
        let first = oid(FIRST);
        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);
        assert_eq!(status, 0, "{:?}", console.err);
        let path = object_path(&case.bare(), &first);
        let ino = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).expect("the object is looked at"));

        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-lfs: installed 1"]);
        let kept = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).expect("the object is looked at"));
        assert_eq!(kept, ino, "the object in the store is kept, not replaced");
        assert_eq!(fs::read(&path).expect("the object is read"), FIRST);
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    /// A refused put of `objects` and `members`: status 1, the `lfs` tag and the put's failure line for `why`, nothing
    /// installed and no stage left.
    fn assert_put_refused(case: &Case, objects: &[&[u8]], members: &[(&str, &[u8])], why: &str) {
        let (status, console) = case.put(objects, members);
        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(console.err, [lfs_tag(), format!("gate: remote lfs put failed: {why}")]);
        assert!(console.out.is_empty(), "{:?}", console.out);
        for (name, _) in members {
            assert!(!object_path(&case.bare(), name).exists(), "{name} is not installed");
        }
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_put_refuses_a_member_not_in_the_manifest() {
        let case = Case::new("not-in-manifest");
        let (first, second) = (oid(FIRST), oid(SECOND));
        let why = format!("the archive's member '{second}' is not an object the manifest names");
        assert_put_refused(&case, &[FIRST], &[(&first, FIRST), (&second, SECOND)], &why);
    }

    #[test]
    fn a_put_refuses_a_member_whose_hash_is_not_its_name() {
        let case = Case::new("wrong-hash");
        let first = oid(FIRST);
        let other: &[u8] = b"FIRST OBJECT\n";
        let why = format!("the archive's member '{first}' has the SHA-256 {}", oid(other));
        assert_put_refused(&case, &[FIRST], &[(&first, other)], &why);
    }

    #[test]
    fn a_put_refuses_a_member_of_the_wrong_size() {
        let case = Case::new("wrong-size");
        let first = oid(FIRST);
        let mut input = format!("{first} {}\n", FIRST.len() + 1).into_bytes();
        input.extend(archive(&[(&first, FIRST)]));

        let (status, console) = answer(&case.json("put"), &input, &BoxSeams::default());

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let why = format!(
            "the archive's member '{first}' is {} bytes, the manifest's {}",
            FIRST.len(),
            FIRST.len() + 1
        );
        assert_eq!(console.err, [lfs_tag(), format!("gate: remote lfs put failed: {why}")]);
        assert!(!object_path(&case.bare(), &first).exists());
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_put_refuses_a_member_named_twice() {
        let case = Case::new("named-twice");
        let first = oid(FIRST);
        let why = format!("{}/lfs-stage/{KEY}-{NONCE}/{first}: File exists (os error 17)", case.root_text());
        assert_put_refused(&case, &[FIRST], &[(&first, FIRST), (&first, FIRST)], &why);
    }

    #[test]
    fn a_put_over_its_budget_is_refused() {
        let case = Case::new("budget");
        let first = oid(FIRST);
        let seams = BoxSeams {
            lfs_budget_bytes: Some(10),
            ..BoxSeams::default()
        };

        let (status, console) = put_with(&case.json("put"), &[FIRST], &[(&first, FIRST)], &seams);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [
                lfs_tag(),
                "gate: remote lfs put failed: the outputs archive holds more than 10 bytes once decompressed".to_owned()
            ]
        );
        assert!(!object_path(&case.bare(), &first).exists());
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_put_never_replaces_a_file_that_is_not_the_object() {
        let case = Case::new("not-the-object");
        let first = oid(FIRST);
        let planted: &[u8] = b"planted, of another size\n";
        let path = placed(&case.bare(), &first, planted, 0o600);

        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let shown = format!("lfs/objects/{}/{}/{first}", &first[..2], &first[2..4]);
        assert_eq!(
            console.err,
            [
                lfs_tag(),
                format!("gate: remote lfs put failed: {shown} is in the store but is not the object")
            ]
        );
        assert_eq!(fs::read(&path).expect("the planted file is read"), planted, "the file is left as it is");
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_put_removes_its_stage_on_every_way_out() {
        let case = Case::new("stage-removed");
        let first = oid(FIRST);

        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);
        assert_eq!(status, 0, "{:?}", console.err);
        assert!(case.root().join("lfs-stage").exists(), "the stages folder is made");
        assert!(case.staged().is_empty(), "an installed put leaves no stage: {:?}", case.staged());

        let wrong: &[u8] = b"not the second's bytes";
        let (status, console) = case.put(&[SECOND], &[(&oid(SECOND), wrong)]);
        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let why = format!("the archive's member '{}' has the SHA-256 {}", oid(SECOND), oid(wrong));
        assert_eq!(console.err, [lfs_tag(), format!("gate: remote lfs put failed: {why}")]);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(case.staged().is_empty(), "a refused put leaves no stage: {:?}", case.staged());

        let mut input = manifest_line(&[THIRD]).into_bytes();
        input.extend(b"not an archive");
        let (status, console) = answer(&case.json("put"), &input, &BoxSeams::default());
        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [lfs_tag(), "gate: remote lfs put failed: Unknown frame descriptor".to_owned()]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(case.staged().is_empty(), "an unreadable archive leaves no stage: {:?}", case.staged());

        let _held = files::held_for_test(Path::new(&case.lock()));
        let (status, console) = case.put(&[THIRD], &[(&oid(THIRD), THIRD)]);
        assert_eq!(status, REFUSED_STATUS, "{:?}", console.err);
        assert_eq!(console.err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(case.staged().is_empty(), "a busy put leaves no stage: {:?}", case.staged());
    }

    #[test]
    fn a_put_refuses_a_staged_object_swapped_before_its_install() {
        let case = Case::new("swapped");
        let first = oid(FIRST);
        let stage = case.base.join("stage");
        folder(&stage);
        let staged = stage.join(&first);
        fs::write(&staged, FIRST).expect("the staged object is written");
        let files = Folder::open(&stage).expect("the stage opens");
        let facts = Facts::of(&files.stat_child(OsStr::new(&first)).expect("the staged object is looked at"));
        let left = BTreeMap::from([(ordinal_upper(&first), facts)]);
        // The unpacked file is kept aside, so its inode is not reused, and a file of the same size takes its name.
        fs::rename(&staged, stage.join("aside")).expect("the staged object is moved aside");
        fs::write(&staged, b"FIRST OBJECT\n").expect("another file takes its name");
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).expect("its mode is set");
        let store = Store::opened(&case.root_text(), &case.bare()).expect("the store opens");
        let wanted = Wanted {
            oid: first.clone(),
            size: FIRST.len() as u64,
        };

        let installed = installed_one(&store, &files, &left, &wanted);

        assert_eq!(
            installed,
            Err(format!("cannot install '{first}': its staged copy changed after the unpack"))
        );
        assert!(!object_path(&case.bare(), &first).exists(), "nothing is installed");
        assert_eq!(
            fs::read(&staged).expect("the swapped file is read"),
            b"FIRST OBJECT\n",
            "it is left as it is"
        );
    }

    #[test]
    fn an_lfs_step_with_no_pair_but_an_lfs_repo_is_a_usage_error() {
        let case = Case::new("repo-no-pair");
        let mut json = case.json("diff");
        json["LfsRepo"] = json!("lib");

        let (status, console) = answer(&json, manifest_line(&[FIRST]).as_bytes(), &BoxSeams::default());

        assert_eq!(status, USAGE_STATUS, "{:?}", console.err);
        assert_eq!(console.err, ["gate: box payload LfsRepo needs a Pair"]);
        assert!(console.out.is_empty(), "{:?}", console.out);
    }

    #[test]
    fn an_lfs_repo_naming_no_sibling_of_the_pair_is_a_usage_error() {
        let case = Case::pair("repo-no-sibling", "lib");
        let mut json = case.json("put");
        json["LfsRepo"] = json!("other");

        let (status, console) = answer(&json, manifest_line(&[FIRST]).as_bytes(), &BoxSeams::default());

        assert_eq!(status, USAGE_STATUS, "{:?}", console.err);
        assert_eq!(console.err, ["gate: box payload LfsRepo other is not a sibling of the pair"]);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_held_work_lock_refuses_the_put_busy() {
        let case = Case::new("put-busy");
        let first = oid(FIRST);
        let _held = files::held_for_test(Path::new(&case.lock()));

        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);

        assert_eq!(status, REFUSED_STATUS);
        assert_eq!(console.err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(!object_path(&case.bare(), &first).exists(), "nothing is installed");
        assert!(case.staged().is_empty(), "{:?}", case.staged());
    }

    #[test]
    fn a_link_at_the_store_lock_is_refused() {
        let case = Case::new("lock-link");
        let outside = case.base.join("outside");
        fs::write(&outside, b"").expect("the outside file is written");
        let lock = Path::new(&case.bare()).join("lfs.lock");
        symlink(&outside, &lock).expect("a link takes the lock's name");

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let why = format!(
            "cannot open the LFS store lock {}: Too many levels of symbolic links (os error 62)",
            lock.display()
        );
        assert_eq!(console.err, [lfs_tag(), format!("gate: remote lfs diff failed: {why}")]);
        assert_eq!(fs::read_link(&lock).ok(), Some(outside), "the link is left as it is");
        assert!(!Path::new(&case.bare()).join("lfs").exists(), "no store folder is made");
    }

    #[test]
    fn a_store_lock_open_to_others_is_refused() {
        let case = Case::new("lock-open");
        let lock = Path::new(&case.bare()).join("lfs.lock");
        fs::write(&lock, b"").expect("the lock is written");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).expect("its mode is set");

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [lfs_tag(), format!("gate: remote lfs diff failed: {} is open to others", lock.display())]
        );
        assert_eq!(mode_of(&lock), 0o644, "the lock is left as it is");
    }

    #[test]
    fn a_link_at_a_store_folder_is_refused() {
        let case = Case::new("folder-link");
        let outside = case.base.join("outside");
        folder(&outside);
        let lfs = Path::new(&case.bare()).join("lfs");
        symlink(&outside, &lfs).expect("a link takes the folder's name");

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        let why = format!("cannot ready {}: Not a directory (os error 20)", lfs.display());
        assert_eq!(console.err, [lfs_tag(), format!("gate: remote lfs diff failed: {why}")]);
        assert!(names_in(&outside).is_empty(), "nothing is made where the link leads");
    }

    #[test]
    fn a_put_into_a_store_folder_of_another_mode_is_refused() {
        let case = Case::new("put-folder-mode");
        let first = oid(FIRST);
        let objects = Path::new(&case.bare()).join("lfs/objects");
        folder(&objects);
        fs::set_permissions(&objects, fs::Permissions::from_mode(0o755)).expect("its mode is set");

        let (status, console) = case.put(&[FIRST], &[(&first, FIRST)]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [lfs_tag(), format!("gate: remote lfs put failed: {} is open to others", objects.display())]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(case.staged().is_empty(), "{:?}", case.staged());
        assert_eq!(mode_of(&objects), 0o755, "the folder is left as it is");
    }

    #[test]
    fn an_lfs_step_with_a_channel_is_refused_channel() {
        let case = Case::new("channel");
        for step in ["put", "diff"] {
            let mut json = case.json(step);
            json["Channel"] = json!({
                "Address": "100.64.0.1",
                "Peer": "100.64.0.2",
                "BoxProof": "00112233445566778899aabbccddeeff",
                "PcProof": "ffeeddccbbaa99887766554433221100",
                "Length": 7,
                "Sha256": "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            });

            let (status, console) = answer(&json, b"not a manifest\n", &BoxSeams::default());

            assert_eq!(status, REFUSED_STATUS, "{step}");
            assert_eq!(
                console.err,
                [
                    format!("gate-remote-refused: {NONCE} channel"),
                    "gate: data channel not completed: this builder has no data channel".to_owned(),
                ],
                "{step}"
            );
            assert!(!Path::new(&case.bare()).join("lfs").exists(), "{step}: no store is readied");
        }
    }

    #[test]
    fn a_malformed_manifest_is_refused_lfs_before_any_lock() {
        let case = Case::new("malformed");
        // A step that took the work lock would be refused busy.
        let _held = files::held_for_test(Path::new(&case.lock()));

        let (status, console) = answer(&case.json("diff"), b"not a manifest\n", &BoxSeams::default());

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [
                lfs_tag(),
                "gate: remote lfs diff failed: the LFS manifest is not a list of '<oid> <size>' entries".to_owned()
            ]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert!(!Path::new(&case.bare()).join("lfs").exists(), "no store is readied");
    }

    #[test]
    fn an_lfs_step_for_a_sibling_uses_the_sibling_s_store() {
        let case = Case::pair("sibling", "lib");
        let sibling = format!("{}/lib.git", case.root_text());
        let first = oid(FIRST);
        let mut json = case.json("put");
        json["LfsRepo"] = json!("lib");

        let (status, console) = put_with(&json, &[FIRST], &[(&first, FIRST)], &BoxSeams::default());

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-lfs: installed 1"]);
        assert_eq!(fs::read(object_path(&sibling, &first)).expect("the object is read"), FIRST);
        assert!(!Path::new(&case.bare()).join("lfs").exists(), "the main repo's store is not touched");

        let mut json = case.json("diff");
        json["LfsRepo"] = json!("lib");
        let (status, console) = answer(&json, manifest_line(&[FIRST, SECOND]).as_bytes(), &BoxSeams::default());
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[SECOND]), "the sibling's store holds the object");
        let (status, console) = case.diff(&[FIRST]);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, need(&[FIRST]), "the main repo's store does not");
    }

    #[test]
    fn a_draining_box_refuses_lfs_drain() {
        let case = Case::new("drain");
        let seams = BoxSeams {
            now: Some(DRAIN_NOW),
            ..BoxSeams::default()
        };
        let mut console = Console::captured();
        let raise = decoded(&json!({ "Root": case.root_text(), "Action": "raise" }));
        assert_eq!(crate::drain::drain(&raise, &seams, &mut console), 0, "{:?}", console.err);

        let (status, console) = answer(&case.json("diff"), manifest_line(&[FIRST]).as_bytes(), &seams);

        assert_eq!(status, REFUSED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [
                refused_line(NONCE, Refusal::Drain),
                "gate: drain place none".to_owned(),
                format!("gate: drain barrier {DRAIN_NOW} {}", DRAIN_NOW + 1800),
            ]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
    }

    #[test]
    fn a_held_work_lock_refuses_the_diff_busy() {
        let case = Case::new("busy");
        let _held = files::held_for_test(Path::new(&case.lock()));

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, REFUSED_STATUS);
        assert_eq!(console.err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(console.out.is_empty(), "{:?}", console.out);
    }

    #[test]
    fn a_store_folder_of_another_mode_is_refused() {
        let case = Case::new("folder-mode");
        let objects = Path::new(&case.bare()).join("lfs/objects");
        folder(&objects);
        fs::set_permissions(&objects, fs::Permissions::from_mode(0o755)).expect("its mode is set");

        let (status, console) = case.diff(&[FIRST]);

        assert_eq!(status, FAILED_STATUS, "{:?}", console.err);
        assert_eq!(
            console.err,
            [
                lfs_tag(),
                format!("gate: remote lfs diff failed: {} is open to others", objects.display())
            ]
        );
        assert!(console.out.is_empty(), "{:?}", console.out);
        assert_eq!(mode_of(&objects), 0o755, "the folder is left as it is");
    }
}

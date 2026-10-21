//! `gate box lfs`: a routed run's Git LFS objects, sent from the PC before the run into the store beside the repo's
//! bare repository, `{Bare}\lfs\objects\{oid[0..2]}\{oid[2..4]}\{oid}`, which every work folder of the repo reads
//! through its alternates. Its request names the `Step`: `diff` reads the PC's manifest line of `<oid> <size>` entries
//! and says which objects the store lacks or holds at another size, and `put` reads the manifest and then an archive of
//! those objects, each member named by its oid, in the outputs archive's format, to the end of stdin, and installs them.
//!
//! Every folder of the store is made with the box's protected security and an existing one is adopted only when it
//! holds that security, so no other account and no lower process writes the store; an unelevated box, whose folders are
//! labelled Medium, refuses every call. Both steps take the store's lock, `{Bare}\lfs.lock`, shared, and the work
//! folder's lock as a run does; the lock is made with the store's protected security and adopted only when it holds
//! it. A put readies the store before it reads the archive, unpacks it into a stage of its own before it takes the work
//! folder's lock, refuses the whole archive when a member is not an object the manifest names, does not hash to its
//! name or is not of the manifest's size, and installs each object by a rename that never replaces: an object already
//! in the store, another put's among them, is kept, and its staged copy deleted.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, Read};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gate_args::BoxSeams;
use gate_router::escaped;
use gate_win32::{Descriptor, Make, Share};
use gate_wire::{Payload, SHA256_DIGITS, grandparent, is_lower_hex};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    READ_CONTROL,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::inputs::{
    Leftover, SCRATCH_SUFFIX_DIGITS, STAGED_ACCESS, Stage, StageNamed, WorkLock, locked, made_stage_named, not_the_box_s, plain_copy,
    protected_folder, protected_security, removed_stage, same_stage, staged_files,
};
use crate::boxside::preflight::is_folder_itself;
use crate::boxside::run::{FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, REFUSED_STATUS, SHARING_VIOLATION};
use crate::console::Console;
use crate::outputs::archive::{Entry, PcFiles, unpack};
use crate::outputs::manifest::{MANIFEST_LIMIT, Manifest};
use crate::remote::lfs::{Listed, NO_SCRATCH, Object, POINTER_MOST, Pointer, manifest_digest, parsed_pointer};
use crate::remote::{Refusal, refused_line as tagged_line};

/// The bytes in one gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most bytes one put unpacks: 16 GiB, unless the `--test-lfs-budget-bytes` seam says otherwise.
const LFS_BUDGET: u64 = 16 * GIB;
/// The most objects one manifest names.
const OBJECTS: usize = 50_000;
/// The folder below the box's root that holds each put's stage, `{root}\lfs-stage\{key}-{nonce}`.
const STAGES: &str = "lfs-stage";
/// The folder in a stage the archive is unpacked into.
const STAGED_FILES: &str = "files";
/// The line refusing a request that names a sibling pair.
const PAIR_REFUSED: &str = "gate box: lfs for a sibling pair is not taken yet";
/// The test seam that makes a build with the `test-seams` feature refuse an unelevated box, when it is `check`.
const ELEVATION_SEAM: &str = "GATE_TEST_BOX_ELEVATION";
/// The store's lock beside the store, `{Bare}\lfs.lock`.
const STORE_LOCK: &str = "lfs.lock";

/// The step a request asks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Diff,
    Put,
}

impl Step {
    fn word(self) -> &'static str {
        match self {
            Step::Diff => "diff",
            Step::Put => "put",
        }
    }
}

/// What an LFS request reads of its payload.
struct Asked<'a> {
    key: &'a str,
    work: &'a str,
    bare: &'a str,
    lock: &'a str,
    nonce: &'a str,
    step: Step,
    /// The box's root, `{root}\work\{key}` being the work folder, which the stages folder sits in.
    root: &'a str,
}

impl Asked<'_> {
    /// The request `payload` holds; else the usage line. A request naming a sibling pair is refused first.
    fn read(payload: &Payload) -> Result<Asked<'_>, String> {
        if payload.sets("Pair") {
            return Err(PAIR_REFUSED.to_owned());
        }
        let (key, work) = payload.work_folder()?;
        let bare = payload.path("Bare")?;
        let lock = payload.beside_work("Lock", work, "lock")?;
        let nonce = payload.nonce()?;
        let step = match payload.text("Step")? {
            "diff" => Step::Diff,
            "put" => Step::Put,
            other => return Err(format!("gate: box payload Step is not diff or put: {}", escaped(other))),
        };
        let root =
            grandparent(work).ok_or_else(|| format!(r"gate: box payload Work must be <root>\work\<key> to stage objects: {}", escaped(work)))?;
        Ok(Asked {
            key,
            work,
            bare,
            lock,
            nonce,
            step,
            root,
        })
    }

    /// The work folder and its lock, as [`locked`] takes them.
    fn work_lock(&self) -> WorkLock<'_> {
        WorkLock {
            key: self.key,
            work: self.work,
            bare: self.bare,
            lock: self.lock,
            nonce: self.nonce,
        }
    }
}

/// One object of the PC's manifest: its oid and its size.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Wanted {
    oid: String,
    size: u64,
}

/// Answers `gate box lfs`: the request read, an unelevated box refused, then the manifest read from stdin before any
/// lock is taken, so a client that stalls holds nothing; then the step. A payload refused is a usage error (2); any
/// other failure is refused `lfs` and fails the step (1).
pub(super) fn lfs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let asked = match Asked::read(payload) {
        Ok(asked) => asked,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    if let Err(why) = elevation() {
        return refused(&asked, &why, console);
    }
    let wanted = match read_manifest(io::stdin().lock()) {
        Ok(wanted) => wanted,
        Err(why) => return refused(&asked, &why, console),
    };
    match asked.step {
        Step::Diff => diff(&asked, &wanted, console),
        Step::Put => put(&asked, &wanted, seams, console),
    }
}

/// Says the step is refused `lfs` and failed for `why`: the tag line, then the step's failure line; 1.
fn refused(asked: &Asked<'_>, why: &str, console: &mut Console) -> u32 {
    console.err(&refused_line(asked.nonce));
    console.err(&format!("gate: remote lfs {} failed: {}", asked.step.word(), escaped(why)));
    FAILED_STATUS
}

/// The status of a step whose work folder could not be locked or laid out ([`locked`]), its own lines said: a refusal
/// of its own (the folder busy, or its lay-out refused), tagged with its reason, as it is; any other failure then
/// tagged `lfs` and said as the step's failure, as every other failure of the step is.
fn unlocked(asked: &Asked<'_>, status: u32, console: &mut Console) -> u32 {
    if status != REFUSED_STATUS {
        refused(asked, "the work folder could not be locked or laid out", console);
    }
    status
}

/// `Ok` when the box may keep the store: it runs elevated, or it does not judge its elevation; else why not.
fn elevation() -> Result<(), String> {
    let elevated = gate_win32::token_facts()
        .map_err(|error| format!("cannot read the box's token: {error}"))?
        .elevated;
    let seam = std::env::var(ELEVATION_SEAM).ok();
    if refuses_unelevated(elevated, cfg!(feature = "test-seams"), seam.as_deref()) {
        return Err("the box is not elevated, so its LFS store could be written by any process of its user".to_owned());
    }
    Ok(())
}

/// Whether a box `elevated` or not is refused: an unelevated one always, in a build without the `test-seams` feature
/// (`seams_build`), and in one with it only when [`ELEVATION_SEAM`] is `check`, since the suite runs unelevated.
fn refuses_unelevated(elevated: bool, seams_build: bool, seam: Option<&str>) -> bool {
    !elevated && (!seams_build || seam == Some("check"))
}

/// The manifest line read from `input`, up to its first newline or the input's end and never more than
/// [`MANIFEST_LIMIT`] bytes, without its `\n` or `\r\n`: space-separated `<oid> <size>` entries, each oid 64 lower-case
/// hex digits and each size a decimal, no oid twice and no more than [`OBJECTS`]; else why it is not one.
fn read_manifest(input: impl BufRead) -> Result<Vec<Wanted>, String> {
    let mut line = super::bounded_line(input, MANIFEST_LIMIT as u64).map_err(|unread| match unread {
        super::Unread::OverLimit => "the LFS manifest reaches 64 MiB without a newline".to_owned(),
        super::Unread::Failed(error) => format!("the LFS manifest could not be read: {error}"),
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

/// The objects the manifest line `text` names ([`read_manifest`]); else why it is not one.
fn parsed_manifest(text: &str) -> Result<Vec<Wanted>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let words: Vec<&str> = text.split(' ').collect();
    if !words.len().is_multiple_of(2) {
        return Err("the LFS manifest is not a list of '<oid> <size>' entries".to_owned());
    }
    if words.len() / 2 > OBJECTS {
        return Err(format!("the LFS manifest names more than {OBJECTS} objects"));
    }
    let mut seen = HashSet::new();
    let mut wanted = Vec::with_capacity(words.len() / 2);
    for &[oid, size] in words.as_chunks::<2>().0 {
        if !is_lower_hex(oid, SHA256_DIGITS) {
            return Err(format!("the LFS manifest names '{}', which is not an oid", escaped(oid)));
        }
        if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!(
                "the LFS manifest gives {oid} the size '{}', which is not a number",
                escaped(size)
            ));
        }
        let size: u64 = size
            .parse()
            .map_err(|_| format!("the LFS manifest gives {oid} a size past what a file holds"))?;
        if !seen.insert(oid) {
            return Err(format!("the LFS manifest names {oid} twice"));
        }
        wanted.push(Wanted { oid: oid.to_owned(), size });
    }
    Ok(wanted)
}

/// The repo's store, readied and held: its lock taken shared, the bare repository, `lfs` and `lfs\objects`, each held
/// without delete sharing, the folders made or adopted protected; the path of `objects`, and the security its folders
/// are made with.
struct Store {
    _lock: File,
    _bare: File,
    _lfs: File,
    objects: File,
    path: PathBuf,
    security: Descriptor,
}

impl Store {
    /// The store of the bare repository `bare`, its lock taken shared ([`store_lock`]) and its folders readied
    /// ([`protected_folder`]): an `lfs` folder of its own whose security is not the box's, as git-lfs left one in a
    /// checkout before the box kept the store, moved aside and made new, said on `console`; any other folder whose
    /// security is not the box's refused. Else why not.
    fn opened(bare: &str, console: &mut Console) -> Result<Store, String> {
        let bare_held = opened_bare(bare)?;
        let lock = store_lock(&bare_held, bare)?;
        let security = protected_security()?;
        let lfs_path = Path::new(bare).join("lfs");
        let lfs = protected_folder(&bare_held, &lfs_path, FILE_GENERIC_READ, &security, Leftover::MoveAside(console))?;
        let path = lfs_path.join("objects");
        let objects = protected_folder(&lfs, &path, FILE_GENERIC_READ, &security, Leftover::Refuse)?;
        Ok(Store {
            _lock: lock,
            _bare: bare_held,
            _lfs: lfs,
            objects,
            path,
            security,
        })
    }

    /// The folder `{oid[0..2]}\{oid[2..4]}` the object `oid` lives in, each readied protected; else why not.
    fn folder_of(&self, oid: &str) -> Result<File, String> {
        let (first, second) = (&oid[..2], &oid[2..4]);
        let first_path = self.path.join(first);
        let outer = protected_folder(&self.objects, &first_path, FILE_GENERIC_READ, &self.security, Leftover::Refuse)?;
        protected_folder(&outer, &first_path.join(second), FILE_GENERIC_READ, &self.security, Leftover::Refuse)
    }

    /// Whether the store holds `wanted` in its held folder `folder`: a plain file at its oid, not a link or a junction,
    /// of its size, whose security is all its protected folder hands down ([`inherits_only`]). A second name of the
    /// file is taken, as git-lfs links an object into a run's scratch folder and a box killed mid-run leaves that link:
    /// a link shares the object's security and needs write access to it to make. Else why it cannot be read.
    fn holds(folder: &File, wanted: &Wanted) -> Result<bool, String> {
        let cannot = |error: io::Error| format!("cannot read the LFS object {}: {error}", wanted.oid);
        let file = match gate_win32::open_beneath_as(folder, &wanted.oid, FILE_READ_ATTRIBUTES | READ_CONTROL, Share::All, Make::Open) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(cannot(error)),
        };
        let metadata = file.metadata().map_err(cannot)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() || metadata.len() != wanted.size {
            return Ok(false);
        }
        Ok(inherits_only(&gate_win32::security_text(&file).map_err(cannot)?))
    }

    /// The objects of `wanted` the store lacks or holds at another size, in their order; else why not.
    fn lacking<'w>(&self, wanted: &'w [Wanted]) -> Result<Vec<&'w Wanted>, String> {
        let mut lacking = Vec::new();
        for object in wanted {
            let folder = self.folder_of(&object.oid)?;
            if !Self::holds(&folder, object)? {
                lacking.push(object);
            }
        }
        Ok(lacking)
    }
}

/// Whether the SDDL `text` of an object's DACL and label holds only what its folder hands down: an unprotected DACL
/// and a label, each of at least one entry and every entry inherited, so the object has the protected security of the
/// store folder it is in and no grant of its own.
fn inherits_only(text: &str) -> bool {
    let stripped = gate_win32::without_auto_inherit(text);
    let Some(lists) = stripped.strip_prefix("D:") else {
        return false;
    };
    let (dacl, label) = lists.split_once("S:").unwrap_or((lists, ""));
    let inherited = |list: &str| {
        let mut entries = list.split('(').skip(1).peekable();
        entries.peek().is_some() && entries.all(|entry| entry.split(';').nth(1).is_some_and(|flags| flags.contains("ID")))
    };
    !dacl.starts_with('P') && inherited(dacl) && inherited(label)
}

/// The store's lock `{bare}\lfs.lock`, opened as the work folder's lock is but shared for reading and writing, so
/// every put and run shares it and a prune that opens it alone waits for them all: made new with the box's protected
/// security beneath the held bare repository `bare_held` ([`gate_win32::make_file_beneath`]), else the one there
/// opened as itself and adopted only when it is a plain file with that security. Else why not, a lock a prune holds
/// or another account made among them.
fn store_lock(bare_held: &File, bare: &str) -> Result<File, String> {
    let path = Path::new(bare).join(STORE_LOCK);
    let cannot = |error: io::Error| format!("cannot open the LFS store lock {}: {error}", path.display());
    let held_alone = || format!("the LFS store lock {} is held alone by another process", path.display());
    let access = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
    match gate_win32::make_file_beneath(bare_held, STORE_LOCK, access, Share::NoDelete, &protected_security()?) {
        Ok(lock) => return Ok(lock),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) => return Err(held_alone()),
        Err(error) => return Err(cannot(error)),
    }
    let lock = match gate_win32::open_beneath_as(bare_held, STORE_LOCK, access, Share::NoDelete, Make::Open) {
        Ok(lock) => lock,
        Err(error) if error.raw_os_error() == Some(SHARING_VIOLATION) => return Err(held_alone()),
        Err(error) => return Err(cannot(error)),
    };
    gate_win32::check_plain_file(&lock).map_err(cannot)?;
    if !gate_win32::is_protected_file(&lock).map_err(cannot)? {
        return Err(not_the_box_s(&path));
    }
    Ok(lock)
}

/// The bare repository `bare` opened as itself and held without delete sharing; else why not, a link, a junction or
/// no folder there among the reasons.
fn opened_bare(bare: &str) -> Result<File, String> {
    let cannot = |error: io::Error| format!("cannot open {bare}: {error}");
    let bare_held = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(bare)
        .map_err(cannot)?;
    if !is_folder_itself(&bare_held).map_err(cannot)? {
        return Err(format!("{bare} is a link, a junction or not a folder"));
    }
    Ok(bare_held)
}

/// The diff, under the work folder's lock and the store's: `gate-remote-lfs: need <n>`, then `gate-remote-lfs: oid
/// <oid>` for each object of the manifest the store lacks or holds at another size, in the manifest's order; 0. Else
/// the status, its lines said.
fn diff(asked: &Asked<'_>, wanted: &[Wanted], console: &mut Console) -> u32 {
    let lacking = locked(&asked.work_lock(), console, |_, console| {
        let store = Store::opened(asked.bare, console)?;
        store
            .lacking(wanted)
            .map(|lacking| lacking.into_iter().map(|object| object.oid.clone()).collect::<Vec<_>>())
    });
    match lacking {
        Ok(Ok(lacking)) => {
            console.out(&format!("gate-remote-lfs: need {}", lacking.len()));
            for oid in lacking {
                console.out(&format!("gate-remote-lfs: oid {oid}"));
            }
            0
        }
        Ok(Err(why)) => refused(asked, &why, console),
        Err(status) => unlocked(asked, status, console),
    }
}

/// The put: the store readied under its lock, then its stage made, the archive on stdin unpacked into it and checked
/// against the manifest `wanted`, then under the work folder's lock and the store's each object installed
/// ([`installed`]); the stage removed beneath its held handles on every way out once made, and `gate-remote-lfs:
/// installed <n>` said; 0. Else the status, its lines said.
fn put(asked: &Asked<'_>, wanted: &[Wanted], seams: &BoxSeams, console: &mut Console) -> u32 {
    if let Err(why) = Store::opened(asked.bare, console) {
        return refused(asked, &why, console);
    }
    let named = StageNamed {
        root: asked.root,
        stages: STAGES,
        key: asked.key,
        nonce: asked.nonce,
    };
    let stage = match made_stage_named(&named, console) {
        Ok(made) => made,
        Err(why) => return refused(asked, &why, console),
    };
    let done = staged(asked, wanted, seams, &stage, console);
    let removal = removed_stage(&stage);
    drop(stage);
    match (done, removal) {
        (Ok(Ok(count)), Ok(())) => {
            console.out(&format!("gate-remote-lfs: installed {count}"));
            0
        }
        (Err(status), Ok(())) => unlocked(asked, status, console),
        (Err(status), Err(why)) => {
            let status = unlocked(asked, status, console);
            console.err(&format!("gate: remote lfs {} failed: {}", asked.step.word(), escaped(&why)));
            status
        }
        (Ok(Err(why)), Ok(())) | (Ok(Ok(_)), Err(why)) => refused(asked, &why, console),
        (Ok(Err(why)), Err(removal)) => refused(asked, &format!("{why}; and {removal}"), console),
    }
}

/// The archive on stdin unpacked into the held stage `stage` within the budget, once its path is shown to name the
/// held folder, each member checked against the manifest `wanted` ([`checked`]), then installed under the work folder's
/// lock ([`locked`], [`installed`]): the count installed or kept, or why not; `Err` with the status when the install
/// never ran, its lines said.
fn staged(asked: &Asked<'_>, wanted: &[Wanted], seams: &BoxSeams, stage: &Stage, console: &mut Console) -> Result<Result<usize, String>, u32> {
    if let Err(why) = same_stage(stage) {
        return Ok(Err(why));
    }
    let budget = seams.lfs_budget_bytes.unwrap_or(LFS_BUDGET);
    let none = Manifest::default();
    let go_on = || true;
    // Copies come from the archive's own members only: no file of the box's is a copy's source.
    let pc = PcFiles {
        root: Path::new(asked.work),
        manifest: &none,
        go_on: &go_on,
    };
    let entries = match unpack(io::stdin().lock(), &stage.path.join(STAGED_FILES), budget, &pc) {
        Ok(entries) => entries,
        Err(error) => return Ok(Err(error.to_string())),
    };
    if let Err(why) = checked(&entries, wanted) {
        return Ok(Err(why));
    }
    let files = match staged_files(stage) {
        Ok(files) => files,
        Err(why) => return Ok(Err(why)),
    };
    locked(&asked.work_lock(), console, |_, console| installed(asked.bare, &files, &entries, console))
}

/// Whether each member of `entries` is an object of the manifest `wanted`, at most once, whose SHA-256 is its name and
/// whose size is the manifest's; else the refusal naming the first that is not. A member the store already holds,
/// another put having installed it since the diff, is checked as any other and kept at its install.
fn checked(entries: &[Entry], wanted: &[Wanted]) -> Result<(), String> {
    let mut sizes: HashMap<&str, u64> = wanted.iter().map(|object| (object.oid.as_str(), object.size)).collect();
    for entry in entries {
        let Some(size) = sizes.remove(entry.path.as_str()) else {
            return Err(format!(
                "the archive's member '{}' is not an object the manifest names",
                escaped(&entry.path)
            ));
        };
        if entry.sha256 != entry.path {
            return Err(format!(
                "the archive's member '{}' has the SHA-256 {}",
                escaped(&entry.path),
                entry.sha256
            ));
        }
        if entry.size != size {
            return Err(format!(
                "the archive's member '{}' is {} bytes, the manifest's {size}",
                escaped(&entry.path),
                entry.size
            ));
        }
    }
    Ok(())
}

/// Installs each object of `entries`, checked ([`checked`]) and unpacked into the held folder `files`, into the store of
/// `bare` under its lock: each renamed by its handle into its folder without replacing anything there and given that
/// folder's security; an object the store already holds at its size is kept and its staged copy deleted. The store's
/// own lines said on `console`. The count installed or kept; else why not.
fn installed(bare: &str, files: &File, entries: &[Entry], console: &mut Console) -> Result<usize, String> {
    let store = Store::opened(bare, console)?;
    for entry in entries {
        let wanted = Wanted {
            oid: entry.path.clone(),
            size: entry.size,
        };
        installed_one(&store, files, &wanted)?;
    }
    Ok(entries.len())
}

/// Installs the staged object `wanted` from the held folder `files` into `store` ([`installed`]); else why not.
fn installed_one(store: &Store, files: &File, wanted: &Wanted) -> Result<(), String> {
    let failed = |why: String| format!("cannot install the LFS object {}: {why}", wanted.oid);
    let folder = store.folder_of(&wanted.oid)?;
    let source = gate_win32::open_beneath_as(files, &wanted.oid, STAGED_ACCESS, Share::All, Make::Open).map_err(|error| failed(error.to_string()))?;
    plain_copy(&source).map_err(|error| failed(error.to_string()))?;
    match gate_win32::rename_beneath_no_replace(&source, &folder, &wanted.oid) {
        Ok(()) => gate_win32::inherit_security(&source).map_err(|error| failed(error.to_string())),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !Store::holds(&folder, wanted)? {
                return Err(failed("the store holds another file at its name".to_owned()));
            }
            gate_win32::delete_opened(&source).map_err(|error| failed(format!("its staged copy cannot be deleted: {error}")))
        }
        Err(error) => Err(failed(error.to_string())),
    }
}

/// The folder below the box's root that holds each run's LFS scratch folder, `{root}\lfs-run\{key}-{nonce}-{random}`.
const RUN_STAGES: &str = "lfs-run";
/// The folder below the box's root that holds an unelevated box's run scratch folders, which never shares the
/// elevated box's [`RUN_STAGES`], as their folders' security differs.
const RUN_STAGES_MEDIUM: &str = "lfs-run-medium";
/// The `-c` pins every git call of a run that may run the LFS filter takes, whether or not the run sent LFS content: no
/// LFS server, a URL on port 0 of the loopback address in place of any a tracked `.lfsconfig` names, which needs no
/// name lookup, goes through no proxy and nothing can listen on, no other remote searched, no path left out of the
/// smudge, no download error skipped, no standalone transfer agent, and git-lfs's own filter driver, required, whatever
/// the work folder's or the builder's config says of it.
pub(super) const CHECKOUT_PINS: [&str; 20] = [
    "-c",
    "lfs.url=http://127.0.0.1:0/",
    "-c",
    "lfs.remote.searchall=false",
    "-c",
    "lfs.fetchinclude=",
    "-c",
    "lfs.fetchexclude=",
    "-c",
    "lfs.skipdownloaderrors=false",
    "-c",
    "lfs.standalonetransferagent=",
    "-c",
    "filter.lfs.process=git-lfs filter-process",
    "-c",
    "filter.lfs.smudge=git-lfs smudge -- %f",
    "-c",
    "filter.lfs.clean=git-lfs clean -- %f",
    "-c",
    "filter.lfs.required=true",
];

/// The line refusing a run `lfs`, for the run's `nonce`.
pub(super) fn refused_line(nonce: &str) -> String {
    tagged_line(nonce, Refusal::Lfs)
}

/// The line after [`refused_line`] of a run whose LFS content is not what the snapshot of the work folder `key` needs,
/// for `why`.
pub(super) fn not_the_snapshots(key: &str, why: &str) -> String {
    format!("gate: remote lfs of work folder {key} is not the snapshot's: {}", escaped(why))
}

/// The line after [`refused_line`] of a run whose LFS scratch folder could not be made, for `why` ([`NO_SCRATCH`]).
pub(super) fn no_scratch_line(why: &str) -> String {
    format!("{NO_SCRATCH}{}", escaped(why))
}

/// A run's protected scratch folder, `{root}\lfs-run\{key}-{nonce}-{random}` (`lfs-run-medium` on an unelevated box),
/// a random 64-bit value last in [`SCRATCH_SUFFIX_DIGITS`] lower-case hex digits, so a run that reuses a killed box's
/// nonce makes its own and the killed one's waits for the stages' age prune, and no other process can name it before
/// the box makes it; held from its making to its removal, which git-lfs keeps its own objects in (`lfs.storage`), so
/// no clean or smudge of the run reads or writes the work folder's `.git\lfs` or the store.
pub(super) struct Scratch {
    stage: Stage,
    /// `lfs.storage=<the scratch folder>`.
    storage: String,
}

impl Scratch {
    /// The scratch folder of the run of `key` and `nonce` in the work folder `work` (`{root}\work\{key}`), made new
    /// with the box's protected security as a put's stage is ([`made_stage_named`]) and shown to be the folder its
    /// path names; else why not, the folder removed again.
    pub(super) fn made(work: &str, key: &str, nonce: &str, console: &mut Console) -> Result<Scratch, String> {
        // Drawn from the process's randomly seeded hasher, the time and the process id, as the PC's nonces are.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let random = RandomState::new().hash_one((now, std::process::id()));
        let width = SCRATCH_SUFFIX_DIGITS;
        Scratch::made_by(work, key, &format!("{nonce}-{random:0width$x}"), console)
    }

    /// The scratch folder [`Scratch::made`] makes, its name `{key}-{process}`, `process` the run's `{nonce}-{random}`.
    fn made_by(work: &str, key: &str, process: &str, console: &mut Console) -> Result<Scratch, String> {
        let root = grandparent(work).ok_or_else(|| format!(r"the work folder {} is not <root>\work\<key>", escaped(work)))?;
        let elevated = gate_win32::token_facts()
            .map_err(|error| format!("cannot read the box's token: {error}"))?
            .elevated;
        let named = StageNamed {
            root,
            stages: if elevated { RUN_STAGES } else { RUN_STAGES_MEDIUM },
            key,
            nonce: process,
        };
        let stage = made_stage_named(&named, console)?;
        if let Err(why) = same_stage(&stage) {
            return Err(match removed_stage(&stage) {
                Ok(()) => why,
                Err(removal) => format!("{why}; and {removal}"),
            });
        }
        let storage = format!("lfs.storage={}", stage.path.display());
        Ok(Scratch { stage, storage })
    }

    /// `lfs.storage=<the scratch folder>`, a `-c` pin.
    pub(super) fn storage(&self) -> &str {
        &self.storage
    }

    /// Removes the scratch folder beneath its held handle; a removal that fails is said.
    pub(super) fn close(self, console: &mut Console) {
        if let Err(why) = removed_stage(&self.stage) {
            console.err(&format!("gate: remote could not remove the LFS scratch folder: {}", escaped(&why)));
        }
    }
}

/// A run's LFS content, covered and held from the cover check through the back commit: the store's lock taken shared,
/// each store folder on the way to an object held without delete sharing and each object held open to read without
/// delete sharing, so no other process deletes or renames one meanwhile; the snapshot's pointers; and the run's
/// [`Scratch`] folder. A run with no `LfsDigest` holds no lock, no object and no pointer, and its scratch folder only
/// once a repo's snapshot names the LFS filter ([`RunLfs::without_content`], [`RunLfs::with_scratch`]); until then its
/// `lfs.storage` is one that cannot be made ([`RunLfs::pins`]).
pub(super) struct RunLfs {
    _lock: Option<File>,
    held: Vec<File>,
    pub(super) pointers: Vec<Pointer>,
    scratch: Option<Scratch>,
}

impl RunLfs {
    /// The LFS state of a run with no LFS content, before any repo's snapshot is read: no scratch folder yet.
    pub(super) fn without_content() -> RunLfs {
        RunLfs {
            _lock: None,
            held: Vec::new(),
            pointers: Vec::new(),
            scratch: None,
        }
    }

    /// Whether the run has its scratch folder.
    pub(super) fn has_scratch(&self) -> bool {
        self.scratch.is_some()
    }

    /// The run's scratch folder made ([`Scratch::made`]) when it has none yet, for the run of `key` and `nonce` in the
    /// work folder `work`: its empty, protected folder, every later smudge and clean of the run keeping its objects
    /// there, so none reads or writes the work folder's `.git\lfs`, and a file committed as its own bytes under a
    /// `filter=lfs` rule checks out as those bytes. Else why it cannot be made.
    pub(super) fn with_scratch(&mut self, work: &str, (key, nonce): (&str, &str), console: &mut Console) -> Result<(), String> {
        if self.scratch.is_none() {
            self.scratch = Some(Scratch::made(work, key, nonce, console)?);
        }
        Ok(())
    }

    /// The `-c` pins of every git call that may run the LFS filter in the run, its checkouts and its back commit's `add
    /// -A`: [`CHECKOUT_PINS`] and `lfs.storage`, the run's scratch folder, or `no_storage`, an `lfs.storage` that cannot
    /// be made ([`no_storage`]), for a run that has none, so a smudge or a clean there fails rather than reading or
    /// writing the work folder's `.git\lfs`.
    pub(super) fn pins<'a>(&'a self, no_storage: &'a str) -> Vec<&'a str> {
        let mut pins = CHECKOUT_PINS.to_vec();
        pins.extend(["-c", self.scratch.as_ref().map_or(no_storage, Scratch::storage)]);
        pins
    }

    /// Lets the objects go first, as the scratch folder's links to them cannot be deleted while they are held, then
    /// removes the scratch folder ([`Scratch::close`]) and lets the store's lock go.
    pub(super) fn close(self, console: &mut Console) {
        let RunLfs { _lock, held, scratch, .. } = self;
        drop(held);
        if let Some(scratch) = scratch {
            scratch.close(console);
        }
    }
}

/// `lfs.storage=<lock>\lfs`, below the run's held lock file `lock`, a folder git-lfs cannot make: the `lfs.storage` of
/// a run with no scratch folder ([`RunLfs::pins`]).
pub(super) fn no_storage(lock: &str) -> String {
    format!(r"lfs.storage={lock}\lfs")
}

/// What a run with no scratch folder says when its back commit's `add -A` fails and its tree's attributes name the LFS
/// filter: a git-lfs clean found no storage it could make ([`no_storage`]).
pub(super) const NO_STORAGE_ADD: &str =
    "the command added a file under the LFS filter, which a run without LFS content cannot bring back; run it with --local";

/// What a run with no scratch folder says when a checkout fails and its tree's attributes name the LFS filter: a
/// git-lfs smudge found no storage it could make ([`no_storage`]), for a file the snapshot's listing did not see.
pub(super) const NO_STORAGE_CHECKOUT: &str =
    "a file under the LFS filter was found that the run's snapshot does not declare in a tracked .gitattributes; run it with --local";

/// The LFS content of a run that sent the `LfsDigest` `sent`, checked against the snapshot's pointers `listed` as the
/// cover check requires it: the digest must be that of the manifest of the objects the pointers name, and each object a
/// plain file of its pointer's size in the store of the bare repository `bare`, opened without following a link and
/// held ([`RunLfs`]), no scratch folder made yet ([`RunLfs::with_scratch`]). Else why the run is refused `lfs`: a
/// digest with no pointers, a digest that differs, an object missing or not that file, an unelevated box, or a store
/// that cannot be readied.
pub(super) fn covered(bare: &str, sent: &str, listed: &Listed) -> Result<RunLfs, String> {
    if listed.pointers.is_empty() {
        return Err("the run sent an LfsDigest, and its snapshot holds no LFS pointer".to_owned());
    }
    let digest = manifest_digest(&listed.objects)?;
    if digest != sent {
        return Err(format!("the snapshot's LFS manifest has the digest {digest}, not the run's {sent}"));
    }
    elevation()?;
    let bare_held = opened_bare(bare)?;
    let lock = store_lock(&bare_held, bare)?;
    let held = held_objects(bare_held, bare, &listed.objects)?;
    Ok(RunLfs {
        _lock: Some(lock),
        held,
        pointers: listed.pointers.clone(),
        scratch: None,
    })
}

/// Each object of `objects` in the store of the bare repository `bare` opened beneath its held store folder to read its
/// data (`FILE_READ_DATA`) and shared without delete ([`Share::NoDelete`]), with every store folder on its way held
/// too, so none is deleted or renamed while held ([`held_folder`], [`held_object`]); [`RunLfs::close`] drops these
/// handles before it removes the run's scratch folder. Else why not, an object missing among them.
fn held_objects(bare_held: File, bare: &str, objects: &[Object]) -> Result<Vec<File>, String> {
    let missing = |object: &Object| format!("the LFS store holds no object {} for {}", object.oid, escaped(&object.path));
    let Some(first) = objects.first() else {
        return Ok(vec![bare_held]);
    };
    let lfs_path = Path::new(bare).join("lfs");
    let lfs = held_folder(&bare_held, &lfs_path, "lfs")?.ok_or_else(|| missing(first))?;
    let objects_path = lfs_path.join("objects");
    let folder = held_folder(&lfs, &objects_path, "objects")?.ok_or_else(|| missing(first))?;
    let mut held = vec![bare_held, lfs];
    for object in objects {
        let (aa, bb) = (&object.oid[..2], &object.oid[2..4]);
        let Some(outer) = held_folder(&folder, &objects_path.join(aa), aa)? else {
            return Err(missing(object));
        };
        let Some(inner) = held_folder(&outer, &objects_path.join(aa).join(bb), bb)? else {
            return Err(missing(object));
        };
        let file = held_object(&inner, object)?.ok_or_else(|| missing(object))?;
        held.extend([outer, inner, file]);
    }
    held.push(folder);
    Ok(held)
}

/// The store folder `name`, whose path is `path`, beneath the held folder `parent`, opened as itself and held without
/// delete sharing: a folder of its own with the security the box makes the store's folders with. `None` when nothing
/// has the name; else why not.
fn held_folder(parent: &File, path: &Path, name: &str) -> Result<Option<File>, String> {
    let cannot = |error: io::Error| format!("cannot open the LFS store folder {}: {error}", path.display());
    let found = match gate_win32::open_beneath_as(parent, name, FILE_GENERIC_READ, Share::NoDelete, Make::Open) {
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cannot(error)),
    };
    if is_folder_itself(&found).map_err(cannot)? && gate_win32::is_protected_folder(&found).map_err(cannot)? {
        Ok(Some(found))
    } else {
        Err(format!("the LFS store folder {} is not one the box made", path.display()))
    }
}

/// The object `object` beneath its held store folder `folder`, opened as itself to read and held without delete
/// sharing, so no other process deletes or renames it, nor deletes any other name of it, while held (an open that asks
/// for no more than attributes would hold nothing, as the system checks no sharing for it): a plain file, not a link
/// or a junction, of its pointer's size. A second name of the file, as git-lfs links an object into a run's scratch
/// folder, is taken: a link shares the object's security and needs write access to make. `None` when nothing has its
/// name; else why it is not that file.
fn held_object(folder: &File, object: &Object) -> Result<Option<File>, String> {
    let file = match gate_win32::open_beneath_as(folder, &object.oid, FILE_READ_ATTRIBUTES | FILE_READ_DATA, Share::NoDelete, Make::Open) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot open the LFS object {}: {error}", object.oid)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot read the LFS object {}: {error}", object.oid))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(format!("the LFS object {} is a link, a junction or not a file", object.oid));
    }
    if metadata.len() != object.size {
        return Err(format!(
            "the LFS object {} holds {} bytes, not the {} its pointer {} names",
            object.oid,
            metadata.len(),
            object.size,
            escaped(&object.path)
        ));
    }
    Ok(Some(file))
}

/// The pointers of `pointers` whose path in the tree `tree` is not a plain file of the pointer's size, or is one of at
/// most [`POINTER_MOST`] bytes holding that very pointer, its oid and size (an object of 128 bytes has a pointer of its
/// own size), in their order: the pointer's own text, a missing file, an unreadable one or a link among them. An object
/// whose content is pointer text naming another object is its content.
pub(super) fn wrong_files<'p>(tree: &str, pointers: &'p [Pointer]) -> Vec<&'p Pointer> {
    pointers
        .iter()
        .filter(|pointer| is_wrong_file(&tree_path(tree, &pointer.path), pointer))
        .collect()
}

/// Whether the file `path` is not a plain file of `pointer`'s size, or is a small one whose text is `pointer` itself
/// ([`wrong_files`]).
fn is_wrong_file(path: &Path, pointer: &Pointer) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return true;
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || metadata.len() != pointer.size {
        return true;
    }
    if pointer.size > POINTER_MOST {
        return false;
    }
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| file.take(POINTER_MOST + 1).read_to_end(&mut bytes));
    read.is_err() || parsed_pointer(&bytes).is_some_and(|(oid, size)| oid == pointer.oid && size == pointer.size)
}

/// Deletes the file of each of `wrong` in the tree `tree`, one already gone taken; else why one could not be deleted.
pub(super) fn deleted_files(tree: &str, wrong: &[&Pointer]) -> Result<(), String> {
    for pointer in wrong {
        match fs::remove_file(tree_path(tree, &pointer.path)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(format!("cannot delete {} to check it out again: {error}", escaped(&pointer.path)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The path in the tree `tree` of the `/`-separated `path`.
fn tree_path(tree: &str, path: &str) -> PathBuf {
    Path::new(tree).join(path.replace('/', "\\"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its fixture must fail loudly")]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::{CHECKOUT_PINS, Object, Pointer, Scratch, held_object, inherits_only, refuses_unelevated, wrong_files};
    use crate::boxside::inputs::is_stage_name;
    use crate::boxside::preflight::GIT_REDIRECTS;
    use crate::console::Console;

    /// A run that reuses the nonce of a box killed before it removed its scratch folder makes a scratch folder of its
    /// own beside it, a random suffix telling the two apart, and the stages' age prune knows both names.
    #[test]
    fn two_scratch_folders_of_one_key_and_nonce_are_each_made() {
        let root = folder("scratch-random");
        let work = root.join("work").join("k");
        fs::create_dir_all(&work).expect("the work folder can be made");
        let work = work.to_str().expect("the folder is Unicode");
        let nonce = "5b".repeat(16);
        let mut console = Console::new();

        let killed = Scratch::made(work, "k", &nonce, &mut console).expect("the first box makes its scratch");
        let next = Scratch::made(work, "k", &nonce, &mut console).expect("the next box makes its own");

        assert_ne!(killed.storage(), next.storage(), "two folders");
        for scratch in [&killed, &next] {
            let name = std::path::Path::new(scratch.storage())
                .file_name()
                .expect("a name")
                .to_str()
                .expect("Unicode");
            assert!(is_stage_name(name), "the age prune knows the scratch name {name}");
        }
        assert!(is_stage_name(&format!("k-{nonce}")), "and a put's stage name");
        assert!(!is_stage_name(&format!("k-{nonce}-101")), "a process id is not a scratch suffix");
        assert!(!is_stage_name(&format!("k-{nonce}-{}", "0a".repeat(7))), "nor 14 hex digits");
        assert!(!is_stage_name(&format!("k-{nonce}-{}", "0A".repeat(8))), "nor upper-case hex");
        next.close(&mut console);
        killed.close(&mut console);
    }

    /// A folder of its own for the test `name`, empty.
    fn folder(name: &str) -> std::path::PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-box-lfs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    #[test]
    fn a_held_object_cannot_be_removed_or_renamed_until_let_go() {
        let folder = folder("held");
        let oid = "ef".repeat(32);
        fs::write(folder.join(&oid), b"content").expect("the object can be written");
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&folder)
            .expect("the folder can be opened");
        let object = Object {
            oid: oid.clone(),
            size: 7,
            path: "a.bin".to_owned(),
        };
        let held = held_object(&opened, &object).expect("the object is read").expect("the object is there");

        assert!(fs::remove_file(folder.join(&oid)).is_err(), "a held object is not removed");
        assert!(
            fs::rename(folder.join(&oid), folder.join("moved")).is_err(),
            "a held object is not renamed"
        );
        drop(held);
        fs::rename(folder.join(&oid), folder.join("moved")).expect("an object let go is renamed");
        fs::remove_file(folder.join("moved")).expect("an object let go is removed");
    }

    #[test]
    fn every_filter_call_pins_the_lfs_driver_and_a_url_nothing_listens_on() {
        let pins: Vec<&str> = CHECKOUT_PINS.chunks(2).map(|pair| pair[1]).collect();
        assert!(CHECKOUT_PINS.chunks(2).all(|pair| pair[0] == "-c"), "{CHECKOUT_PINS:?}");
        for pin in [
            "lfs.url=http://127.0.0.1:0/",
            "lfs.remote.searchall=false",
            "lfs.fetchinclude=",
            "lfs.fetchexclude=",
            "lfs.skipdownloaderrors=false",
            "lfs.standalonetransferagent=",
            "filter.lfs.process=git-lfs filter-process",
            "filter.lfs.smudge=git-lfs smudge -- %f",
            "filter.lfs.clean=git-lfs clean -- %f",
            "filter.lfs.required=true",
        ] {
            assert!(pins.contains(&pin), "{pin} in {pins:?}");
        }
        for name in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_LFS_SKIP_SMUDGE",
            "GIT_LFS_SKIP_DOWNLOAD_ERRORS",
        ] {
            assert!(GIT_REDIRECTS.contains(&name), "{name} in {GIT_REDIRECTS:?}");
        }
    }

    #[test]
    fn only_an_object_whose_security_is_all_inherited_is_protected() {
        let inherited = "D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)S:AI(ML;ID;NW;;;HI)";
        assert!(inherits_only(inherited), "all handed down");
        assert!(!inherits_only("D:PAI(A;ID;FA;;;SY)S:AI(ML;ID;NW;;;HI)"), "a protected DACL");
        assert!(!inherits_only("D:AI(A;ID;FA;;;SY)(A;;FA;;;WD)S:AI(ML;ID;NW;;;HI)"), "a grant of its own");
        assert!(!inherits_only("D:AI(A;ID;FA;;;SY)"), "no label");
        assert!(!inherits_only("D:AI(A;ID;FA;;;SY)S:(ML;;NW;;;ME)"), "a label of its own");
        assert!(!inherits_only("D:NO_ACCESS_CONTROLS:AI(ML;ID;NW;;;HI)"), "a null DACL");
    }

    #[test]
    fn a_small_file_of_its_pointer_s_size_holding_pointer_text_is_wrong() {
        let folder = folder("pointer");
        let tree = folder.to_str().expect("the folder is Unicode");
        let oid = "12".repeat(32);
        let text = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 128\n");
        assert_eq!(text.len(), 128, "a 128-byte object's pointer is 128 bytes");
        fs::write(folder.join("pointer.bin"), &text).expect("the pointer text can be written");
        fs::write(folder.join("content.bin"), vec![b'x'; 128]).expect("the content can be written");
        let pointer = |path: &str| Pointer {
            path: path.to_owned(),
            oid: oid.clone(),
            size: 128,
        };
        let pointers = [pointer("pointer.bin"), pointer("content.bin")];

        let wrong = wrong_files(tree, &pointers);

        assert_eq!(wrong, [&pointers[0]], "only the pointer text is wrong");
    }

    #[test]
    fn an_object_with_a_second_hard_link_is_held() {
        let folder = crate::test_support::temp_dir().join(format!("gate-box-lfs-{}-linked", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the folder can be made");
        let oid = "ab".repeat(32);
        fs::write(folder.join(&oid), b"content").expect("the object can be written");
        fs::hard_link(folder.join(&oid), folder.join("second")).expect("a second name can be made");
        let held = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&folder)
            .expect("the folder can be opened");
        let object = |size| Object {
            oid: oid.clone(),
            size,
            path: "a.bin".to_owned(),
        };

        assert!(matches!(held_object(&held, &object(7)), Ok(Some(_))), "an object of two names is taken");
        let wrong = held_object(&held, &object(8)).expect_err("another size is refused");
        assert!(wrong.contains("holds 7 bytes"), "{wrong}");
        let none = Object {
            oid: "cd".repeat(32),
            ..object(7)
        };
        assert!(matches!(held_object(&held, &none), Ok(None)), "nothing at the name is missing");
        drop(File::open(folder.join("second")).expect("the second name is still there"));
    }

    #[test]
    fn a_build_without_test_seams_refuses_an_unelevated_box() {
        assert!(refuses_unelevated(false, false, None), "no seams build: refused");
        assert!(
            refuses_unelevated(false, false, Some("anything")),
            "no seams build: the variable is not read"
        );
        assert!(!refuses_unelevated(true, false, None), "an elevated box is taken");
        assert!(!refuses_unelevated(false, true, None), "a seams build takes it unless told to check");
        assert!(refuses_unelevated(false, true, Some("check")), "a seams build told to check refuses it");
    }
}

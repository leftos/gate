//! `gate-box box inputs`: a routed run's ignored inputs, sent from the PC before the run, answered as the Windows box
//! answers them, line for line. The request names the `Step`: `diff` reads the PC's inputs manifest and says which
//! files the work folder lacks or holds otherwise, and `put` reads the manifest and then an archive of those files, in
//! the outputs archive's format, to the end of stdin, and installs them. A put unpacks its archive into a stage of its
//! own before it takes any lock; then both take the work folder's lock as a run does, refuse `busy` while an earlier
//! run's recorded group still runs, and lay the work folder out as a run of one repo does, so a later run finds the
//! layout unchanged and clears nothing.
//!
//! What a put installed is kept in the inputs record `{Work}.inputs`, beside the layout record: the manifest's digest,
//! and per file its path, size, SHA-256, and the mtime in nanoseconds and `{dev}-{ino}` the box left it with. A diff
//! trusts a copy whose size, mtime and id the record holds, a negative mtime never trusted, and hashes any other; one
//! that needs nothing finishes as a put of nothing would. A put deletes a recorded copy that left the manifest only
//! while it still holds what was installed, keeps any other in the record as a kept copy, rewrites the tree's
//! `.git/info/exclude` from the record, so no input is cleaned by the checkout or committed back, and writes the
//! record's digest last, only once the work folder holds every file of the manifest.
//!
//! A run whose payload carries `InputsDigest` checks the record once its work folder is laid out and before its
//! checkout ([`unmatched`]): the record must hold that digest and every recorded copy the stamp it was installed with,
//! and the exclude file is rewritten from it; else the run is refused `inputs` and the PC runs the command itself.
//!
//! The command the box runs, and anything it leaves running, is the box's own user and may rename, link or rewrite
//! anything in the work folder, so every name below the work folder or a stage is reached beneath its folder's
//! descriptor, one name at a time, a link never followed ([`crate::beneath`]); a file below the work folder arrives by
//! a rename over its name and leaves by an unlink, never written through.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gate_args::BoxSeams;
use gate_inputs::archive::{
    CopyFrom, Entry, FolderBudget, Limits, MEMBER_LIMIT, Place, UnpackError, copy_mismatch, extract, no_pc_file, outside_destination,
};
use gate_inputs::hash::{Hashing, sha256_of};
use gate_inputs::manifest::{Held, HeldBy, MANIFEST_LIMIT, Manifest, read_sent, refused_path};
use gate_wire::{
    OUTPUTS_NONCE_DIGITS, PathForm, Payload, Refusal, SHA256_DIGITS, busy_line, escaped, is_lower_hex, layout_line, ordinal_upper, refused_line,
    trees,
};
use serde_json::{Map, Value, json};

use crate::beneath::{self, Folder, NOT_PRIVATE};
use crate::console::Console;
use crate::files::{self, HeldLock, Locked};
use crate::git::Git;
use crate::lines::{FAILED_STATUS, USAGE_STATUS};
use crate::preflight::checked_root;
use crate::request::{Unread, bounded_line};
use crate::run::{self, Laying, NotLaidOut, REFUSED_STATUS, Repo, Unbuilt};
use crate::stop::{records_folder, unusable_refusal};

/// The bytes in one gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most bytes one put unpacks, decompressed: 4 GiB, unless the `--test-inputs-budget-bytes` seam says otherwise.
const INPUTS_BUDGET: u64 = 4 * GIB;
/// The folder below the box's root that holds each put's stage and its lock file.
const STAGES: &str = "inputs-stage";
/// How old a stage's lock file is, in seconds, before a put removes the stage when no live put holds it: a day.
const STAGE_AGE_SECONDS: i64 = 24 * 60 * 60;
/// The line refusing a request that names a sibling pair.
const PAIR_REFUSED: &str = "gate box: inputs for a sibling pair are not taken yet";
/// The most bytes the inputs record may hold, read or written: twice the manifest's limit, 128 MiB.
const RECORD_CAP: u64 = 2 * MANIFEST_LIMIT as u64;
/// The most names `{name}.gate-moved-{n}` a folder at the exclude file's name is tried at.
const MOVED_ASIDE_LIMIT: u32 = 100;
/// The nanoseconds in a second.
const NANOS: i64 = 1_000_000_000;
/// The mtime a stamp holds for a file whose mtime in nanoseconds the box cannot say: negative, so never trusted.
const NO_STAMP: i64 = -1;
/// Why a copy, a target or a staged file is not one the box takes.
const NOT_PLAIN_COPY: &str = "it is a link or not a plain file of one name";

/// How many temp files this process has made for the exclude file, so two writes never pick one name.
static TEMPS: AtomicU64 = AtomicU64::new(0);

/// The step a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// What an inputs request reads of its payload.
struct Asked<'a> {
    key: &'a str,
    work: &'a str,
    bare: &'a str,
    lock: &'a str,
    nonce: &'a str,
    step: Step,
    /// The box's root, `{root}/work/{key}` being the work folder, which the stages folder sits in.
    root: String,
    /// The folder holding the work folders, `{root}/work`.
    parent: &'a Path,
    /// Whether the payload sets `SentBin`, the PC's build of one project, which this builder does not take.
    sent_bin: bool,
}

impl<'a> Asked<'a> {
    /// The request `payload` holds, read in the Windows box's order; else the usage line. A request naming a sibling
    /// pair is refused first; the work root and `Bare` are judged as a run's are ([`checked_root`]); `SentBin` is
    /// noted last, where the Windows box reads it.
    fn read(payload: &'a Payload) -> Result<Asked<'a>, String> {
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
        let (root, parent) = checked_root(work, bare, false)?;
        Ok(Asked {
            key,
            work,
            bare,
            lock,
            nonce,
            step,
            root,
            parent,
            sent_bin: payload.sets("SentBin"),
        })
    }

    /// The line saying the step failed for `why`.
    fn failed(&self, why: &str) -> String {
        format!("gate: remote inputs {} failed: {}", self.step.word(), escaped(why))
    }

    /// The inputs record, `{work}.inputs`.
    fn record(&self) -> String {
        format!("{}.inputs", self.work)
    }
}

/// The PC's inputs manifest, and the SHA-256 of its line as received, without its line end.
struct Sent {
    manifest: Manifest,
    digest: String,
}

/// A request and the manifest it sent, as the steps under the lock take them.
#[derive(Clone, Copy)]
struct Job<'j> {
    asked: &'j Asked<'j>,
    sent: &'j Sent,
}

/// The manifest line read from `input`, up to its first newline or the input's end and never more than
/// [`MANIFEST_LIMIT`] bytes ([`read_sent`]); else why it cannot be read, a path in it the box does not take
/// ([`refused_path`]) among them.
fn read_manifest(input: &mut impl BufRead) -> Result<Sent, String> {
    let line = match bounded_line(&mut *input, MANIFEST_LIMIT as u64) {
        Ok(line) => line,
        Err(Unread::OverLimit) => return Err("the inputs manifest reaches 64 MiB without a newline".to_owned()),
        Err(Unread::Failed(error)) => return Err(format!("the inputs manifest could not be read: {error}")),
    };
    let (manifest, digest) = read_sent(line)?;
    Ok(Sent { manifest, digest })
}

/// Answers `gate-box box inputs`: the request read, then the manifest from stdin, before any lock is taken, so a client
/// that stalls holds nothing; then the work root, `<root>/work` and the bare repository judged as a run judges them
/// ([`root_checked`]); then the step. A payload refused is a usage error (2), a `SentBin` the builder does not take, a
/// held lock, a live earlier group or a layout refusal is refused (75), a manifest that cannot be read fails the step
/// (1), and a root or folder the box does not use says the run's line and status.
#[must_use]
pub fn inputs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u8 {
    answered(payload, seams, &mut io::stdin().lock(), console)
}

/// [`inputs`], the manifest and a put's archive read from `input`.
fn answered(payload: &Payload, seams: &BoxSeams, input: &mut impl BufRead, console: &mut Console) -> u8 {
    let asked = match Asked::read(payload) {
        Ok(asked) => asked,
        Err(line) => {
            console.err(&line);
            return USAGE_STATUS;
        }
    };
    if asked.sent_bin {
        let unbuilt = Unbuilt {
            refusal: Refusal::Sent,
            key: "SentBin",
        };
        console.err(&refused_line(asked.nonce, unbuilt.refusal));
        console.err(&unbuilt.line(asked.bare));
        return REFUSED_STATUS;
    }
    let sent = match read_manifest(input) {
        Ok(sent) => sent,
        Err(why) => return failed(&asked, &why, console),
    };
    if let Err((line, status)) = root_checked(&asked) {
        console.err(&line);
        return status;
    }
    match asked.step {
        Step::Diff => match locked(&asked, console, |work, lock, console| diff(&asked, &sent, work, lock, console)) {
            Ok(Ok(())) => 0,
            Ok(Err(why)) => failed(&asked, &why, console),
            Err(status) => status,
        },
        Step::Put => put(&asked, &sent, seams, input, console),
    }
}

/// Checks, making nothing and before the lock, what the step lies in, as a run's `root_checked` does: the work root
/// trusted and `<root>/work` the box user's own folder when they are there ([`records_folder`]), and the bare
/// repository the box user's own folder when it is there ([`files::unusable_folder`]). Else the line and the status.
fn root_checked(asked: &Asked<'_>) -> Result<(), (String, u8)> {
    records_folder(&asked.root, asked.parent)?;
    files::unusable_folder(Path::new(asked.bare), false).map_or(Ok(()), |flaw| Err(unusable_refusal(flaw, false)))
}

/// Says the step failed for `why`; 1.
fn failed(asked: &Asked<'_>, why: &str, console: &mut Console) -> u8 {
    console.err(&asked.failed(why));
    FAILED_STATUS
}

/// Says the work folder of `asked` is busy: the refusal tag, then the busy line; 75.
fn busy(asked: &Asked<'_>, console: &mut Console) -> u8 {
    console.err(&refused_line(asked.nonce, Refusal::Busy));
    console.err(&busy_line(asked.key));
    REFUSED_STATUS
}

/// `step` run on the work folder, held, under its lock: the lock taken as a run takes it, a held lock refused `busy`
/// (75), and the work folder laid out as a run of one repo lays it out ([`laid_out`]) and held through the step. `Err`
/// with the status to answer when the step never ran, its lines said.
fn locked<T>(
    asked: &Asked<'_>,
    console: &mut Console,
    step: impl FnOnce(&Work, &HeldLock, &mut Console) -> Result<T, String>,
) -> Result<Result<T, String>, u8> {
    let lock = match files::take_lock(Path::new(asked.lock)) {
        Ok(Locked::Held(lock)) => lock,
        Ok(Locked::Busy) => return Err(busy(asked, console)),
        Err(error) => return Err(not_laid_out(asked, run::lock_untaken(&error, asked.lock), console)),
    };
    let work = laid_out(asked).map_err(|not| not_laid_out(asked, not, console))?;
    Ok(step(&work, &lock, console))
}

/// Lays the work folder of `asked` out as a run of one repo does, under its lock: an earlier run's live group refused
/// `busy` before git's lock files are swept ([`run::earlier_group_cleared`], [`run::swept`]), the layout checked and
/// the tree laid out with the preflight's plain git ([`run::layout_checked`], [`run::trees_laid_out`]); then the work
/// folder held beneath its judged root ([`Work::held`]). Else why not.
fn laid_out(asked: &Asked<'_>) -> Result<Work, NotLaidOut> {
    let (tree, _) = trees(asked.work, None, PathForm::Unix);
    let main = Repo {
        name: None,
        tree,
        bare: asked.bare.to_owned(),
        snapshot: "",
    };
    let laying = Laying {
        work: asked.work,
        parent: asked.parent,
        key: asked.key,
        main: &main,
        siblings: &[],
    };
    run::earlier_group_cleared(&laying)?;
    run::swept(&laying)?;
    let checked = run::layout_checked(&laying)?;
    run::trees_laid_out(&laying, &Git::of(&asked.root, asked.parent), &checked)?;
    Work::held(asked.work).map_err(|error| NotLaidOut::Refused(format!("cannot open {}: {error}", asked.work)))
}

/// Says why the work folder of `asked` could not be laid out: `busy`, or `layout` with the layout line naming the repo,
/// each after its refusal tag, or a step's lines; the status.
fn not_laid_out(asked: &Asked<'_>, not: NotLaidOut, console: &mut Console) -> u8 {
    match not {
        NotLaidOut::Busy => busy(asked, console),
        NotLaidOut::Refused(why) => {
            console.err(&refused_line(asked.nonce, Refusal::Layout));
            console.err(&layout_line(run::repo_name(asked.bare), &escaped(&why)));
            REFUSED_STATUS
        }
        NotLaidOut::Failed(failed) => {
            for line in &failed.lines {
                console.err(line);
            }
            failed.status
        }
    }
}

/// A file's identity: its device and inode.
type Id = (libc::dev_t, libc::ino_t);

/// The work folder, held: `{root}/work` and the work folder reached beneath the judged root, the work folder's path,
/// and the identity of its `.git` folder, which no put's walk may pass through.
struct Work {
    parent: Folder,
    folder: Folder,
    path: String,
    git: Option<Id>,
}

impl Work {
    /// The work folder at `path`, held and judged beneath its judged root and `<root>/work`
    /// ([`beneath::work_and_parent_beneath_root`]), and its `.git` looked at as itself.
    fn held(path: &str) -> io::Result<Work> {
        let (parent, folder) = beneath::work_and_parent_beneath_root(Path::new(path))?;
        let git = match folder.stat_child(OsStr::new(".git")) {
            Ok(stat) => Some(Facts::of(&stat)).filter(Facts::is_folder).map(|facts| (facts.dev, facts.ino)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Work {
            parent,
            folder,
            path: path.to_owned(),
            git,
        })
    }
}

/// What a file is, as a look at it said: its identity, size, mtime, names and kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Facts {
    dev: libc::dev_t,
    ino: libc::ino_t,
    size: libc::off_t,
    mtime: libc::time_t,
    mtime_nsec: libc::c_long,
    nlink: libc::nlink_t,
    kind: libc::mode_t,
}

impl Facts {
    fn of(stat: &libc::stat) -> Facts {
        Facts {
            dev: stat.st_dev,
            ino: stat.st_ino,
            size: stat.st_size,
            mtime: stat.st_mtime,
            mtime_nsec: stat.st_mtime_nsec,
            nlink: stat.st_nlink,
            kind: stat.st_mode & libc::S_IFMT,
        }
    }

    fn is_folder(&self) -> bool {
        self.kind == libc::S_IFDIR
    }

    /// Whether it is a plain file of one name.
    fn is_plain(&self) -> bool {
        self.kind == libc::S_IFREG && self.nlink == 1
    }

    /// Its stamp: its size, its mtime in nanoseconds ([`NO_STAMP`] past what nanoseconds can hold), and its
    /// `{dev}-{ino}`.
    fn stamp(&self) -> Stamp {
        let modified = self
            .mtime
            .checked_mul(NANOS)
            .and_then(|nanos| nanos.checked_add(self.mtime_nsec))
            .unwrap_or(NO_STAMP);
        Stamp {
            size: self.size.unsigned_abs(),
            modified,
            id: format!("{}-{}", self.dev, self.ino),
        }
    }
}

/// A copy's stamp as the box left it: its size, its mtime in nanoseconds, and its `{dev}-{ino}`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: i64,
    id: String,
}

impl Stamp {
    /// Whether a copy found with this stamp is the copy recorded with `recorded`, unhashed: the same stamp, its mtime
    /// not negative, as a mtime before 1970 is no stamp.
    fn trusts(&self, recorded: &Stamp) -> bool {
        self == recorded && self.modified >= 0
    }
}

/// One installed input as the record holds it.
#[derive(Clone, Debug)]
struct Copy {
    path: String,
    sha256: String,
    stamp: Stamp,
}

/// The inputs record: the digest of the manifest last installed from, empty while an install is under way or after
/// one that did not finish; each input installed, by its path's simple uppercase; and each kept stale copy, by the same
/// key, which the exclude file lists, which no sweep deletes and which is never trusted as installed.
#[derive(Clone, Default)]
struct Record {
    digest: String,
    files: BTreeMap<String, Copy>,
    kept: BTreeMap<String, String>,
}

impl Record {
    /// The record `text` holds; else what is wrong with it, a path that is not a plain relative path among them.
    fn parse(text: &str) -> Result<Record, String> {
        let not_a_record = || "is not an inputs record".to_owned();
        let value: Value = serde_json::from_str(text).map_err(|_| not_a_record())?;
        let digest = value.get("digest").and_then(Value::as_str).ok_or_else(not_a_record)?;
        let files = value.get("files").and_then(Value::as_object).ok_or_else(not_a_record)?;
        let mut record = Record {
            digest: digest.to_owned(),
            ..Record::default()
        };
        for (path, file) in files {
            if refused_path(path).is_some() {
                return Err(not_a_record());
            }
            if file.get("kept") == Some(&Value::Bool(true)) {
                record.kept.insert(ordinal_upper(path), path.clone());
                continue;
            }
            let copy = recorded_copy(path, file).ok_or_else(not_a_record)?;
            record.files.insert(ordinal_upper(path), copy);
        }
        Ok(record)
    }

    /// The record's text: one line of JSON, a kept copy's entry `{"kept": true}`.
    fn text(&self) -> String {
        let mut files: Map<String, Value> = self
            .files
            .values()
            .map(|copy| {
                let stamp = &copy.stamp;
                let file = json!({ "size": stamp.size, "sha256": copy.sha256, "modified": stamp.modified, "id": stamp.id });
                (copy.path.clone(), file)
            })
            .collect();
        for path in self.kept.values() {
            files.insert(path.clone(), json!({ "kept": true }));
        }
        json!({ "digest": self.digest, "files": files }).to_string()
    }

    /// Every path the record names, installed or kept, in the order of their keys.
    fn paths(&self) -> Vec<&str> {
        let mut paths: BTreeMap<&str, &str> = self.kept.iter().map(|(key, path)| (key.as_str(), path.as_str())).collect();
        paths.extend(self.files.iter().map(|(key, copy)| (key.as_str(), copy.path.as_str())));
        paths.into_values().collect()
    }

    /// The copy recorded at `path`, found without regard to case.
    fn get(&self, path: &str) -> Option<&Copy> {
        self.files.get(&ordinal_upper(path))
    }
}

/// The copy the record's entry `file` for `path` holds; `None` when it holds none.
fn recorded_copy(path: &str, file: &Value) -> Option<Copy> {
    let sha256 = file.get("sha256")?.as_str().filter(|sha256| is_lower_hex(sha256, SHA256_DIGITS))?;
    Some(Copy {
        path: path.to_owned(),
        sha256: sha256.to_owned(),
        stamp: Stamp {
            size: file.get("size")?.as_u64()?,
            modified: file.get("modified")?.as_i64()?,
            id: file.get("id")?.as_str()?.to_owned(),
        },
    })
}

/// The work folder's inputs record of `asked`, read whole ([`files::read_whole`]). One that is not a record is taken as
/// empty, and one that cannot be read (a link, a folder, a file of more than one name, one past [`RECORD_CAP`], one
/// that is not UTF-8) is removed as itself beneath the held `{root}/work`, never followed, once the lock `lock` is shown
/// still held, and taken as empty, each said; else why such a record could not be removed.
fn recorded(asked: &Asked<'_>, work: &Work, lock: &HeldLock, console: &mut Console) -> Result<Record, String> {
    let path = &asked.record();
    match read_record(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Record::default()),
        Ok(text) => Ok(Record::parse(&text).unwrap_or_else(|why| {
            console.err(&format!("gate: remote inputs record {path} {why}; every copy is hashed"));
            Record::default()
        })),
        Err(error) => {
            lock_held(asked, lock)?;
            let name = Path::new(path).file_name().unwrap_or_default();
            work.parent
                .remove_child_tree(name)
                .map_err(|removal| format!("cannot remove {path}, which cannot be read ({error}): {removal}"))?;
            console.err(&format!(
                "gate: remote removed the inputs record {path}, which cannot be read: {}; every copy is hashed",
                escaped(&error.to_string())
            ));
            Ok(Record::default())
        }
    }
}

/// The inputs record at `path`, read whole as itself ([`files::read_whole`]), at most [`RECORD_CAP`] bytes, and UTF-8;
/// else the system's error, `NotFound` for a record that is not there.
fn read_record(path: &str) -> io::Result<String> {
    files::read_whole(Path::new(path), RECORD_CAP).and_then(|bytes| String::from_utf8(bytes).map_err(|_| io::Error::other("it is not UTF-8")))
}

/// Writes `record` as the record at `path`, whole ([`files::replace_plain`]); else why not.
fn write_record(path: &str, record: &Record) -> Result<(), String> {
    write_record_capped(path, record, RECORD_CAP)
}

/// [`write_record`], a record of more than `cap` bytes never written.
fn write_record_capped(path: &str, record: &Record, cap: u64) -> Result<(), String> {
    let text = record.text();
    if text.len() as u64 > cap {
        return Err(format!("cannot write {path}: the record would be longer than {cap} bytes"));
    }
    files::replace_plain(Path::new(path), text.as_bytes()).map_err(|failure| format!("cannot write {path}: {failure}"))
}

/// Writes the record at `path` with `record`'s files and no digest, before anything below the work folder changes, so
/// no run passes the digest check until the install finishes. Else why not.
fn unsealed(path: &str, record: &Record) -> Result<(), String> {
    write_record(
        path,
        &Record {
            digest: String::new(),
            ..record.clone()
        },
    )
}

/// Whether the work lock `lock` of `asked` still names the file this box locked; else why not.
fn lock_held(asked: &Asked<'_>, lock: &HeldLock) -> Result<(), String> {
    if matches!(lock.still_held(), Ok(true)) {
        Ok(())
    } else {
        Err(format!("the work folder lock {} is no longer held", asked.lock))
    }
}

/// Where a walk of a `/`-separated path's folders below a held folder ended ([`walk`]).
enum Walked<'p> {
    /// A folder on the way is missing.
    Missing,
    /// The part of the path `prefix` is a file, a link or anything but a folder: its name `name` in `folder`.
    Blocked { prefix: &'p str, folder: Folder, name: &'p str },
    /// Every folder on the way is a folder itself: the last one, held, and the file's name.
    Reached(Folder, &'p str),
}

/// Walks the folders of the `/`-separated `path` below the held folder `top`, each opened as itself beneath the one
/// before, never through a link, each made when missing when `make`; a folder on the way whose identity is `avoid`
/// (the work folder's `.git`, however spelt) refused. Else the system's error.
fn walk<'p>(top: &Folder, path: &'p str, make: bool, avoid: Option<Id>) -> io::Result<Walked<'p>> {
    let (parents, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    let mut folder = top.duplicate()?;
    let mut end = 0;
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        end += part.len() + 1;
        let opened = match child_folder(&folder, OsStr::new(part), make) {
            Ok(opened) => opened,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Walked::Missing),
            Err(error) if is_not_folder(&error) => {
                let prefix = &path[..end - 1];
                return Ok(Walked::Blocked { prefix, folder, name: part });
            }
            Err(error) => return Err(error),
        };
        let facts = Facts::of(&opened.stat()?);
        if avoid == Some((facts.dev, facts.ino)) {
            return Err(io::Error::other(format!("'{}' is the work folder's .git", &path[..end - 1])));
        }
        folder = opened;
    }
    Ok(Walked::Reached(folder, leaf))
}

/// The folder `name` in `folder`, opened as itself; made, 0700, when missing and `make`.
fn child_folder(folder: &Folder, name: &OsStr, make: bool) -> io::Result<Folder> {
    match folder.open_child(name) {
        Err(error) if make && error.kind() == io::ErrorKind::NotFound => match folder.make_child(name) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => folder.open_child(name),
            made => made,
        },
        opened => opened,
    }
}

/// Whether a folder's open failed as the name is a link or no folder (`ELOOP`, `ENOTDIR`).
fn is_not_folder(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

/// The error saying the part `prefix` of a path is no folder of its own.
fn not_a_folder(prefix: &str) -> io::Error {
    io::Error::other(format!("'{prefix}' is a link or not a folder"))
}

/// Whether `facts` say a plain file of one name; else [`NOT_PLAIN_COPY`].
fn plain_copy(facts: &Facts) -> io::Result<()> {
    if facts.is_plain() {
        Ok(())
    } else {
        Err(io::Error::other(NOT_PLAIN_COPY))
    }
}

/// A copy below the work folder, open to read: its folder, held, its name there, the file, and what it is.
struct Opened {
    folder: Folder,
    leaf: String,
    file: File,
    facts: Facts,
}

/// The copy at the `/`-separated `path` below the held work folder `work`, opened to read as itself through its
/// folders ([`walk`]): a plain file of one name ([`plain_copy`]). `None` when nothing is at its path; else why it is no
/// such file.
fn opened_copy(work: &Work, path: &str) -> io::Result<Option<Opened>> {
    let (folder, leaf) = match walk(&work.folder, path, false, work.git)? {
        Walked::Missing => return Ok(None),
        Walked::Blocked { prefix, .. } => return Err(not_a_folder(prefix)),
        Walked::Reached(folder, leaf) => (folder, leaf),
    };
    let file = match folder.open_file(OsStr::new(leaf)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened?,
    };
    let facts = Facts::of(&beneath::file_stat(&file)?);
    plain_copy(&facts)?;
    Ok(Some(Opened {
        folder,
        leaf: leaf.to_owned(),
        file,
        facts,
    }))
}

/// The copy at `path` as the record would hold it, under the manifest's spelling of `path`, when it holds what the
/// manifest's `held` says: the record's own entry for a copy whose stamp it trusts ([`Stamp::trusts`]), unhashed, else
/// the copy's hash and stamp read now. `None` for a copy missing, that is no plain file, that cannot be read, or that
/// holds anything else.
fn matched_copy(work: &Work, path: &str, held: &Held, record: &Record) -> Option<Copy> {
    let opened = opened_copy(work, path).ok().flatten()?;
    let stamp = opened.facts.stamp();
    if stamp.size != held.stamp.size {
        return None;
    }
    if let Some(copy) = record.get(path).filter(|copy| stamp.trusts(&copy.stamp)) {
        return (copy.sha256 == held.sha256).then(|| Copy {
            path: path.to_owned(),
            ..copy.clone()
        });
    }
    let (sha256, _) = sha256_of(&opened.file).ok()?;
    (sha256 == held.sha256).then(|| Copy {
        path: path.to_owned(),
        sha256,
        stamp,
    })
}

/// The diff: `gate-remote-inputs: need <n>`, then `gate-remote-inputs: path <path>` for each file of the manifest the
/// work folder `work` lacks or holds otherwise, in the manifest's order, each path escaped. One that needs nothing first
/// finishes the install as a put of nothing would ([`swept`], [`finished`]), from the copies this diff matched, each
/// untrusted one hashed once, under the lock `lock` still held. Else why not.
fn diff(asked: &Asked<'_>, sent: &Sent, work: &Work, lock: &HeldLock, console: &mut Console) -> Result<(), String> {
    let book = asked.record();
    let record = recorded(asked, work, lock, console)?;
    let mut now = Record::default();
    let mut needed = Vec::new();
    for (path, held) in sent.manifest.files() {
        match matched_copy(work, path, held, &record) {
            Some(copy) => {
                now.files.insert(ordinal_upper(path), copy);
            }
            None => needed.push(path),
        }
    }
    if needed.is_empty() {
        lock_held(asked, lock)?;
        unsealed(&book, &record)?;
        now.kept = swept(work, &record, &sent.manifest, &held_ids(work, &sent.manifest), console);
        for line in finished(Job { asked, sent }, work, &record, now, lock)? {
            console.err(&line);
        }
    }
    console.out(&format!("gate-remote-inputs: need {}", needed.len()));
    for path in needed {
        console.out(&format!("gate-remote-inputs: path {}", escaped(path)));
    }
    Ok(())
}

/// The put: its stage made ([`made_stage`]), the archive on `input` unpacked into it before any lock is taken, then
/// under the lock its files moved into the work folder ([`installed`]); the stage and its lock file removed on every way
/// out once made ([`Stage::removed`]), and `gate-remote-inputs: installed <n>` said; 0. Else the status, its lines said.
fn put(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, input: &mut impl BufRead, console: &mut Console) -> u8 {
    let stage = match made_stage(asked, SystemTime::now(), console) {
        Ok(made) => made,
        Err(why) => return failed(asked, &why, console),
    };
    let done = staged(Job { asked, sent }, seams, input, &stage, console);
    let removal = stage.removed();
    drop(stage);
    match (done, removal) {
        (Ok(Ok(count)), Ok(())) => {
            console.out(&format!("gate-remote-inputs: installed {count}"));
            0
        }
        (Err(status), Ok(())) => status,
        (Err(status), Err(why)) => {
            console.err(&asked.failed(&why));
            status
        }
        (Ok(Err(why)), Ok(())) | (Ok(Ok(_)), Err(why)) => failed(asked, &why, console),
        (Ok(Err(why)), Err(removal)) => failed(asked, &format!("{why}; and {removal}"), console),
    }
}

/// The archive on `input` unpacked into the held stage `stage` within the budget ([`Staging`]), then its files
/// installed under the work folder's lock ([`locked`], [`installed`]): the count installed, or why not; `Err` with the
/// status when the install never ran, its lines said.
fn staged(job: Job<'_>, seams: &BoxSeams, input: &mut impl BufRead, stage: &Stage, console: &mut Console) -> Result<Result<usize, String>, u8> {
    let limits = Limits {
        bytes: seams.inputs_budget_bytes.unwrap_or(INPUTS_BUDGET),
        files: MEMBER_LIMIT,
    };
    let mut place = Staging::new(&stage.folder, &stage.path);
    // Copies come from the archive's own members only: no file of the box's is a copy's source.
    let entries = match extract(&mut *input, limits, &HeldBy::new(), &|| true, &mut place) {
        Ok(entries) => entries,
        Err(error) => return Ok(Err(error.to_string())),
    };
    let staged = Staged {
        folder: &stage.folder,
        path: &stage.path,
        left: &place.left,
        entries: &entries,
    };
    locked(job.asked, console, |work, lock, console| installed(job, &staged, work, lock, console))
}

/// A put's stage, made and held: the stages folder `{root}/inputs-stage`, the stage's lock file beside the stage,
/// `{key}-{nonce}.lock`, held with `flock` for the put's whole life, and the stage `{key}-{nonce}` itself, with its
/// name and path.
struct Stage {
    stages: Folder,
    #[expect(dead_code, reason = "held, never read: its flock tells the old-stage sweep this stage is live")]
    lock: File,
    name: String,
    folder: Folder,
    path: PathBuf,
}

impl Stage {
    /// Removes the stage beneath the held stages folder, everything in it as itself, then its lock file. Else why not.
    fn removed(&self) -> Result<(), String> {
        let lock = lock_name(&self.name);
        self.stages
            .remove_child_tree(OsStr::new(&self.name))
            .and_then(|()| self.stages.remove_child(OsStr::new(&lock), false))
            .map_err(|error| format!("cannot remove the stage {}: {error}", self.path.display()))
    }
}

/// The name of the lock file of the stage `name`: `{name}.lock`.
fn lock_name(name: &str) -> String {
    format!("{name}.lock")
}

/// This put's stage, `{root}/inputs-stage/{key}-{nonce}`, made and held ([`Stage`]): the root opened and judged on its
/// descriptor ([`Folder::open_root`]); the stages folder made 0700, or adopted only when it is the box user's own and
/// private ([`Folder::open_private_child`]); every stage a day old that no live put holds removed first ([`pruned`],
/// at `now`); then the stage's lock file made new and locked, and the stage made new beside it, never adopted. Else
/// why not.
fn made_stage(asked: &Asked<'_>, now: SystemTime, console: &mut Console) -> Result<Stage, String> {
    let root = Folder::open_root(Path::new(&asked.root)).map_err(|error| format!("cannot open {}: {error}", asked.root))?;
    let stages_path = Path::new(&asked.root).join(STAGES);
    let stages = match root.make_child(OsStr::new(STAGES)) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => root.open_private_child(OsStr::new(STAGES)),
        made => made,
    }
    .map_err(|error| stages_refusal(&stages_path, &error))?;
    for line in pruned(&stages, &stages_path, now) {
        console.err(&line);
    }
    let name = format!("{}-{}", asked.key, asked.nonce);
    let path = stages_path.join(&name);
    let cannot = |error: io::Error| format!("cannot make the stage {}: {error}", path.display());
    let lock = stages.create_file(OsStr::new(&lock_name(&name))).map_err(cannot)?;
    let made = match flock_taken(&lock) {
        Ok(true) => stages.make_child(OsStr::new(&name)),
        Ok(false) => Err(io::Error::other("another put holds its lock")),
        Err(error) => Err(error),
    };
    match made {
        Ok(folder) => Ok(Stage {
            stages,
            lock,
            name,
            folder,
            path,
        }),
        Err(error) => Err(match stages.remove_child(OsStr::new(&lock_name(&name)), false) {
            Ok(()) => cannot(error),
            Err(left) => format!("{}; and its lock file could not be removed: {left}", cannot(error)),
        }),
    }
}

/// The refusal of the stages folder at `path`, which could not be made or adopted for `error`, as the Windows box says
/// it with the Unix cause: one not private to the box, a link or no folder, or any other failure.
fn stages_refusal(path: &Path, error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied && error.to_string() == NOT_PRIVATE {
        format!("cannot use {}: its owner or mode is not the box's; remove it", path.display())
    } else if is_not_folder(error) {
        format!("{} is a link or not a folder", path.display())
    } else {
        format!("cannot ready {}: {error}", path.display())
    }
}

/// Takes `flock(LOCK_EX|LOCK_NB)` on `file`: whether it was free and is now held, held until `file` is closed.
fn flock_taken(file: &File) -> io::Result<bool> {
    // SAFETY: the descriptor is `file`'s, open for the whole call; `flock` reads nothing else.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Removes, beneath the held stages folder `stages`, whose path is `path`, every old stage no live put holds: one whose
/// lock file `{stage}.lock` was last written more than a day before `now` and is held by no put ([`old_lock`]), the
/// stage, everything in it as itself, then whatever is at its lock file's name, as itself ([`locked_stage_removed`]);
/// and one with no lock file beside it, itself last written more than a day before `now`
/// ([`unlocked_stage_removed`]). Nothing but a stage and its lock file's name is touched. The lines saying each kept
/// because it cannot be looked at, and each that could not be removed.
fn pruned(stages: &Folder, path: &Path, now: SystemTime) -> Vec<String> {
    let names = match stages.entries() {
        Ok(names) => names,
        Err(error) => return vec![format!("gate: remote could not list {}: {}", path.display(), escaped(&error.to_string()))],
    };
    let mut lines = Vec::new();
    for name in names.iter().filter_map(|name| name.to_str()) {
        let (stage, removal) = match name.strip_suffix(".lock").filter(|stage| is_stage_name(stage)) {
            Some(stage) => (stage, locked_stage_removed(stages, stage, name, now)),
            None if is_stage_name(name) => (name, unlocked_stage_removed(stages, name, now)),
            None => continue,
        };
        let stage = path.join(stage);
        match removal {
            Ok(()) => {}
            Err(Unswept::Kept(why)) => lines.push(escaped(&format!("gate: remote kept the stage {}: {why}", stage.display()))),
            Err(Unswept::Failed(error)) => lines.push(format!(
                "gate: remote could not remove the old stage {}: {}",
                stage.display(),
                escaped(&error.to_string())
            )),
        }
    }
    lines
}

/// Why an old stage was not swept.
enum Unswept {
    /// It was kept, as it could not be looked at, opened or locked, for this reason.
    Kept(String),
    /// Its removal failed.
    Failed(io::Error),
}

/// Removes the stage `stage` in the held stages folder `stages`, then whatever is at its lock file's name `lock`, each
/// as itself, when its lock file is old and no live put holds it ([`old_lock`]); nothing when it is not. Else why not.
fn locked_stage_removed(stages: &Folder, stage: &str, lock: &str, now: SystemTime) -> Result<(), Unswept> {
    let held = match old_lock(stages, lock, now) {
        Ok(Some(held)) => held,
        Ok(None) => return Ok(()),
        Err(why) => return Err(Unswept::Kept(why)),
    };
    let removal = stages
        .remove_child_tree(OsStr::new(stage))
        .and_then(|()| stages.remove_child_tree(OsStr::new(lock)));
    drop(held);
    removal.map_err(Unswept::Failed)
}

/// Removes the stage `stage` in the held stages folder `stages`, as itself, when nothing is at its lock file's name and
/// the stage itself was last written more than a day before `now`: a put makes its lock file before its stage and
/// removes it after, so such a stage is no live put's. Nothing when a lock file is there or the stage is younger or
/// gone. Else why not.
fn unlocked_stage_removed(stages: &Folder, stage: &str, now: SystemTime) -> Result<(), Unswept> {
    match stages.stat_child(OsStr::new(&lock_name(stage))) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Ok(()),
        Err(error) => return Err(Unswept::Kept(format!("its lock file cannot be looked at: {error}"))),
    }
    let mtime = match stages.stat_child(OsStr::new(stage)) {
        Ok(stat) => Facts::of(&stat).mtime,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Unswept::Kept(format!("it cannot be looked at: {error}"))),
    };
    if !older_than_a_day(mtime, now) {
        return Ok(());
    }
    stages.remove_child_tree(OsStr::new(stage)).map_err(Unswept::Failed)
}

/// Whether a last write at `mtime`, in seconds since 1970, is more than a day before `now`.
fn older_than_a_day(mtime: libc::time_t, now: SystemTime) -> bool {
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
    now.saturating_sub(mtime) > STAGE_AGE_SECONDS
}

/// An old stage's lock file no live put holds, as [`old_lock`] found it.
enum OldLock {
    /// A plain file, locked by this put until dropped, so no put takes it meanwhile.
    Held(#[expect(dead_code, reason = "held, never read: its flock keeps the stage's lock taken while it is removed")] File),
    /// Anything else at the name, which no put makes.
    NotAFile,
}

/// The stage lock file `lock` in the held stages folder `stages`, when it was last written more than a day before
/// `now` and no live put holds it ([`OldLock`]); `None` when it is younger, gone or held. Else why not: it cannot be
/// looked at, opened or locked.
fn old_lock(stages: &Folder, lock: &str, now: SystemTime) -> Result<Option<OldLock>, String> {
    let facts = match stages.stat_child(OsStr::new(lock)) {
        Ok(stat) => Facts::of(&stat),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("its lock file cannot be looked at: {error}")),
    };
    if !older_than_a_day(facts.mtime, now) {
        return Ok(None);
    }
    if facts.kind != libc::S_IFREG {
        return Ok(Some(OldLock::NotAFile));
    }
    let file = match stages.open_file(OsStr::new(lock)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened.map_err(|error| format!("its lock file cannot be opened: {error}"))?,
    };
    match flock_taken(&file) {
        Ok(true) => Ok(Some(OldLock::Held(file))),
        Ok(false) => Ok(None),
        Err(error) => Err(format!("its lock file cannot be locked: {error}")),
    }
}

/// Whether `name` is a stage's name: `{key}-{nonce}`, a key and an outputs nonce of lower-case hex.
fn is_stage_name(name: &str) -> bool {
    name.rsplit_once('-')
        .is_some_and(|(key, nonce)| !key.is_empty() && is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS))
}

/// The Unix box's [`Place`]: each member and copy written into the held stage `stage`, at `path`, as a new 0600 file
/// beneath the folders it makes there, which `budget` counts, and stamped with its last write; `left` holds what each
/// staged file is once written, by its path's simple uppercase, which the install checks before it moves it.
struct Staging<'s> {
    stage: &'s Folder,
    path: &'s Path,
    budget: FolderBudget,
    left: BTreeMap<String, Facts>,
}

impl<'s> Staging<'s> {
    fn new(stage: &'s Folder, path: &'s Path) -> Staging<'s> {
        Staging {
            stage,
            path,
            budget: FolderBudget::new(MEMBER_LIMIT),
            left: BTreeMap::new(),
        }
    }

    /// The error `error` met at the staged `path`, naming it.
    fn in_context(&self, path: &str, error: &io::Error) -> io::Error {
        io::Error::new(error.kind(), format!("{}/{path}: {error}", self.path.display()))
    }

    /// The new file `path` made in the stage, each folder on its way opened as itself or made, within the budget;
    /// else the refusal: a folder on the way that is a link or no folder is outside the destination.
    fn made_file(&mut self, path: &str) -> Result<File, UnpackError> {
        let (parents, leaf) = path.rsplit_once('/').unwrap_or(("", path));
        let mut folder = self.stage.duplicate()?;
        for part in parents.split('/').filter(|part| !part.is_empty()) {
            let name = OsStr::new(part);
            folder = match folder.open_child(name) {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.budget.check()?;
                    let made = folder.make_child(name).map_err(|error| self.in_context(path, &error))?;
                    self.budget.count();
                    made
                }
                Err(error) if is_not_folder(&error) => return Err(outside_destination(path)),
                Err(error) => return Err(self.in_context(path, &error).into()),
            };
        }
        Ok(folder.create_file(OsStr::new(leaf)).map_err(|error| self.in_context(path, &error))?)
    }

    /// Stamps the staged `file` at `path` with `modified` and notes what it is now in `left`.
    fn stamped(&mut self, path: &str, file: &File, modified: SystemTime) -> Result<(), UnpackError> {
        file.set_modified(modified)
            .and_then(|()| beneath::file_stat(file))
            .map(|stat| {
                self.left.insert(ordinal_upper(path), Facts::of(&stat));
            })
            .map_err(|error| UnpackError::Fault(self.in_context(path, &error)))
    }
}

impl Place for Staging<'_> {
    fn member(&mut self, path: &str, modified: SystemTime, data: &mut dyn Read) -> Result<(), UnpackError> {
        let mut file = self.made_file(path)?;
        io::copy(data, &mut file).map_err(|error| self.in_context(path, &error))?;
        self.stamped(path, &file, modified)
    }

    fn copy(&mut self, entry: &Entry, modified: SystemTime, from: CopyFrom<'_>) -> Result<(), UnpackError> {
        let CopyFrom::Member(member) = from else {
            return Err(no_pc_file(entry, &[]));
        };
        let source = match walk(self.stage, member, false, None)? {
            Walked::Reached(folder, leaf) => folder.open_file(OsStr::new(leaf)).map_err(|error| self.in_context(member, &error))?,
            Walked::Missing | Walked::Blocked { .. } => return Err(self.in_context(member, &io::ErrorKind::NotFound.into()).into()),
        };
        let mut file = self.made_file(&entry.path)?;
        let mut hashing = Hashing::new(source.take(entry.size.saturating_add(1)));
        io::copy(&mut hashing, &mut file).map_err(|error| self.in_context(&entry.path, &error))?;
        let (sha256, size) = hashing.finish();
        if sha256 != entry.sha256 || size != entry.size {
            return Err(copy_mismatch(entry, size, &sha256));
        }
        self.stamped(&entry.path, &file, modified)
    }
}

/// What a put unpacked: the held stage, its path, what each staged file was left as, and the entries of the archive.
struct Staged<'s> {
    folder: &'s Folder,
    path: &'s Path,
    left: &'s BTreeMap<String, Facts>,
    entries: &'s [Entry],
}

/// Checks every entry of `staged` against the manifest and every target in the held work folder `work`, before any is
/// written, and the stage on the work folder's volume; then, the lock `lock` still held, writes the record without a
/// digest ([`unsealed`]), deletes the stale copies ([`swept`]), moves each file in ([`moved_in`]) and finishes the
/// install ([`finished`]). The count of files installed; else why not.
fn installed(job: Job<'_>, staged: &Staged<'_>, work: &Work, lock: &HeldLock, console: &mut Console) -> Result<usize, String> {
    let Job { asked, sent } = job;
    let book = asked.record();
    let record = recorded(asked, work, lock, console)?;
    let spelled: HashSet<&str> = sent.manifest.files().map(|(path, _)| path).collect();
    for entry in staged.entries {
        in_manifest(entry, &sent.manifest, &spelled)?;
        free_target(work, &entry.path, &record, &sent.manifest)?;
    }
    let devices = staged.folder.stat().and_then(|stage| Ok((stage.st_dev, work.folder.stat()?.st_dev)));
    let (stage_dev, work_dev) = devices.map_err(|error| format!("cannot look at the stage {}: {error}", staged.path.display()))?;
    volume_checked(stage_dev, work_dev, staged.path, &work.path)?;
    lock_held(asked, lock)?;
    unsealed(&book, &record)?;
    let mut now = Record {
        kept: swept(work, &record, &sent.manifest, &held_ids(work, &sent.manifest), console),
        ..Record::default()
    };
    for entry in staged.entries {
        let stamp = moved_in(staged, work, &entry.path)?;
        let copy = Copy {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            stamp,
        };
        now.files.insert(ordinal_upper(&entry.path), copy);
    }
    for line in finished(job, work, &record, now, lock)? {
        console.err(&line);
    }
    Ok(staged.entries.len())
}

/// Whether the stage at `stage_path`, on the device `stage`, is on the work folder's device `work`, so its files move
/// into the work folder `work_path` by a rename; else why not.
fn volume_checked(stage: libc::dev_t, work: libc::dev_t, stage_path: &Path, work_path: &str) -> Result<(), String> {
    if stage == work {
        Ok(())
    } else {
        Err(format!(
            "the stage {} is on another volume than the work folder {work_path}",
            stage_path.display()
        ))
    }
}

/// Whether the manifest holds the member `entry` under its exact spelling, one of `spelled`, the manifest's own paths,
/// with its size and SHA-256; else the refusal naming it. A member spelt otherwise than the manifest, in case alone
/// included, is not in it: the box installs every file under the name the manifest gives it.
fn in_manifest(entry: &Entry, manifest: &Manifest, spelled: &HashSet<&str>) -> Result<(), String> {
    match manifest.get(&entry.path).filter(|_| spelled.contains(entry.path.as_str())) {
        None => Err(format!("the archive's member '{}' is not in the manifest", escaped(&entry.path))),
        Some(held) if held.sha256 != entry.sha256 || held.stamp.size != entry.size => Err(format!(
            "the archive's member '{}' is {} bytes with SHA-256 {}, the manifest's {} bytes with SHA-256 {}",
            escaped(&entry.path),
            entry.size,
            entry.sha256,
            held.stamp.size,
            held.sha256
        )),
        Some(_) => Ok(()),
    }
}

/// Whether the file `path` may be installed below the held work folder `work`: each folder on its way there a folder
/// of its own ([`walk`]), or a file that is an unchanged recorded copy leaving the manifest, which is deleted before
/// the moves ([`swept`]); and whatever is at its path missing or a plain file of one name ([`plain_copy`]). Else the
/// refusal naming the path.
fn free_target(work: &Work, path: &str, record: &Record, manifest: &Manifest) -> Result<(), String> {
    let refused = |why: String| format!("cannot install '{}': {why}", escaped(path));
    match walk(&work.folder, path, false, work.git).map_err(|error| refused(error.to_string()))? {
        Walked::Missing => Ok(()),
        Walked::Blocked { prefix, folder, name } => match is_stale_unchanged(&folder, name, prefix, record, manifest) {
            Ok(true) => Ok(()),
            Ok(false) => Err(refused(not_a_folder(prefix).to_string())),
            Err(why) => Err(refused(why)),
        },
        Walked::Reached(folder, leaf) => match folder.stat_child(OsStr::new(leaf)) {
            Ok(stat) => plain_copy(&Facts::of(&stat)).map_err(|error| refused(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(refused(error.to_string())),
        },
    }
}

/// Whether `name` in `folder`, at `path`, is a plain file of one name that `record` holds, that left `manifest` and
/// that still hashes to what was installed; else why it could not be hashed.
fn is_stale_unchanged(folder: &Folder, name: &str, path: &str, record: &Record, manifest: &Manifest) -> Result<bool, String> {
    let Some(copy) = record.get(path).filter(|_| manifest.get(path).is_none()) else {
        return Ok(false);
    };
    let Ok(file) = folder.open_file(OsStr::new(name)) else {
        return Ok(false);
    };
    if !beneath::file_stat(&file).is_ok_and(|stat| Facts::of(&stat).is_plain()) {
        return Ok(false);
    }
    let (sha256, _) = sha256_of(&file).map_err(|error| format!("'{}' cannot be hashed: {error}", escaped(path)))?;
    Ok(sha256 == copy.sha256)
}

/// Moves the staged file `path` of `staged` to the same path below the held work folder `work`, replacing a file or a
/// link there as itself: each folder on its way made when missing and refused when it is a link or not a folder
/// ([`walk`]); the staged file checked to be what the unpack left (identity, size, mtime), renamed beneath both
/// folders, and the name it landed at looked at as itself, which must be that same file, a plain file of one name. The
/// moved file's stamp, read then; else why not.
fn moved_in(staged: &Staged<'_>, work: &Work, path: &str) -> Result<Stamp, String> {
    let failed = |why: &str| format!("cannot install '{}': {why}", escaped(path));
    let (folder, leaf) = match walk(&work.folder, path, true, work.git).map_err(|error| failed(&error.to_string()))? {
        Walked::Reached(folder, leaf) => (folder, leaf),
        Walked::Blocked { prefix, .. } => return Err(failed(&not_a_folder(prefix).to_string())),
        Walked::Missing => return Err(failed("a folder on its way could not be made")),
    };
    let not_staged = || failed("its staged copy is not in the stage");
    let Walked::Reached(source, source_leaf) = walk(staged.folder, path, false, None).map_err(|_| not_staged())? else {
        return Err(not_staged());
    };
    let left = staged.left.get(&ordinal_upper(path)).ok_or_else(not_staged)?;
    let before = source.stat_child(OsStr::new(source_leaf)).map_err(|error| failed(&error.to_string()))?;
    if Facts::of(&before) != *left || !left.is_plain() {
        return Err(failed("its staged copy changed after the unpack"));
    }
    source
        .rename_child(OsStr::new(source_leaf), &folder, OsStr::new(leaf))
        .map_err(|error| failed(&error.to_string()))?;
    let landed = Facts::of(&folder.stat_child(OsStr::new(leaf)).map_err(|error| failed(&error.to_string()))?);
    if (landed.dev, landed.ino) != (left.dev, left.ino) || !landed.is_plain() {
        return Err(failed("what is at its name after the move is not its staged copy"));
    }
    Ok(landed.stamp())
}

/// The `{dev}-{ino}` of every file of `manifest` the held work folder `work` holds at the manifest's spelling of its
/// path, opened as [`opened_copy`] opens a copy and not hashed: the files the record goes on naming, which no sweep
/// may unlink, however another spelling of a recorded path reaches them.
fn held_ids(work: &Work, manifest: &Manifest) -> HashSet<String> {
    manifest
        .files()
        .filter_map(|(path, _)| opened_copy(work, path).ok().flatten())
        .map(|opened| opened.facts.stamp().id)
        .collect()
}

/// Deletes each copy `record` holds whose exact spelling left `manifest`, beneath its held folder, only while it is a
/// plain file of one name below the held work folder `work` that is none of the files `held` names ([`held_ids`]),
/// that still hashes to what the record holds and is, just before its unlink, still the file hashed
/// ([`removed_stale`]); a copy changed on the box, or one a manifest path still reaches, is kept. Says each copy kept
/// because it could not be opened or hashed, and each that could not be deleted. The copies kept, by their key: each
/// but those deleted and those no longer there.
fn swept(work: &Work, record: &Record, manifest: &Manifest, held: &HashSet<String>, console: &mut Console) -> BTreeMap<String, String> {
    let spelled: HashSet<&str> = manifest.files().map(|(path, _)| path).collect();
    let mut kept_copies = BTreeMap::new();
    for (key, copy) in record.files.iter().filter(|(_, copy)| !spelled.contains(copy.path.as_str())) {
        let kept = |why: String| format!("gate: remote kept the stale input {}: {}", escaped(&copy.path), escaped(&why));
        let opened = match opened_copy(work, &copy.path) {
            Ok(Some(opened)) if held.contains(&opened.facts.stamp().id) => {
                kept_copies.insert(key.clone(), copy.path.clone());
                continue;
            }
            Ok(Some(opened)) => opened,
            Ok(None) => continue,
            Err(error) => {
                console.err(&kept(error.to_string()));
                kept_copies.insert(key.clone(), copy.path.clone());
                continue;
            }
        };
        let deleted = match sha256_of(&opened.file) {
            Ok((sha256, _)) if sha256 == copy.sha256 => removed_stale(&opened, copy, console),
            Ok(_) => false,
            Err(error) => {
                console.err(&kept(format!("it cannot be hashed: {error}")));
                false
            }
        };
        if !deleted {
            kept_copies.insert(key.clone(), copy.path.clone());
        }
    }
    kept_copies
}

/// Deletes the stale `copy`, opened as `opened`, beneath its held folder, once its name, looked at as itself just
/// before, still shows the file hashed (identity, size, mtime); says so when it cannot be deleted. Whether it was.
fn removed_stale(opened: &Opened, copy: &Copy, console: &mut Console) -> bool {
    let name = OsStr::new(&opened.leaf);
    if !opened.folder.stat_child(name).is_ok_and(|stat| Facts::of(&stat) == opened.facts) {
        return false;
    }
    let Err(error) = opened.folder.remove_child(name, false) else {
        return true;
    };
    console.err(&format!(
        "gate: remote could not remove the stale input {}: {}",
        escaped(&copy.path),
        escaped(&error.to_string())
    ));
    false
}

/// Adds to `now`, which holds the files this put installed, every other file of `manifest` the held work folder `work`
/// holds as the manifest says ([`matched_copy`]).
fn present(work: &Work, manifest: &Manifest, record: &Record, now: &mut Record) {
    for (path, held) in manifest.files() {
        let key = ordinal_upper(path);
        if now.files.contains_key(&key) {
            continue;
        }
        if let Some(copy) = matched_copy(work, path, held, record) {
            now.files.insert(key, copy);
        }
    }
}

/// Finishes an install whose moved or matched files and newly kept copies are `now`'s: adds every other file of the
/// manifest the held work folder `work` holds as the manifest says ([`present`]), and keeps every copy `record` kept
/// that no file of `now` replaces and that is still there; fails, the record written with no digest once the lock
/// `lock` is shown still held, when a file of the manifest is still missing; else rewrites the exclude file from the
/// record ([`write_exclude`]), checks the lock still held and every file of `now` still the copy its stamp names
/// ([`changed_before_seal`]), failing the same way when one is not, and, last, writes the record with the manifest's
/// digest. The lines to say: where a folder at the exclude file's name was moved aside, when one was. Else why not.
fn finished(job: Job<'_>, work: &Work, record: &Record, mut now: Record, lock: &HeldLock) -> Result<Vec<String>, String> {
    let Job { asked, sent } = job;
    let book = asked.record();
    present(work, &sent.manifest, record, &mut now);
    now.kept.extend(record.kept.clone());
    now.kept.retain(|key, _| !now.files.contains_key(key));
    // A copy that cannot be opened or is no plain file stays kept: only one shown gone leaves.
    now.kept.retain(|_, path| !matches!(opened_copy(work, path), Ok(None)));
    if let Some((path, _)) = sent.manifest.files().find(|(path, _)| now.get(path).is_none()) {
        let why = match opened_copy(work, path) {
            Err(error) => format!("{path} cannot be opened: {error}"),
            Ok(Some(_)) => format!("{path} does not hold what the manifest names after the install"),
            Ok(None) => format!("{path} is missing after the install"),
        };
        return Err(left_unsealed(job, record, now, lock).err().unwrap_or(why));
    }
    let lines: Vec<String> = write_exclude(work, &now)?.into_iter().collect();
    lock_held(asked, lock)?;
    if let Some(why) = changed_before_seal(work, &now) {
        return Err(left_unsealed(job, record, now, lock).err().unwrap_or(why));
    }
    now.digest.clone_from(&sent.digest);
    write_record(&book, &now)?;
    Ok(lines)
}

/// Writes the record of `job` with no digest, `record`'s files with `now`'s over them and `now`'s kept copies, once the
/// lock `lock` is shown still held; else why not.
fn left_unsealed(job: Job<'_>, record: &Record, now: Record, lock: &HeldLock) -> Result<(), String> {
    lock_held(job.asked, lock)?;
    let mut files = record.files.clone();
    files.extend(now.files);
    write_record(
        &job.asked.record(),
        &Record {
            digest: String::new(),
            files,
            kept: now.kept,
        },
    )
}

/// Why a file of `now` is not, just before the seal, the copy its stamp names below the held work folder `work`:
/// gone, unreadable, or of another stamp; `None` when each still is.
fn changed_before_seal(work: &Work, now: &Record) -> Option<String> {
    now.files.values().find_map(|copy| match opened_copy(work, &copy.path) {
        Ok(Some(opened)) if opened.facts.stamp() == copy.stamp => None,
        Ok(_) => Some(format!("{} changed before the record was sealed", copy.path)),
        Err(error) => Some(format!("{} cannot be opened before the record is sealed: {error}", copy.path)),
    })
}

/// Why the work folder `work`, its lock held by a run, does not hold the inputs of the manifest whose digest is `sent`,
/// as the Windows box's check says it: its record `{work}.inputs` missing, unreadable ([`read_record`]), not a record
/// or of another digest, an unsealed one's empty digest among them; a recorded copy gone, not a plain file of one name,
/// or not the copy installed ([`still_installed`]), each reached beneath the work folder held through its judged root
/// ([`Work::held`]); or a tree whose exclude file cannot be written from the record ([`write_exclude`]), each
/// [`Unmatched::Inputs`]. The exclude file is written only while the run's work lock `lock` is shown still held, as the
/// run's other record writes are; else [`Unmatched::LockLost`]. `Ok` when it holds them, the exclude file written, with
/// the line saying where a folder at the exclude file's name was moved aside, when one was, for the caller to say.
pub(crate) fn unmatched(work: &str, sent: &str, lock: &HeldLock) -> Result<Option<String>, Unmatched> {
    let record = matching_record(&format!("{work}.inputs"), sent).map_err(Unmatched::Inputs)?;
    let held = Work::held(work).map_err(|error| Unmatched::Inputs(format!("cannot open {work}: {error}")))?;
    if let Some(why) = changed_copy(&held, &record) {
        return Err(Unmatched::Inputs(why));
    }
    let shown = lock.path().display();
    match lock.still_held() {
        Ok(true) => write_exclude(&held, &record).map_err(Unmatched::Inputs),
        Ok(false) => Err(Unmatched::LockLost(format!("the work folder lock {shown} is no longer held"))),
        Err(error) => Err(Unmatched::LockLost(format!(
            "the work folder lock {shown} could not be looked at: {error}"
        ))),
    }
}

/// Why a run's inputs check did not pass ([`unmatched`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unmatched {
    /// The work folder does not hold the inputs sent, or its exclude file cannot be written: the run is refused `inputs`.
    Inputs(String),
    /// The exclude file may not be written, the work lock lost or not looked at: the run fails, as its other record
    /// writes do.
    LockLost(String),
}

/// The record at `path` when it holds the digest `sent`; else why not.
fn matching_record(path: &str, sent: &str) -> Result<Record, String> {
    match read_record(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!("{path} does not exist")),
        Ok(text) => match Record::parse(&text) {
            Ok(record) if record.digest == sent => Ok(record),
            Ok(_) => Err(format!("{path} holds another manifest's digest")),
            Err(why) => Err(format!("{path} {why}")),
        },
        Err(error) => Err(format!("{path} cannot be read: {error}")),
    }
}

/// Why a copy `record` holds is not below the held work folder `work` as installed; `None` when each is.
fn changed_copy(work: &Work, record: &Record) -> Option<String> {
    record.files.values().find_map(|copy| match opened_copy(work, &copy.path) {
        Ok(Some(opened)) if still_installed(&opened, copy) => None,
        Ok(Some(_)) => Some(format!("the input {} is not the copy installed", copy.path)),
        Ok(None) => Some(format!("the input {} is missing", copy.path)),
        Err(error) => Some(format!("the input {} cannot be opened: {error}", copy.path)),
    })
}

/// Whether the copy `opened` is the one `copy` records: its stamp the record's and trusted ([`Stamp::trusts`]),
/// unhashed; or its stamp the record's but not trusted, a mtime before 1970, and its bytes hashing to the record's.
fn still_installed(opened: &Opened, copy: &Copy) -> bool {
    let stamp = opened.facts.stamp();
    if stamp.trusts(&copy.stamp) {
        return true;
    }
    stamp == copy.stamp && sha256_of(&opened.file).is_ok_and(|(sha256, _)| sha256 == copy.sha256)
}

/// Rewrites the tree's `.git/info/exclude` from `record`: one line per path it names, installed or kept, as a literal
/// pattern ([`literal_pattern`]), in the order of their keys. The `.git` must be a folder of its own, opened beneath the
/// held work folder `work`; `.git/info` is made a folder of its own beneath it ([`info_folder`]), and the file written
/// beneath that ([`written_beneath`]), so no link there is followed; the line saying where a folder at the file's name
/// was moved aside, when one was. Else why not.
fn write_exclude(work: &Work, record: &Record) -> Result<Option<String>, String> {
    let git_path = Path::new(&work.path).join(".git");
    let git = work
        .folder
        .open_child(OsStr::new(".git"))
        .map_err(|_| format!("{} is not a folder of its own", git_path.display()))?;
    let info_path = git_path.join("info");
    let info = info_folder(&git, &info_path)?;
    let mut text = String::new();
    for path in record.paths() {
        text.push_str(&literal_pattern(path));
        text.push('\n');
    }
    let exclude = info_path.join("exclude");
    written_beneath(&info, "exclude", &text)
        .map(|moved| moved.map(|moved| moved_aside_line(&exclude, &moved)))
        .map_err(|why| format!("cannot write {}: {why}", exclude.display()))
}

/// The folder `info` beneath the held `.git` folder `git`: a link or a file at its name removed as itself, and the
/// folder made, 0700, when missing. Else why not, naming it by `path`.
fn info_folder(git: &Folder, path: &Path) -> Result<Folder, String> {
    let cannot = |error: io::Error| format!("cannot ready {}: {error}", path.display());
    let name = OsStr::new("info");
    match git.stat_child(name) {
        Ok(stat) if Facts::of(&stat).is_folder() => {}
        Ok(_) => git.remove_child(name, false).map_err(cannot)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(cannot(error)),
    }
    child_folder(git, name, true).map_err(cannot)
}

/// Writes `text` as the file `name` in the held folder `folder` unless a plain file there already holds exactly that:
/// a folder at the name moved aside beneath the folder to `{name}.gate-moved-{n}` for the first free `n`, never walked
/// ([`moved_aside`]); then a new temp file made beneath the folder, written, and renamed over the name, replacing a
/// file or a link there as itself. The name a folder was moved aside to, when one was. Else why not.
fn written_beneath(folder: &Folder, name: &str, text: &str) -> Result<Option<String>, String> {
    let moved = match folder.stat_child(OsStr::new(name)) {
        Ok(stat) if Facts::of(&stat).is_folder() => Some(moved_aside(folder, name)?),
        Ok(stat) if Facts::of(&stat).kind == libc::S_IFREG && holds(folder, name, text) => return Ok(None),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let temp = format!("{name}.gate-new-{}-{}", std::process::id(), TEMPS.fetch_add(1, Ordering::Relaxed));
    let temp_name = OsStr::new(&temp);
    // A leftover of that name, which only an ended process with this one's pid can have left, is removed and the
    // making tried once more, as `files::replace_plain` does.
    let mut file = match folder.create_file(temp_name) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => folder
            .remove_child(temp_name, false)
            .map_err(|left| format!("the leftover temp file {temp} could not be removed: {left}"))
            .and_then(|()| folder.create_file(temp_name).map_err(|error| error.to_string()))?,
        made => made.map_err(|error| error.to_string())?,
    };
    let replaced = file
        .write_all(text.as_bytes())
        .and_then(|()| folder.rename_child(temp_name, folder, OsStr::new(name)));
    drop(file);
    if let Err(error) = replaced {
        return Err(match folder.remove_child(temp_name, false) {
            Ok(()) => error.to_string(),
            Err(left) => format!("{error}; and the temp file {temp} could not be removed: {left}"),
        });
    }
    Ok(moved)
}

/// Whether the plain file `name` in `folder` holds exactly `text`.
fn holds(folder: &Folder, name: &str, text: &str) -> bool {
    let Ok(file) = folder.open_file(OsStr::new(name)) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(text.len() as u64 + 1).read_to_end(&mut bytes).is_ok() && bytes == text.as_bytes()
}

/// Moves the folder `name` in `folder` aside beneath it, as itself, to `{name}.gate-moved-{n}` for the first `n` up to
/// [`MOVED_ASIDE_LIMIT`] nothing has; its new name. Else why not.
fn moved_aside(folder: &Folder, name: &str) -> Result<String, String> {
    for n in 1..=MOVED_ASIDE_LIMIT {
        let moved = format!("{name}.gate-moved-{n}");
        match folder.stat_child(OsStr::new(&moved)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => continue,
            Err(error) => return Err(format!("cannot move aside the folder {name}: {error}")),
        }
        match folder.rename_child(OsStr::new(name), folder, OsStr::new(&moved)) {
            Ok(()) => return Ok(moved),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY | libc::ENOTDIR)) => {}
            Err(error) => return Err(format!("cannot move aside the folder {name}: {error}")),
        }
    }
    Err(format!(
        "cannot move aside the folder {name}: {name}.gate-moved-1 to {name}.gate-moved-{MOVED_ASIDE_LIMIT} are all taken"
    ))
}

/// `gate: remote moved aside the folder <file> to <file's folder>/<moved>`.
fn moved_aside_line(file: &Path, moved: &str) -> String {
    format!(
        "gate: remote moved aside the folder {} to {}",
        file.display(),
        file.with_file_name(moved).display()
    )
}

/// The ignore pattern matching the `/`-separated `path` alone: anchored with `/`, each `\`, `*`, `?` and `[` escaped.
fn literal_pattern(path: &str) -> String {
    let mut pattern = String::with_capacity(path.len() + 1);
    pattern.push('/');
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose folders, archives or payloads cannot be made must fail loudly"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use gate_args::BoxSeams;
    use gate_inputs::archive::{CopyFrom, Entry, Place, copy_mismatch, no_pc_file};
    use gate_inputs::hash::sha256_of;
    use gate_inputs::manifest::{Held, Manifest, Stamp as HeldStamp, read_sent};
    use gate_inputs::pack::Packer;
    use gate_wire::{PathForm, Payload, ROOT_STATUS, Refusal, busy_line, ordinal_upper, refused_line, root_unsafe_line};
    use serde_json::{Map, Value, json};

    use super::{
        Asked, Copy as Installed, Facts, Job, PAIR_REFUSED, RECORD_CAP, Record, Sent, Staged, Staging, Stamp, Unmatched, Work, answered, finished,
        flock_taken, free_target, installed, made_stage, matched_copy, moved_in, opened_copy, pruned, recorded, unmatched, volume_checked,
        write_exclude, write_record_capped,
    };
    use crate::beneath::Folder;
    use crate::console::Console;
    use crate::files;
    use crate::lines::{FAILED_STATUS, USAGE_STATUS, WORK_ROOT_STEP, folder_failure, job_record_text};
    use crate::pid_info::start_of;
    use crate::run::REFUSED_STATUS;

    const KEY: &str = "repo-0123abcd";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    /// The last write every packed file carries, in seconds since 1970.
    const WRITTEN: u64 = 1_700_000_000;
    /// How long a diff may take before the case calls it hung: a hang guard, never reached by a diff that works.
    const HANG_GUARD: Duration = Duration::from_secs(300);

    /// Files of a manifest or an archive: each path and its bytes.
    type Files<'a> = [(&'a str, &'a [u8])];

    /// A work root `<base>/gate-remote`, private to the box user, with its `<root>/work`, in a temp folder of the case
    /// `name`, private to this process and removed when the case ends.
    struct Case {
        base: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Case {
            let base = std::env::temp_dir().join(format!("gate-box-unix-inputs-{}-{name}", std::process::id()));
            if base.exists() {
                fs::remove_dir_all(&base).expect("a stale case folder is removed");
            }
            let case = Case { base };
            // Mode 0700 as the box demands of a root and its `<root>/work`, whatever the umask.
            std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o700)
                .create(case.root().join("work"))
                .expect("the work folders' parent is made");
            case
        }

        fn root(&self) -> PathBuf {
            self.base.join("gate-remote")
        }
        fn work(&self) -> String {
            format!("{}/work/{KEY}", self.root().to_str().expect("the temp folder is UTF-8"))
        }

        fn lock(&self) -> String {
            format!("{}.lock", self.work())
        }

        /// The inputs request of this case's work folder for `step`.
        fn json(&self, step: &str) -> Value {
            let root = self.root().to_str().expect("the temp folder is UTF-8").to_owned();
            let work = self.work();
            json!({
                "Key": KEY,
                "Bare": format!("{root}/repo.git"),
                "Work": work,
                "Lock": format!("{work}.lock"),
                "Index": format!("{work}.index"),
                "OutputFolder": format!("{root}/outputs"),
                "Job": format!("gate-remote-{KEY}-0123456789ab"),
                "Nonce": NONCE,
                "Step": step,
            })
        }

        fn payload(&self, step: &str) -> Payload {
            decoded(&self.json(step))
        }

        /// The diff of the manifest of `files`: its status and what it said.
        fn diff(&self, files: &Files<'_>) -> (u8, Console) {
            answer(&self.json("diff"), manifest_of(files).line().as_bytes())
        }

        /// The put of the manifest of `files` with an archive of `members` and `copies`: its status and what it said.
        fn put(&self, files: &Files<'_>, members: &Files<'_>, copies: &Files<'_>) -> (u8, Console) {
            let mut input = manifest_of(files).line().into_bytes();
            input.extend(archive(members, copies));
            answer(&self.json("put"), &input)
        }

        /// The work folder laid out by a diff of no file; the case fails unless it passed.
        fn laid_out(&self) {
            let (status, console) = self.diff(&[]);
            assert_eq!(status, 0, "{:?}", console.err);
        }

        /// The bytes of `path` below the work folder.
        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(format!("{}/{path}", self.work())).expect("the input is read")
        }

        fn has(&self, path: &str) -> bool {
            fs::symlink_metadata(format!("{}/{path}", self.work())).is_ok()
        }

        fn record_path(&self) -> String {
            format!("{}.inputs", self.work())
        }

        fn record(&self) -> Value {
            serde_json::from_slice(&fs::read(self.record_path()).expect("the record is read")).expect("the record is JSON")
        }

        fn exclude(&self) -> String {
            fs::read_to_string(format!("{}/.git/info/exclude", self.work())).expect("the exclude file is read")
        }

        /// The work folder, made by hand with a `.git/info`, held as a step under the lock holds it.
        fn work_held(&self) -> Work {
            fs::create_dir_all(format!("{}/.git/info", self.work())).expect("the work folder is made");
            Work::held(&self.work()).expect("the work folder is held")
        }

        /// A folder `stage` beside the root holding `files`, held, and what each was left as.
        fn stage(&self, files: &Files<'_>) -> (PathBuf, Folder, BTreeMap<String, Facts>) {
            let path = self.base.join("stage");
            fs::create_dir_all(&path).expect("the stage is made");
            let folder = Folder::open(&path).expect("the stage opens");
            let mut left = BTreeMap::new();
            for (name, bytes) in files {
                fs::write(path.join(name), bytes).expect("a staged file is written");
                let stat = folder.stat_child(OsStr::new(name)).expect("the staged file is looked at");
                left.insert(ordinal_upper(name), Facts::of(&stat));
            }
            (path, folder, left)
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            // Best effort: a folder left behind is in the temp folder and named for this process.
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn decoded(json: &Value) -> Payload {
        Payload::decode(&STANDARD.encode(json.to_string()), PathForm::Unix).expect("the payload decodes")
    }

    /// The answer to the request `json` with `input` on its stdin: the status and what it said.
    fn answer(json: &Value, input: &[u8]) -> (u8, Console) {
        let mut console = Console::captured();
        let mut input = input;
        let status = answered(&decoded(json), &BoxSeams::default(), &mut input, &mut console);
        (status, console)
    }

    fn sha(bytes: &[u8]) -> String {
        sha256_of(bytes).expect("bytes hash").0
    }

    fn manifest_of(files: &Files<'_>) -> Manifest {
        let mut manifest = Manifest::default();
        for (path, bytes) in files {
            let stamp = HeldStamp {
                size: bytes.len() as u64,
                modified: 0,
            };
            manifest.insert((*path).to_owned(), Held { stamp, sha256: sha(bytes) });
        }
        manifest
    }

    fn sent_of(files: &Files<'_>) -> Sent {
        let digest = read_sent(manifest_of(files).line().into_bytes()).expect("the manifest reads").1;
        Sent {
            manifest: manifest_of(files),
            digest,
        }
    }

    fn entry(path: &str, bytes: &[u8]) -> Entry {
        Entry {
            path: path.to_owned(),
            size: bytes.len() as u64,
            sha256: sha(bytes),
        }
    }

    /// An inputs archive of `members`, then `copies` listed as copies of an earlier member.
    fn archive(members: &Files<'_>, copies: &Files<'_>) -> Vec<u8> {
        let mut sink = Vec::new();
        let mut packer = Packer::new(&mut sink, 20).expect("a packer is made");
        for (path, bytes) in members {
            packer.add(path, bytes.len() as u64, WRITTEN, *bytes).expect("a member is packed");
        }
        for (path, bytes) in copies {
            packer.copy(path, bytes.len() as u64, WRITTEN, &sha(bytes)).expect("a copy is listed");
        }
        packer.finish().expect("the archive ends");
        sink
    }

    /// The permission bits of `path`, looked at as itself.
    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("the path is looked at").permissions().mode() & 0o7777
    }

    /// The names in the folder `path`, sorted.
    fn names_in(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(path)
            .expect("the folder is listed")
            .map(|entry| entry.expect("an entry is read").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A `sleep 600` leading a process group of its own, as a git an earlier run left running does.
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawn lock is the Windows gate's; this test stands in for an earlier run's git"
    )]
    fn sleeping_group() -> Child {
        Command::new("sleep").arg("600").process_group(0).spawn().expect("sleep starts")
    }

    #[test]
    fn a_sibling_pair_is_refused_as_usage_before_the_manifest_is_read() {
        let case = Case::new("pair");
        let mut json = case.json("diff");
        json["Pair"] = json!({ "Leaf": "app" });

        let (status, console) = answer(&json, b"not a manifest\n");

        assert_eq!(status, USAGE_STATUS);
        assert_eq!(console.err, [PAIR_REFUSED]);
        assert_eq!(PAIR_REFUSED, "gate box: inputs for a sibling pair are not taken yet");
    }

    #[test]
    fn a_manifest_naming_a_path_inputs_never_carry_fails_the_step_before_the_lock() {
        let case = Case::new("manifest-refused");
        let _held = files::held_for_test(Path::new(&case.lock()));

        let (status, console) = case.diff(&[(".env", b"x")]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(
            console.err,
            ["gate: remote inputs diff failed: the inputs manifest names '.env', which inputs never carry"]
        );
    }

    #[test]
    fn a_held_lock_refuses_the_step_busy_its_tag_then_the_busy_line() {
        let case = Case::new("busy");
        let _held = files::held_for_test(Path::new(&case.lock()));

        let (status, console) = case.diff(&[("a.txt", b"a\n")]);

        assert_eq!(status, REFUSED_STATUS);
        assert_eq!(console.err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(console.out.is_empty(), "{:?}", console.out);
    }

    #[test]
    fn an_earlier_run_s_live_group_refuses_the_step_busy_and_is_left_running() {
        let case = Case::new("earlier-live");
        let mut child = sleeping_group();
        let record = format!("{}.pgid", case.work());
        let pid = i32::try_from(child.id()).expect("a pid is a pid_t");
        // SAFETY: `getsid(0)` asks for the caller's own session and touches no memory.
        let session = unsafe { libc::getsid(0) };
        let started = start_of(pid).expect("the sleep's start is read");
        fs::write(
            &record,
            job_record_text(&format!("gate-remote-{KEY}-ffffffffffff"), pid, session, started),
        )
        .expect("the record is written");

        let (status, console) = case.diff(&[("a.txt", b"a\n")]);

        let alive = child.try_wait().expect("the sleep is looked at").is_none();
        // Best effort: the sleep is this case's own.
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, REFUSED_STATUS, "{:?}", console.err);
        assert_eq!(console.err, [refused_line(NONCE, Refusal::Busy), busy_line(KEY)]);
        assert!(alive, "the earlier group is never signalled");
        assert!(!case.has(".git"), "nothing is laid out");
    }

    #[test]
    fn a_diff_lays_the_work_folder_out_and_names_each_file_it_lacks() {
        let case = Case::new("diff");

        let (status, console) = case.diff(&[("a.txt", b"a\n"), ("dir/b.txt", b"b\n")]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(
            console.out,
            [
                "gate-remote-inputs: need 2",
                "gate-remote-inputs: path a.txt",
                "gate-remote-inputs: path dir/b.txt"
            ]
        );
        assert!(case.has(".git/HEAD"), "the tree is laid out as a run lays it out");
        assert!(!Path::new(&case.record_path()).exists(), "a diff that needs files writes no record");
    }

    #[test]
    fn a_put_installs_its_files_seals_the_record_and_then_a_diff_needs_none() {
        let case = Case::new("put");
        let files: [(&str, &[u8]); 3] = [("a.txt", b"a\n"), ("dir/b.txt", b"b\n"), ("dir/c.txt", b"a\n")];

        let (status, console) = case.put(&files, &files[..2], &files[2..]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-inputs: installed 3"]);
        assert_eq!(case.read("dir/c.txt"), b"a\n", "the copy holds its member's bytes");
        assert_eq!(mode_of(Path::new(&format!("{}/a.txt", case.work()))), 0o600);
        assert_eq!(case.record()["digest"], sent_of(&files).digest);
        assert_eq!(case.exclude(), "/a.txt\n/dir/b.txt\n/dir/c.txt\n");
        assert!(
            names_in(&case.root().join("inputs-stage")).is_empty(),
            "the stage and its lock are removed"
        );

        let (status, console) = case.diff(&files);
        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-inputs: need 0"]);
    }

    #[test]
    fn a_member_the_manifest_lacks_fails_the_put_and_changes_nothing_below_work() {
        let case = Case::new("unlisted");

        let (status, console) = case.put(&[("a.txt", b"a\n")], &[("a.txt", b"a\n"), ("extra.txt", b"x\n")], &[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(
            console.err,
            ["gate: remote inputs put failed: the archive's member 'extra.txt' is not in the manifest"]
        );
        assert!(!case.has("a.txt") && !case.has("extra.txt"), "nothing is installed");
        assert!(!Path::new(&case.record_path()).exists(), "no record is written");
    }

    #[test]
    fn a_link_on_an_input_s_way_is_never_followed() {
        let case = Case::new("link-on-way");
        case.laid_out();
        let outside = case.base.join("outside");
        fs::create_dir(&outside).expect("the outside folder is made");
        symlink(&outside, format!("{}/sub", case.work())).expect("the link is made");
        let files: [(&str, &[u8]); 1] = [("sub/x.txt", b"x\n")];

        let (status, console) = case.put(&files, &files, &[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(
            console.err,
            ["gate: remote inputs put failed: cannot install 'sub/x.txt': 'sub' is a link or not a folder"]
        );
        assert!(names_in(&outside).is_empty(), "nothing is written where the link leads");
    }

    #[test]
    fn a_fifo_at_an_input_s_path_does_not_hang_the_diff() {
        let case = Case::new("fifo");
        case.laid_out();
        let fifo = CString::new(format!("{}/a.txt", case.work())).expect("the path holds no NUL");
        // SAFETY: `fifo` is a NUL-terminated path that lives across the call; `mkfifo` only reads it.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "{}", std::io::Error::last_os_error());

        let (sender, receiver) = mpsc::channel();
        let json = case.json("diff");
        thread::spawn(move || {
            let (status, console) = answer(&json, manifest_of(&[("a.txt", b"a\n")]).line().as_bytes());
            sender.send((status, console.out))
        });
        let (status, out) = receiver.recv_timeout(HANG_GUARD).expect("the diff does not wait on the FIFO");

        assert_eq!(status, 0);
        assert_eq!(out, ["gate-remote-inputs: need 1", "gate-remote-inputs: path a.txt"]);
    }

    #[test]
    fn an_input_arrives_by_rename_and_the_file_it_replaces_keeps_its_bytes() {
        let case = Case::new("rename-in");
        case.laid_out();
        let path = format!("{}/a.txt", case.work());
        fs::write(&path, "old\n").expect("the old file is written");
        let mut old = File::open(&path).expect("the old file is held open");
        let old_inode = old.metadata().expect("the old file is looked at").ino();
        let files: [(&str, &[u8]); 1] = [("a.txt", b"new\n")];

        let (status, console) = case.put(&files, &files, &[]);

        assert_eq!(status, 0, "{:?}", console.err);
        let mut bytes = Vec::new();
        old.read_to_end(&mut bytes).expect("the old file is read");
        assert_eq!(bytes, b"old\n", "nothing was written through the old file");
        assert_eq!(case.read("a.txt"), b"new\n");
        assert_ne!(fs::metadata(&path).expect("the input is looked at").ino(), old_inode);
    }

    #[test]
    fn a_staged_file_changed_after_the_unpack_is_not_installed() {
        let case = Case::new("staged-changed");
        let work = case.work_held();
        let (path, folder, left) = case.stage(&[("a.txt", b"a\n")]);
        fs::OpenOptions::new()
            .append(true)
            .open(path.join("a.txt"))
            .and_then(|mut file| file.write_all(b"more\n"))
            .expect("the staged file is changed");
        let staged = Staged {
            folder: &folder,
            path: &path,
            left: &left,
            entries: &[],
        };

        let moved = moved_in(&staged, &work, "a.txt");

        assert_eq!(
            moved.err(),
            Some("cannot install 'a.txt': its staged copy changed after the unpack".to_owned())
        );
        assert!(!case.has("a.txt"));
    }

    #[test]
    fn a_way_through_the_work_folder_s_git_by_another_name_is_refused() {
        let case = Case::new("git-by-id");
        let mut work = case.work_held();
        fs::create_dir(format!("{}/sub", case.work())).expect("a folder is made");
        // A folder standing for `.git` reached by another spelling: the walk knows `.git` by its identity alone.
        let sub = Facts::of(&work.folder.stat_child(OsStr::new("sub")).expect("the folder is looked at"));
        work.git = Some((sub.dev, sub.ino));
        let (path, folder, left) = case.stage(&[]);
        let staged = Staged {
            folder: &folder,
            path: &path,
            left: &left,
            entries: &[],
        };

        let freed = free_target(&work, "sub/x.txt", &Record::default(), &Manifest::default());
        let moved = moved_in(&staged, &work, "sub/x.txt");

        assert_eq!(freed, Err("cannot install 'sub/x.txt': 'sub' is the work folder's .git".to_owned()));
        assert_eq!(
            moved.err(),
            Some("cannot install 'sub/x.txt': 'sub' is the work folder's .git".to_owned())
        );
    }

    #[test]
    fn a_put_whose_lock_was_lost_changes_nothing_below_work() {
        let case = Case::new("lock-lost");
        let payload = case.payload("put");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        fs::remove_file(case.lock()).expect("another removes the lock file");
        let files: [(&str, &[u8]); 1] = [("a.txt", b"a\n")];
        let (path, folder, left) = case.stage(&files);
        let entries = [entry("a.txt", b"a\n")];
        let staged = Staged {
            folder: &folder,
            path: &path,
            left: &left,
            entries: &entries,
        };
        let sent = sent_of(&files);
        let mut console = Console::captured();

        let done = installed(Job { asked: &asked, sent: &sent }, &staged, &work, &lock, &mut console);

        assert_eq!(done, Err(format!("the work folder lock {} is no longer held", case.lock())));
        assert!(!case.has("a.txt"), "nothing is installed");
        assert!(!Path::new(&case.record_path()).exists(), "no record is written");
    }

    #[test]
    fn a_put_leaving_a_manifest_file_missing_leaves_the_record_unsealed() {
        let case = Case::new("unsealed");
        let files: [(&str, &[u8]); 2] = [("a.txt", b"a\n"), ("b.txt", b"b\n")];

        let (status, console) = case.put(&files, &files[..1], &[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(console.err, ["gate: remote inputs put failed: b.txt is missing after the install"]);
        let record = case.record();
        assert_eq!(record["digest"], "", "no digest until every file is there");
        assert!(record["files"]["a.txt"].is_object(), "{record}");
    }

    #[test]
    fn a_put_rechecks_a_file_the_box_changed_after_its_diff() {
        let case = Case::new("recheck");
        case.laid_out();
        fs::write(format!("{}/b.txt", case.work()), "B\n").expect("the box's copy is changed");
        let files: [(&str, &[u8]); 2] = [("a.txt", b"a\n"), ("b.txt", b"b\n")];

        let (status, console) = case.put(&files, &files[..1], &[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(
            console.err,
            ["gate: remote inputs put failed: b.txt does not hold what the manifest names after the install"]
        );
        assert_eq!(case.record()["digest"], "");
    }

    #[test]
    fn a_stale_copy_is_deleted_only_while_it_holds_what_was_installed() {
        let case = Case::new("stale");
        let first: [(&str, &[u8]); 2] = [("a.txt", b"a\n"), ("b.txt", b"b\n")];
        let (status, console) = case.put(&first, &first, &[]);
        assert_eq!(status, 0, "{:?}", console.err);
        fs::write(format!("{}/b.txt", case.work()), "B\n").expect("the box changes a copy");
        let second: [(&str, &[u8]); 1] = [("c.txt", b"c\n")];

        let (status, console) = case.put(&second, &second, &[]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert!(!case.has("a.txt"), "the unchanged stale copy is deleted");
        assert_eq!(case.read("b.txt"), b"B\n", "the changed one is kept");
        assert_eq!(case.record()["files"]["b.txt"], json!({ "kept": true }));
        assert_eq!(case.exclude(), "/b.txt\n/c.txt\n");
    }

    #[test]
    fn a_record_past_its_cap_is_refused_whole_and_removed() {
        let case = Case::new("record-cap");
        let payload = case.payload("diff");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        let path = case.record_path();
        File::create(&path)
            .and_then(|file| file.set_len(RECORD_CAP + 1))
            .expect("a long record is made");
        let mut console = Console::captured();

        let record = recorded(&asked, &work, &lock, &mut console).expect("the record is removed");

        assert!(record.files.is_empty() && record.digest.is_empty());
        assert_eq!(
            console.err,
            [format!(
                "gate: remote removed the inputs record {path}, which cannot be read: it is longer than {RECORD_CAP} bytes; every copy is hashed"
            )]
        );
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn a_record_past_its_cap_is_never_written() {
        let case = Case::new("record-write-cap");
        let path = case.record_path();
        let cap = Record::default().text().len() as u64;

        let refused = write_record_capped(&path, &Record::default(), cap - 1);

        assert_eq!(
            refused,
            Err(format!("cannot write {path}: the record would be longer than {} bytes", cap - 1))
        );
        assert!(!Path::new(&path).exists(), "nothing is written");
        assert_eq!(
            write_record_capped(&path, &Record::default(), cap),
            Ok(()),
            "a record at the cap is written"
        );
    }

    #[test]
    fn a_record_naming_a_path_that_is_not_plain_is_not_a_record() {
        let entry = json!({ "size": 1, "sha256": "0".repeat(64), "modified": 1, "id": "1-2" });
        let record_of = |path: &str| {
            let mut files = Map::new();
            files.insert(path.to_owned(), entry.clone());
            Record::parse(&json!({ "digest": "", "files": files }).to_string()).err()
        };

        for path in ["../x", "a/../b", "/abs", ".git/config"] {
            assert_eq!(record_of(path), Some("is not an inputs record".to_owned()), "{path}");
        }
        assert_eq!(record_of("a/b.txt"), None);
    }

    #[test]
    fn the_stage_is_made_new_and_private_and_an_open_stages_folder_is_refused() {
        let case = Case::new("stage");
        let payload = case.payload("put");
        let asked = Asked::read(&payload).expect("the request reads");
        let stages = case.root().join("inputs-stage");
        fs::create_dir(&stages).expect("the stages folder is made");
        fs::set_permissions(&stages, fs::Permissions::from_mode(0o755)).expect("its mode is set");
        let mut console = Console::captured();

        let refused = made_stage(&asked, SystemTime::now(), &mut console).map(drop);
        assert_eq!(
            refused,
            Err(format!("cannot use {}: its owner or mode is not the box's; remove it", stages.display()))
        );

        fs::set_permissions(&stages, fs::Permissions::from_mode(0o700)).expect("its mode is set");
        let stage = made_stage(&asked, SystemTime::now(), &mut console).expect("the stage is made");
        let name = format!("{KEY}-{NONCE}");
        assert_eq!(mode_of(&stages.join(&name)), 0o700);
        assert_eq!(mode_of(&stages.join(format!("{name}.lock"))), 0o600);
        assert!(
            made_stage(&asked, SystemTime::now(), &mut console).is_err(),
            "a stage of that name is never shared"
        );
        stage.removed().expect("the stage is removed");
        assert!(names_in(&stages).is_empty(), "{:?}", names_in(&stages));
        drop(stage);

        fs::remove_dir(&stages).expect("the stages folder is removed");
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the folder a link leads to is made");
        symlink(&elsewhere, &stages).expect("a link at the stages folder is made");
        let refused = made_stage(&asked, SystemTime::now(), &mut console).map(drop);
        assert_eq!(refused, Err(format!("{} is a link or not a folder", stages.display())));
        assert!(names_in(&elsewhere).is_empty(), "nothing is made where the link leads");
    }

    #[test]
    fn a_root_others_may_write_or_a_linked_bare_fails_before_the_lock() {
        let case = Case::new("root-checked");
        let root = case.root().display().to_string();
        fs::set_permissions(case.root(), fs::Permissions::from_mode(0o770)).expect("the root's mode is set");

        let (status, console) = case.diff(&[]);

        assert_eq!(status, ROOT_STATUS);
        let user = files::user_name(files::effective_uid());
        assert_eq!(
            console.err,
            [root_unsafe_line(
                &format!("{root} is not owned by {user} or is open to others"),
                WORK_ROOT_STEP
            )]
        );
        assert!(!Path::new(&case.lock()).exists(), "no lock is taken");

        fs::set_permissions(case.root(), fs::Permissions::from_mode(0o700)).expect("the root's mode is set");
        let elsewhere = case.base.join("elsewhere");
        fs::create_dir(&elsewhere).expect("the folder a link leads to is made");
        symlink(&elsewhere, case.root().join("repo.git")).expect("a link at the bare repository is made");

        let (status, console) = case.diff(&[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(console.err, [folder_failure(&format!("{root}/repo.git"), "it is a symbolic link")]);
        assert!(!Path::new(&case.lock()).exists(), "no lock is taken");
    }

    #[test]
    fn a_sent_build_is_refused_as_not_built_before_the_manifest_is_read() {
        let case = Case::new("sent-bin");
        let mut json = case.json("put");
        json["SentBin"] = json!("app/bin/Debug");

        let (status, console) = answer(&json, b"not a manifest\n");

        assert_eq!(status, REFUSED_STATUS);
        assert_eq!(
            console.err,
            [
                refused_line(NONCE, Refusal::Sent),
                "gate: remote SentBin is not built on this builder".to_owned()
            ]
        );
    }

    #[test]
    fn a_member_spelt_otherwise_than_the_manifest_is_refused() {
        let case = Case::new("spelling");

        let (status, console) = case.put(&[("a.txt", b"a\n")], &[("A.txt", b"a\n")], &[]);

        assert_eq!(status, FAILED_STATUS);
        assert_eq!(
            console.err,
            ["gate: remote inputs put failed: the archive's member 'A.txt' is not in the manifest"]
        );
        assert!(!case.has("a.txt"), "nothing is installed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_stale_copy_spelt_otherwise_that_names_a_matched_file_is_never_unlinked() {
        let case = Case::new("nfd-nfc");
        let (nfd, nfc) = ("e\u{301}.txt", "\u{e9}.txt");
        let first: [(&str, &[u8]); 1] = [(nfd, b"e\n")];
        let (status, console) = case.put(&first, &first, &[]);
        assert_eq!(status, 0, "{:?}", console.err);
        let second: [(&str, &[u8]); 1] = [(nfc, b"e\n")];

        let (status, console) = case.diff(&second);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-inputs: need 0"]);
        assert_eq!(case.read(nfc), b"e\n", "the file the manifest names is never swept");
        assert_eq!(case.record()["digest"], sent_of(&second).digest);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_case_only_rename_never_unlinks_the_file_the_new_spelling_names() {
        let case = Case::new("case-rename");
        let first: [(&str, &[u8]); 1] = [("A.txt", b"a\n")];
        let (status, console) = case.put(&first, &first, &[]);
        assert_eq!(status, 0, "{:?}", console.err);

        let (status, console) = case.diff(&[("a.txt", b"a\n")]);

        assert_eq!(status, 0, "{:?}", console.err);
        assert_eq!(console.out, ["gate-remote-inputs: need 0"]);
        assert_eq!(case.read("a.txt"), b"a\n", "the file the new spelling names is kept");
    }

    #[test]
    fn a_copy_changed_before_the_seal_leaves_the_record_unsealed() {
        let case = Case::new("seal-recheck");
        let payload = case.payload("diff");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        let path = format!("{}/a.txt", case.work());
        fs::write(&path, "a\n").expect("the copy is written");
        let stamp = opened_copy(&work, "a.txt")
            .expect("the copy opens")
            .expect("the copy is there")
            .facts
            .stamp();
        let mut now = Record::default();
        let copy = Installed {
            path: "a.txt".to_owned(),
            sha256: sha(b"a\n"),
            stamp,
        };
        now.files.insert(ordinal_upper("a.txt"), copy);
        fs::write(&path, "changed\n").expect("the box changes the copy");
        let sent = sent_of(&[("a.txt", b"a\n")]);

        let done = finished(Job { asked: &asked, sent: &sent }, &work, &Record::default(), now, &lock);

        assert_eq!(done, Err("a.txt changed before the record was sealed".to_owned()));
        assert_eq!(case.record()["digest"], "", "the record is left unsealed");
    }

    #[test]
    fn a_record_not_utf8_or_of_two_names_is_removed_as_unreadable() {
        let case = Case::new("record-unreadable");
        let payload = case.payload("diff");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        let path = case.record_path();
        let line = |why: &str| format!("gate: remote removed the inputs record {path}, which cannot be read: {why}; every copy is hashed");

        fs::write(&path, [0xff, 0xfe]).expect("a record that is not UTF-8 is written");
        let mut console = Console::captured();
        assert!(recorded(&asked, &work, &lock, &mut console).is_ok());
        assert_eq!(console.err, [line("it is not UTF-8")]);
        assert!(!Path::new(&path).exists());

        fs::write(&path, "{}").expect("a record is written");
        let other = case.base.join("other-name");
        fs::hard_link(&path, &other).expect("a second name is made");
        let mut console = Console::captured();
        assert!(recorded(&asked, &work, &lock, &mut console).is_ok());
        assert_eq!(console.err, [line("it has 2 names")]);
        assert!(!Path::new(&path).exists());
        assert!(other.exists(), "the other name is left");
    }

    #[test]
    fn a_lost_lock_keeps_an_unreadable_record_in_place() {
        let case = Case::new("record-lock-lost");
        let payload = case.payload("diff");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        fs::remove_file(case.lock()).expect("another removes the lock file");
        fs::create_dir(case.record_path()).expect("a folder at the record's name is made");
        let mut console = Console::captured();

        let read = recorded(&asked, &work, &lock, &mut console).err();

        assert_eq!(read, Some(format!("the work folder lock {} is no longer held", case.lock())));
        assert!(Path::new(&case.record_path()).is_dir(), "nothing is removed");
    }

    #[test]
    fn a_lost_lock_writes_no_unsealed_record_when_a_file_is_missing() {
        let case = Case::new("finish-lock-lost");
        let payload = case.payload("diff");
        let asked = Asked::read(&payload).expect("the request reads");
        let work = case.work_held();
        let lock = files::held_for_test(Path::new(&case.lock()));
        fs::remove_file(case.lock()).expect("another removes the lock file");
        let sent = sent_of(&[("a.txt", b"a\n")]);

        let done = finished(Job { asked: &asked, sent: &sent }, &work, &Record::default(), Record::default(), &lock);

        assert_eq!(done, Err(format!("the work folder lock {} is no longer held", case.lock())));
        assert!(!Path::new(&case.record_path()).exists(), "no record is written");
    }

    #[test]
    fn a_leftover_exclude_temp_of_this_process_is_replaced() {
        let case = Case::new("exclude-leftover");
        let work = case.work_held();
        let info = format!("{}/.git/info", case.work());
        // The first temp name this process gives: each case runs in a process of its own under nextest.
        let leftover = format!("{info}/exclude.gate-new-{}-0", std::process::id());
        fs::write(&leftover, "left\n").expect("a leftover temp file is planted");

        let moved = write_exclude(&work, &Record::default()).expect("the exclude file is written");

        assert_eq!(moved, None);
        assert!(!Path::new(&leftover).exists(), "the leftover is gone");
        assert_eq!(fs::read_to_string(format!("{info}/exclude")).expect("the exclude file is read"), "");
    }

    /// Sets the last write of the file or folder at `path` to `when`.
    fn aged(path: &Path, when: SystemTime) {
        File::open(path).and_then(|file| file.set_modified(when)).expect("the last write is set");
    }

    #[test]
    fn an_old_entry_at_a_stage_s_lock_name_that_is_no_file_is_removed_as_itself() {
        let case = Case::new("sweep-lock-folder");
        let stages_path = case.base.join("stages");
        let stage = format!("{KEY}-{}", "4".repeat(32));
        fs::create_dir_all(stages_path.join(&stage)).expect("the stage is made");
        let lock = stages_path.join(format!("{stage}.lock"));
        fs::create_dir_all(lock.join("inner")).expect("a folder at the lock's name is made");
        aged(&lock, SystemTime::now() - Duration::from_hours(48));
        let stages = Folder::open(&stages_path).expect("the stages folder opens");

        let lines = pruned(&stages, &stages_path, SystemTime::now());

        assert!(lines.is_empty(), "{lines:?}");
        assert!(names_in(&stages_path).is_empty(), "{:?}", names_in(&stages_path));
    }

    #[test]
    fn an_old_stage_with_no_lock_file_is_swept_and_a_young_one_kept() {
        let case = Case::new("sweep-unlocked");
        let stages_path = case.base.join("stages");
        let (old, young) = (format!("{KEY}-{}", "5".repeat(32)), format!("{KEY}-{}", "6".repeat(32)));
        for stage in [&old, &young] {
            fs::create_dir_all(stages_path.join(stage).join("dir")).expect("a stage is made");
        }
        aged(&stages_path.join(&old), SystemTime::now() - Duration::from_hours(48));
        let stages = Folder::open(&stages_path).expect("the stages folder opens");

        let lines = pruned(&stages, &stages_path, SystemTime::now());

        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(names_in(&stages_path), [young]);
    }

    #[test]
    fn only_an_old_stage_whose_lock_is_free_is_swept() {
        let case = Case::new("sweep");
        let stages_path = case.base.join("stages");
        let old = SystemTime::now() - Duration::from_hours(48);
        let stage = |digit: char| format!("{KEY}-{}", digit.to_string().repeat(32));
        let (free, held, young) = (stage('1'), stage('2'), stage('3'));
        for name in [&free, &held, &young, &"other".to_owned()] {
            fs::create_dir_all(stages_path.join(name)).expect("a stage is made");
            let lock = File::create(stages_path.join(format!("{name}.lock"))).expect("its lock is made");
            if name != &young {
                lock.set_modified(old).expect("its lock is aged");
            }
        }
        let outside = case.base.join("outside");
        fs::create_dir(&outside).expect("the outside folder is made");
        fs::write(outside.join("kept.txt"), "kept\n").expect("the outside file is written");
        symlink(&outside, stages_path.join(&free).join("out")).expect("a link in the old stage is made");
        let live = File::open(stages_path.join(format!("{held}.lock"))).expect("the live put's lock opens");
        assert!(flock_taken(&live).expect("the live put locks"), "the live put holds its lock");
        let stages = Folder::open(&stages_path).expect("the stages folder opens");

        let lines = pruned(&stages, &stages_path, SystemTime::now());

        assert!(lines.is_empty(), "{lines:?}");
        let mut kept = vec![held.clone(), format!("{held}.lock"), "other".to_owned(), "other.lock".to_owned()];
        kept.extend([young.clone(), format!("{young}.lock")]);
        kept.sort();
        assert_eq!(names_in(&stages_path), kept);
        assert_eq!(fs::read(outside.join("kept.txt")).expect("the outside file is read"), b"kept\n");
    }

    #[test]
    fn a_copy_is_made_only_from_a_member_and_only_of_its_item_s_bytes() {
        let case = Case::new("copies");
        let (path, folder, _) = case.stage(&[]);
        let mut place = Staging::new(&folder, &path);
        place.member("a.txt", SystemTime::now(), &mut &b"a\n"[..]).expect("the member is staged");

        let from_pc = entry("p.txt", b"a\n");
        let refused = place
            .copy(&from_pc, SystemTime::now(), CopyFrom::Pc)
            .expect_err("a copy from the PC is refused");
        assert_eq!(refused.to_string(), no_pc_file(&from_pc, &[]).to_string());
        assert!(!path.join("p.txt").exists(), "no box file stands in for the PC's");

        let other = entry("o.txt", b"b\n");
        let mismatch = place
            .copy(&other, SystemTime::now(), CopyFrom::Member("a.txt"))
            .expect_err("a copy of other bytes is refused");
        assert_eq!(mismatch.to_string(), copy_mismatch(&other, 2, &sha(b"a\n")).to_string());
    }

    #[test]
    fn the_exclude_file_lists_exactly_the_recorded_paths_and_a_folder_there_is_moved_aside() {
        let case = Case::new("exclude");
        let work = case.work_held();
        let exclude = format!("{}/.git/info/exclude", case.work());
        fs::create_dir_all(format!("{exclude}/inner")).expect("a folder at the exclude file's name is made");
        let mut record = Record::default();
        for path in ["b[1].txt", "a.txt"] {
            let stamp = Stamp {
                size: 0,
                modified: 0,
                id: String::new(),
            };
            let copy = Installed {
                path: path.to_owned(),
                sha256: "0".repeat(64),
                stamp,
            };
            record.files.insert(ordinal_upper(path), copy);
        }
        record.kept.insert(ordinal_upper("k*.txt"), "k*.txt".to_owned());

        let moved = write_exclude(&work, &record).expect("the exclude file is written");

        assert_eq!(
            moved,
            Some(format!("gate: remote moved aside the folder {exclude} to {exclude}.gate-moved-1"))
        );
        assert_eq!(
            fs::read_to_string(&exclude).expect("the exclude file is read"),
            "/a.txt\n/b\\[1].txt\n/k\\*.txt\n"
        );
        assert!(Path::new(&format!("{exclude}.gate-moved-1/inner")).is_dir(), "the folder moved whole");
    }

    #[test]
    fn a_stage_on_another_volume_than_the_work_folder_fails_the_put() {
        let stage = Path::new("/r/inputs-stage/s");

        assert_eq!(
            volume_checked(1, 2, stage, "/r/work/k"),
            Err("the stage /r/inputs-stage/s is on another volume than the work folder /r/work/k".to_owned())
        );
        assert_eq!(volume_checked(1, 1, stage, "/r/work/k"), Ok(()));
    }

    #[test]
    fn a_copy_with_a_negative_mtime_is_hashed_every_time() {
        let case = Case::new("negative-mtime");
        let work = case.work_held();
        let path = format!("{}/a.txt", case.work());
        let before_1970 = UNIX_EPOCH - Duration::from_secs(10);
        let written = |bytes: &[u8]| {
            fs::write(&path, bytes)
                .and_then(|()| File::options().write(true).open(&path))
                .and_then(|file| file.set_modified(before_1970))
                .expect("the copy is written before 1970");
        };
        written(b"a\n");
        let stamp = opened_copy(&work, "a.txt")
            .expect("the copy opens")
            .expect("the copy is there")
            .facts
            .stamp();
        assert!(stamp.modified < 0, "{stamp:?}");
        let mut record = Record::default();
        let copy = Installed {
            path: "a.txt".to_owned(),
            sha256: sha(b"a\n"),
            stamp,
        };
        record.files.insert(ordinal_upper("a.txt"), copy);
        // Same size, same inode, same mtime: only its bytes changed.
        written(b"b\n");
        let held = Held {
            stamp: HeldStamp { size: 2, modified: 0 },
            sha256: sha(b"a\n"),
        };

        assert!(
            matched_copy(&work, "a.txt", &held, &record).is_none(),
            "the copy is hashed, never trusted"
        );
    }

    /// Two inputs, one in a folder of its own.
    const TWO: [(&str, &[u8]); 2] = [("a.txt", b"a\n"), ("d/b.txt", b"b\n")];

    /// `files` put into the work folder of `case` and sealed; the digest of their manifest.
    fn sealed(case: &Case, files: &Files<'_>) -> String {
        let (status, console) = case.put(files, files, &[]);
        assert_eq!(status, 0, "{:?}", console.err);
        sent_of(files).digest
    }

    /// The record of `case` rewritten with `edit` made to its JSON.
    fn record_edited(case: &Case, edit: impl FnOnce(&mut Value)) {
        let mut record = case.record();
        edit(&mut record);
        fs::write(case.record_path(), record.to_string()).expect("the record is rewritten");
    }

    /// The run's check of the work folder `work` for the digest `sent`, its work lock held: why the run is refused, or a
    /// lost lock's line marked as such.
    fn checked(work: &str, sent: &str) -> Result<Option<String>, String> {
        let lock = files::held_for_test(Path::new(&format!("{work}.lock")));
        unmatched(work, sent, &lock).map_err(|unmatched| match unmatched {
            Unmatched::Inputs(why) => why,
            Unmatched::LockLost(why) => format!("lock lost: {why}"),
        })
    }

    /// The run's check of a case `name` whose sealed [`TWO`] had its `a.txt` changed by `change`.
    fn changed(name: &str, change: impl FnOnce(&Path)) -> Result<Option<String>, String> {
        let case = Case::new(name);
        let digest = sealed(&case, &TWO);
        change(Path::new(&format!("{}/a.txt", case.work())));
        checked(&case.work(), &digest)
    }

    #[test]
    fn a_sealed_record_whose_copies_are_unchanged_passes_and_the_exclude_file_lists_each() {
        let case = Case::new("unmatched-pass");
        let digest = sealed(&case, &TWO);
        fs::write(format!("{}/.git/info/exclude", case.work()), "").expect("the exclude file is emptied");

        assert_eq!(checked(&case.work(), &digest), Ok(None));
        assert_eq!(case.exclude(), "/a.txt\n/d/b.txt\n");
    }

    #[test]
    fn a_missing_record_refuses_the_run() {
        let case = Case::new("unmatched-none");
        drop(case.work_held());

        assert_eq!(
            checked(&case.work(), &sent_of(&TWO).digest),
            Err(format!("{} does not exist", case.record_path()))
        );
    }

    #[test]
    fn a_record_of_another_digest_unsealed_or_no_record_at_all_refuses_the_run() {
        let case = Case::new("unmatched-digest");
        let digest = sealed(&case, &TWO);
        let path = case.record_path();
        let another = Err(format!("{path} holds another manifest's digest"));

        assert_eq!(checked(&case.work(), &"f".repeat(64)), another);
        record_edited(&case, |record| record["digest"] = json!(""));
        assert_eq!(checked(&case.work(), &digest), another, "an unsealed record");
        fs::write(&path, "[]").expect("the record is spoilt");
        assert_eq!(checked(&case.work(), &digest), Err(format!("{path} is not an inputs record")));
    }

    #[test]
    fn a_record_that_is_a_link_cannot_be_read_and_what_it_names_is_left() {
        let case = Case::new("unmatched-record-link");
        let digest = sealed(&case, &TWO);
        let path = case.record_path();
        let aside = format!("{path}.aside");
        fs::rename(&path, &aside).expect("the record is moved aside");
        symlink(&aside, &path).expect("a link takes the record's name");

        let refused = checked(&case.work(), &digest).expect_err("a linked record is refused");

        assert!(refused.starts_with(&format!("{path} cannot be read: ")), "{refused}");
        assert!(Path::new(&aside).is_file(), "what the link names is left");
    }

    #[test]
    fn a_copy_changed_in_size_mtime_or_inode_is_not_the_copy_installed() {
        let not_installed = Err("the input a.txt is not the copy installed".to_owned());
        let size = changed("unmatched-size", |path| fs::write(path, "aa\n").expect("the copy grows"));
        assert_eq!(size, not_installed, "its size");
        let mtime = changed("unmatched-mtime", |path| {
            File::options()
                .write(true)
                .open(path)
                .and_then(|file| file.set_modified(UNIX_EPOCH + Duration::from_secs(WRITTEN + 1)))
                .expect("the copy's mtime moves");
        });
        assert_eq!(mtime, not_installed, "its mtime");
        let inode = changed("unmatched-inode", |path| {
            let modified = fs::metadata(path).and_then(|metadata| metadata.modified()).expect("the mtime is read");
            let other = path.with_file_name("other");
            fs::write(&other, "z\n")
                .and_then(|()| File::options().write(true).open(&other))
                .and_then(|file| file.set_modified(modified))
                .and_then(|()| fs::rename(&other, path))
                .expect("another file of the same size and mtime takes the copy's name");
        });
        assert_eq!(inode, not_installed, "its inode");
    }

    #[test]
    fn a_copy_missing_a_link_or_a_folder_refuses_the_run() {
        let missing = changed("unmatched-missing", |path| fs::remove_file(path).expect("the copy goes"));
        assert_eq!(missing, Err("the input a.txt is missing".to_owned()));
        let link = changed("unmatched-copy-link", |path| {
            fs::remove_file(path)
                .and_then(|()| symlink("d/b.txt", path))
                .expect("a link takes the copy's name");
        });
        let folder = changed("unmatched-copy-folder", |path| {
            fs::remove_file(path)
                .and_then(|()| fs::create_dir(path))
                .expect("a folder takes the copy's name");
        });
        for (refused, what) in [(link, "a link"), (folder, "a folder")] {
            let why = refused.expect_err(what);
            assert!(why.starts_with("the input a.txt cannot be opened: "), "{what}: {why}");
        }
    }

    #[test]
    fn a_copy_with_a_negative_mtime_passes_only_while_its_bytes_hash_to_the_record() {
        let case = Case::new("unmatched-negative");
        let digest = sealed(&case, &TWO);
        let path = format!("{}/a.txt", case.work());
        let written = |bytes: &[u8]| {
            fs::write(&path, bytes)
                .and_then(|()| File::options().write(true).open(&path))
                .and_then(|file| file.set_modified(UNIX_EPOCH - Duration::from_secs(10)))
                .expect("the copy is written before 1970");
        };
        let stamp = || {
            let work = Work::held(&case.work()).expect("the work folder is held");
            opened_copy(&work, "a.txt")
                .expect("the copy opens")
                .expect("the copy is there")
                .facts
                .stamp()
        };
        written(b"a\n");
        let before = stamp();
        assert!(before.modified < 0, "{before:?}");
        record_edited(&case, |record| record["files"]["a.txt"]["modified"] = json!(before.modified));

        assert_eq!(checked(&case.work(), &digest), Ok(None), "the same bytes, hashed, pass");
        written(b"z\n");
        assert_eq!(stamp(), before, "only its bytes changed");
        assert_eq!(
            checked(&case.work(), &digest),
            Err("the input a.txt is not the copy installed".to_owned())
        );
    }

    #[test]
    fn a_folder_at_the_exclude_file_s_name_is_moved_aside_and_its_line_returned() {
        let case = Case::new("unmatched-moved");
        let digest = sealed(&case, &TWO);
        let exclude = format!("{}/.git/info/exclude", case.work());
        fs::remove_file(&exclude)
            .and_then(|()| fs::create_dir(&exclude))
            .expect("a folder takes the exclude file's name");

        assert_eq!(
            checked(&case.work(), &digest),
            Ok(Some(format!("gate: remote moved aside the folder {exclude} to {exclude}.gate-moved-1")))
        );
        assert_eq!(case.exclude(), "/a.txt\n/d/b.txt\n");
    }

    #[test]
    fn a_work_folder_others_may_write_is_not_held() {
        let case = Case::new("open-work");
        fs::create_dir(case.work()).expect("the work folder is made");
        fs::set_permissions(case.work(), fs::Permissions::from_mode(0o775)).expect("the work folder is opened to the group");

        let refused = Work::held(&case.work()).map(drop).expect_err("an open work folder is not held");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        let user = files::user_name(files::effective_uid());
        assert_eq!(
            refused.to_string(),
            format!("{} is not owned by {user} or is writable by others", case.work())
        );
    }
}

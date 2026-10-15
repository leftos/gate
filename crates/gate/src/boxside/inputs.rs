//! `gate box inputs`: a routed run's ignored inputs, sent from the PC before the run. Its request names the `Step`:
//! `diff` reads the PC's inputs manifest and says which files the work folder lacks or holds otherwise, and `put` reads
//! the manifest and then an archive of those files, in the outputs archive's format, to the end of stdin, and installs
//! them. A put unpacks its archive into a stage of its own before it takes any lock; then both take the work folder's
//! lock as a run does and lay the work folder out as a run does first, so a later run finds the layout unchanged and
//! clears nothing.
//!
//! What a put installed is kept in the inputs record `{Work}.inputs`, beside the tools and layout records: the
//! manifest's digest, and per file its path, size, SHA-256, and the last write and file id the box left it with. A diff
//! trusts a copy whose size, last write and file id the record holds and hashes any other; one that needs nothing
//! finishes as a put of nothing would. A put deletes a recorded copy that left the manifest only while it still holds
//! what was installed, rewrites the tree's `.git\info\exclude` from the record, so no input is cleaned by the checkout
//! or committed back, and writes the record's digest last, only once the work folder holds every file of the manifest.
//!
//! The box runs elevated while the command and any other process of the builder's user may write in the work folder,
//! so every file and folder the box makes, moves, writes or deletes below it is reached through handles opened one name
//! at a time beneath the held work folder, never through a link, and is acted on through the handle it was checked by.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gate_args::BoxSeams;
use gate_win32::{Make, Share};
use serde_json::{Map, Value, json};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::payload::{OUTPUTS_NONCE_DIGITS, Payload, escaped, grandparent, ordinal_upper};
use crate::boxside::preflight::{self, is_folder_itself};
use crate::boxside::run::{self, FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, REFUSED_STATUS};
use crate::boxside::tools;
use crate::console::Console;
use crate::outputs::archive::{Entry, PcFiles, unpack};
use crate::outputs::hash::{SHA256_DIGITS, is_lower_hex, sha256_of};
use crate::outputs::manifest::{Held, MANIFEST_LIMIT, Manifest};
use crate::outputs::names::{is_git_s, is_lfs_config, is_plain_path};
use crate::remote::{Refusal, busy_line, refused_line as tagged_line};

/// The bytes in one gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most bytes one put unpacks, decompressed: 4 GiB, unless the `--test-inputs-budget-bytes` seam says otherwise.
const INPUTS_BUDGET: u64 = 4 * GIB;
/// The folder below the box's root that holds each put's stage, `{root}\inputs-stage\{key}-{nonce}`.
const STAGES: &str = "inputs-stage";
/// The folder in a stage the archive is unpacked into.
const STAGED_FILES: &str = "files";
/// How old a stage is before any put removes it: a put that left one ended a day ago.
const STAGE_AGE: Duration = Duration::from_hours(24);
/// The line refusing a request that names a sibling pair.
const PAIR_REFUSED: &str = "gate box: inputs for a sibling pair are not taken yet";
/// The access a staged file is opened with to be moved into the work folder: its stamp read, its name moved, its DACL
/// and label set.
const STAGED_ACCESS: u32 = FILE_READ_ATTRIBUTES | DELETE | READ_CONTROL | WRITE_DAC | WRITE_OWNER;

/// The record of the inputs installed in the work folder `work`: `{work}.inputs`.
pub(super) fn record_path(work: &str) -> String {
    format!("{work}.inputs")
}

/// The tag a run whose `InputsDigest` the work folder's inputs record does not hold is refused with.
pub(super) fn refused_line(nonce: &str) -> String {
    format!("gate-remote-refused: {nonce} inputs")
}

/// Why the work folder `work`, held as `held`, does not hold the inputs of the manifest whose digest is `sent`: no
/// inputs record, one that cannot be read or of another digest, a recorded copy gone or not the one installed (its
/// size, last write or file id differs), or a tree whose exclude file cannot be written from the record; `None` when
/// it holds them, the exclude file written.
pub(super) fn unmatched(work: &str, held: &File, sent: &str) -> Option<String> {
    let record = match matching_record(work, sent) {
        Ok(record) => record,
        Err(why) => return Some(why),
    };
    if let Some(why) = changed_copy(held, &record) {
        return Some(why);
    }
    write_exclude(held, work, &record).err()
}

/// The work folder `work`'s inputs record when it holds the digest `sent`; else why not.
fn matching_record(work: &str, sent: &str) -> Result<Record, String> {
    let path = record_path(work);
    match tools::read_record(&path) {
        Ok(None) => Err(format!("{path} does not exist")),
        Ok(Some(text)) => match Record::parse(&text) {
            Ok(record) if record.digest == sent => Ok(record),
            Ok(_) => Err(format!("{path} holds another manifest's digest")),
            Err(why) => Err(format!("{path} {why}")),
        },
        Err(error) => Err(format!("{path} cannot be read: {error}")),
    }
}

/// Why a copy `record` holds is not in the work folder `work` as installed; `None` when each is, by its stamp alone.
fn changed_copy(work: &File, record: &Record) -> Option<String> {
    record
        .files
        .values()
        .find_map(|copy| match opened_copy(work, &copy.path, FILE_GENERIC_READ) {
            Ok(Some((_, stamp))) if stamp == copy.stamp => None,
            Ok(Some(_)) => Some(format!("the input {} is not the copy installed", copy.path)),
            Ok(None) => Some(format!("the input {} is missing", copy.path)),
            Err(error) => Some(format!("the input {} cannot be opened: {error}", copy.path)),
        })
}

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

/// What an inputs request reads of its payload.
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
            grandparent(work).ok_or_else(|| format!(r"gate: box payload Work must be <root>\work\<key> to stage inputs: {}", escaped(work)))?;
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

    /// The line saying the step failed for `why`.
    fn failed(&self, why: &str) -> String {
        format!("gate: remote inputs {} failed: {}", self.step.word(), escaped(why))
    }
}

/// The PC's inputs manifest, and the SHA-256 of its line as received, without its line end.
struct Sent {
    manifest: Manifest,
    digest: String,
}

/// The manifest line read from `input`, up to its first newline or the input's end and never more than
/// [`MANIFEST_LIMIT`] bytes, without its `\n` or `\r\n`; else why it cannot be read, a path in it the box does not
/// take ([`refused_path`]) among them.
fn read_manifest(input: impl BufRead) -> Result<Sent, String> {
    let mut line = super::bounded_line(input, MANIFEST_LIMIT as u64).map_err(|unread| match unread {
        super::Unread::OverLimit => "the inputs manifest reaches 64 MiB without a newline".to_owned(),
        super::Unread::Failed(error) => format!("the inputs manifest could not be read: {error}"),
    })?;
    if line.ends_with(b"\n") {
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
    }
    let (digest, _) = sha256_of(line.as_slice()).map_err(|error| format!("the inputs manifest could not be hashed: {error}"))?;
    let text = String::from_utf8(line).map_err(|_| "the inputs manifest is not UTF-8".to_owned())?;
    let manifest = Manifest::parse(&text).map_err(|why| why.replacen("the outputs manifest", "the inputs manifest", 1))?;
    if let Some((path, why)) = manifest.files().find_map(|(path, _)| refused_path(path).map(|why| (path, why))) {
        return Err(format!("the inputs manifest names '{}', {why}", escaped(path)));
    }
    Ok(Sent { manifest, digest })
}

/// Why the box does not take the manifest's `path`: one that is not a plain relative path, or one the PC never sends
/// (a `.git*` name, a `.lfsconfig`, or a `.env` or `*.env` file); `None` when it takes it.
fn refused_path(path: &str) -> Option<&'static str> {
    if !is_plain_path(path) {
        return Some("which is not a plain relative path");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let env = name.to_ascii_lowercase().ends_with(".env");
    (is_git_s(path) || is_lfs_config(path) || env).then_some("which inputs never carry")
}

/// Answers `gate box inputs`: the request read, then the manifest from stdin, before any lock is taken, so a client
/// that stalls holds nothing; then the step. A payload refused is a usage error (2), a manifest that cannot be read
/// fails the step (1).
pub(super) fn inputs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let asked = match Asked::read(payload) {
        Ok(asked) => asked,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    let sent = match read_manifest(io::stdin().lock()) {
        Ok(sent) => sent,
        Err(why) => return failed(&asked, &why, console),
    };
    match asked.step {
        Step::Diff => match locked(&asked, console, |work, console| diff(&asked, &sent, work, console)) {
            Ok(Ok(())) => 0,
            Ok(Err(why)) => failed(&asked, &why, console),
            Err(status) => status,
        },
        Step::Put => put(&asked, &sent, seams, console),
    }
}

/// Says the step failed for `why`; 1.
fn failed(asked: &Asked<'_>, why: &str, console: &mut Console) -> u32 {
    console.err(&asked.failed(why));
    FAILED_STATUS
}

/// `step` run on the work folder, held, under its lock: the lock taken as a run takes it, a held lock refused `busy`
/// (75), and the work folder laid out as a run lays it out ([`run::lay_out_alone`]) and held through the step. `Err`
/// with the status to answer when the step never ran, its lines said.
fn locked<T>(
    asked: &Asked<'_>,
    console: &mut Console,
    step: impl FnOnce(&File, &mut Console) -> Result<T, String>,
) -> Result<Result<T, String>, u32> {
    let _lock = match run::take_lock(asked.lock) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            console.err(&tagged_line(asked.nonce, Refusal::Busy));
            console.err(&busy_line(asked.key));
            return Err(REFUSED_STATUS);
        }
        Err(line) => {
            console.err(&line);
            return Err(FAILED_STATUS);
        }
    };
    let held = run::lay_out_alone(asked.work, asked.bare, asked.lock, asked.nonce, console)?;
    Ok(step(&held, console))
}

/// A copy's stamp as the box left it: its size, its last write as a Windows file time, and its file id.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: u64,
    id: String,
}

/// One installed input as the record holds it.
#[derive(Clone, Debug)]
struct Copy {
    path: String,
    sha256: String,
    stamp: Stamp,
}

/// The inputs record: the digest of the manifest last installed from, empty while an install is under way or after
/// one that did not finish, and each input installed, by its path's simple uppercase.
#[derive(Default)]
struct Record {
    digest: String,
    files: BTreeMap<String, Copy>,
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
            files: BTreeMap::new(),
        };
        for (path, file) in files {
            let copy = recorded_copy(path, file).filter(|_| is_plain_path(path)).ok_or_else(not_a_record)?;
            record.files.insert(ordinal_upper(path), copy);
        }
        Ok(record)
    }

    /// The record's text: one line of JSON.
    fn text(&self) -> String {
        let files: Map<String, Value> = self
            .files
            .values()
            .map(|copy| {
                let stamp = &copy.stamp;
                let file = json!({ "size": stamp.size, "sha256": copy.sha256, "modified": stamp.modified, "id": stamp.id });
                (copy.path.clone(), file)
            })
            .collect();
        json!({ "digest": self.digest, "files": files }).to_string()
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
            modified: file.get("modified")?.as_u64()?,
            id: file.get("id")?.as_str()?.to_owned(),
        },
    })
}

/// The work folder's inputs record. One that is not a record is taken as empty, and one that cannot be read (a link,
/// a folder, a file of more than one name) is removed as itself, never followed, and taken as empty, each said; else
/// why such a record could not be removed.
fn recorded(work: &str, console: &mut Console) -> Result<Record, String> {
    let path = record_path(work);
    match tools::read_record(&path) {
        Ok(None) => Ok(Record::default()),
        Ok(Some(text)) => Ok(Record::parse(&text).unwrap_or_else(|why| {
            console.err(&format!("gate: remote inputs record {path} {why}; every copy is hashed"));
            Record::default()
        })),
        Err(error) => {
            preflight::remove_entry(Path::new(&path))
                .map_err(|removal| format!("cannot remove {path}, which cannot be read ({error}): {removal}"))?;
            console.err(&format!(
                "gate: remote removed the inputs record {path}, which cannot be read: {}; every copy is hashed",
                escaped(&error.to_string())
            ));
            Ok(Record::default())
        }
    }
}

/// Writes `record` as the work folder `work`'s inputs record; else why not.
fn write_record(work: &str, record: &Record) -> Result<(), String> {
    let path = record_path(work);
    tools::write_record_file(&path, &record.text()).map_err(|error| format!("cannot write {path}: {error}"))
}

/// The stamp of the open file `file`.
fn stamp_of(file: &File) -> io::Result<Stamp> {
    let metadata = file.metadata()?;
    let (volume, id) = gate_win32::file_id(file)?;
    let hex = id.iter().fold(String::with_capacity(2 * id.len()), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    Ok(Stamp {
        size: metadata.file_size(),
        modified: metadata.last_write_time(),
        id: format!("{volume:016x}-{hex}"),
    })
}

/// Where a walk of a `/`-separated path's folders below a held folder ended ([`walk`]).
enum Walked<'a> {
    /// A folder on the way is missing.
    Missing,
    /// The part of the path up to this entry, which is a file, a link or a junction, opened as itself.
    Blocked(&'a str, File),
    /// Every folder on the way is a folder itself: the last one, held without delete sharing, and the file's name.
    Reached(File, &'a str),
}

/// Walks the folders of the `/`-separated `path` below the held folder `top`, each opened as itself beneath the one
/// before and held without delete sharing, never through a link; each made when missing when `make`. Else the system's
/// error.
fn walk<'a>(top: &File, path: &'a str) -> io::Result<Walked<'a>> {
    walk_making(top, path, false)
}

/// [`walk`], each folder made when missing when `make`.
fn walk_making<'a>(top: &File, path: &'a str, make: bool) -> io::Result<Walked<'a>> {
    let (parents, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    let mut folder = top.try_clone()?;
    let mut end = 0;
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        end += part.len() + 1;
        let how = if make { Make::Folder } else { Make::Open };
        let opened = match gate_win32::open_beneath_as(&folder, part, FILE_GENERIC_READ, Share::NoDelete, how) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Walked::Missing),
            opened => opened?,
        };
        if !is_folder_itself(&opened)? {
            return Ok(Walked::Blocked(&path[..end - 1], opened));
        }
        folder = opened;
    }
    Ok(Walked::Reached(folder, leaf))
}

/// The error saying the part `prefix` of a path is no folder of its own.
fn not_a_folder(prefix: &str) -> io::Error {
    io::Error::other(format!("'{prefix}' is a link, a junction or not a folder"))
}

/// Whether the open `file` is a plain disk file of one name, not a link or a junction; else why not.
fn plain_copy(file: &File) -> io::Result<()> {
    gate_win32::check_plain_file(file)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("it is a link or a junction"));
    }
    Ok(())
}

/// The copy at the `/`-separated `path` below the held work folder `work`, opened with `access` as itself through the
/// handles of its folders ([`walk`]): a plain disk file of one name ([`plain_copy`]); with its stamp. `None` when
/// nothing is at its path; else why it is no such file.
fn opened_copy(work: &File, path: &str, access: u32) -> io::Result<Option<(File, Stamp)>> {
    let (folder, leaf) = match walk(work, path)? {
        Walked::Missing => return Ok(None),
        Walked::Blocked(prefix, _) => return Err(not_a_folder(prefix)),
        Walked::Reached(folder, leaf) => (folder, leaf),
    };
    let file = match gate_win32::open_beneath_as(&folder, leaf, access, Share::All, Make::Open) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    plain_copy(&file)?;
    let stamp = stamp_of(&file)?;
    Ok(Some((file, stamp)))
}

/// The copy at `path` as the record would hold it, under the manifest's spelling of `path`, when it holds what the
/// manifest's `held` says: the record's own entry for a copy whose stamp it holds, trusted without hashing, else the
/// copy's hash and stamp read now. `None` for a copy missing, that is no plain file, that cannot be read, or that holds
/// anything else.
fn matched_copy(work: &File, path: &str, held: &Held, record: &Record) -> Option<Copy> {
    let (file, stamp) = opened_copy(work, path, FILE_GENERIC_READ).ok().flatten()?;
    if stamp.size != held.stamp.size {
        return None;
    }
    if let Some(copy) = record.get(path).filter(|copy| copy.stamp == stamp) {
        return (copy.sha256 == held.sha256).then(|| Copy {
            path: path.to_owned(),
            ..copy.clone()
        });
    }
    let (sha256, _) = sha256_of(&file).ok()?;
    (sha256 == held.sha256).then(|| Copy {
        path: path.to_owned(),
        sha256,
        stamp,
    })
}

/// The diff: `gate-remote-inputs: need <n>`, then `gate-remote-inputs: path <path>` for each file of the manifest the
/// work folder `work` lacks or holds otherwise, in the manifest's order, each path escaped. One that needs nothing first
/// finishes the install as a put of nothing would ([`swept`], [`finished`]). Else why not.
fn diff(asked: &Asked<'_>, sent: &Sent, work: &File, console: &mut Console) -> Result<(), String> {
    let record = recorded(asked.work, console)?;
    let needed: Vec<&str> = sent
        .manifest
        .files()
        .filter(|(path, held)| matched_copy(work, path, held, &record).is_none())
        .map(|(path, _)| path)
        .collect();
    if needed.is_empty() {
        unsealed(asked.work, &record)?;
        swept(work, &record, &sent.manifest, console);
        finished(asked.work, work, sent, &record, Record::default())?;
    }
    console.out(&format!("gate-remote-inputs: need {}", needed.len()));
    for path in needed {
        console.out(&format!("gate-remote-inputs: path {}", escaped(path)));
    }
    Ok(())
}

/// The put: its stage made ([`made_stage`]), the archive on stdin unpacked into it before any lock is taken, then
/// under the lock its files moved into the work folder ([`installed`]); the stage removed on every way out once made,
/// and `gate-remote-inputs: installed <n>` said; 0. Else the status, its lines said.
fn put(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, console: &mut Console) -> u32 {
    let (stage_path, stage) = match made_stage(asked, console) {
        Ok(made) => made,
        Err(why) => return failed(asked, &why, console),
    };
    let done = staged(asked, sent, seams, (&stage_path, &stage), console);
    drop(stage);
    let removal = preflight::remove_entry(&stage_path).map_err(|error| format!("cannot remove the stage {}: {error}", stage_path.display()));
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

/// The archive on stdin unpacked into the stage `stage`, at the path and held as the pair says, within the budget, then
/// its files installed under the work folder's lock ([`locked`], [`installed`]): the count installed, or why not;
/// `Err` with the status when the install never ran, its lines said.
fn staged(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, stage: (&Path, &File), console: &mut Console) -> Result<Result<usize, String>, u32> {
    let budget = seams.inputs_budget_bytes.unwrap_or(INPUTS_BUDGET);
    let none = Manifest::default();
    let go_on = || true;
    // Copies come from the archive's own members only: no file of the box's is a copy's source.
    let pc = PcFiles {
        root: Path::new(asked.work),
        manifest: &none,
        go_on: &go_on,
    };
    let entries = match unpack(io::stdin().lock(), &stage.0.join(STAGED_FILES), budget, &pc) {
        Ok(entries) => entries,
        Err(error) => return Ok(Err(error.to_string())),
    };
    let files = match gate_win32::open_beneath(stage.1, STAGED_FILES, Share::NoDelete) {
        Ok(files) => files,
        Err(error) => return Ok(Err(format!("cannot open the staged files: {error}"))),
    };
    locked(asked, console, |work, console| installed(asked, sent, (&files, &entries), work, console))
}

/// This put's stage, `{root}\inputs-stage\{key}-{nonce}`, made as a new folder and held, and its path: the stages
/// folder opened or made beneath the root, refused when it is a link, a junction or not a folder, and given the
/// security that keeps every lower process out ([`gate_win32::protect_folder`]); every stage older than a day removed
/// first ([`pruned`]). A stage of that name already there is refused, never shared. Else why not.
fn made_stage(asked: &Asked<'_>, console: &mut Console) -> Result<(PathBuf, File), String> {
    let root = OpenOptions::new()
        .read(true)
        .share_mode(Share::All.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(asked.root)
        .map_err(|error| format!("cannot open {}: {error}", asked.root))?;
    let path = Path::new(asked.root).join(STAGES);
    let cannot = |error: io::Error| format!("cannot ready {}: {error}", path.display());
    let stages =
        gate_win32::open_beneath_as(&root, STAGES, FILE_GENERIC_READ | WRITE_DAC | WRITE_OWNER, Share::NoDelete, Make::Folder).map_err(cannot)?;
    if !is_folder_itself(&stages).map_err(cannot)? {
        return Err(format!("{} is a link, a junction or not a folder", path.display()));
    }
    gate_win32::protect_folder(&stages).map_err(cannot)?;
    for line in pruned(&path, SystemTime::now()) {
        console.err(&line);
    }
    let name = format!("{}-{}", asked.key, asked.nonce);
    let stage = gate_win32::open_beneath_as(&stages, &name, FILE_GENERIC_READ, Share::NoDelete, Make::NewFolder)
        .map_err(|error| format!("cannot make the stage {}: {error}", path.join(&name).display()))?;
    Ok((path.join(name), stage))
}

/// Removes every stage in the stages folder `stages` last written more than a day before `now`, each as itself, of
/// any work folder: one a put left when it could not remove it. The lines saying each that could not be removed.
fn pruned(stages: &Path, now: SystemTime) -> Vec<String> {
    let unlisted = |error: io::Error| {
        vec![format!(
            "gate: remote could not list {}: {}",
            stages.display(),
            escaped(&error.to_string())
        )]
    };
    let entries = match std::fs::read_dir(stages) {
        Ok(entries) => entries,
        Err(error) => return unlisted(error),
    };
    let mut lines = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return unlisted(error),
        };
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|written| now.duration_since(written).is_ok_and(|age| age > STAGE_AGE));
        if old
            && entry.file_name().to_str().is_some_and(is_stage_name)
            && let Err(error) = preflight::remove_entry(&entry.path())
        {
            lines.push(format!(
                "gate: remote could not remove the old stage {}: {}",
                entry.path().display(),
                escaped(&error.to_string())
            ));
        }
    }
    lines
}

/// Whether `name` is a stage's name, `{key}-{nonce}`: a key and an outputs nonce of lower-case hex.
fn is_stage_name(name: &str) -> bool {
    name.rsplit_once('-')
        .is_some_and(|(key, nonce)| !key.is_empty() && is_lower_hex(nonce, OUTPUTS_NONCE_DIGITS))
}

/// Checks every member of `staged`, the held folder of the unpacked files and their entries, against the manifest and
/// every target in the held work folder `work`, before any is written; then writes the record without a digest
/// ([`unsealed`]), deletes the stale copies ([`swept`]), moves each file in ([`moved_in`]) and finishes the install
/// ([`finished`]). The count of files installed; else why not.
fn installed(asked: &Asked<'_>, sent: &Sent, staged: (&File, &[Entry]), work: &File, console: &mut Console) -> Result<usize, String> {
    let (files, entries) = staged;
    let record = recorded(asked.work, console)?;
    for entry in entries {
        in_manifest(entry, &sent.manifest)?;
        free_target(work, &entry.path, &record, &sent.manifest)?;
    }
    unsealed(asked.work, &record)?;
    swept(work, &record, &sent.manifest, console);
    let mut now = Record::default();
    for entry in entries {
        let stamp = moved_in(files, work, &entry.path)?;
        let copy = Copy {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            stamp,
        };
        now.files.insert(ordinal_upper(&entry.path), copy);
    }
    finished(asked.work, work, sent, &record, now)?;
    Ok(entries.len())
}

/// Writes the work folder `work`'s record with `record`'s files and no digest, before anything below the work folder
/// changes, so no run passes the digest check until the install finishes. Else why not.
fn unsealed(work: &str, record: &Record) -> Result<(), String> {
    write_record(
        work,
        &Record {
            digest: String::new(),
            files: record.files.clone(),
        },
    )
}

/// Finishes an install whose moved files are `now`'s: adds every other file of the manifest the held work folder
/// `work` holds as the manifest says ([`present`]); fails, the record written with no digest, when a file of the
/// manifest is still missing; else rewrites the exclude file and, last, writes the record with the manifest's digest.
/// Else why not.
fn finished(work_path: &str, work: &File, sent: &Sent, record: &Record, mut now: Record) -> Result<(), String> {
    present(work, &sent.manifest, record, &mut now);
    if let Some((path, _)) = sent.manifest.files().find(|(path, _)| now.get(path).is_none()) {
        let mut files = record.files.clone();
        files.extend(now.files);
        write_record(
            work_path,
            &Record {
                digest: String::new(),
                files,
            },
        )?;
        return Err(format!("{path} is missing after the install"));
    }
    write_exclude(work, work_path, &now)?;
    now.digest.clone_from(&sent.digest);
    write_record(work_path, &now)
}

/// Whether the manifest holds the member `entry` with its size and SHA-256; else the refusal naming it.
fn in_manifest(entry: &Entry, manifest: &Manifest) -> Result<(), String> {
    match manifest.get(&entry.path) {
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
/// the moves ([`swept`]); and whatever is at its path a plain file of one name ([`plain_copy`]). Else the refusal
/// naming the path.
fn free_target(work: &File, path: &str, record: &Record, manifest: &Manifest) -> Result<(), String> {
    let refused = |why: String| format!("cannot install '{}': {why}", escaped(path));
    match walk(work, path).map_err(|error| refused(error.to_string()))? {
        Walked::Missing => Ok(()),
        Walked::Blocked(prefix, blocker) if is_stale_unchanged(&blocker, prefix, record, manifest) => Ok(()),
        Walked::Blocked(prefix, _) => Err(refused(not_a_folder(prefix).to_string())),
        Walked::Reached(folder, leaf) => match gate_win32::open_beneath(&folder, leaf, Share::All) {
            Ok(file) => plain_copy(&file).map_err(|error| refused(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(refused(error.to_string())),
        },
    }
}

/// Whether `file`, opened at `path`, is a plain file `record` holds that left `manifest` and still hashes to what was
/// installed.
fn is_stale_unchanged(file: &File, path: &str, record: &Record, manifest: &Manifest) -> bool {
    manifest.get(path).is_none()
        && record
            .get(path)
            .is_some_and(|copy| plain_copy(file).is_ok() && sha256_of(file).is_ok_and(|(sha256, _)| sha256 == copy.sha256))
}

/// Moves the staged file `path` below the held folder `files` to the same path below the held work folder `work`,
/// replacing a file or a link there as itself: each folder on its way made when missing and refused when it is a
/// link, a junction or not a folder ([`walk_making`]), the file renamed by its handle into the held folder
/// ([`gate_win32::rename_beneath`]) and its DACL and label then set to what that folder hands down. The moved file's
/// stamp, read through that handle; else why not.
fn moved_in(files: &File, work: &File, path: &str) -> Result<Stamp, String> {
    let failed = |why: String| format!("cannot install '{}': {why}", escaped(path));
    let (folder, leaf) = match walk_making(work, path, true).map_err(|error| failed(error.to_string()))? {
        Walked::Reached(folder, leaf) => (folder, leaf),
        Walked::Blocked(prefix, _) => return Err(failed(not_a_folder(prefix).to_string())),
        Walked::Missing => return Err(failed("a folder on its way could not be made".to_owned())),
    };
    let Walked::Reached(staged_folder, staged_leaf) = walk(files, path).map_err(|error| failed(error.to_string()))? else {
        return Err(failed("its staged copy is not in the stage".to_owned()));
    };
    let source =
        gate_win32::open_beneath_as(&staged_folder, staged_leaf, STAGED_ACCESS, Share::All, Make::Open).map_err(|error| failed(error.to_string()))?;
    gate_win32::rename_beneath(&source, &folder, leaf).map_err(|error| failed(error.to_string()))?;
    gate_win32::inherit_security(&source).map_err(|error| failed(error.to_string()))?;
    stamp_of(&source).map_err(|error| failed(error.to_string()))
}

/// Deletes each copy `record` holds whose path left `manifest`, by the handle it was checked through, only while it is
/// a plain file below the held work folder `work` that still hashes to what the record holds; a copy changed on the box
/// is kept. Says each copy kept because it could not be opened or hashed, and each that could not be deleted.
fn swept(work: &File, record: &Record, manifest: &Manifest, console: &mut Console) {
    for copy in record.files.values().filter(|copy| manifest.get(&copy.path).is_none()) {
        let kept = |why: String| format!("gate: remote kept the stale input {}: {}", escaped(&copy.path), escaped(&why));
        let file = match opened_copy(work, &copy.path, FILE_GENERIC_READ | DELETE) {
            Ok(Some((file, _))) => file,
            Ok(None) => continue,
            Err(error) => {
                console.err(&kept(error.to_string()));
                continue;
            }
        };
        match sha256_of(&file) {
            Ok((sha256, _)) if sha256 == copy.sha256 => removed_stale(&file, copy, console),
            Ok(_) => {}
            Err(error) => console.err(&kept(format!("it cannot be hashed: {error}"))),
        }
    }
}

/// Deletes the stale `copy`, opened as `file`, by its handle; says so when it cannot be deleted.
fn removed_stale(file: &File, copy: &Copy, console: &mut Console) {
    if let Err(error) = gate_win32::delete_opened(file) {
        console.err(&format!(
            "gate: remote could not remove the stale input {}: {}",
            escaped(&copy.path),
            escaped(&error.to_string())
        ));
    }
}

/// Adds to `now`, which holds the files this put installed, every other file of `manifest` the held work folder `work`
/// holds as the manifest says ([`matched_copy`]).
fn present(work: &File, manifest: &Manifest, record: &Record, now: &mut Record) {
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

/// Rewrites the tree's `.git\info\exclude` from `record`: one line per recorded path as a literal pattern, anchored
/// with a leading `/` and each glob character escaped. The `.git` must be a folder of its own, opened beneath the held
/// work folder `work`, whose path is `work_path`; `.git\info` is made a folder of its own beneath it ([`info_folder`]),
/// and the file written beneath that ([`preflight::written_beneath`]), so no link there is followed. Else why not.
fn write_exclude(work: &File, work_path: &str, record: &Record) -> Result<(), String> {
    let git_path = Path::new(work_path).join(".git");
    let not_own = || format!("{} is not a folder of its own", git_path.display());
    let git = gate_win32::open_beneath(work, ".git", Share::NoDelete).map_err(|_| not_own())?;
    if !is_folder_itself(&git).map_err(|error| error.to_string())? {
        return Err(not_own());
    }
    let info_path = git_path.join("info");
    let info = info_folder(&git, &info_path)?;
    let mut text = String::new();
    for copy in record.files.values() {
        text.push_str(&literal_pattern(&copy.path));
        text.push('\n');
    }
    preflight::written_beneath(&info, &info_path, "exclude", &text)
        .map_err(|why| format!("cannot write {}: {why}", info_path.join("exclude").display()))
}

/// The folder `info` beneath the held `.git` folder `git`, held without delete sharing: a link, a junction or a file
/// at its name deleted as itself, and the folder made when missing. Else why not, naming it by `path`.
fn info_folder(git: &File, path: &Path) -> Result<File, String> {
    let cannot = |error: io::Error| format!("cannot ready {}: {error}", path.display());
    match gate_win32::open_beneath_as(git, "info", FILE_GENERIC_READ | DELETE, Share::All, Make::Open) {
        Ok(found) if is_folder_itself(&found).map_err(cannot)? => {}
        Ok(found) => gate_win32::delete_opened(&found).map_err(cannot)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(cannot(error)),
    }
    let info = gate_win32::open_beneath_as(git, "info", FILE_GENERIC_READ, Share::NoDelete, Make::Folder).map_err(cannot)?;
    if !is_folder_itself(&info).map_err(cannot)? {
        return Err(format!("{} is a link, a junction or not a folder", path.display()));
    }
    Ok(info)
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

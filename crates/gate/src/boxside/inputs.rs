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
//! what was installed, keeps any other in the record as a kept copy until a manifest names its path again or the copy
//! is gone, rewrites the tree's `.git\info\exclude` from the record, so no input is cleaned by the checkout or
//! committed back, and writes the record's digest last, only once the work folder holds every file of the manifest.
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
use gate_win32::{Descriptor, Make, Share};
use serde_json::{Map, Value, json};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, READ_CONTROL,
    WRITE_DAC, WRITE_OWNER,
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
/// The folder in a stage the archive is unpacked into.
const STAGED_FILES: &str = "files";
/// The access the stages folder is held with: listed, its attributes and its security read, and no more, so an
/// existing folder is judged through a handle that could change nothing in it.
const STAGES_ACCESS: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY;
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
/// size, last write or file id differs), or a tree whose exclude file cannot be written from the record; `Ok` when it
/// holds them, the exclude file written, with the line saying where a folder at the exclude file's name was moved
/// aside, when one was ([`preflight::moved_aside_line`]), for the caller to say.
pub(super) fn unmatched(work: &str, held: &File, sent: &str) -> Result<Option<String>, String> {
    let record = matching_record(work, sent)?;
    if let Some(why) = changed_copy(held, &record) {
        return Err(why);
    }
    write_exclude(held, work, &record)
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
        .find_map(|copy| match opened_copy(work, &copy.path, FILE_GENERIC_READ, Share::All) {
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
/// one that did not finish; each input installed, by its path's simple uppercase; and each kept stale copy, by the
/// same key: one a put left in place, as it changed on the box or could not be opened, hashed or deleted, which the
/// exclude file lists, which no sweep deletes and which is never trusted as installed.
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
fn opened_copy(work: &File, path: &str, access: u32, share: Share) -> io::Result<Option<(File, Stamp)>> {
    let (folder, leaf) = match walk(work, path)? {
        Walked::Missing => return Ok(None),
        Walked::Blocked(prefix, _) => return Err(not_a_folder(prefix)),
        Walked::Reached(folder, leaf) => (folder, leaf),
    };
    let file = match gate_win32::open_beneath_as(&folder, leaf, access, share, Make::Open) {
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
    let (file, stamp) = opened_copy(work, path, FILE_GENERIC_READ, Share::All).ok().flatten()?;
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
/// finishes the install as a put of nothing would ([`swept`], [`finished`]), from the copies this diff matched, each
/// untrusted one hashed once. Else why not.
fn diff(asked: &Asked<'_>, sent: &Sent, work: &File, console: &mut Console) -> Result<(), String> {
    let record = recorded(asked.work, console)?;
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
        unsealed(asked.work, &record)?;
        now.kept = swept(work, &record, &sent.manifest, console);
        if let Some(line) = finished(asked.work, work, sent, &record, now)? {
            console.err(&line);
        }
    }
    console.out(&format!("gate-remote-inputs: need {}", needed.len()));
    for path in needed {
        console.out(&format!("gate-remote-inputs: path {}", escaped(path)));
    }
    Ok(())
}

/// The put: its stage made ([`made_stage`]), the archive on stdin unpacked into it before any lock is taken, then
/// under the lock its files moved into the work folder ([`installed`]); the stage removed beneath its held handles on
/// every way out once made ([`removed_stage`]), and `gate-remote-inputs: installed <n>` said; 0. Else the status, its
/// lines said.
fn put(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, console: &mut Console) -> u32 {
    let stage = match made_stage(asked, console) {
        Ok(made) => made,
        Err(why) => return failed(asked, &why, console),
    };
    let done = staged(asked, sent, seams, &stage, console);
    let removal = removed_stage(&stage);
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

/// The archive on stdin unpacked into the held stage `stage` by its path, once that path is shown to name the held
/// folder ([`same_stage`]), within the budget, then its files installed under the work folder's lock ([`locked`],
/// [`installed`]): the count installed, or why not; `Err` with the status when the install never ran, its lines said.
fn staged(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, stage: &Stage, console: &mut Console) -> Result<Result<usize, String>, u32> {
    if let Err(why) = same_stage(stage) {
        return Ok(Err(why));
    }
    let budget = seams.inputs_budget_bytes.unwrap_or(INPUTS_BUDGET);
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
    let files = match staged_files(stage) {
        Ok(files) => files,
        Err(why) => return Ok(Err(why)),
    };
    locked(asked, console, |work, console| installed(asked, sent, (&files, &entries), work, console))
}

/// The folder of the unpacked files, opened as itself beneath the held stage and held without delete sharing, once it
/// is shown to be a folder of its own that the path the archive was unpacked into names too: the folder that path
/// opens, through whatever links are on its way, is the held one by volume and file id ([`gate_win32::same_file`]).
/// Else why not.
fn staged_files(stage: &Stage) -> Result<File, String> {
    let path = stage.path.join(STAGED_FILES);
    let cannot = |error: io::Error| format!("cannot open the staged files {}: {error}", path.display());
    let files = gate_win32::open_beneath(&stage.held, STAGED_FILES, Share::NoDelete).map_err(cannot)?;
    if !is_folder_itself(&files).map_err(cannot)? {
        return Err(format!("the staged files {} are a link, a junction or not a folder", path.display()));
    }
    let by_path = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&path)
        .map_err(cannot)?;
    if !gate_win32::same_file(&by_path, &files).map_err(cannot)? {
        return Err(format!(
            "the staged files {} lead to another folder than the one in the stage the box made",
            path.display()
        ));
    }
    Ok(files)
}

/// A put's stage, made and held: the box's root, the stages folder below it and the stage below that, each held
/// without delete sharing, so none of them can be renamed or swapped for a link while the put works; and the stage's
/// path, which the archive is unpacked below.
struct Stage {
    #[cfg_attr(not(test), expect(dead_code, reason = "held, never read: the handle keeps the root from being renamed"))]
    root: File,
    #[cfg_attr(not(test), expect(dead_code, reason = "held, never read: the handle keeps the folder from being renamed"))]
    stages: File,
    /// The stage itself, opened with `DELETE` access, so it is removed through this handle.
    held: File,
    path: PathBuf,
}

/// This put's stage, `{root}\{stages}\{key}-{nonce}`, made as a new folder with the security that keeps every other
/// account and every lower process out from the instant it exists ([`gate_win32::protected_descriptor`]), and held
/// with its root and stages folder ([`Stage`]): the root opened as itself and refused when it is a link, a junction or
/// not a folder; the stages folder of this box's elevation ([`stages_name`]) readied beneath it ([`stages_folder`]);
/// every stage older than a day removed first ([`pruned`]). A stage of that name already there is refused, never
/// shared. Else why not.
fn made_stage(asked: &Asked<'_>, console: &mut Console) -> Result<Stage, String> {
    let facts = gate_win32::token_facts().map_err(|error| format!("cannot read the box's token: {error}"))?;
    let security = gate_win32::protected_descriptor(&facts).map_err(|error| format!("cannot make the box's security: {error}"))?;
    let root = OpenOptions::new()
        .read(true)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(asked.root)
        .map_err(|error| format!("cannot open {}: {error}", asked.root))?;
    if !is_folder_itself(&root).map_err(|error| format!("cannot open {}: {error}", asked.root))? {
        return Err(format!("{} is a link, a junction or not a folder", asked.root));
    }
    let stages_name = stages_name(facts.elevated);
    let path = Path::new(asked.root).join(stages_name);
    let stages = stages_folder(&root, &path, stages_name, &security)?;
    for line in pruned(&stages, &path, SystemTime::now()) {
        console.err(&line);
    }
    let name = format!("{}-{}", asked.key, asked.nonce);
    let stage = gate_win32::make_folder_beneath(&stages, &name, FILE_GENERIC_READ | DELETE, Share::NoDelete, &security)
        .map_err(|error| format!("cannot make the stage {}: {error}", path.join(&name).display()))?;
    Ok(Stage {
        root,
        stages,
        held: stage,
        path: path.join(name),
    })
}

/// The name of the folder below the box's root that holds each put's stage, `{root}\{stages}\{key}-{nonce}`:
/// `inputs-stage` for an elevated box, whose folders Administrators own and whose High label keeps every medium
/// process out, and `inputs-stage-medium` for an unelevated one, whose folders its own user owns and labels Medium.
/// Neither box ever touches the other's folder.
fn stages_name(elevated: bool) -> &'static str {
    if elevated { "inputs-stage" } else { "inputs-stage-medium" }
}

/// The stages folder `name`, whose path is `path`, readied beneath the held root `root` and held without delete
/// sharing: made new with the security `security`, so it is never seen with any other, when nothing has the name;
/// else the folder there adopted ([`adopted`]), so two puts that make it at once share it. Else why not.
fn stages_folder(root: &File, path: &Path, name: &str, security: &Descriptor) -> Result<File, String> {
    match gate_win32::make_folder_beneath(root, name, STAGES_ACCESS, Share::NoDelete, security) {
        Ok(made) => Ok(made),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => adopted(root, path, name),
        Err(error) => Err(format!("cannot ready {}: {error}", path.display())),
    }
}

/// The stages folder `name` already at `path` beneath the held root `root`, adopted: opened as itself with no more
/// access than lists it and reads its security ([`STAGES_ACCESS`]), and kept, as [`stages_folder`] holds it, only when
/// it is a folder of its own whose owner, DACL and label are those this box makes its folders with
/// ([`gate_win32::is_protected_folder`]), read through that same handle. Anything else at the name is another
/// account's, one the box may not open included, and is refused: never walked, emptied or replaced, since another
/// account may have filled it. Else why not.
fn adopted(root: &File, path: &Path, name: &str) -> Result<File, String> {
    let foreign = || {
        format!(
            "cannot use {}: another account made it; remove it, or give it to Administrators",
            path.display()
        )
    };
    let cannot = |error: io::Error| format!("cannot ready {}: {error}", path.display());
    let found = match gate_win32::open_beneath_as(root, name, STAGES_ACCESS, Share::NoDelete, Make::Open) {
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(foreign()),
        Err(error) => return Err(cannot(error)),
    };
    if is_folder_itself(&found).map_err(cannot)? && gate_win32::is_protected_folder(&found).map_err(cannot)? {
        Ok(found)
    } else {
        Err(foreign())
    }
}

/// Whether the stage's path names the held stage itself: the folder it opens, through whatever links are on its way,
/// is the held stage by volume and file id ([`gate_win32::same_file`]). Else why not, so nothing is unpacked anywhere
/// else.
fn same_stage(stage: &Stage) -> Result<(), String> {
    let cannot = |error: io::Error| format!("cannot check the stage {}: {error}", stage.path.display());
    let by_path = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&stage.path)
        .map_err(cannot)?;
    if !gate_win32::same_file(&by_path, &stage.held).map_err(cannot)? {
        return Err(format!(
            "the stage {} leads to another folder than the stage the box made",
            stage.path.display()
        ));
    }
    Ok(())
}

/// Removes the stage beneath its held handle as itself, everything in it first, deepest first and each entry as
/// itself ([`gate_win32::delete_tree_opened`]), while the stages folder is still held. Else why not.
fn removed_stage(stage: &Stage) -> Result<(), String> {
    gate_win32::delete_tree_opened(&stage.held).map_err(|error| format!("cannot remove the stage {}: {error}", stage.path.display()))
}

/// Removes every stage in the held stages folder `stages`, whose path is `path`, last written more than a day before
/// `now`, of any work folder: one a put left when it could not remove it. Each is listed, opened and removed beneath
/// the held folder as itself ([`old_stage`], [`gate_win32::delete_tree_opened`]). The lines saying each kept because it
/// cannot be opened or its last write cannot be read, and each that could not be removed.
fn pruned(stages: &File, path: &Path, now: SystemTime) -> Vec<String> {
    let names = match gate_win32::names_beneath(stages) {
        Ok(names) => names,
        Err(error) => return vec![format!("gate: remote could not list {}: {}", path.display(), escaped(&error.to_string()))],
    };
    let mut lines = Vec::new();
    for name in names.iter().filter_map(|name| name.to_str()).filter(|name| is_stage_name(name)) {
        let stage = path.join(name);
        let removal = match old_stage(stages, name, now) {
            Ok(None) => continue,
            Ok(Some(old)) => gate_win32::delete_tree_opened(&old),
            Err(why) => {
                lines.push(escaped(&format!("gate: remote kept the stage {}: {why}", stage.display())));
                continue;
            }
        };
        if let Err(error) = removal {
            lines.push(format!(
                "gate: remote could not remove the old stage {}: {}",
                stage.display(),
                escaped(&error.to_string())
            ));
        }
    }
    lines
}

/// The entry `name` of the held stages folder `stages`, opened as itself with `DELETE` access, when it was last written
/// more than a day before `now`; `None` when it is younger or gone. Its age is read first through a handle that asks
/// for no more than its attributes, so a stage another put holds is not opened against that put's sharing, then again
/// through the handle it is removed by. Else why not: it cannot be opened to delete, or its last write cannot be read.
fn old_stage(stages: &File, name: &str, now: SystemTime) -> Result<Option<File>, String> {
    let is_old = |entry: &File| -> Result<bool, String> {
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("its last write cannot be read: {error}"))?;
        Ok(now.duration_since(modified).is_ok_and(|age| age > STAGE_AGE))
    };
    let open = |access| match gate_win32::open_beneath_as(stages, name, access, Share::All, Make::Open) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        opened => opened.map(Some),
    };
    let first = open(FILE_READ_ATTRIBUTES).map_err(|error| format!("its last write cannot be read: {error}"))?;
    match first {
        Some(entry) if is_old(&entry)? => {}
        _ => return Ok(None),
    }
    match open(FILE_READ_ATTRIBUTES | DELETE).map_err(|error| format!("it cannot be opened to delete: {error}"))? {
        Some(entry) if is_old(&entry)? => Ok(Some(entry)),
        _ => Ok(None),
    }
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
    let mut now = Record {
        kept: swept(work, &record, &sent.manifest, console),
        ..Record::default()
    };
    for entry in entries {
        let stamp = moved_in(files, work, &entry.path)?;
        let copy = Copy {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            stamp,
        };
        now.files.insert(ordinal_upper(&entry.path), copy);
    }
    if let Some(line) = finished(asked.work, work, sent, &record, now)? {
        console.err(&line);
    }
    Ok(entries.len())
}

/// Writes the work folder `work`'s record with `record`'s files and no digest, before anything below the work folder
/// changes, so no run passes the digest check until the install finishes. Else why not.
fn unsealed(work: &str, record: &Record) -> Result<(), String> {
    write_record(
        work,
        &Record {
            digest: String::new(),
            ..record.clone()
        },
    )
}

/// Finishes an install whose moved or matched files and newly kept copies are `now`'s: adds every other file of the
/// manifest the held work folder `work` holds as the manifest says ([`present`]), and keeps every copy `record` kept
/// that no file of `now` replaces and that is still there (a kept copy gone from its path leaves the record and the
/// exclude file); fails, the record written with no digest, when a file of the manifest is still missing; else
/// rewrites the exclude file and, last, writes the record with the manifest's digest. Else why not.
fn finished(work_path: &str, work: &File, sent: &Sent, record: &Record, mut now: Record) -> Result<Option<String>, String> {
    present(work, &sent.manifest, record, &mut now);
    now.kept.extend(record.kept.clone());
    now.kept.retain(|key, _| !now.files.contains_key(key));
    // A copy that cannot be opened or is no plain file stays kept: only one shown gone leaves.
    now.kept
        .retain(|_, path| !matches!(opened_copy(work, path, FILE_READ_ATTRIBUTES, Share::All), Ok(None)));
    if let Some((path, _)) = sent.manifest.files().find(|(path, _)| now.get(path).is_none()) {
        let mut files = record.files.clone();
        files.extend(now.files);
        write_record(
            work_path,
            &Record {
                digest: String::new(),
                files,
                kept: now.kept,
            },
        )?;
        return Err(match opened_copy(work, path, FILE_GENERIC_READ, Share::All) {
            Err(error) => format!("{path} cannot be opened: {error}"),
            Ok(Some(_)) => format!("{path} does not hold what the manifest names after the install"),
            Ok(None) => format!("{path} is missing after the install"),
        });
    }
    let moved_aside = write_exclude(work, work_path, &now)?;
    now.digest.clone_from(&sent.digest);
    write_record(work_path, &now)?;
    Ok(moved_aside)
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
        Walked::Blocked(prefix, blocker) => match is_stale_unchanged(&blocker, prefix, record, manifest) {
            Ok(true) => Ok(()),
            Ok(false) => Err(refused(not_a_folder(prefix).to_string())),
            Err(why) => Err(refused(why)),
        },
        Walked::Reached(folder, leaf) => match gate_win32::open_beneath(&folder, leaf, Share::All) {
            Ok(file) => plain_copy(&file).map_err(|error| refused(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(refused(error.to_string())),
        },
    }
}

/// Whether `file`, opened at `path`, is a plain file `record` holds that left `manifest` and still hashes to what was
/// installed; else why it could not be hashed.
fn is_stale_unchanged(file: &File, path: &str, record: &Record, manifest: &Manifest) -> Result<bool, String> {
    let Some(copy) = record.get(path).filter(|_| manifest.get(path).is_none()) else {
        return Ok(false);
    };
    if plain_copy(file).is_err() {
        return Ok(false);
    }
    let (sha256, _) = sha256_of(file).map_err(|error| format!("'{}' cannot be hashed: {error}", escaped(path)))?;
    Ok(sha256 == copy.sha256)
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
    let stamp = stamp_of(&source).map_err(|error| failed(error.to_string()))?;
    gate_win32::inherit_security(&source).map_err(|error| failed(error.to_string()))?;
    Ok(stamp)
}

/// Deletes each copy `record` holds whose path left `manifest`, by the handle it was checked through, only while it is
/// a plain file below the held work folder `work` that still hashes to what the record holds; a copy changed on the box
/// is kept. Says each copy kept because it could not be opened or hashed, and each that could not be deleted. The
/// copies kept, by their key: each but those deleted and those no longer there.
fn swept(work: &File, record: &Record, manifest: &Manifest, console: &mut Console) -> BTreeMap<String, String> {
    let mut kept_copies = BTreeMap::new();
    for (key, copy) in record.files.iter().filter(|(_, copy)| manifest.get(&copy.path).is_none()) {
        let kept = |why: String| format!("gate: remote kept the stale input {}: {}", escaped(&copy.path), escaped(&why));
        // No writer may share it, so the copy hashed is the copy deleted.
        let file = match opened_copy(work, &copy.path, FILE_GENERIC_READ | DELETE, Share::NoWrite) {
            Ok(Some((file, _))) => file,
            Ok(None) => continue,
            Err(error) => {
                console.err(&kept(error.to_string()));
                kept_copies.insert(key.clone(), copy.path.clone());
                continue;
            }
        };
        let deleted = match sha256_of(&file) {
            Ok((sha256, _)) if sha256 == copy.sha256 => removed_stale(&file, copy, console),
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

/// Deletes the stale `copy`, opened as `file`, by its handle; says so when it cannot be deleted. Whether it was.
fn removed_stale(file: &File, copy: &Copy, console: &mut Console) -> bool {
    let Err(error) = gate_win32::delete_opened(file) else {
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

/// Rewrites the tree's `.git\info\exclude` from `record`: one line per recorded path, installed or kept, as a literal pattern, anchored
/// with a leading `/` and each glob character escaped. The `.git` must be a folder of its own, opened beneath the held
/// work folder `work`, whose path is `work_path`; `.git\info` is made a folder of its own beneath it ([`info_folder`]),
/// and the file written beneath that ([`preflight::written_beneath`]), so no link there is followed; the line saying
/// where a folder at the file's name was moved aside, when one was ([`preflight::moved_aside_line`]). Else why not.
fn write_exclude(work: &File, work_path: &str, record: &Record) -> Result<Option<String>, String> {
    let git_path = Path::new(work_path).join(".git");
    let not_own = || format!("{} is not a folder of its own", git_path.display());
    let git = gate_win32::open_beneath(work, ".git", Share::NoDelete).map_err(|_| not_own())?;
    if !is_folder_itself(&git).map_err(|error| error.to_string())? {
        return Err(not_own());
    }
    let info_path = git_path.join("info");
    let info = info_folder(&git, &info_path)?;
    let mut text = String::new();
    for path in record.paths() {
        text.push_str(&literal_pattern(path));
        text.push('\n');
    }
    let exclude = info_path.join("exclude");
    preflight::written_beneath(&info, "exclude", &text)
        .map(|moved| moved.map(|moved| preflight::moved_aside_line(&exclude, &moved)))
        .map_err(|why| format!("cannot write {}: {why}", exclude.display()))
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

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose folders cannot be made must fail loudly")]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use gate_win32::{Descriptor, Share, file_id, make_folder_beneath, protected_descriptor, same_file, token_facts};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ};

    use super::{Asked, Copy, Record, STAGES_ACCESS, Stage, Stamp, Step, made_stage, moved_in, removed_stage, same_stage, stages_name, swept};
    use crate::boxside::payload::ordinal_upper;
    use crate::boxside::preflight::written_beneath;
    use crate::console::Console;
    use crate::outputs::manifest::Manifest;
    use crate::test_support::junction;

    /// A new, empty folder of the test `name`'s own.
    fn folder(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-inputs-{name}-{}", std::process::id()));
        if fs::symlink_metadata(&folder).is_ok() {
            fs::remove_dir_all(&folder).expect("an earlier run's folder is removed");
        }
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    /// The folder `path` held open, sharing deletion, so a racer may still rename it away.
    fn held(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .share_mode(Share::All.mode())
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("the folder can be held")
    }

    /// The number of entries in `folder`.
    fn entries(folder: &Path) -> usize {
        fs::read_dir(folder).expect("the folder can be listed").count()
    }

    /// The nonce of every stage these tests make.
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// A put's request of the work folder `k` below the box root `root`.
    fn put_asked<'a>(root: &'a str, work: &'a str) -> Asked<'a> {
        Asked {
            key: "k",
            work,
            bare: "",
            lock: "",
            nonce: NONCE,
            step: Step::Put,
            root,
        }
    }

    /// `path` as text.
    fn text(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// A folder below `folder` holding the file `keep.txt`, of the name `name`.
    fn keeping(folder: &Path, name: &str) -> PathBuf {
        let made = folder.join(name);
        fs::create_dir_all(&made).expect("the folder can be made");
        fs::write(made.join("keep.txt"), "keep").expect("the file can be written");
        made
    }

    /// The stages folder this box, elevated or not, uses below `root`.
    fn stages_path(root: &Path) -> PathBuf {
        root.join(stages_name(token_facts().expect("this process's token can be read").elevated))
    }

    /// The volume and file id of the file `path`.
    fn id_of(path: &Path) -> (u64, [u8; 16]) {
        file_id(&File::open(path).expect("the file can be opened")).expect("its id can be read")
    }

    /// A folder planted at this box's stages name below `root`, made with the security `sddl` names, holding `a.txt`:
    /// the path of that file.
    fn planted_stages(root: &Path, sddl: &str) -> PathBuf {
        fs::create_dir_all(root).expect("the root can be made");
        let descriptor = Descriptor::from_sddl(sddl).expect("the SDDL names a descriptor");
        let name = stages_name(token_facts().expect("this process's token can be read").elevated);
        drop(make_folder_beneath(&held(root), name, FILE_GENERIC_READ, Share::All, &descriptor).expect("the folder can be planted"));
        let planted = stages_path(root).join("a.txt");
        fs::write(&planted, "a").expect("a file in it can be written");
        planted
    }

    /// The owner of this box's folders, and this process's user and integrity level, as SDDL spells them.
    fn sddl_facts() -> (String, String, u32) {
        let facts = token_facts().expect("this process's token can be read");
        let owner = if facts.elevated { "BA".to_owned() } else { facts.user.clone() };
        (owner, facts.user, facts.integrity)
    }

    #[test]
    fn removed_stage_deletes_the_held_stage_and_leaves_what_its_path_names_untouched() {
        let folder = folder("stage-decoy");
        let root = folder.join("root");
        fs::create_dir_all(&root).expect("the root can be made");
        let (decoy, outside) = (keeping(&folder, "decoy"), keeping(&folder, "outside"));
        let link = folder.join("link");
        junction(&link, &outside);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));
        for (what, path) in [("a decoy folder", &decoy), ("a junction to an outside folder", &link)] {
            let Stage {
                root,
                stages,
                held,
                path: real,
            } = made_stage(&put_asked(&root_text, &work_text), &mut Console::new()).expect("the stage is made");
            fs::create_dir_all(real.join(r"files\a")).expect("an unpacked folder can be made");
            fs::write(real.join(r"files\a\b.txt"), "b").expect("an unpacked file can be written");
            let stage = Stage {
                root,
                stages,
                held,
                path: path.clone(),
            };

            let removal = removed_stage(&stage);
            drop(stage);

            removal.expect("the stage is removed");
            assert!(fs::symlink_metadata(&real).is_err(), "{what}: the held stage is deleted");
            assert!(path.join("keep.txt").exists(), "{what}: what the stage's path names is untouched");
        }
        assert_eq!(entries(&outside), 1, "nothing outside is removed");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn same_stage_refuses_a_path_that_leads_to_another_folder_than_the_held_stage() {
        let folder = folder("stage-mismatch");
        let root = folder.join("root");
        fs::create_dir_all(&root).expect("the root can be made");
        let decoy = keeping(&folder, "decoy");
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));
        let Stage {
            root,
            stages,
            held,
            path: real,
        } = made_stage(&put_asked(&root_text, &work_text), &mut Console::new()).expect("the stage is made");
        let stage = Stage {
            root,
            stages,
            held,
            path: decoy,
        };

        let checked = same_stage(&stage);

        let why = checked.expect_err("the decoy is refused");
        assert!(why.contains("leads to another folder than the stage the box made"), "{why}");
        removed_stage(&stage).expect("the stage is removed");
        drop(stage);
        assert!(fs::symlink_metadata(&real).is_err(), "the held stage is deleted");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn made_stage_refuses_a_junction_for_the_root() {
        let folder = folder("root-junction");
        let (target, root) = (folder.join("target"), folder.join("root"));
        fs::create_dir_all(&target).expect("the junction's target can be made");
        junction(&root, &target);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let made = made_stage(&put_asked(&root_text, &work_text), &mut Console::new());

        let why = made.map(drop).expect_err("the junction is refused");
        assert!(why.contains("is a link, a junction or not a folder"), "{why}");
        assert_eq!(entries(&target), 0, "nothing is made where the junction leads");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_foreign_stages_folder_is_refused_and_nothing_in_it_is_deleted() {
        let folder = folder("stages-foreign");
        let root = folder.join("root");
        let stages = stages_path(&root);
        fs::create_dir_all(stages.join(r"old\deep")).expect("the planted folder can be made");
        let planted = stages.join(r"old\deep\a.txt");
        fs::write(&planted, "a").expect("a file in it can be written");
        let id = id_of(&planted);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let made = made_stage(&put_asked(&root_text, &work_text), &mut Console::new());

        let why = made.map(drop).expect_err("the planted folder is refused");
        assert_eq!(
            why,
            format!(
                "cannot use {}: another account made it; remove it, or give it to Administrators",
                stages.display()
            )
        );
        assert_eq!(id_of(&planted), id, "the planted file survives as itself");
        assert_eq!(entries(&stages), 1, "nothing is made in the planted folder");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_stages_folder_another_put_just_made_is_adopted() {
        let folder = folder("stages-concurrent");
        let root = folder.join("root");
        fs::create_dir_all(&root).expect("the root can be made");
        let facts = token_facts().expect("this process's token can be read");
        let security = protected_descriptor(&facts).expect("the box's security can be made");
        let root_held = held(&root);
        let other_put = make_folder_beneath(&root_held, stages_name(facts.elevated), STAGES_ACCESS, Share::NoDelete, &security)
            .expect("the other put makes the stages folder");
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let stage = made_stage(&put_asked(&root_text, &work_text), &mut Console::new()).expect("the folder the other put made is adopted");

        assert!(same_file(&other_put, &stage.stages).expect("the ids can be read"), "the same folder");
        removed_stage(&stage).expect("the stage is removed");
        drop((stage, other_put, root_held));
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_with_the_box_dacl_but_no_label_is_not_adopted() {
        let folder = folder("stages-unlabelled");
        let root = folder.join("root");
        let (owner, user, _) = sddl_facts();
        let planted = planted_stages(&root, &format!("O:{owner}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user})"));
        let id = id_of(&planted);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let made = made_stage(&put_asked(&root_text, &work_text), &mut Console::new());

        let why = made.map(drop).expect_err("the unlabelled folder is refused");
        assert!(why.contains("another account made it"), "{why}");
        assert_eq!(id_of(&planted), id, "the planted file survives as itself");
        assert_eq!(entries(&stages_path(&root)), 1, "nothing is made in the planted folder");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn a_folder_denying_the_box_write_dac_is_refused_as_foreign() {
        let folder = folder("stages-denying");
        let root = folder.join("root");
        let (owner, user, integrity) = sddl_facts();
        let planted = planted_stages(
            &root,
            &format!("O:{owner}D:P(D;;WD;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user})S:(ML;OICI;NW;;;S-1-16-{integrity})"),
        );
        let id = id_of(&planted);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let made = made_stage(&put_asked(&root_text, &work_text), &mut Console::new());

        let why = made.map(drop).expect_err("the denying folder is refused");
        assert!(why.contains("another account made it"), "{why}");
        assert_eq!(id_of(&planted), id, "the planted file survives as itself");
        assert_eq!(entries(&stages_path(&root)), 1, "nothing is made in the planted folder");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    /// Elevated, the planted `inputs-stage` is this box's own name, so the put refuses it as another account's; either
    /// way the planted folder is left as it is.
    #[test]
    fn an_unelevated_put_leaves_the_elevated_stages_folder_alone() {
        assert_eq!(stages_name(true), "inputs-stage");
        assert_eq!(stages_name(false), "inputs-stage-medium");
        let folder = folder("stages-unelevated");
        let root = folder.join("root");
        let elevated = root.join("inputs-stage");
        fs::create_dir_all(&elevated).expect("the planted folder can be made");
        let planted = elevated.join("a.txt");
        fs::write(&planted, "a").expect("a file in it can be written");
        let id = id_of(&planted);
        let (root_text, work_text) = (text(&root), text(&root.join(r"work\k")));

        let made = made_stage(&put_asked(&root_text, &work_text), &mut Console::new());

        if token_facts().expect("this process's token can be read").elevated {
            made.map(drop).expect_err("the planted folder is refused");
        } else {
            let stage = made.expect("the put has a stages folder of its own");
            assert!(root.join("inputs-stage-medium").is_dir(), "the unelevated box's own folder");
            removed_stage(&stage).expect("the stage is removed");
            drop(stage);
        }
        assert_eq!(id_of(&planted), id, "the planted file survives as itself");
        assert_eq!(entries(&elevated), 1, "nothing is made in the planted folder");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn moved_in_refuses_a_folder_swapped_for_a_junction_and_writes_nothing_outside() {
        let folder = folder("moved-in");
        let (work, files, outside) = (folder.join("work"), folder.join("files"), folder.join("outside"));
        for made in [work.join("data"), files.join("data"), outside.clone()] {
            fs::create_dir_all(made).expect("the folder can be made");
        }
        fs::write(files.join(r"data\b.txt"), "b").expect("the staged file can be written");
        let (work_held, files_held) = (held(&work), held(&files));
        fs::rename(work.join("data"), work.join("data-away")).expect("the folder can be renamed away");
        junction(&work.join("data"), &outside);

        let moved = moved_in(&files_held, &work_held, "data/b.txt");

        let why = moved.expect_err("the junction is refused");
        assert!(why.contains("'data' is a link, a junction or not a folder"), "{why}");
        assert_eq!(entries(&outside), 0, "nothing is written outside");
        assert_eq!(entries(&work.join("data-away")), 0, "nothing is written in the folder renamed away");
        assert!(files.join(r"data\b.txt").exists(), "the staged copy stays in the stage");
        drop((work_held, files_held));
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn written_beneath_moves_a_folder_at_the_name_aside() {
        let folder = folder("written-beneath");
        let (info, outside) = (folder.join("info"), folder.join("outside"));
        fs::create_dir_all(info.join(r"exclude\inner")).expect("a folder can take the file's name");
        let inner = info.join(r"exclude\inner\a.txt");
        fs::write(&inner, "a").expect("a file in it can be written");
        let id = id_of(&inner);
        let taken = info.join("exclude.gate-moved-1");
        fs::write(&taken, "taken").expect("a file can take the first moved-aside name");
        let taken_id = id_of(&taken);
        fs::create_dir_all(outside.join("exclude")).expect("the outside folder can be made");
        fs::write(outside.join(r"exclude\keep.txt"), "keep").expect("the outside file can be written");
        let info_held = held(&info);
        let away = folder.join("info-away");
        fs::rename(&info, &away).expect("the held folder, sharing deletion, can be renamed away");
        junction(&info, &outside);

        let moved = written_beneath(&info_held, "exclude", "/a.txt\n").expect("the file is written");

        drop(info_held);
        assert_eq!(moved.as_deref(), Some("exclude.gate-moved-2"), "the first free name");
        assert_eq!(
            id_of(&away.join("exclude.gate-moved-1")),
            taken_id,
            "the file at the first name is not replaced"
        );
        assert_eq!(
            fs::read_to_string(away.join("exclude")).expect("the file is in the held folder"),
            "/a.txt\n",
            "the file takes the name"
        );
        assert_eq!(
            id_of(&away.join(r"exclude.gate-moved-2\inner\a.txt")),
            id,
            "the folder is moved aside whole, nothing in it deleted"
        );
        assert!(outside.join(r"exclude\keep.txt").exists(), "nothing outside is deleted or written");
        assert_eq!(entries(&outside), 1, "nothing else is made outside");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }

    #[test]
    fn swept_keeps_a_stale_copy_below_a_folder_swapped_for_a_junction_and_deletes_nothing_outside() {
        let folder = folder("swept");
        let (work, outside) = (folder.join("work"), folder.join("outside"));
        fs::create_dir_all(work.join("sub")).expect("the folder can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        fs::write(work.join(r"sub\x.txt"), "x").expect("the copy can be written");
        fs::write(outside.join("x.txt"), "x").expect("the outside file can be written");
        let (sha256, _) = crate::outputs::hash::sha256_of(&b"x"[..]).expect("the bytes can be hashed");
        let copy = Copy {
            path: "sub/x.txt".to_owned(),
            sha256,
            stamp: Stamp {
                size: 1,
                modified: 1,
                id: "0-0".to_owned(),
            },
        };
        let mut record = Record::default();
        record.files.insert(ordinal_upper(&copy.path), copy);
        let work_held = held(&work);
        fs::rename(work.join("sub"), work.join("sub-away")).expect("the folder can be renamed away");
        junction(&work.join("sub"), &outside);

        let kept = swept(&work_held, &record, &Manifest::default(), &mut Console::new());

        drop(work_held);
        assert_eq!(kept.values().collect::<Vec<_>>(), ["sub/x.txt"], "the copy is kept");
        assert!(outside.join("x.txt").exists(), "nothing outside is deleted");
        assert!(work.join(r"sub-away\x.txt").exists(), "nothing in the folder renamed away is deleted");
        fs::remove_dir_all(&folder).expect("the folder can be removed");
    }
}

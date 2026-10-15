//! `gate box inputs`: a routed run's ignored inputs, sent from the PC before the run. Its request names the `Step`:
//! `diff` reads the PC's inputs manifest and says which files the work folder lacks or holds otherwise, and `put` reads
//! the manifest and then an archive of those files, in the outputs archive's format, to the end of stdin, and installs
//! them. Both take the work folder's lock as a run does and lay the work folder out as a run does first, so a later run
//! finds the layout unchanged and clears nothing.
//!
//! What a put installed is kept in the inputs record `{Work}.inputs`, beside the tools and layout records: the
//! manifest's digest, and per file its path, size, SHA-256, and the last write and file id the box left it with. A diff
//! trusts a copy whose size, last write and file id the record holds and hashes any other. A put deletes a recorded
//! copy that left the manifest only while it still holds what was installed, and rewrites the tree's
//! `.git\info\exclude` from the record, so no input is cleaned by the checkout or committed back.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use gate_args::BoxSeams;
use serde_json::{Map, Value, json};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::answer::USAGE_STATUS;
use crate::boxside::payload::{Payload, escaped, grandparent, ordinal_upper};
use crate::boxside::preflight;
use crate::boxside::run::{self, FAILED_STATUS, FILE_FLAG_OPEN_REPARSE_POINT, REFUSED_STATUS};
use crate::boxside::tools;
use crate::console::Console;
use crate::outputs::archive::{Entry, PcFiles, below, check_folder, final_path, unpack};
use crate::outputs::hash::{SHA256_DIGITS, is_lower_hex, sha256_of};
use crate::outputs::manifest::{Held, MANIFEST_LIMIT, Manifest};
use crate::outputs::names::is_plain_path;
use crate::remote::{Refusal, busy_line, refused_line as tagged_line};

/// The bytes in one gibibyte.
const GIB: u64 = 1024 * 1024 * 1024;
/// The most bytes one put unpacks, decompressed: 4 GiB, unless the `--test-inputs-budget-bytes` seam says otherwise.
const INPUTS_BUDGET: u64 = 4 * GIB;
/// The folder below the box's root that holds each put's stage, `{root}\inputs-stage\{key}-{nonce}`.
const STAGES: &str = "inputs-stage";
/// The hex digits of a run's outputs nonce, which a stage's name ends with.
const NONCE_DIGITS: usize = 32;
/// The line refusing a request that names a sibling pair.
const PAIR_REFUSED: &str = "gate box: inputs for a sibling pair are not taken yet";
/// Every sharing given: a copy the box reads is neither held against nor holds off any other opener.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// The record of the inputs installed in the work folder `work`: `{work}.inputs`.
pub(super) fn record_path(work: &str) -> String {
    format!("{work}.inputs")
}

/// The tag a run whose `InputsDigest` the work folder's inputs record does not hold is refused with.
pub(super) fn refused_line(nonce: &str) -> String {
    format!("gate-remote-refused: {nonce} inputs")
}

/// Why the work folder `work` does not hold the inputs of the manifest whose digest is `sent`: no inputs record, one
/// that cannot be read, or one of another digest; `None` when it holds them.
pub(super) fn unmatched(work: &str, sent: &str) -> Option<String> {
    let path = record_path(work);
    match tools::read_record(&path) {
        Ok(None) => Some(format!("{path} does not exist")),
        Ok(Some(text)) => match Record::parse(&text) {
            Ok(record) if record.digest == sent => None,
            Ok(_) => Some(format!("{path} holds another manifest's digest")),
            Err(why) => Some(format!("{path} {why}")),
        },
        Err(error) => Some(format!("{path} cannot be read: {error}")),
    }
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

/// The PC's inputs manifest, and the SHA-256 of its line as received, without its newline.
struct Sent {
    manifest: Manifest,
    digest: String,
}

/// The manifest line read from `input`, up to its first newline or the input's end and never more than
/// [`MANIFEST_LIMIT`] bytes; else why it cannot be read, a path in it that is not a plain relative path among them.
fn read_manifest(input: impl BufRead) -> Result<Sent, String> {
    let mut line = super::bounded_line(input, MANIFEST_LIMIT as u64).map_err(|unread| match unread {
        super::Unread::OverLimit => "the inputs manifest reaches 64 MiB without a newline".to_owned(),
        super::Unread::Failed(error) => format!("the inputs manifest could not be read: {error}"),
    })?;
    if line.ends_with(b"\n") {
        line.pop();
    }
    let (digest, _) = sha256_of(line.as_slice()).map_err(|error| format!("the inputs manifest could not be hashed: {error}"))?;
    let text = String::from_utf8(line).map_err(|_| "the inputs manifest is not UTF-8".to_owned())?;
    let manifest = Manifest::parse(&text)?;
    if let Some((path, _)) = manifest.files().find(|(path, _)| !is_plain_path(path)) {
        return Err(format!(
            "the inputs manifest names '{}', which is not a plain relative path",
            escaped(path)
        ));
    }
    Ok(Sent { manifest, digest })
}

/// Answers `gate box inputs`: the request read, then the manifest from stdin, before any lock is taken, so a client
/// that stalls holds nothing; then the lock, the layout and the step ([`locked`]). A payload refused is a usage error
/// (2), a manifest that cannot be read fails the step (1).
pub(super) fn inputs(payload: &Payload, seams: &BoxSeams, console: &mut Console) -> u32 {
    let asked = match Asked::read(payload) {
        Ok(asked) => asked,
        Err(line) => {
            console.err(&line);
            return u32::from(USAGE_STATUS);
        }
    };
    match read_manifest(io::stdin().lock()) {
        Ok(sent) => locked(&asked, &sent, seams, console),
        Err(why) => {
            console.err(&asked.failed(&why));
            FAILED_STATUS
        }
    }
}

/// The step once the manifest is read: the work folder's lock taken as a run takes it, a held lock refused `busy`
/// (75), and the work folder laid out as a run lays it out ([`run::lay_out_alone`]) and held through the step.
fn locked(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, console: &mut Console) -> u32 {
    let _lock = match run::take_lock(asked.lock) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            console.err(&tagged_line(asked.nonce, Refusal::Busy));
            console.err(&busy_line(asked.key));
            return REFUSED_STATUS;
        }
        Err(line) => {
            console.err(&line);
            return FAILED_STATUS;
        }
    };
    let _held = match run::lay_out_alone(asked.work, asked.bare, asked.lock, asked.nonce, console) {
        Ok(held) => held,
        Err(status) => return status,
    };
    let done = match asked.step {
        Step::Diff => diff(asked, sent, console),
        Step::Put => put(asked, sent, seams, console),
    };
    done.unwrap_or_else(|why| {
        console.err(&asked.failed(&why));
        FAILED_STATUS
    })
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

/// The inputs record: the digest of the manifest last installed from, and each input installed, by its path's simple
/// uppercase.
#[derive(Default)]
struct Record {
    digest: String,
    files: BTreeMap<String, Copy>,
}

impl Record {
    /// The record `text` holds; else what is wrong with it.
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
            let copy = recorded_copy(path, file).ok_or_else(not_a_record)?;
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

/// The work folder's inputs record, and the line saying a record that is not one was taken as empty; else why the
/// record cannot be read: one that is a link, a folder or a file of more than one name is not read.
fn recorded(work: &str) -> Result<(Record, Option<String>), String> {
    let path = record_path(work);
    match tools::read_record(&path) {
        Ok(None) => Ok((Record::default(), None)),
        Ok(Some(text)) => Ok(Record::parse(&text).map_or_else(
            |why| {
                (
                    Record::default(),
                    Some(format!("gate: remote inputs record {path} {why}; every copy is hashed")),
                )
            },
            |record| (record, None),
        )),
        Err(error) => Err(format!("{path} cannot be read: {error}")),
    }
}

/// The work folder's full and final path, as each copy's final path is compared with it.
fn final_work(work: &str) -> Result<PathBuf, String> {
    fs::canonicalize(work).map_err(|error| format!("cannot resolve {work}: {error}"))
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

/// The copy at the `/`-separated `path` below `work`, opened as itself and never through a link there: a plain disk
/// file of one name whose final path is inside `inside`, the work folder's own; with its stamp. `None` when nothing is
/// at its path; else why it is no such file.
fn opened_copy(work: &str, inside: &Path, path: &str) -> io::Result<Option<(File, Stamp)>> {
    let full = below(Path::new(work), path);
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&full);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    gate_win32::check_plain_file(&file)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("it is a link or a junction"));
    }
    if !final_path(&file)?.starts_with(inside) {
        return Err(io::Error::other("it resolves outside the work folder"));
    }
    let stamp = stamp_of(&file)?;
    Ok(Some((file, stamp)))
}

/// The copy at `path` as the record would hold it, when it holds what the manifest's `held` says: the record's own
/// entry for a copy whose stamp it holds, else the copy's hash and stamp read now. `None` for a copy missing, that is
/// no plain file inside the work folder, that cannot be read, or that holds anything else.
fn matched_copy(work: &str, inside: &Path, path: &str, held: &Held, record: &Record) -> Option<Copy> {
    let (file, stamp) = opened_copy(work, inside, path).ok().flatten()?;
    if stamp.size != held.stamp.size {
        return None;
    }
    if let Some(copy) = record.get(path).filter(|copy| copy.stamp == stamp) {
        return (copy.sha256 == held.sha256).then(|| copy.clone());
    }
    let (sha256, _) = sha256_of(&file).ok()?;
    (sha256 == held.sha256).then(|| Copy {
        path: path.to_owned(),
        sha256,
        stamp,
    })
}

/// The diff: `gate-remote-inputs: need <n>`, then `gate-remote-inputs: path <path>` for each file of the manifest the
/// work folder lacks or holds otherwise, in the manifest's order, each path escaped; 0. Else why not.
fn diff(asked: &Asked<'_>, sent: &Sent, console: &mut Console) -> Result<u32, String> {
    let record = match recorded(asked.work) {
        Ok((record, line)) => {
            if let Some(line) = line {
                console.err(&line);
            }
            record
        }
        Err(why) => {
            console.err(&format!("gate: remote inputs record {}; every copy is hashed", escaped(&why)));
            Record::default()
        }
    };
    let inside = final_work(asked.work)?;
    let needed: Vec<&str> = sent
        .manifest
        .files()
        .filter(|(path, held)| matched_copy(asked.work, &inside, path, held, &record).is_none())
        .map(|(path, _)| path)
        .collect();
    console.out(&format!("gate-remote-inputs: need {}", needed.len()));
    for path in needed {
        console.out(&format!("gate-remote-inputs: path {}", escaped(path)));
    }
    Ok(0)
}

/// The put: the archive on stdin unpacked into a stage of its own and its files moved into the work folder
/// ([`installed`]), then `gate-remote-inputs: installed <n>`; 0. Else why not. The stage is removed on every way out.
fn put(asked: &Asked<'_>, sent: &Sent, seams: &BoxSeams, console: &mut Console) -> Result<u32, String> {
    let (record, line) = recorded(asked.work)?;
    if let Some(line) = line {
        console.err(&line);
    }
    let stage = fresh_stage(asked)?;
    let done = installed(asked, sent, &record, &stage, seams, console);
    let removal = preflight::remove_entry(&stage).map_err(|error| format!("cannot remove the stage {}: {error}", stage.display()));
    let count = done?;
    removal?;
    console.out(&format!("gate-remote-inputs: installed {count}"));
    Ok(0)
}

/// The stage of this put, `{root}\inputs-stage\{key}-{nonce}`, not made yet: the stages folder made when missing and
/// refused when it is a link, a junction or not a folder, and every stage an earlier put of the work folder left
/// there removed. Else why not.
fn fresh_stage(asked: &Asked<'_>) -> Result<PathBuf, String> {
    let stages = Path::new(asked.root).join(STAGES);
    fs::create_dir_all(&stages).map_err(|error| format!("cannot create {}: {error}", stages.display()))?;
    check_folder(&stages).map_err(|error| error.to_string())?;
    let unlisted = |error: io::Error| format!("cannot list {}: {error}", stages.display());
    for entry in fs::read_dir(&stages).map_err(unlisted)? {
        let entry = entry.map_err(unlisted)?;
        if entry.file_name().to_str().is_some_and(|name| is_stage_of(name, asked.key)) {
            let path = entry.path();
            preflight::remove_entry(&path).map_err(|error| format!("cannot remove the stage {}: {error}", path.display()))?;
        }
    }
    Ok(stages.join(format!("{}-{}", asked.key, asked.nonce)))
}

/// Whether `name` is the name of a stage of the work folder `key`: `{key}-{nonce}`, the key in any ASCII case.
fn is_stage_of(name: &str, key: &str) -> bool {
    let nonce = name
        .get(..key.len())
        .filter(|start| start.eq_ignore_ascii_case(key))
        .and_then(|_| name.get(key.len()..))
        .and_then(|rest| rest.strip_prefix('-'));
    nonce.is_some_and(|nonce| is_lower_hex(nonce, NONCE_DIGITS))
}

/// Unpacks the archive on stdin into `stage` within the budget, checks every member against the manifest and every
/// target in the work folder before any is written, then moves each file in, deletes the stale copies, writes the
/// record and the tree's exclude file. The count of files installed; else why not. Nothing in the work folder is
/// written before every check has passed.
fn installed(asked: &Asked<'_>, sent: &Sent, record: &Record, stage: &Path, seams: &BoxSeams, console: &mut Console) -> Result<usize, String> {
    let budget = seams.inputs_budget_bytes.unwrap_or(INPUTS_BUDGET);
    let none = Manifest::default();
    let go_on = || true;
    // Copies come from the archive's own members only: no file of the box's is a copy's source.
    let pc = PcFiles {
        root: Path::new(asked.work),
        manifest: &none,
        go_on: &go_on,
    };
    let entries = unpack(io::stdin().lock(), stage, budget, &pc).map_err(|error| error.to_string())?;
    let inside = final_work(asked.work)?;
    for entry in &entries {
        in_manifest(entry, &sent.manifest)?;
        free_target(asked.work, &inside, &entry.path)?;
    }
    let mut now = Record {
        digest: sent.digest.clone(),
        files: BTreeMap::new(),
    };
    for entry in &entries {
        let stamp = moved_in(stage, asked.work, &inside, &entry.path)?;
        let copy = Copy {
            path: entry.path.clone(),
            sha256: entry.sha256.clone(),
            stamp,
        };
        now.files.insert(ordinal_upper(&entry.path), copy);
    }
    for line in removed_stale(asked.work, &inside, record, &sent.manifest) {
        console.err(&line);
    }
    present(asked.work, &inside, &sent.manifest, record, &mut now);
    let path = record_path(asked.work);
    tools::write_record_file(&path, &now.text()).map_err(|error| format!("cannot write {path}: {error}"))?;
    write_exclude(asked.work, &now)?;
    Ok(entries.len())
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

/// Whether the file `path` may be installed below `work`: each folder on its way that is there a folder of its own,
/// never a link or a junction, and whatever is at its path a plain file of one name inside the work folder
/// ([`opened_copy`]); else the refusal naming the path.
fn free_target(work: &str, inside: &Path, path: &str) -> Result<(), String> {
    let refused = |why: String| format!("cannot install '{}': {why}", escaped(path));
    let parents = path.rsplit_once('/').map_or("", |(parents, _)| parents);
    let mut folder = PathBuf::from(work);
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        folder.push(part);
        match fs::symlink_metadata(&folder) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(refused(error.to_string())),
            Ok(_) => check_folder(&folder).map_err(|error| refused(error.to_string()))?,
        }
    }
    opened_copy(work, inside, path).map(|_| ()).map_err(|error| refused(error.to_string()))
}

/// Moves the staged file `path` below `stage` to the same path below `work`, replacing a file there: each folder on
/// its way made when missing and refused when it is a link, a junction or not a folder, and the folder it goes in
/// refused when its final path is not inside `inside`. The moved file's stamp, read through a handle opened on the
/// staged file before the move; else why not.
fn moved_in(stage: &Path, work: &str, inside: &Path, path: &str) -> Result<Stamp, String> {
    let failed = |why: String| format!("cannot install '{}': {why}", escaped(path));
    let parents = path.rsplit_once('/').map_or("", |(parents, _)| parents);
    let mut folder = PathBuf::from(work);
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        folder.push(part);
        match fs::create_dir(&folder) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(failed(error.to_string())),
            _ => check_folder(&folder).map_err(|error| failed(error.to_string()))?,
        }
    }
    let opened = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&folder)
        .and_then(|held| final_path(&held));
    if !opened.map_err(|error| failed(error.to_string()))?.starts_with(inside) {
        return Err(failed("its folder resolves outside the work folder".to_owned()));
    }
    let source = below(stage, path);
    // Its attributes alone, every sharing given: the handle neither refuses the move nor any other opener.
    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .open(&source)
        .map_err(|error| failed(error.to_string()))?;
    fs::rename(&source, below(Path::new(work), path)).map_err(|error| failed(error.to_string()))?;
    stamp_of(&handle).map_err(|error| failed(error.to_string()))
}

/// Deletes each copy `record` holds whose path left `manifest`, only while it is a plain file inside the work folder
/// that still hashes to what the record holds; every other file is left. The lines saying each that could not be
/// deleted.
fn removed_stale(work: &str, inside: &Path, record: &Record, manifest: &Manifest) -> Vec<String> {
    let mut lines = Vec::new();
    for copy in record.files.values().filter(|copy| manifest.get(&copy.path).is_none()) {
        let Ok(Some((file, _))) = opened_copy(work, inside, &copy.path) else {
            continue;
        };
        let unchanged = sha256_of(&file).is_ok_and(|(sha256, _)| sha256 == copy.sha256);
        drop(file);
        if !unchanged {
            continue;
        }
        if let Err(error) = fs::remove_file(below(Path::new(work), &copy.path)) {
            lines.push(format!(
                "gate: remote could not remove the stale input {}: {}",
                escaped(&copy.path),
                escaped(&error.to_string())
            ));
        }
    }
    lines
}

/// Adds to `now`, which holds the files this put installed, every other file of `manifest` the work folder holds as the
/// manifest says ([`matched_copy`]).
fn present(work: &str, inside: &Path, manifest: &Manifest, record: &Record, now: &mut Record) {
    for (path, held) in manifest.files() {
        let key = ordinal_upper(path);
        if now.files.contains_key(&key) {
            continue;
        }
        if let Some(copy) = matched_copy(work, inside, path, held, record) {
            now.files.insert(key, copy);
        }
    }
}

/// Rewrites the tree's `.git\info\exclude` from `record`: one line per recorded path as a literal pattern, anchored
/// with a leading `/` and each glob character escaped. The `.git` must be a folder of its own; `.git\info` is made a
/// folder of its own, a link there removed as itself, and the file written without following a link
/// ([`preflight::written`]). Else why not.
fn write_exclude(work: &str, record: &Record) -> Result<(), String> {
    let git = Path::new(work).join(".git");
    if !preflight::is_real_folder(&git) {
        return Err(format!("{} is not a folder of its own", git.display()));
    }
    let info = git.join("info");
    preflight::made_folder(&info, true)?;
    let mut text = String::new();
    for copy in record.files.values() {
        text.push_str(&literal_pattern(&copy.path));
        text.push('\n');
    }
    let exclude = info.join("exclude");
    preflight::written(&exclude, &text).map_err(|why| format!("cannot write {}: {why}", exclude.display()))
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

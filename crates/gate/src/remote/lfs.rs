//! A routed run's Git LFS objects on the PC: the pointers a snapshot commit's tree holds, the manifest of the objects
//! they name, where the repository keeps those objects, and the archive of the ones the box says it lacks.
//!
//! A pointer is a blob of at most 1024 bytes whose path's `filter` attribute is `lfs` and whose bytes are exactly a
//! spec-v1 pointer: its version line, its `oid sha256:` line and its `size` line, in that order and nothing else. Any
//! other blob, a raw file committed under `filter=lfs` included, is left alone. Objects are read from the repository's
//! LFS store, `<common>\lfs\objects` or the `objects` folder of `lfs.storage` when the repository sets it, and each is
//! hashed and its size checked against its pointer as it is packed.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gate_router::escaped;
use gate_state::FallbackReason;
use gate_win32::Interrupt;
use gate_wire::{SHA256_DIGITS, is_lower_hex};
use sha2::{Digest, Sha256};

use super::channel;
use super::client::{Attempt, End, RawSink, Sent, Watch};
use super::git::{Call, Git, GitBounds, GitError, OUTPUT_CAP};
use super::inputs::{Asking, MB, Step, TMP, Unsent, judged, tmp_file, unexpected};
use super::outputs::{Archive, Unbuilt};
use super::payload::{self, InputsStep, Location};
use super::snapshot::{self, Bounded};
use super::syncback::{SLICE_CAP, attribute_values, slices_asking};
use crate::outputs::archive::pack;
use crate::outputs::hash::sha256_of;
use crate::outputs::manifest::{Chosen, HeldBy};
use crate::outputs::walk::{Found, stamp_of};

/// The most bytes a blob read as a pointer may hold.
pub(crate) const POINTER_MOST: u64 = 1024;
/// The first line of every spec-v1 pointer.
const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1\n";
/// The most objects one manifest names.
pub(super) const OBJECTS: usize = 50_000;
/// The room `git cat-file --batch` is allowed per object beside its bytes: its header line and the newline after it.
const HEADER_ROOM: usize = 128;
/// The most bytes of `git config --get lfs.storage` read.
const STORAGE_MOST: u64 = 64 * 1024;
/// What a refusal of an object the store does not hold as its pointer names says to do.
const FETCH: &str = "run `git lfs fetch` and route again";
/// The box's diff lines.
const NEED: &str = "gate-remote-lfs: need ";
const OID: &str = "gate-remote-lfs: oid ";
/// The `-c` pin of every git call that reads a tree's attributes, on the PC and on the box: no `core.attributesFile`,
/// so a path's attributes are those the tree's own `.gitattributes` files give it, as the builder reads them.
pub(crate) const NO_ATTRIBUTES_FILE: [&str; 2] = ["-c", "core.attributesFile="];
/// The variable, set to `1`, that has git read no system-wide attributes file, given to every git call that reads a
/// tree's attributes, on the PC and on the box.
pub(crate) const NO_SYSTEM_ATTRIBUTES: &str = "GIT_ATTR_NOSYSTEM";

/// One pointer of a commit's tree: its `/`-separated path, and the oid and size of the object it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pointer {
    pub(crate) path: String,
    pub(crate) oid: String,
    pub(crate) size: u64,
}

/// One object a commit's pointers name: its oid, its size, and the path of the first pointer naming it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Object {
    pub(crate) oid: String,
    pub(crate) size: u64,
    pub(crate) path: String,
}

/// A commit's pointers, in the tree's order, and the objects they name, each once, in the order of their first pointer.
#[derive(Debug, Default)]
pub(crate) struct Listed {
    pub(crate) pointers: Vec<Pointer>,
    pub(crate) objects: Vec<Object>,
}

/// The git calls about one repository: the git found, the repository as its calls ask about it, the bounds of each
/// and the console interrupt that ends it.
struct Calls<'a> {
    git: &'a Git,
    repo: Bounded<'a>,
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
}

impl Calls<'_> {
    /// What git `args` wrote on standard output, fed `stdin`, read up to `cap` bytes; else the interrupt, or why not.
    /// Each call reads the tree's attributes as the builder does ([`NO_ATTRIBUTES_FILE`], [`NO_SYSTEM_ATTRIBUTES`]).
    fn output(&self, args: &[&str], stdin: Option<&[u8]>, cap: usize) -> Result<Vec<u8>, Unbuilt> {
        let mut set = self.repo.variables();
        let pinned = as_the_builder(&mut set, args);
        let call = Call {
            set: &set,
            stdin,
            interrupt: self.interrupt,
            ..Call::bounded(self.repo.root, &pinned, self.bounds)
        };
        self.git.output_bytes_within(&call, cap).map_err(unbuilt)
    }
}

/// The arguments of a listing call of `args` that reads a tree's attributes as the builder does:
/// [`NO_ATTRIBUTES_FILE`] before `args`, and [`NO_SYSTEM_ATTRIBUTES`] added to the call's variables `set`.
fn as_the_builder<'a>(set: &mut Vec<(&'a str, &'a OsStr)>, args: &[&'a str]) -> Vec<&'a str> {
    set.push((NO_SYSTEM_ATTRIBUTES, OsStr::new("1")));
    let mut pinned = NO_ATTRIBUTES_FILE.to_vec();
    pinned.extend_from_slice(args);
    pinned
}

/// A git call's failure as a step's: the interrupt, or why it failed.
pub(super) fn unbuilt(error: GitError) -> Unbuilt {
    match error {
        GitError::Interrupted => Unbuilt::Interrupted,
        GitError::Failed(why) | GitError::OverCap(why) => Unbuilt::Failed(why),
    }
}

/// The pointers of `commit`'s tree in the repository `repo`, and the objects they name, as [`pointers_of`] reads them;
/// else the interrupt, or why git or its answer failed.
pub(super) fn listed(repo: Bounded<'_>, commit: &str, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Listed, Unbuilt> {
    let git = Git::find(repo.root).map_err(Unbuilt::Failed)?;
    let calls = Calls {
        git: &git,
        repo,
        bounds,
        interrupt,
    };
    pointers_of(commit, &mut |args, stdin, cap| calls.output(args, stdin, cap), Unbuilt::Failed)
}

/// The pointers of `commit`'s tree in the repository `repo` among its blobs `small`, read from a listing the caller
/// made, as [`pointers_among`] reads them; else the interrupt, or why git or its answer failed.
pub(super) fn listed_among(
    repo: Bounded<'_>,
    commit: &str,
    small: &Small,
    bounds: GitBounds,
    interrupt: Option<Interrupt>,
) -> Result<Listed, Unbuilt> {
    if !small.worth_asking() {
        return Ok(Listed::default());
    }
    let git = Git::find(repo.root).map_err(Unbuilt::Failed)?;
    let calls = Calls {
        git: &git,
        repo,
        bounds,
        interrupt,
    };
    pointers_among(commit, small, &mut |args, stdin, cap| calls.output(args, stdin, cap), Unbuilt::Failed)
}

/// Why the builder would read other attributes of the repository `repo`'s tree than this PC does: its common git
/// folder's `info/attributes` is there and not empty, which this PC's `git check-attr` reads and the builder's checkout
/// of the snapshot never sees. `None` when it is missing or empty; else the interrupt, or why git or the file failed.
pub(super) fn unseen_attributes(repo: Bounded<'_>, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<Option<String>, Unbuilt> {
    let common = match repo.place {
        Some(place) => place.common.clone(),
        None => snapshot::place_of(repo, bounds, interrupt).map_err(unbuilt)?.common,
    };
    // git prints the folder with `/`; the line names the file as Windows spells a path.
    let path = PathBuf::from(common.replace('/', "\\")).join("info").join("attributes");
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() == 0 => Ok(None),
        Ok(_) => Ok(Some(format!("{} sets attributes the builder cannot see", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Unbuilt::Failed(format!("cannot read {}: {error}", path.display()))),
    }
}

/// The most bytes of a `.gitattributes` file read for the LFS filter.
const ATTRIBUTES_MOST: u64 = 1024 * 1024;

/// Whether the repo at the worktree `root` uses LFS at all: a `.gitattributes` file git tracks there, at any depth (`git
/// ls-files`), names the LFS filter as it is on disk ([`names_lfs_filter`]). Else the interrupt, or why git could not
/// list them.
pub(crate) fn tracks_lfs_filter(root: &Path, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<bool, GitError> {
    let git = Git::find(root).map_err(GitError::Failed)?;
    let args = ["ls-files", "-z", "--", ":(glob)**/.gitattributes"];
    let call = Call {
        interrupt,
        ..Call::bounded(root, &args, bounds)
    };
    let listing = git.output_bytes_within(&call, OUTPUT_CAP)?;
    Ok(String::from_utf8_lossy(&listing)
        .split('\0')
        .filter(|path| !path.is_empty())
        .any(|path| names_lfs_filter(&root.join(path))))
}

/// Whether any of the `.gitattributes` files at the `/`-separated `paths` below the worktree `root` names the LFS
/// filter as it is on disk ([`names_lfs_filter`]).
pub(crate) fn names_lfs_filter_among(root: &Path, paths: &[String]) -> bool {
    paths.iter().any(|path| names_lfs_filter(&root.join(path)))
}

/// Whether the `.gitattributes` file at `path`, as it is on disk, names the LFS filter: a line that is not a comment
/// holding the word `filter=lfs` within its first [`ATTRIBUTES_MOST`] bytes, or any file longer than that, which the
/// builder's listing reads whole and may find the word in. A file that is not there, or cannot be read, names none.
fn names_lfs_filter(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(ATTRIBUTES_MOST + 1).read_to_end(&mut bytes).is_err() {
        return false;
    }
    if bytes.len() as u64 > ATTRIBUTES_MOST {
        return true;
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| line.split_ascii_whitespace().any(|word| word == "filter=lfs"))
}

/// A tree's blobs the pointer listing reads: each regular file of at most [`POINTER_MOST`] bytes
/// ([`is_small_blob`]), in the tree's order, and whether the tree holds a `.gitattributes` file at any depth
/// ([`is_attributes`]).
#[derive(Debug, Default)]
pub(crate) struct Small {
    pub(crate) blobs: Vec<Blob>,
    pub(crate) attributes: bool,
}

impl Small {
    /// Whether git is asked about the blobs at all: there are some, and the tree holds an attributes file, without
    /// which no path of it has the `lfs` filter as the box reads the snapshot's attributes.
    fn worth_asking(&self) -> bool {
        !self.blobs.is_empty() && self.attributes
    }
}

/// Whether a tree's entry of `mode` and `kind`, `size` bytes, may be a pointer: a regular file, never a link or a
/// submodule, of at most [`POINTER_MOST`] bytes.
pub(crate) fn is_small_blob(mode: &str, kind: &str, size: u64) -> bool {
    kind == "blob" && mode != "120000" && size <= POINTER_MOST
}

/// Whether the `/`-separated `path` names a `.gitattributes` file.
pub(crate) fn is_attributes(path: &str) -> bool {
    path.rsplit('/').next() == Some(".gitattributes")
}

/// A git call of the listing: what git wrote on standard output for its arguments, fed its standard input when it has
/// one, read up to its cap in bytes; else the caller's failure.
pub(crate) type GitOutput<'a, E> = dyn FnMut(&[&str], Option<&[u8]>, usize) -> Result<Vec<u8>, E> + 'a;

/// The pointers of `commit`'s tree and the objects they name, each git call made through `output`: its blobs of at most
/// 1024 bytes (`git ls-tree`), then those [`pointers_among`] reads. A tree with no `.gitattributes` holds no pointer,
/// so nothing more of its listing is read, a path that is not UTF-8 included. The box's checkout reads a tree this way,
/// and the PC's listing of a sibling's tree.
pub(crate) fn pointers_of<E>(commit: &str, output: &mut GitOutput<'_, E>, failed: fn(String) -> E) -> Result<Listed, E> {
    let listing = output(&["ls-tree", "-r", "-l", "-z", commit], None, OUTPUT_CAP)?;
    if !holds_attributes(&listing) {
        return Ok(Listed::default());
    }
    let small = Small {
        blobs: small_blobs(&listing).map_err(failed)?,
        attributes: true,
    };
    pointers_among(commit, &small, output, failed)
}

/// The pointers among `commit`'s blobs `small` and the objects they name, each git call made through `output`: with no
/// blob, or no `.gitattributes` in the tree, none and no call; else those whose `filter` is `lfs` at that commit (`git
/// check-attr --source`), read in one `git cat-file --batch` and kept when they parse as a pointer. With no blob whose
/// `filter` is `lfs`, nothing is read. Two pointers naming one object at two sizes are refused. Else `output`'s
/// failure, or why git's answer cannot be read, made a failure by `failed`.
fn pointers_among<E>(commit: &str, small: &Small, output: &mut GitOutput<'_, E>, failed: fn(String) -> E) -> Result<Listed, E> {
    if !small.worth_asking() {
        return Ok(Listed::default());
    }
    let tracked = filtered_lfs(output, failed, commit, &small.blobs)?;
    if tracked.is_empty() {
        return Ok(Listed::default());
    }
    let pointers = read_pointers(output, failed, &tracked)?;
    let objects = objects_of(&pointers).map_err(failed)?;
    Ok(Listed { pointers, objects })
}

/// A blob of a tree: its `/`-separated path and its object id.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Blob {
    pub(crate) path: String,
    pub(crate) id: String,
}

/// Whether `git ls-tree -r -z`'s `listing` names a `.gitattributes` file at any depth.
fn holds_attributes(listing: &[u8]) -> bool {
    listing
        .split(|byte| *byte == 0)
        .filter_map(|record| record.iter().position(|byte| *byte == b'\t').map(|tab| &record[tab + 1..]))
        .any(|path| std::str::from_utf8(path).is_ok_and(is_attributes))
}

/// The blobs of at most [`POINTER_MOST`] bytes that `git ls-tree -r -l -z`'s `listing` names, in its order: regular
/// files only, never a link or a submodule. Else why the listing cannot be read, a path that is not UTF-8 among them.
fn small_blobs(listing: &[u8]) -> Result<Vec<Blob>, String> {
    let mut blobs = Vec::new();
    for record in listing.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "git ls-tree printed an entry with no tab".to_owned())?;
        let head = std::str::from_utf8(&record[..tab]).map_err(|_| "git ls-tree printed an entry that is not UTF-8".to_owned())?;
        let words: Vec<&str> = head.split_ascii_whitespace().collect();
        let [mode, kind, id, size] = words[..] else {
            return Err(format!("git ls-tree printed '{}'", escaped(head)));
        };
        // Only a blob has a size: git prints `-` for a submodule.
        if kind != "blob" {
            continue;
        }
        let size: u64 = size.parse().map_err(|_| format!("git ls-tree printed '{}'", escaped(head)))?;
        if !is_small_blob(mode, kind, size) {
            continue;
        }
        let path = std::str::from_utf8(&record[tab + 1..]).map_err(|_| {
            format!(
                "the tree holds a path that is not UTF-8: {}",
                escaped(&String::from_utf8_lossy(&record[tab + 1..]))
            )
        })?;
        blobs.push(Blob {
            path: path.to_owned(),
            id: id.to_owned(),
        });
    }
    Ok(blobs)
}

/// The blobs of `blobs` whose `filter` attribute is `lfs` at `commit`, in their order, asked of `git check-attr
/// --source <commit> -z --stdin filter` in slices whose answers stay under the runner's cap; else the interrupt, or why
/// git or its answer failed.
fn filtered_lfs<'b, E>(output: &mut GitOutput<'_, E>, failed: fn(String) -> E, commit: &str, blobs: &'b [Blob]) -> Result<Vec<&'b Blob>, E> {
    let args = ["check-attr", "--source", commit, "-z", "--stdin", "filter"];
    let paths: Vec<&str> = blobs.iter().map(|blob| blob.path.as_str()).collect();
    let mut lfs = Vec::new();
    let mut at = 0;
    for slice in slices_asking(&paths, 1, SLICE_CAP) {
        let fed: String = slice.iter().flat_map(|path| [*path, "\0"]).collect();
        let answer = output(&args, Some(fed.as_bytes()), OUTPUT_CAP)?;
        let unreadable = |why: &str| failed(format!("git check-attr's answer cannot be read: {why}"));
        let answer = String::from_utf8(answer).map_err(|_| unreadable("it is not UTF-8"))?;
        let values = attribute_values(&answer, slice, ["filter"]).map_err(|why| unreadable(&why))?;
        lfs.extend(
            blobs[at..at + slice.len()]
                .iter()
                .zip(values)
                .filter(|(_, [filter])| *filter == "lfs")
                .map(|(blob, _)| blob),
        );
        at += slice.len();
    }
    Ok(lfs)
}

/// The pointers among `blobs`, in their order, read in one `git cat-file --batch`; else the interrupt, or why git or
/// its answer failed.
fn read_pointers<E>(output: &mut GitOutput<'_, E>, failed: fn(String) -> E, blobs: &[&Blob]) -> Result<Vec<Pointer>, E> {
    let fed: String = blobs.iter().flat_map(|blob| [blob.id.as_str(), "\n"]).collect();
    let per_blob = usize::try_from(POINTER_MOST).unwrap_or(usize::MAX).saturating_add(HEADER_ROOM);
    let cap = blobs.len().saturating_mul(per_blob).saturating_add(1);
    let answer = output(&["cat-file", "--batch"], Some(fed.as_bytes()), cap)?;
    pointers_in(&answer, blobs).map_err(|why| failed(format!("git cat-file's answer cannot be read: {why}")))
}

/// The pointers of `blobs` in `git cat-file --batch`'s `answer`: for each blob, in order, its header `<id> blob <size>`,
/// its bytes and a newline, kept when the bytes parse as a pointer ([`parsed_pointer`]); else why the answer is not one.
fn pointers_in(answer: &[u8], blobs: &[&Blob]) -> Result<Vec<Pointer>, String> {
    let mut rest = answer;
    let mut pointers = Vec::new();
    for blob in blobs {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| format!("no header for {}", escaped(&blob.path)))?;
        let size = batch_size(&rest[..end], blob)?;
        let start = end + 1;
        let bytes = rest
            .get(start..start + size)
            .ok_or_else(|| format!("the bytes of {} are cut short", escaped(&blob.path)))?;
        if rest.get(start + size) != Some(&b'\n') {
            return Err(format!("the bytes of {} do not end with a newline", escaped(&blob.path)));
        }
        if let Some((oid, size)) = parsed_pointer(bytes) {
            pointers.push(Pointer {
                path: blob.path.clone(),
                oid,
                size,
            });
        }
        rest = &rest[start + size + 1..];
    }
    if !rest.is_empty() {
        return Err(format!("it answered more than the {} blob(s) it was asked", blobs.len()));
    }
    Ok(pointers)
}

/// The size `git cat-file --batch`'s `header` gives the blob `blob`; else why it is not that blob's header.
fn batch_size(header: &[u8], blob: &Blob) -> Result<usize, String> {
    let text = String::from_utf8_lossy(header);
    let words: Vec<&str> = text.split(' ').collect();
    match words[..] {
        [id, "blob", size] if id == blob.id => size.parse().map_err(|_| format!("it answered '{}'", escaped(&text))),
        _ => Err(format!("it answered '{}' for {}", escaped(&text), escaped(&blob.path))),
    }
}

/// The oid and size of the spec-v1 pointer `bytes` holds: exactly its version line, `oid sha256:<64 lower-case hex>`
/// and `size <decimal>`, each ending with a newline, in that order and nothing more; `None` for any other bytes.
pub(crate) fn parsed_pointer(bytes: &[u8]) -> Option<(String, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix(VERSION_LINE)?.strip_prefix("oid sha256:")?;
    let (oid, rest) = rest.split_once('\n')?;
    if !is_lower_hex(oid, SHA256_DIGITS) {
        return None;
    }
    let size = rest.strip_prefix("size ")?.strip_suffix('\n')?;
    if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((oid.to_owned(), size.parse().ok()?))
}

/// The objects `pointers` name, each once, in the order of the first pointer naming it; else the refusal of two
/// pointers naming one object at two sizes.
fn objects_of(pointers: &[Pointer]) -> Result<Vec<Object>, String> {
    let mut objects: Vec<Object> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for pointer in pointers {
        if let Some(&at) = seen.get(pointer.oid.as_str()) {
            let first = &objects[at];
            if first.size != pointer.size {
                return Err(format!(
                    "the LFS pointers {} and {} name the object {} at two sizes, {} and {} bytes",
                    escaped(&first.path),
                    escaped(&pointer.path),
                    pointer.oid,
                    first.size,
                    pointer.size
                ));
            }
            continue;
        }
        seen.insert(&pointer.oid, objects.len());
        objects.push(Object {
            oid: pointer.oid.clone(),
            size: pointer.size,
            path: pointer.path.clone(),
        });
    }
    Ok(objects)
}

/// The manifest line of `objects` the box's diff reads: `<oid> <size>` for each, separated by spaces, with its newline;
/// else the refusal of more than [`OBJECTS`] objects.
pub(crate) fn manifest_line(objects: &[Object]) -> Result<String, String> {
    if objects.len() > OBJECTS {
        return Err(format!(
            "the commit's LFS pointers name {} objects, more than one route carries ({OBJECTS})",
            objects.len()
        ));
    }
    let entries: Vec<String> = objects.iter().map(|object| format!("{} {}", object.oid, object.size)).collect();
    Ok(format!("{}\n", entries.join(" ")))
}

/// The run's `LfsDigest` for `objects`: the lower-case hex SHA-256 of their manifest line ([`manifest_line`]) without
/// its newline, as the inputs' digest is of theirs; else the refusal of more than [`OBJECTS`] objects, or why the line
/// could not be hashed.
pub(crate) fn manifest_digest(objects: &[Object]) -> Result<String, String> {
    let line = manifest_line(objects)?;
    sha256_of(line.trim_end_matches('\n').as_bytes())
        .map(|(digest, _)| digest)
        .map_err(|error| format!("the LFS manifest could not be hashed: {error}"))
}

/// The folder the repository `repo` keeps its LFS objects in: `objects` below `lfs.storage` when its git config sets
/// it, relative to its common git folder when relative, else `<common>\lfs\objects`; `<common>` as
/// [`snapshot::place_of`] finds it. Else the interrupt, or why git failed.
pub(super) fn objects_folder(repo: Bounded<'_>, bounds: GitBounds, interrupt: Option<Interrupt>) -> Result<PathBuf, Unbuilt> {
    let place = snapshot::place_of(repo, bounds, interrupt).map_err(unbuilt)?;
    let placed = Bounded { place: Some(&place), ..repo };
    let git = Git::find(repo.root).map_err(Unbuilt::Failed)?;
    let set = placed.variables();
    // As a path, so `~` expands to the user's folder as git-lfs expands it.
    let args = ["config", "--type=path", "--get", "lfs.storage"];
    let call = Call {
        set: &set,
        interrupt,
        ..Call::bounded(repo.root, &args, bounds)
    };
    let failed = |why: String| Unbuilt::Failed(format!("git config --get lfs.storage failed: {why}"));
    let (ran, errors) = git.run_kept(&call).map_err(unbuilt)?;
    let common = Path::new(&place.common);
    let storage = match ran.status.code() {
        Some(0) => {
            let mut said = String::new();
            ran.stdout
                .take(STORAGE_MOST)
                .read_to_string(&mut said)
                .map_err(|error| failed(error.to_string()))?;
            said.trim_end_matches(['\r', '\n']).to_owned()
        }
        // 1 says the key is not set.
        Some(1) => String::new(),
        code => return Err(failed(format!("status {code:?}: {}", String::from_utf8_lossy(&errors).trim()))),
    };
    if storage.is_empty() {
        return Ok(common.join("lfs").join("objects"));
    }
    Ok(common.join(storage).join("objects"))
}

/// The path of the object `oid` below the objects folder `folder`: `<folder>\<oid[0..2]>\<oid[2..4]>\<oid>`.
fn object_path(folder: &Path, oid: &str) -> PathBuf {
    folder.join(&oid[..2]).join(&oid[2..4]).join(oid)
}

/// The object `object` as the pack reads it from the objects folder `folder`, each member named by its oid; else why
/// not: missing, not a file, or not of its pointer's size, each naming its pointer's path and saying to fetch it.
fn found_object(folder: &Path, object: &Object) -> Result<Found, String> {
    let full = object_path(folder, &object.oid);
    let named = || format!("the LFS object {} of {} at {}", object.oid, escaped(&object.path), full.display());
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(format!("{} is missing; {FETCH}", named())),
        Err(error) => return Err(format!("{} cannot be read: {error}; {FETCH}", named())),
    };
    if !metadata.is_file() {
        return Err(format!("{} is not a file; {FETCH}", named()));
    }
    if metadata.len() != object.size {
        return Err(format!(
            "{} holds {} bytes, not the {} its pointer names; {FETCH}",
            named(),
            metadata.len(),
            object.size
        ));
    }
    Ok(Found {
        path: object.oid.clone(),
        full,
        stamp: stamp_of(&metadata),
    })
}

/// Packs the objects `needed`, read from the objects folder `folder`, into the open `archive` in the inputs archive's
/// format, each member named by its oid, `go_on` asked before the pack and before each write. Each object is hashed as
/// it is read and its size and SHA-256 checked against its pointer; one missing, of another size or hash, or that
/// changes while it is packed is refused, naming its pointer's path and saying to fetch it. The archive's compressed
/// size and, when `hashed`, the SHA-256 of its bytes, read back from the file once it is packed; else the interrupt, or
/// why not.
pub(super) fn packed(
    archive: &File,
    folder: &Path,
    needed: &[&Object],
    go_on: &dyn Fn() -> bool,
    hashed: bool,
) -> Result<(u64, Option<[u8; 32]>), Unbuilt> {
    let found = needed
        .iter()
        .map(|object| found_object(folder, object))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Unbuilt::Failed)?;
    if !go_on() {
        return Err(Unbuilt::Interrupted);
    }
    let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
    let stopped = Cell::new(false);
    let sink = Asking {
        inner: archive,
        go_on,
        stopped: &stopped,
    };
    let result = pack(sink, folder, &chosen, &HeldBy::new());
    if stopped.get() {
        return Err(Unbuilt::Interrupted);
    }
    let packed = result.map_err(|error| Unbuilt::Failed(format!("cannot pack the LFS objects: {error}; {FETCH}")))?;
    for (entry, object) in packed.entries.iter().zip(needed) {
        if entry.sha256 != object.oid || entry.size != object.size {
            return Err(Unbuilt::Failed(format!(
                "the LFS object {} of {} holds {} bytes with SHA-256 {}, not what its pointer names; {FETCH}",
                object.oid,
                escaped(&object.path),
                entry.size,
                entry.sha256
            )));
        }
    }
    let sha256 = if hashed {
        Some(archive_sha256(archive, packed.compressed, go_on)?)
    } else {
        None
    };
    Ok((packed.compressed, sha256))
}

/// The SHA-256 of the first `length` bytes of `archive`, `go_on` asked before each read, as the pack asks it before each
/// write; else the interrupt, or why not, a file shorter than `length` included. Each read names its own offset, counted
/// from the file's start, so where the handle's position stands does not matter.
fn archive_sha256(archive: &File, length: u64, go_on: &dyn Fn() -> bool) -> Result<[u8; 32], Unbuilt> {
    let cannot = |why: String| Unbuilt::Failed(format!("cannot read the LFS archive back: {why}"));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < length {
        if !go_on() {
            return Err(Unbuilt::Interrupted);
        }
        let want = usize::try_from(length - offset).unwrap_or(usize::MAX).min(buffer.len());
        let read = archive
            .seek_read(&mut buffer[..want], offset)
            .map_err(|error| cannot(error.to_string()))?;
        if read == 0 {
            return Err(cannot(format!("the archive ends after {offset} of its {length} bytes")));
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    Ok(hasher.finalize().into())
}

/// The objects of `objects` the diff's standard output `said` names: `need <n>`, then `n` oids, each an object of the
/// manifest and none named twice; else why the answer is not one.
pub(super) fn needed<'o>(said: &str, objects: &'o [Object]) -> Result<Vec<&'o Object>, String> {
    let mut lines = said.lines().filter(|line| !line.trim().is_empty());
    let count: usize = lines
        .next()
        .and_then(|line| line.strip_prefix(NEED))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| format!("the builder printed no '{NEED}<n>' line"))?;
    let by_oid: HashMap<&str, &Object> = objects.iter().map(|object| (object.oid.as_str(), object)).collect();
    let mut seen = std::collections::HashSet::new();
    let needed = lines
        .map(|line| {
            let oid = line.strip_prefix(OID).ok_or_else(|| format!("the builder printed '{}'", escaped(line)))?;
            let object = by_oid
                .get(oid)
                .copied()
                .ok_or_else(|| format!("the builder named '{}', which the manifest does not hold", escaped(oid)))?;
            if !seen.insert(oid) {
                return Err(format!("the builder named {} twice", escaped(oid)));
            }
            Ok(object)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if needed.len() != count {
        return Err(format!("the builder said it needs {count} object(s) and named {}", needed.len()));
    }
    Ok(needed)
}

/// The most bytes one put of LFS objects carries: 16 GiB, unless `--test-remote-lfs-budget-bytes` says otherwise.
pub(super) const LFS_BUDGET: u64 = 16 * 1024 * 1024 * 1024;
/// The box's put line.
const INSTALLED: &str = "gate-remote-lfs: installed ";
/// Why a run the box refused `lfs` falls back.
const NOT_HELD: &str = "the builder's LFS store does not hold the content the snapshot's pointers name";
/// The start of the box's line for a run it refused `lfs` because it could not make the run's LFS scratch folder, the
/// reason after it; the PC reads it to say why the run falls back ([`run_refused`]).
pub(crate) const NO_SCRATCH: &str = "gate: remote cannot make the run's LFS scratch folder: ";
/// Why a run the box refused `lfs` for its scratch folder falls back.
const SCRATCH_UNMADE: &str = "the builder could not make the run's LFS scratch folder";

/// A run's LFS content, listed before its preflight: the objects its snapshot's pointers name, their manifest line with
/// its newline, and the SHA-256 of that line without it, the run call's `LfsDigest`.
pub(super) struct Sending {
    objects: Vec<Object>,
    line: String,
    pub(super) digest: String,
}

/// The content the pointers `listed` name; else the refusal of more objects than one route carries, or why the
/// manifest could not be hashed.
pub(super) fn sending(listed: Listed) -> Result<Sending, String> {
    let line = manifest_line(&listed.objects)?;
    let digest = manifest_digest(&listed.objects)?;
    Ok(Sending {
        objects: listed.objects,
        line,
        digest,
    })
}

/// The log line saying a run's LFS content was not sent, for `why`.
pub(super) fn not_sent(why: &str) -> String {
    format!("gate: remote lfs not sent: {}", escaped(why))
}

/// Logs why the LFS content was not sent, kept for the log of the run here; the route's `lfs` refusal.
pub(super) fn refused(attempt: &mut Attempt<'_, '_>, why: &str) -> End {
    attempt.logged_kept(not_sent(why));
    End::Refused(FallbackReason::Lfs)
}

/// The end of a run call the box refused `lfs`, why said in the log: it could not make the run's LFS scratch folder when
/// its last error line `last_error` says so ([`NO_SCRATCH`]), else its store does not hold what the snapshot's pointers
/// name.
pub(super) fn run_refused(attempt: &mut Attempt<'_, '_>, last_error: Option<&str>) -> End {
    let why = if last_error.is_some_and(|line| line.starts_with(NO_SCRATCH)) {
        SCRATCH_UNMADE
    } else {
        NOT_HELD
    };
    refused(attempt, why)
}

/// Why a pair whose repository `repo` holds LFS pointers sends none.
pub(super) fn pair_why(repo: &str) -> String {
    format!("{repo} holds LFS pointers, and a sibling pair does not send LFS content yet")
}

/// Why the LFS pointers of the repository `repo` could not be listed, for `why`.
pub(super) fn unlisted(repo: &str, why: &str) -> String {
    format!("cannot list the LFS pointers of {repo}: {why}")
}

/// Sends `sending` to the store of the repo at `location` for the run of `nonce`: the diff call, then, when the box
/// lacks any object, the put of an archive of exactly those, timed in the run's split. `--test-remote-interrupt-at lfs`
/// raises the console interrupt first. A Ctrl+C ends the run; any other failure refuses the route `lfs`, its reason in
/// the log. An end no LFS call returns fails the run.
pub(super) fn send(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, sending: &Sending) -> Result<(), End> {
    if let Some(interrupt) = attempt.call.interrupt.filter(|_| attempt.raises_at("lfs")) {
        interrupt.raise();
    }
    attempt.go_on()?;
    let started = Instant::now();
    let ended = sent_to(attempt, location, nonce, sending);
    attempt.split.lfs = Some(started.elapsed());
    match ended {
        Ok(()) => Ok(()),
        Err(Unsent::Ended(End::Interrupted)) => Err(End::Interrupted),
        Err(Unsent::Ended(End::Failed(why)) | Unsent::Refused(why)) => Err(refused(attempt, &why)),
        // The box refused for its install, its line said: the run falls back for that, not for its LFS content.
        Err(Unsent::Ended(End::Refused(FallbackReason::Install))) => Err(End::Refused(FallbackReason::Install)),
        // The builder's drain refused a session no longer live: the run waits behind it, then starts again.
        Err(Unsent::Ended(End::Draining(drain))) => Err(End::Draining(drain)),
        Err(Unsent::Ended(End::Refused(reason))) => Err(refused(attempt, &format!("the LFS call ended {}", reason.as_str()))),
        Err(Unsent::Ended(other)) => Err(End::Failed(format!("the LFS call ended {}, which no LFS call does", unexpected(&other)))),
    }
}

/// The diff, then the put of what it needs ([`send`]), the worktree's `.tmp` held from before the diff's answer is made
/// until the put has ended.
fn sent_to(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, sending: &Sending) -> Result<(), Unsent> {
    let root = Path::new(attempt.call.root);
    let folder = root.join(TMP);
    let mut held = Archive::new(root, folder.clone());
    fs::create_dir_all(&folder)
        .and_then(|()| held.hold(&folder))
        .map_err(|error| Unsent::Refused(format!("cannot hold {}: {error}", folder.display())))?;
    let needed = diffed(attempt, location, nonce, sending)?;
    if needed.is_empty() {
        attempt.logged(&present_line(sending.objects.len()));
        return Ok(());
    }
    let bytes = needed.iter().fold(0_u64, |total, object| total.saturating_add(object.size));
    if bytes > attempt.lfs_budget {
        return Err(Unsent::Refused(format!(
            "the LFS objects the builder needs hold {bytes} bytes, more than one put carries ({} bytes)",
            attempt.lfs_budget
        )));
    }
    let started = Instant::now();
    let alone = Bounded {
        root,
        ceiling: root.parent().unwrap_or(root),
        place: None,
    };
    let objects = objects_folder(alone, attempt.git, attempt.call.interrupt).map_err(unsent)?;
    let path = folder.join(format!("{}-{nonce}.lfs.tar.zst", attempt.call.key));
    let archive = tmp_file(root, &path, "the LFS archive").map_err(Unsent::Refused)?;
    // The archive's SHA-256 is read back only for a put that asks for the builder's data channel, which declares it.
    let hashed = matches!(channel::planned(attempt.call.builder), Ok(Some(_)));
    let (compressed, sha256) = packed(&archive, &objects, &needed, &|| attempt.go_on().is_ok(), hashed).map_err(unsent)?;
    put(attempt, location, sending, (&archive, (compressed, sha256.as_ref()), needed.len()))?;
    drop(archive);
    drop(held);
    attempt.say(sent_line(needed.len(), bytes, started.elapsed()));
    Ok(())
}

/// A step's failure as an LFS step's: the interrupt, or why it failed.
fn unsent(unbuilt: Unbuilt) -> Unsent {
    match unbuilt {
        Unbuilt::Interrupted => Unsent::Ended(End::Interrupted),
        Unbuilt::Failed(why) => Unsent::Refused(why),
    }
}

/// The log line of a diff that found the store holding all `count` objects.
fn present_line(count: usize) -> String {
    format!("gate: lfs: {count} object(s) present")
}

/// The console line of a put that sent `count` objects of `bytes` bytes in `took`.
#[expect(clippy::cast_precision_loss, reason = "a size said to one decimal of a megabyte")]
fn sent_line(count: usize, bytes: u64, took: Duration) -> String {
    format!("gate: lfs: {count} object(s), {:.1} MB in {:.1} s", bytes as f64 / MB, took.as_secs_f64())
}

/// The objects of `sending` the box's diff call says the store lacks or holds at another size; else why not. Its
/// answer is written raw into a file of the held `.tmp` and read back through the same handle; the call is watched by
/// its own output and a transfer ceiling counted from its own start, as a fetch is.
fn diffed<'s>(attempt: &mut Attempt<'_, '_>, location: &Location, nonce: &str, sending: &'s Sending) -> Result<Vec<&'s Object>, Unsent> {
    // Each call's word carries a nonce new to it, which the box's refusal tags for the call name; the PC's own files
    // keep the run's.
    let call_nonce = payload::run_nonce();
    let head = payload::input(&payload::inputs_word(location, &call_nonce, InputsStep::Diff, None), Some(&sending.line));
    let root = Path::new(attempt.call.root);
    let path = root.join(TMP).join(format!("{}-{nonce}.lfs-diff", attempt.call.key));
    let answer = tmp_file(root, &path, "the LFS diff answer").map_err(Unsent::Refused)?;
    let cap = NonZeroU64::new(2 * (sending.line.len() as u64) + 1024 * 1024).unwrap_or(NonZeroU64::MIN);
    let shown = path.display().to_string();
    // Each attempt writes the answer anew from its start, through a clone of the one handle, as the inputs diff does.
    let make = || -> Result<RawSink, String> {
        let cannot = |error: io::Error| format!("cannot write {shown}: {error}");
        let mut file = answer.try_clone().map_err(cannot)?;
        file.set_len(0).map_err(cannot)?;
        file.seek(SeekFrom::Start(0)).map_err(cannot)?;
        Ok(RawSink {
            file,
            shown: shown.clone(),
            cap,
        })
    };
    let watch = Watch::Fetch(attempt.limits_for("lfs-diff"), None);
    // A diff asks for no data channel and keeps the head and nonce it made: the maker is the same every attempt.
    let maker = |_: Option<&channel::Asked>| (call_nonce.clone(), Arc::clone(&head));
    let called = attempt.lfs_call(&maker, None, Some(&make), watch).map_err(Unsent::Ended)?;
    judged(attempt, &called, Step::LfsDiff)?;
    let mut text = Vec::new();
    (&answer)
        .seek(SeekFrom::Start(0))
        .and_then(|_| (&answer).take(cap.get()).read_to_end(&mut text))
        .map_err(|error| Unsent::Refused(format!("cannot read {shown}: {error}")))?;
    needed(&String::from_utf8_lossy(&text), &sending.objects).map_err(Unsent::Refused)
}

/// The put call of the `sent` objects packed into the open archive, its compressed length and SHA-256, and their count:
/// the request, the manifest and then the archive, over ssh or the builder's data channel, watched as a put; else why
/// not, a builder that installed another count than was sent included.
fn put(attempt: &mut Attempt<'_, '_>, location: &Location, sending: &Sending, sent: (&File, (u64, Option<&[u8; 32]>), usize)) -> Result<(), Unsent> {
    let (archive, (length, sha256), count) = sent;
    // Each attempt's word carries a nonce new to it, which the box's refusal tags for the call name and the box's
    // stage is named by, and the data channel the attempt asks for, with proofs of its own.
    let head = |asked: Option<&channel::Asked>| {
        let nonce = payload::run_nonce();
        let channel = asked.zip(sha256).map(|(asked, sha256)| (asked, length, sha256));
        let word = payload::put_word(location, &nonce, None, channel);
        (nonce, payload::input(&word, Some(&sending.line)))
    };
    let watch = Watch::Put(attempt.limits_for("lfs"));
    let archive = Sent {
        file: archive,
        length,
        sha256,
    };
    let called = attempt.lfs_call(&head, Some(archive), None, watch).map_err(Unsent::Ended)?;
    judged(attempt, &called, Step::LfsPut)?;
    installed_all(&called.lines, count).map_err(Unsent::Refused)
}

/// Whether the put's `lines` say the builder installed each of the `sent` objects, by its `installed <n>` line; else
/// why not.
fn installed_all(lines: &[String], sent: usize) -> Result<(), String> {
    let said = lines
        .iter()
        .find_map(|line| line.strip_prefix(INSTALLED))
        .ok_or_else(|| format!("the builder printed no '{INSTALLED}<n>' line"))?;
    match said.trim().parse::<usize>() {
        Ok(count) if count == sent => Ok(()),
        Ok(count) => Err(format!("the builder installed {count} of the {sent} LFS objects sent")),
        Err(_) => Err(format!("the builder printed '{}'", escaped(&format!("{INSTALLED}{said}")))),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its fixture must fail loudly")]
mod tests {
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use super::{
        ATTRIBUTES_MOST, Blob, Listed, Object, as_the_builder, installed_all, listed, manifest_line, names_lfs_filter_among, not_sent, object_path,
        objects_folder, packed, pair_why, pointers_of, present_line, sending, sent_line, small_blobs,
    };

    /// Every listing call on the PC reads a path's attributes as the builder does: no `core.attributesFile` and no
    /// system-wide attributes file.
    #[test]
    fn a_listing_call_reads_the_tree_s_own_attributes_alone() {
        let mut set = Vec::new();

        let args = as_the_builder(&mut set, &["check-attr", "filter"]);

        assert_eq!(args, ["-c", "core.attributesFile=", "check-attr", "filter"]);
        assert_eq!(set, [("GIT_ATTR_NOSYSTEM", std::ffi::OsStr::new("1"))]);
    }
    use crate::outputs::hash::sha256_of;
    use crate::remote::git::GIT_BOUNDS;
    use crate::remote::outputs::Unbuilt;
    use crate::remote::snapshot::Bounded;
    use gate_router::escaped;

    /// A test's scratch folder, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-remote-lfs-{}-{name}", std::process::id()));
        // A folder left by an earlier run of the test may not be there.
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the scratch folder can be made");
        folder
    }

    #[expect(clippy::disallowed_methods, reason = "a test's git setup takes no spawn lock")]
    fn git(folder: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost", "-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .expect("git can be run");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A git repository at `folder`, git-lfs installed in it alone.
    fn repo(folder: &Path) {
        fs::create_dir_all(folder).expect("the repository's folder can be made");
        git(folder, &["init", "--quiet"]);
        git(folder, &["lfs", "install", "--local"]);
    }

    /// Writes `bytes` at `path` below `root`.
    fn put(root: &Path, path: &str, bytes: &[u8]) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("a folder")).expect("the folder can be made");
        fs::write(full, bytes).expect("the file can be written");
    }

    /// Adds everything below `root` and commits it.
    fn commit(root: &Path) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "-m", "c"]);
    }

    /// A repository in the scratch folder `name` whose `*.bin` files are tracked by git-lfs; the scratch folder and the
    /// repository's root.
    fn tracked(name: &str) -> (PathBuf, PathBuf) {
        let folder = scratch(name);
        let root = folder.join("repo");
        repo(&root);
        git(&root, &["lfs", "track", "*.bin"]);
        (folder, root)
    }

    /// The repository at `root`, its search stopping at `ceiling`.
    fn bounded<'a>(root: &'a Path, ceiling: &'a Path) -> Bounded<'a> {
        Bounded { root, ceiling, place: None }
    }

    fn list(root: &Path, ceiling: &Path) -> Listed {
        listed(bounded(root, ceiling), "HEAD", GIT_BOUNDS, None).expect("the pointers are listed")
    }

    fn sha(bytes: &[u8]) -> String {
        sha256_of(bytes).expect("the bytes can be hashed").0
    }

    /// The spec-v1 pointer text of `bytes`.
    fn pointer_text(bytes: &[u8]) -> String {
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            sha(bytes),
            bytes.len()
        )
    }

    /// Packs every object of `listed`, read from the objects folder of `root`, into a new archive in `folder`.
    fn pack_all(folder: &Path, root: &Path, listed: &Listed) -> Result<u64, Unbuilt> {
        let objects = objects_folder(bounded(root, folder), GIT_BOUNDS, None).expect("the objects folder is found");
        let path = folder.join("objects.tar.zst");
        // A previous pack of the same test may have left it.
        let _ = fs::remove_file(&path);
        let archive = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("the archive can be made");
        let needed: Vec<&Object> = listed.objects.iter().collect();
        packed(&archive, &objects, &needed, &|| true, false).map(|(length, _)| length)
    }

    /// Why `result` failed, which it must have.
    fn failure(result: Result<u64, Unbuilt>) -> String {
        match result {
            Err(Unbuilt::Failed(why)) => why,
            other => format!("not a failure: {other:?}"),
        }
    }

    #[test]
    fn the_lfs_lines_are_worded_as_the_console_and_the_log_say_them() {
        assert_eq!(
            sent_line(2, 3 * 1024 * 1024 / 2, Duration::from_millis(2_340)),
            "gate: lfs: 2 object(s), 1.5 MB in 2.3 s"
        );
        assert_eq!(present_line(3), "gate: lfs: 3 object(s) present");
        assert_eq!(
            not_sent(&pair_why("app-sibling")),
            "gate: remote lfs not sent: app-sibling holds LFS pointers, and a sibling pair does not send LFS content yet"
        );
        assert_eq!(not_sent("a\nb"), format!("gate: remote lfs not sent: {}", escaped("a\nb")));
    }

    /// The git calls a listing of the tree `listing` makes, by their first word, every other call answering nothing.
    fn calls_made(listing: &str) -> Vec<String> {
        let mut called: Vec<String> = Vec::new();
        {
            let mut output = |args: &[&str], _: Option<&[u8]>, _: usize| -> Result<Vec<u8>, String> {
                called.push(args[0].to_owned());
                Ok(if args[0] == "ls-tree" { listing.as_bytes().to_vec() } else { Vec::new() })
            };
            // The empty check-attr answer cannot be read; only the calls made matter here.
            let _ = pointers_of("HEAD", &mut output, |why| why);
        }
        called
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused_only_in_a_tree_with_an_attributes_file() {
        let id = "a".repeat(40);
        let listed = |listing: Vec<u8>| {
            let mut output = |args: &[&str], _: Option<&[u8]>, _: usize| -> Result<Vec<u8>, String> {
                Ok(if args[0] == "ls-tree" { listing.clone() } else { Vec::new() })
            };
            pointers_of("HEAD", &mut output, |why| why)
        };
        let mut without = format!("100644 blob {id}       3\t").into_bytes();
        without.extend_from_slice(b"caf\xe9.txt\x00");
        let mut with = without.clone();
        with.extend_from_slice(format!("100644 blob {id}      30\t.gitattributes\x00").as_bytes());

        let plain = listed(without).expect("a tree with no attributes file lists nothing");
        let refused = listed(with).expect_err("a tree with an attributes file cannot be listed");

        assert!(plain.pointers.is_empty() && plain.objects.is_empty(), "{plain:?}");
        assert!(refused.contains("not UTF-8"), "{refused}");
    }

    #[test]
    fn a_tree_with_small_blobs_and_no_attributes_file_asks_no_check_attr() {
        let id = "a".repeat(40);
        let without = format!("100644 blob {id}       3\ta.txt\x00160000 commit {id}       -\tsub\x00");
        let nested = format!("{without}100644 blob {id}    2000\tdeep/.gitattributes\x00");

        assert_eq!(calls_made(&without), ["ls-tree"]);
        assert_eq!(
            calls_made(&nested),
            ["ls-tree", "check-attr"],
            "a large attributes file at any depth counts"
        );
        assert!(super::is_attributes(".gitattributes") && !super::is_attributes("x.gitattributes"));
    }

    #[test]
    fn a_put_that_installs_another_count_than_was_sent_failed() {
        let lines = |said: &[&str]| said.iter().map(|line| (*line).to_owned()).collect::<Vec<_>>();

        assert_eq!(installed_all(&lines(&["gate-remote-lfs: installed 2"]), 2), Ok(()));
        assert_eq!(
            installed_all(&lines(&["gate-remote-lfs: installed 1"]), 2),
            Err("the builder installed 1 of the 2 LFS objects sent".to_owned())
        );
        assert_eq!(
            installed_all(&lines(&["something else"]), 2),
            Err("the builder printed no 'gate-remote-lfs: installed <n>' line".to_owned())
        );
    }

    #[test]
    fn the_sending_digest_is_the_manifest_line_s_without_its_newline() {
        let object = Object {
            oid: "a".repeat(64),
            size: 3,
            path: "a.bin".to_owned(),
        };
        let listed = Listed {
            pointers: Vec::new(),
            objects: vec![object],
        };

        let sending = sending(listed).expect("one object is sent");

        assert_eq!(sending.line, format!("{} 3\n", "a".repeat(64)));
        assert_eq!(sending.digest, sha(format!("{} 3", "a".repeat(64)).as_bytes()));
    }

    #[test]
    fn a_tracked_lfs_file_is_listed_as_its_pointer() {
        let (folder, root) = tracked("tracked");
        put(&root, "assets/a.bin", b"hello");
        commit(&root);

        let listed = list(&root, &folder);

        assert_eq!(listed.pointers.len(), 1, "{listed:?}");
        let pointer = &listed.pointers[0];
        assert_eq!(
            (pointer.path.as_str(), pointer.oid.as_str(), pointer.size),
            ("assets/a.bin", sha(b"hello").as_str(), 5)
        );
        assert!(pack_all(&folder, &root, &listed).is_ok_and(|compressed| compressed > 0));
    }

    #[test]
    fn a_raw_file_committed_under_filter_lfs_is_not_a_pointer() {
        let folder = scratch("raw");
        let root = folder.join("repo");
        repo(&root);
        put(&root, "raw.bin", b"raw bytes, never cleaned");
        commit(&root);
        git(&root, &["lfs", "track", "*.bin"]);
        git(&root, &["add", ".gitattributes"]);
        git(&root, &["commit", "--quiet", "-m", "track"]);

        let listed = list(&root, &folder);

        assert!(listed.pointers.is_empty(), "{listed:?}");
    }

    #[test]
    fn a_pointer_shaped_blob_without_filter_lfs_is_not_listed() {
        let (folder, root) = tracked("untracked-pointer");
        put(&root, "p.txt", pointer_text(b"x").as_bytes());
        commit(&root);

        let listed = list(&root, &folder);

        assert!(listed.pointers.is_empty(), "{listed:?}");
    }

    #[test]
    fn a_blob_over_1024_bytes_is_not_read_as_a_pointer() {
        let id = "a".repeat(40);
        let listing = format!("100644 blob {id}    1024\tat.bin\x00100644 blob {id}    1025\tover.bin\x00");

        let blobs = small_blobs(listing.as_bytes()).expect("the listing is read");

        assert_eq!(
            blobs,
            [Blob {
                path: "at.bin".to_owned(),
                id
            }]
        );
    }

    #[test]
    fn a_commit_with_no_lfs_attribute_lists_nothing() {
        let folder = scratch("no-attribute");
        let root = folder.join("repo");
        repo(&root);
        put(&root, "a.bin", pointer_text(b"y").as_bytes());
        commit(&root);

        let listed = list(&root, &folder);

        assert!(listed.pointers.is_empty() && listed.objects.is_empty(), "{listed:?}");
    }

    #[test]
    fn two_paths_naming_one_object_put_it_in_the_manifest_once() {
        let (folder, root) = tracked("one-object");
        put(&root, "a.bin", b"same");
        put(&root, "b.bin", b"same");
        commit(&root);

        let listed = list(&root, &folder);

        assert_eq!(listed.pointers.len(), 2, "{listed:?}");
        assert_eq!(manifest_line(&listed.objects), Ok(format!("{} 4\n", sha(b"same"))));
    }

    #[test]
    fn an_object_missing_from_the_store_refuses_with_lfs_and_names_git_lfs_fetch() {
        let (folder, root) = tracked("missing");
        put(&root, "a.bin", b"gone");
        commit(&root);
        let listed = list(&root, &folder);
        let objects = objects_folder(bounded(&root, &folder), GIT_BOUNDS, None).expect("the objects folder is found");
        fs::remove_file(object_path(&objects, &sha(b"gone"))).expect("the object can be removed");

        let why = failure(pack_all(&folder, &root, &listed));

        assert!(
            why.contains("a.bin") && why.contains("is missing") && why.contains("git lfs fetch"),
            "{why}"
        );
    }

    #[test]
    fn an_object_whose_bytes_differ_from_its_pointer_refuses() {
        let (folder, root) = tracked("differs");
        put(&root, "a.bin", b"true");
        commit(&root);
        let listed = list(&root, &folder);
        let objects = objects_folder(bounded(&root, &folder), GIT_BOUNDS, None).expect("the objects folder is found");
        fs::write(object_path(&objects, &sha(b"true")), b"lies").expect("the object can be rewritten at its size");

        let why = failure(pack_all(&folder, &root, &listed));

        assert!(
            why.contains("a.bin") && why.contains(&sha(b"lies")) && why.contains("git lfs fetch"),
            "{why}"
        );
    }

    #[test]
    fn lfs_storage_set_in_the_repo_is_where_objects_are_read() {
        let folder = scratch("storage");
        let root = folder.join("repo");
        repo(&root);
        git(&root, &["config", "lfs.storage", "lfs-store"]);
        git(&root, &["lfs", "track", "*.bin"]);
        put(&root, "a.bin", b"stored");
        commit(&root);
        let listed = list(&root, &folder);

        let objects = objects_folder(bounded(&root, &folder), GIT_BOUNDS, None).expect("the objects folder is found");

        let expected = root.join(r".git\lfs-store\objects");
        assert_eq!(
            fs::canonicalize(&objects).expect("the folder is there"),
            fs::canonicalize(&expected).expect("git-lfs wrote there")
        );
        assert!(object_path(&expected, &sha(b"stored")).is_file(), "git-lfs stored the object there");
        assert!(
            !root.join(r".git\lfs\objects").join(&sha(b"stored")[..2]).exists(),
            "not in the default store"
        );
        assert!(pack_all(&folder, &root, &listed).is_ok());
    }

    #[test]
    fn lfs_storage_starting_with_a_tilde_reads_as_the_user_s_folder() {
        let folder = scratch("storage-home");
        let root = folder.join("repo");
        repo(&root);
        git(&root, &["config", "lfs.storage", "~/gate-lfs-store"]);

        let objects = objects_folder(bounded(&root, &folder), GIT_BOUNDS, None).expect("the objects folder is found");

        assert!(objects.is_absolute(), "{}", objects.display());
        assert!(!objects.to_string_lossy().contains('~'), "{}", objects.display());
        assert!(objects.ends_with(r"gate-lfs-store\objects"), "{}", objects.display());
        assert!(!objects.starts_with(&root), "not below the repository: {}", objects.display());
    }

    #[test]
    fn a_worktree_reads_objects_from_its_common_dir() {
        let (folder, root) = tracked("worktree");
        put(&root, "a.bin", b"shared");
        commit(&root);
        let other = folder.join("other");
        git(
            &root,
            &["worktree", "add", "--quiet", "-b", "other", other.to_str().expect("a UTF-8 path")],
        );
        let listed = list(&other, &folder);

        let objects = objects_folder(bounded(&other, &folder), GIT_BOUNDS, None).expect("the objects folder is found");

        assert_eq!(
            fs::canonicalize(&objects).expect("the folder is there"),
            fs::canonicalize(root.join(r".git\lfs\objects")).expect("the main store is there")
        );
        assert_eq!(listed.objects.len(), 1, "{listed:?}");
        assert!(pack_all(&folder, &other, &listed).is_ok());
    }

    /// An attributes file longer than the PC reads names the LFS filter, as the builder's listing reads it whole and may
    /// find the word past the cap: the PC asks for git-lfs rather than have a builder without it fail the checkout. A
    /// file of exactly the cap is read whole.
    #[test]
    fn an_attributes_file_past_the_read_cap_names_the_lfs_filter() {
        let folder = scratch("attributes-cap");
        let cap = usize::try_from(ATTRIBUTES_MOST).expect("the cap fits in memory");
        let padding = "# padding\n".repeat(cap / 10 + 1);
        fs::write(folder.join("past.gitattributes"), format!("{padding}*.bin filter=lfs -text\n")).expect("the file can be written");
        fs::write(folder.join("at.gitattributes"), format!("{}\n", "#".repeat(cap - 1))).expect("the file can be written");

        assert!(
            names_lfs_filter_among(&folder, &["past.gitattributes".to_owned()]),
            "a filter=lfs line past the cap"
        );
        assert!(
            !names_lfs_filter_among(&folder, &["at.gitattributes".to_owned()]),
            "a file of the cap naming no filter"
        );
    }
}

//! What the PC and each box must judge alike about a routed run's Git LFS content: the pointers a commit's tree holds,
//! read through git calls the caller makes, the manifest of the objects they name and its digest, a box's reading of
//! that manifest and its check of the archive of objects it is sent, whether a checked-out file is still its pointer,
//! and the `-c` pins of every git call of a run that may run the LFS filter.
//!
//! A pointer is a blob of at most 1024 bytes whose path's `filter` attribute is `lfs` and whose bytes are exactly a
//! spec-v1 pointer: its version line, its `oid sha256:` line and its `size` line, in that order and nothing else. Any
//! other blob, a raw file committed under `filter=lfs` included, is left alone.

use std::collections::{HashMap, HashSet};
use std::io;

use gate_router::escaped;
use gate_wire::{SHA256_DIGITS, is_lower_hex};

use crate::OUTPUT_CAP;
use crate::archive::Entry;
use crate::hash::sha256_of;

/// The attributes `git check-attr` is asked of each listed path, in the order it answers them.
pub const ATTRIBUTES: [&str; 6] = ["filter", "ident", "working-tree-encoding", "text", "eol", "crlf"];
/// The bytes `git check-attr -z` is allowed per attribute of a path beside the path itself: two NULs, the attribute's
/// name and its value, with room to spare, so that each slice of paths it is fed answers well under the runner's cap.
/// An answer that is larger even so fails closed: the runner's over-cap failure ends the run, the back commit kept.
pub const ATTRIBUTE_ROOM: usize = 256;
/// The bytes the answer of each slice of paths fed to `git check-attr` is planned within: the runner's cap.
#[cfg(not(feature = "test-seams"))]
pub const SLICE_CAP: usize = OUTPUT_CAP;
/// The bytes the answer of each slice of paths fed to `git check-attr` is planned within, in a `test-seams` build: room
/// for two paths of up to 32 bytes, so that a suite case's few files span slices.
#[cfg(feature = "test-seams")]
pub const SLICE_CAP: usize = 2 * ATTRIBUTES.len() * (32 + ATTRIBUTE_ROOM);
/// The most bytes a blob read as a pointer may hold.
pub const POINTER_MOST: u64 = 1024;
/// The first line of every spec-v1 pointer.
const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1\n";
/// The most objects one manifest names.
pub const OBJECTS: usize = 50_000;
/// The most bytes one put of LFS objects carries or unpacks: 16 GiB, unless a test seam says otherwise (the PC's
/// `--test-remote-lfs-budget-bytes`, a box's `--test-lfs-budget-bytes`).
pub const LFS_BUDGET: u64 = 16 * 1024 * 1024 * 1024;
/// The room `git cat-file --batch` is allowed per object beside its bytes: its header line and the newline after it.
const HEADER_ROOM: usize = 128;

/// The `-c` pins every git call of a run that may run the LFS filter takes, whether or not the run sent LFS content: no
/// LFS server, a URL on port 0 of the loopback address in place of any a tracked `.lfsconfig` names, which needs no
/// name lookup, goes through no proxy and nothing can listen on, no other remote searched, no path left out of the
/// smudge, no download error skipped, no standalone transfer agent, and git-lfs's own filter driver, required, whatever
/// the work folder's or the builder's config says of it.
pub const CHECKOUT_PINS: [&str; 20] = [
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

/// One pointer of a commit's tree: its `/`-separated path, and the oid and size of the object it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    /// The pointer's `/`-separated path in the tree.
    pub path: String,
    /// The oid of the object it names, 64 lower-case hex digits.
    pub oid: String,
    /// The size in bytes of the object it names.
    pub size: u64,
}

/// One object a commit's pointers name: its oid, its size, and the path of the first pointer naming it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// The object's oid, 64 lower-case hex digits.
    pub oid: String,
    /// The object's size in bytes.
    pub size: u64,
    /// The `/`-separated path of the first pointer naming it.
    pub path: String,
}

/// A commit's pointers, in the tree's order, and the objects they name, each once, in the order of their first pointer.
#[derive(Debug, Default)]
pub struct Listed {
    /// The pointers, in the tree's order.
    pub pointers: Vec<Pointer>,
    /// The objects they name, each once, in the order of their first pointer.
    pub objects: Vec<Object>,
}

/// A tree's blobs the pointer listing reads: each regular file of at most [`POINTER_MOST`] bytes
/// ([`is_small_blob`]), in the tree's order, and whether the tree holds a `.gitattributes` file at any depth
/// ([`is_attributes`]).
#[derive(Debug, Default)]
pub struct Small {
    /// The small blobs, in the tree's order.
    pub blobs: Vec<Blob>,
    /// Whether the tree holds a `.gitattributes` file at any depth.
    pub attributes: bool,
}

impl Small {
    /// Whether git is asked about the blobs at all: there are some, and the tree holds an attributes file, without
    /// which no path of it has the `lfs` filter as the box reads the snapshot's attributes.
    #[must_use]
    pub fn worth_asking(&self) -> bool {
        !self.blobs.is_empty() && self.attributes
    }
}

/// Whether a tree's entry of `mode` and `kind`, `size` bytes, may be a pointer: a regular file, never a link or a
/// submodule, of at most [`POINTER_MOST`] bytes.
#[must_use]
pub fn is_small_blob(mode: &str, kind: &str, size: u64) -> bool {
    kind == "blob" && mode != "120000" && size <= POINTER_MOST
}

/// Whether the `/`-separated `path` names a `.gitattributes` file.
#[must_use]
pub fn is_attributes(path: &str) -> bool {
    path.rsplit('/').next() == Some(".gitattributes")
}

/// A git call of the listing: what git wrote on standard output for its arguments, fed its standard input when it has
/// one, read up to its cap in bytes; else the caller's failure.
pub type GitOutput<'a, E> = dyn FnMut(&[&str], Option<&[u8]>, usize) -> Result<Vec<u8>, E> + 'a;

/// The pointers of `commit`'s tree and the objects they name, each git call made through `output`: its blobs of at most
/// 1024 bytes (`git ls-tree`), then those [`pointers_among`] reads. A tree with no `.gitattributes` holds no pointer,
/// so nothing more of its listing is read, a path that is not UTF-8 included. The box's checkout reads a tree this way,
/// and the PC's listing of a sibling's tree.
pub fn pointers_of<E>(commit: &str, output: &mut GitOutput<'_, E>, failed: fn(String) -> E) -> Result<Listed, E> {
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
pub fn pointers_among<E>(commit: &str, small: &Small, output: &mut GitOutput<'_, E>, failed: fn(String) -> E) -> Result<Listed, E> {
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
pub struct Blob {
    /// The blob's `/`-separated path in the tree.
    pub path: String,
    /// The blob's object id.
    pub id: String,
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

/// The pointers of `blobs` in `git cat-file --batch`'s `answer`: for each blob, in order, its header `<id> blob
/// <size>`, its bytes and a newline, kept when the bytes parse as a pointer ([`parsed_pointer`]); else why the answer
/// is not one.
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
#[must_use]
fn parsed_pointer(bytes: &[u8]) -> Option<(String, u64)> {
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
pub fn manifest_line(objects: &[Object]) -> Result<String, String> {
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
pub fn manifest_digest(objects: &[Object]) -> Result<String, String> {
    let line = manifest_line(objects)?;
    sha256_of(line.trim_end_matches('\n').as_bytes())
        .map(|(digest, _)| digest)
        .map_err(|error| format!("the LFS manifest could not be hashed: {error}"))
}

/// `paths` cut into slices, in order, each holding as many paths as `git check-attr` asked `attributes` attributes of
/// each path can answer within `cap` bytes, [`ATTRIBUTE_ROOM`] allowed for each attribute beside the path; a path that
/// alone needs more is a slice of its own.
#[must_use]
pub fn slices_asking<'a>(paths: &'a [&'a str], attributes: usize, cap: usize) -> Vec<&'a [&'a str]> {
    let mut slices = Vec::new();
    let mut rest = paths;
    while !rest.is_empty() {
        let mut used = 0_usize;
        let count = rest
            .iter()
            .take_while(|path| {
                used = used.saturating_add(attributes * (path.len() + ATTRIBUTE_ROOM));
                used <= cap
            })
            .count()
            .max(1);
        let (slice, after) = rest.split_at(count);
        slices.push(slice);
        rest = after;
    }
    slices
}

/// The values of the attributes `names` of each of `paths` from `git check-attr -z`'s `answer`: a `path`, attribute,
/// value triple for each of `names` of each path, in their order; else why the answer cannot be read.
pub fn attribute_values<'a, const N: usize>(answer: &'a str, paths: &[&str], names: [&str; N]) -> Result<Vec<[&'a str; N]>, String> {
    let mut fields = answer.split('\0');
    let mut read = Vec::with_capacity(paths.len());
    for path in paths {
        let mut values = [""; N];
        for (value, name) in values.iter_mut().zip(names) {
            match (fields.next(), fields.next(), fields.next()) {
                (Some(said), Some(attribute), Some(given)) if said == *path && attribute == name => *value = given,
                _ => return Err(format!("it did not answer {name} for {path}")),
            }
        }
        read.push(values);
    }
    if fields.any(|field| !field.is_empty()) {
        return Err(format!("it answered more than the {} path(s) it was asked", paths.len()));
    }
    Ok(read)
}

/// One object of the PC's manifest: its oid and its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    /// The object's oid, 64 lower-case hex digits.
    pub oid: String,
    /// The object's size in bytes.
    pub size: u64,
}

/// The objects the manifest line `text`, without its newline, names: space-separated `<oid> <size>` entries, each oid
/// 64 lower-case hex digits and each size a decimal, no oid twice and no more than [`OBJECTS`]; an empty line names
/// none. Else why it is not one.
pub fn parsed_manifest(text: &str) -> Result<Vec<Wanted>, String> {
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

/// Whether each member of `entries` is an object of the manifest `wanted`, at most once, whose SHA-256 is its name and
/// whose size is the manifest's; else the refusal naming the first that is not. A member the store already holds,
/// another put having installed it since the diff, is checked as any other and kept at its install.
pub fn checked(entries: &[Entry], wanted: &[Wanted]) -> Result<(), String> {
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

/// Whether the checked-out file at `pointer`'s path is wrong: `size` is that of the plain file there, `None` when it is
/// missing, a link or not a plain file, and the file is wrong when it is not of the pointer's size, or is of at most
/// [`POINTER_MOST`] bytes and holds that very pointer, its oid and size (an object of 128 bytes has a pointer of its
/// own size). `head` reads at most [`POINTER_MOST`] + 1 bytes of the file, called only for such a small file; a read
/// that fails is wrong. An object whose content is pointer text naming another object is its content.
pub fn is_wrong_file(size: Option<u64>, pointer: &Pointer, head: impl FnOnce() -> io::Result<Vec<u8>>) -> bool {
    if size != Some(pointer.size) {
        return true;
    }
    if pointer.size > POINTER_MOST {
        return false;
    }
    head().map_or(true, |bytes| {
        parsed_pointer(&bytes).is_some_and(|(oid, size)| oid == pointer.oid && size == pointer.size)
    })
}

/// `lfs.storage=<lock><separator>lfs`, below the run's held lock file `lock`, a folder git-lfs cannot make: the
/// `lfs.storage` of a run with no LFS scratch folder, so a smudge or a clean there fails rather than reading or writing
/// the work folder's own LFS storage.
#[must_use]
pub fn no_storage(lock: &str) -> String {
    format!("lfs.storage={lock}{}lfs", std::path::MAIN_SEPARATOR)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its fixture must fail loudly")]
mod tests {
    #[cfg(not(feature = "test-seams"))]
    use super::SLICE_CAP;
    use super::{Blob, no_storage, pointers_of, small_blobs};
    #[cfg(not(feature = "test-seams"))]
    use crate::OUTPUT_CAP;

    #[cfg(not(feature = "test-seams"))]
    #[test]
    fn a_build_without_test_seams_slices_at_the_output_cap() {
        assert_eq!(SLICE_CAP, OUTPUT_CAP);
    }

    /// A run with no LFS scratch folder names a storage below its lock file, which git-lfs cannot make.
    #[test]
    fn a_run_without_a_scratch_folder_stores_below_its_lock_file() {
        assert_eq!(no_storage("lock-path"), format!("lfs.storage=lock-path{}lfs", std::path::MAIN_SEPARATOR));
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
}

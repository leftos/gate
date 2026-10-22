//! The archive a run's outputs travel in: a tar stream compressed with zstd, named `.tar.zst`. Each file is read once,
//! hashed with SHA-256 as it is packed, and the list of every path brought back (path, size, SHA-256) follows the files
//! as the last member, [`LIST_NAME`]. A path whose content an earlier member or a file the PC holds already has is
//! listed as a copy and not packed. Reading checks each member's path before anything is written, extracts it while
//! hashing it, checks the members against the list at the end, then makes each copy from its member or the PC's file,
//! hashing it as it is written. Writer and reader stream: what either holds in memory does not grow with a file's size.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_router::escaped;
use gate_wire::{SHA256_DIGITS, is_lower_hex, ordinal_upper};
use serde_json::{Value, json};
use tar::{EntryType, Header};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};

use crate::outputs::hash::Hashing;
use crate::outputs::manifest::{Chosen, HeldBy, Manifest};
use crate::outputs::names::is_plain_path;
use crate::outputs::walk::{Found, in_context, stamp_of};

/// The name of the archive's last member, the list of every path the archive brings back.
pub(crate) const LIST_NAME: &str = ".gate-outputs.json";
/// The zstd level the archive is compressed at: zstd's default, which keeps pace with a gigabit link. The writer adds
/// long-distance matching over a window sized to the archive's files ([`window_log`]), which finds a repeat that far
/// back in the stream.
const LEVEL: i32 = 3;
/// The most bytes the list member is read to.
const LIST_LIMIT: u64 = 4 * 1024 * 1024;
/// The most files an archive may hold: more than a list of [`LIST_LIMIT`] bytes can name, each of its items taking at
/// least 98 bytes.
const MEMBER_LIMIT: usize = 50_000;
/// The most bytes one member may hold.
const MEMBER_SIZE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
/// The most bytes a GNU long name member may hold.
const LONG_NAME_LIMIT: u64 = 4 * 1024;
/// What the stream as sent may hold beyond zstd's worst-case expansion of the limit's bytes: frame and block headers.
const SENT_SLACK: u64 = 1024 * 1024;

/// The most bytes a `.tar.zst` stream may hold for an archive of a `limit`-byte limit: zstd's worst case expansion of
/// the limit's bytes, the ratio a decoder could reach through a skippable frame or empty blocks, plus [`SENT_SLACK`].
/// The reader refuses a longer stream, and the client fetching one caps its fetch at the same figure, so every archive
/// the box can legitimately send is one the reader would take. Saturating, and never 0: [`SENT_SLACK`] alone is above
/// it, so the cap the client writes under can never drop every byte.
pub(crate) fn sent_limit(limit: u64) -> NonZeroU64 {
    NonZeroU64::new(limit.saturating_add(limit / 64).saturating_add(SENT_SLACK)).unwrap_or(NonZeroU64::MIN)
}
/// The largest zstd window the writer compresses with and the reader accepts, as a power of two: 128 MiB.
const WINDOW_LOG_MAX: u32 = 27;
/// The smallest zstd window the writer compresses with, as a power of two: 1 MiB.
const WINDOW_LOG_MIN: u32 = 20;
/// The most characters of a path a refusal of the list shows.
const SHOWN_PATH_CHARS: usize = 200;
/// The most PC files a refusal of a copy names among those it tried.
const TRIED_NAMED: usize = 5;
/// How far past the reader's clock a member's last write may be: a day, for clocks that disagree.
const CLOCK_SLACK: Duration = Duration::from_hours(24);

/// One packed file: its `/`-separated path relative to the root it was packed from, its size and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// One item of an archive's list: a file packed as a member, or a copy, whose bytes the archive does not hold, as an
/// earlier member or a file the PC holds has them. `copy` is a copy's own last write, in seconds since 1970 as a
/// member's header holds it; `None` for a member.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    entry: Entry,
    copy: Option<u64>,
}

/// The PC's own files a copy may be made from: the worktree root, and the manifest of the files below it that the PC
/// sent the box, each path holding a copy's SHA-256 tried in turn; and `go_on`, asked before each copy is made and
/// after each buffer it copies, which stops the unpack once it says no.
pub(crate) struct PcFiles<'a> {
    pub root: &'a Path,
    pub manifest: &'a Manifest,
    pub go_on: &'a dyn Fn() -> bool,
}

/// What a pack wrote: its entries in order, and the archive's size, compressed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Packed {
    pub entries: Vec<Entry>,
    pub compressed: u64,
}

/// Packs `files` into `sink` as a `.tar.zst` stream, in their order, over a zstd window sized to their bytes
/// ([`window_log`]). Returns the entries of every path listed and the compressed size; else why the pack failed, the
/// file's path named.
///
/// Each file is opened at its turn, the file itself and never a link's target: a link or a junction, a file last
/// written before 1970, and a file found below `root` whose final path, as the system resolves the open file, is not
/// inside `root`'s (one reached through a folder swapped for a junction after the walk) fail the pack. A file hashed
/// before the pack is then listed as a copy, none of its bytes packed and its own last write listed, when its size and
/// last write are still those the walk found before it was hashed, and that hash is the SHA-256 a member packed before
/// it was read to or one `held` holds. Every other file is packed as a member: read once, hashed as it is read, its
/// header holding the size and last write it had when it was opened; one whose size or last write changes while it is
/// read fails the pack.
pub(crate) fn pack(sink: impl Write, root: &Path, files: &[Chosen<'_>], held: &HeldBy<'_>) -> io::Result<Packed> {
    // Resolved once, as each file's final path is: `canonicalize` is `GetFinalPathNameByHandleW` on the opened folder.
    let root = fs::canonicalize(root).map_err(|error| in_context(root, &error))?;
    let total = files.iter().fold(0_u64, |total, chosen| total.saturating_add(chosen.file.stamp.size));
    let mut sent: HashMap<String, u64> = HashMap::new();
    let mut packer = Packer::new(sink, window_log(total))?;
    for chosen in files {
        let known = |sha256: &str| sent.get(sha256).copied().or_else(|| held_size(held, sha256));
        pack_one(&mut packer, &root, chosen, known).map_err(|error| in_context(&chosen.file.full, &error))?;
        if let Some(Item { entry, copy: None }) = packer.entries.last() {
            sent.entry(entry.sha256.clone()).or_insert(entry.size);
        }
    }
    packer.finish()
}

/// The zstd window, as a power of two, for an archive of files of `total` bytes: the smallest that holds them, within
/// [`WINDOW_LOG_MIN`] and [`WINDOW_LOG_MAX`].
fn window_log(total: u64) -> u32 {
    total.saturating_sub(1).bit_width().clamp(WINDOW_LOG_MIN, WINDOW_LOG_MAX)
}

/// Packs `chosen`, found below the final path `root`, as [`pack`] does: a copy when it was hashed before the pack, it
/// still has the stamp the walk found, and `known` gives the size of a content with that SHA-256; else a member, read
/// now. Else why it failed.
fn pack_one<W: Write>(packer: &mut Packer<W>, root: &Path, chosen: &Chosen<'_>, known: impl Fn(&str) -> Option<u64>) -> io::Result<()> {
    let file = chosen.file;
    let opened = open_plain(&file.full)?;
    check_inside(&opened, root, file)?;
    let before = opened.metadata()?;
    let modified = seconds_since_1970(file, before.modified()?)?;
    let copied = chosen
        .sha256
        .as_deref()
        .filter(|_| stamp_of(&before) == file.stamp)
        .and_then(|sha256| Some((sha256, known(sha256)?)));
    match copied {
        Some((sha256, size)) => packer.copy(&file.path, size, modified, sha256),
        None => pack_file(packer, file, &opened, &opened),
    }
}

/// The last write `written` of `file`, in seconds since 1970; else the refusal of a file last written before 1970.
fn seconds_since_1970(file: &Found, written: SystemTime) -> io::Result<u64> {
    written
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .map_err(|_| refused(format!("{} was last written before 1970", file.path)))
}

/// Refuses the open file `opened`, found as `file`, when its final path is not inside the final path `root`.
fn check_inside(opened: &File, root: &Path, file: &Found) -> io::Result<()> {
    if final_path(opened)?.starts_with(root) {
        Ok(())
    } else {
        Err(refused(format!("{} resolves outside the work folder", file.path)))
    }
}

/// The final path of the open file `opened`, as `GetFinalPathNameByHandleW` gives it: `\\?\`-prefixed, normalized, its
/// links and junctions resolved as the open went through them.
pub(crate) fn final_path(opened: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: the handle is open while `opened` lives, and `buffer` has room for `capacity` UTF-16 units.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                opened.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        let length = usize::try_from(length).map_err(io::Error::other)?;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        // Too small: `length` is the room the path needs, its terminating NUL included.
        buffer.resize(length, 0);
    }
}

/// The file at `path` opened for reading: the file itself, never a link's target; else why not, a link or a junction
/// refused.
fn open_plain(path: &Path) -> io::Result<File> {
    let opened = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    if opened.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(opened)
    } else {
        Err(refused("it is a link or a junction, not a file".to_owned()))
    }
}

/// Packs the open file `opened`, found as `file`, its bytes read from `data`: its header holds the size and last write
/// `opened` has before the read, and a file whose size or last write differs after it fails the pack.
fn pack_file<W: Write>(packer: &mut Packer<W>, file: &Found, opened: &File, data: impl Read) -> io::Result<()> {
    let before = opened.metadata()?;
    let written = before.modified()?;
    let modified = seconds_since_1970(file, written)?;
    packer.add(&file.path, before.len(), modified, data)?;
    let after = opened.metadata()?;
    if after.len() == before.len() && after.modified()? == written {
        Ok(())
    } else {
        Err(refused(format!(
            "{} changed while it was packed: its size or last write differs from when it was opened",
            file.path
        )))
    }
}

/// Extracts the `.tar.zst` stream `source` into the folder `destination`, which it creates and which must not exist
/// yet, and into the folders it makes below it, each file with the last write its header holds. Returns the entries;
/// else why the archive was refused.
///
/// A member whose path is not a plain relative path inside the destination (see [`member_path`]), that is not a plain
/// file, that is larger than [`MEMBER_SIZE_LIMIT`] or than what is left of `limit`, whose last write is before 1970 or
/// more than a day past this machine's clock, that would be written through a link or a junction, or that would make
/// the archive's folders more than [`MEMBER_LIMIT`], is refused before anything of it is written. An archive of more
/// than [`MEMBER_LIMIT`] files, that decompresses to more than `limit` bytes, or whose stream as sent passes zstd's
/// worst case for `limit` bytes, is refused as it passes the limit; a member whose size or SHA-256 differs from the
/// list's, a member the list lacks or one it holds that the archive lacks is refused after extraction.
///
/// Each copy the list holds is then made in the destination, with the last write its item lists: from the first member
/// holding its SHA-256, else from a file below `pc`'s root that its manifest holds with that SHA-256, found inside the
/// root and checked by hashing its bytes as they are copied. A list naming a path twice, without regard to case, or a
/// path longer than [`LONG_NAME_LIMIT`], and a copy whose SHA-256 no member listed before it and no file of the manifest
/// has, whose last write is more than a day past this machine's clock, or past the file or byte limit, are refused
/// before any copy is written; a copy whose bytes differ from its item as they are written is refused then, and so is
/// one that no file of the manifest still holds, the refusal naming each file tried and why it was passed over.
///
/// Nothing is written outside the destination, and on a refusal `unpack` removes the destination it made, and all it
/// extracted there; when that removal fails too, the refusal says so. The error says whether a member of the archive is
/// refused for what it is ([`UnpackError::Members`]) or the stream could not be read ([`UnpackError::Fault`]).
pub(crate) fn unpack(source: impl Read, destination: &Path, limit: u64, pc: &PcFiles<'_>) -> Result<Vec<Entry>, UnpackError> {
    fs::create_dir(destination).map_err(|error| in_context(destination, &error))?;
    extract_into(source, destination, limit, MEMBER_LIMIT, pc).map_err(|error| match fs::remove_dir_all(destination) {
        Ok(()) => error,
        Err(removal) => error.and_removal(destination, &removal),
    })
}

/// Extracts `source` into the folder `destination`, which exists, as [`unpack`] does, making at most `folder_limit`
/// folders.
fn extract_into(source: impl Read, destination: &Path, limit: u64, folder_limit: usize, pc: &PcFiles<'_>) -> Result<Vec<Entry>, UnpackError> {
    let root = std::path::absolute(destination).map_err(|error| in_context(destination, &error))?;
    check_folder(&root)?;
    let mut folders = Folders {
        made: 0,
        limit: folder_limit,
    };
    let held = pc.manifest.by_hash(&[]);
    let items = read_members(source, limit, MEMBER_LIMIT, &held, |path, modified, data| {
        let target = target_below(&root, path, &mut folders)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| in_context(&target, &error))?;
        io::copy(data, &mut file).map_err(|error| in_context(&target, &error))?;
        file.set_modified(modified)
            .map_err(|error| UnpackError::Fault(in_context(&target, &error)))
    })?;
    make_copies(&root, &items, pc, &held, &mut folders)?;
    Ok(items.into_iter().map(|item| item.entry).collect())
}

/// Makes each copy of `items` below `root`, where its members were extracted, asking `pc`'s `go_on` before each and
/// after each buffer it copies: from the first member holding its SHA-256, else from the first file below `pc`'s root
/// that `held` names for it whose bytes still hash to it as they are copied ([`copy_from_pc`]). Each copy takes the
/// last write its item lists. Else the refusal, or the interrupt once `go_on` says no.
fn make_copies(root: &Path, items: &[Item], pc: &PcFiles<'_>, held: &HeldBy<'_>, folders: &mut Folders) -> Result<(), UnpackError> {
    let now = SystemTime::now();
    let mut members: HashMap<&str, &str> = HashMap::new();
    for Item { entry, copy } in items {
        let Some(seconds) = copy else {
            members.entry(entry.sha256.as_str()).or_insert(entry.path.as_str());
            continue;
        };
        if !(pc.go_on)() {
            return Err(stopped());
        }
        let modified = copy_last_write(&entry.path, *seconds, now)?;
        let target = target_below(root, &entry.path, folders)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| in_context(&target, &error))?;
        match members.get(entry.sha256.as_str()) {
            Some(member) => copy_member(&below(root, member), &mut file, &target, entry, pc.go_on)?,
            None => copy_from_pc(pc, held, &mut file, &target, entry)?,
        }
        file.set_modified(modified)
            .map_err(|error| UnpackError::Fault(in_context(&target, &error)))?;
    }
    Ok(())
}

/// The interrupt of an unpack its `go_on` stopped while its copies were made.
fn stopped() -> UnpackError {
    io::Error::new(io::ErrorKind::Interrupted, "the unpack was stopped while its copies were made").into()
}

/// Copies the extracted member at `full` into `file`, the new file `target`, as the copy `entry`, asking `go_on` after
/// each buffer; else the refusal, a copy whose bytes differ from `entry`'s refused, or the interrupt.
fn copy_member(full: &Path, file: &mut File, target: &Path, entry: &Entry, go_on: &dyn Fn() -> bool) -> Result<(), UnpackError> {
    let source = File::open(full).map_err(|error| in_context(full, &error))?;
    let (sha256, size) = copy_into(source, file, target, entry.size, go_on)?.map_err(|error| in_context(full, &error))?;
    if sha256 != entry.sha256 || size != entry.size {
        return Err(refused(format!(
            "the outputs archive copy '{}' does not match its list: {size} bytes with SHA-256 {sha256}, listed as {} bytes \
             with SHA-256 {}",
            escaped(&entry.path),
            entry.size,
            entry.sha256
        ))
        .into());
    }
    Ok(())
}

/// Copies into `file`, the new file `target`, the first file below `pc`'s root that `held` names for `entry`'s
/// SHA-256, in the manifest's order, whose bytes hash to it as they are copied, asking `pc`'s `go_on` after each
/// buffer. One missing, a link or a junction, outside the root ([`pc_source`]), unreadable, or whose bytes now differ is
/// passed over for the next. Else the refusal, naming each file tried and why it was passed over ([`said_tried`]), or
/// the interrupt.
fn copy_from_pc(pc: &PcFiles<'_>, held: &HeldBy<'_>, file: &mut File, target: &Path, entry: &Entry) -> Result<(), UnpackError> {
    // Resolved as each source's final path is: `canonicalize` is `GetFinalPathNameByHandleW` on the opened folder.
    let inside = fs::canonicalize(pc.root).map_err(|error| in_context(pc.root, &error))?;
    let mut tried: Vec<(&str, String)> = Vec::new();
    for &(path, _) in held.get(entry.sha256.as_str()).into_iter().flatten() {
        let source = match pc_source(&below(pc.root, path), &inside) {
            Ok(source) => source,
            Err(why) => {
                tried.push((path, why));
                continue;
            }
        };
        match copy_into(source, file, target, entry.size, pc.go_on)? {
            Ok((sha256, size)) if sha256 == entry.sha256 && size == entry.size => return Ok(()),
            Ok(_) => tried.push((path, "other content".to_owned())),
            Err(error) => tried.push((path, format!("unreadable: {error}"))),
        }
    }
    Err(refused(format!(
        "no PC file still holds the content of {}, SHA-256 {}{}",
        escaped(&entry.path),
        entry.sha256,
        said_tried(&tried)
    ))
    .into())
}

/// The PC file `full` opened as a copy's source: the file itself, never a link's target, whose final path, as the
/// system resolves the open file, is inside the final path `inside`; else why it is passed over.
fn pc_source(full: &Path, inside: &Path) -> Result<File, String> {
    let source = open_plain(full).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => "missing".to_owned(),
        io::ErrorKind::InvalidData => "not a plain file".to_owned(),
        _ => format!("unreadable: {error}"),
    })?;
    match final_path(&source) {
        Ok(path) if path.starts_with(inside) => Ok(source),
        Ok(_) => Err("outside the root".to_owned()),
        Err(error) => Err(format!("unreadable: {error}")),
    }
}

/// What a refusal of a copy says of the PC files `tried`, each path with why it was passed over: the first
/// [`TRIED_NAMED`], then how many more; nothing when none was tried.
fn said_tried(tried: &[(&str, String)]) -> String {
    if tried.is_empty() {
        return String::new();
    }
    let named: Vec<String> = tried
        .iter()
        .take(TRIED_NAMED)
        .map(|(path, why)| format!("{} ({why})", escaped(path)))
        .collect();
    let more = match tried.len().saturating_sub(TRIED_NAMED) {
        0 => String::new(),
        more => format!(" and {more} more"),
    };
    format!(": tried {}{more}", named.join(", "))
}

/// Writes the bytes of `source` over whatever `file`, the file `target`, holds, hashing them as they pass and asking
/// `go_on` after each buffer: their SHA-256 and count, else the error reading `source`; else the error writing
/// `target`, or the interrupt once `go_on` says no. No more than one byte past `size` is read, so a source that grew
/// writes no more than the limit counted for the copy.
fn copy_into(source: File, file: &mut File, target: &Path, size: u64, go_on: &dyn Fn() -> bool) -> Result<io::Result<(String, u64)>, UnpackError> {
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .map_err(|error| in_context(target, &error))?;
    let mut hashing = Hashing::new(source.take(size.saturating_add(1)));
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match hashing.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Ok(Err(error)),
        };
        file.write_all(&buffer[..read]).map_err(|error| in_context(target, &error))?;
        if !go_on() {
            return Err(stopped());
        }
    }
    Ok(Ok(hashing.finish()))
}

/// The size of the first file `held` holds with the SHA-256 `sha256`; `None` when it holds none.
fn held_size(held: &HeldBy<'_>, sha256: &str) -> Option<u64> {
    held.get(sha256).and_then(|paths| paths.first()).map(|(_, file)| file.stamp.size)
}

/// The full path of the `/`-separated `path` below `root`.
pub(crate) fn below(root: &Path, path: &str) -> PathBuf {
    path.split('/').fold(root.to_path_buf(), |full, part| full.join(part))
}

/// The full path of the member `path` below the folder `root`, making each folder between them that does not exist;
/// else the refusal, when the full path leaves `root`, a folder on the way is a link, a junction or not a folder, or
/// `folders` may make no more.
fn target_below(root: &Path, path: &str, folders: &mut Folders) -> Result<PathBuf, UnpackError> {
    let target = below(root, path);
    let full = std::path::absolute(&target).map_err(|error| in_context(&target, &error))?;
    if !full.starts_with(root) {
        return Err(member_refused(format!(
            "the outputs archive member '{}' is not a relative path inside the destination",
            escaped(path)
        )));
    }
    let parents = path.rsplit_once('/').map_or("", |(parents, _)| parents);
    let mut folder = root.to_path_buf();
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        folder.push(part);
        folders.make(&folder)?;
        check_folder(&folder)?;
    }
    Ok(target)
}

/// The folders an extraction has made, and the most it may make.
struct Folders {
    made: usize,
    limit: usize,
}

impl Folders {
    /// Makes `folder` when it does not exist, counting it; else why not, the limit refused before the folder is made.
    fn make(&mut self, folder: &Path) -> Result<(), UnpackError> {
        match fs::symlink_metadata(folder) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Ok(()),
        }
        if self.made == self.limit {
            return Err(member_refused(format!("the outputs archive makes more than {} folders", self.limit)));
        }
        match fs::create_dir(folder) {
            Ok(()) => {
                self.made += 1;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(in_context(folder, &error).into()),
        }
    }
}

/// Whether `folder` is a folder itself, not a link or a junction; else the refusal.
pub(crate) fn check_folder(folder: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(folder).map_err(|error| in_context(folder, &error))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(in_context(folder, &refused("it is a link or a junction, not a folder".to_owned())))
    } else if metadata.is_dir() {
        Ok(())
    } else {
        Err(in_context(folder, &refused("it is not a folder".to_owned())))
    }
}

/// A reader that fails once more than `limit` bytes have been read through it, `left` of them still to read, which it
/// shares; the refusal names the bytes it counts, `counted`.
struct Limited<R> {
    inner: R,
    left: Rc<Cell<u64>>,
    limit: u64,
    counted: &'static str,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        let left = self
            .left
            .get()
            .checked_sub(read as u64)
            .ok_or_else(|| refused(format!("the outputs archive holds more than {} bytes {}", self.limit, self.counted)))?;
        self.left.set(left);
        Ok(read)
    }
}

/// A writer that counts the bytes written through it.
struct Counting<W> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.count += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// An archive being written: the tar stream over the zstd encoder over the sink, and the items listed so far.
struct Packer<W: Write> {
    tar: tar::Builder<zstd::stream::write::Encoder<'static, Counting<W>>>,
    entries: Vec<Item>,
}

impl<W: Write> Packer<W> {
    /// An archive written to `sink`, compressed over a window of `2^window_log` bytes.
    fn new(sink: W, window_log: u32) -> io::Result<Packer<W>> {
        let mut encoder = zstd::stream::write::Encoder::new(Counting { inner: sink, count: 0 }, LEVEL)?;
        encoder.long_distance_matching(true)?;
        encoder.window_log(window_log)?;
        Ok(Packer {
            tar: tar::Builder::new(encoder),
            entries: Vec::new(),
        })
    }

    /// Packs the `size` bytes of `data` as the file `path`, last written `modified` (seconds since 1970), hashing them
    /// as they pass. Data that ends before `size` bytes fails the pack; bytes past it are left unread.
    fn add(&mut self, path: &str, size: u64, modified: u64, data: impl Read) -> io::Result<()> {
        check_name(path)?;
        let mut hashing = Hashing::new(data.take(size));
        self.tar.append_data(&mut file_header(size, modified), path, &mut hashing)?;
        let (sha256, count) = hashing.finish();
        if count != size {
            return Err(refused(format!(
                "{path} changed while it was packed: {count} of its {size} bytes were read"
            )));
        }
        let path = path.to_owned();
        self.entries.push(Item {
            entry: Entry { path, size, sha256 },
            copy: None,
        });
        Ok(())
    }

    /// Lists the file `path`, last written `modified` (seconds since 1970), as a copy of the `size` bytes with SHA-256
    /// `sha256` that an earlier member or a file the PC holds has, none of its bytes packed.
    fn copy(&mut self, path: &str, size: u64, modified: u64, sha256: &str) -> io::Result<()> {
        check_name(path)?;
        self.entries.push(Item {
            entry: Entry {
                path: path.to_owned(),
                size,
                sha256: sha256.to_owned(),
            },
            copy: Some(modified),
        });
        Ok(())
    }

    /// Packs the list, ends the tar and zstd streams and flushes the sink.
    fn finish(mut self) -> io::Result<Packed> {
        let list = list_json(&self.entries);
        let mut header = file_header(list.len() as u64, 0);
        self.tar.append_data(&mut header, LIST_NAME, list.as_bytes())?;
        let mut sink = self.tar.into_inner()?.finish()?;
        sink.flush()?;
        Ok(Packed {
            entries: self.entries.into_iter().map(|item| item.entry).collect(),
            compressed: sink.count,
        })
    }
}

/// Whether the output `path` may be listed: not the list's own name, and a name the reader takes; else the refusal.
fn check_name(path: &str) -> io::Result<()> {
    if path.eq_ignore_ascii_case(LIST_NAME) {
        return Err(refused(format!("the output {path} has the name of the archive's own list")));
    }
    if !is_plain_path(path) {
        return Err(refused(format!(
            "the output '{}' has a name the outputs archive's reader refuses",
            escaped(path)
        )));
    }
    Ok(())
}

/// The header of a plain file of `size` bytes last written `modified`, its path still to be set.
fn file_header(size: u64, modified: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(modified);
    header
}

/// Reads the `.tar.zst` stream `source`, decompressed to at most `limit` bytes, handing each file member's path, last
/// write and data to `member` and hashing the data as `member` reads it, then checks the members against the list
/// ([`agree`], each copy's SHA-256 a member's before it or one `held` holds) and the list against the limits: at most
/// `files` paths, and each copy's bytes within what is left of `limit` ([`Members::fit_copies`]). Returns the list's
/// items, else why the archive was refused.
fn read_members(
    source: impl Read,
    limit: u64,
    files: usize,
    held: &HeldBy<'_>,
    mut member: impl FnMut(&str, SystemTime, &mut dyn Read) -> Result<(), UnpackError>,
) -> Result<Vec<Item>, UnpackError> {
    // zstd's worst case for `limit` bytes, so a skippable frame or empty blocks cannot be read without end either.
    let sent = sent_limit(limit).get();
    let source = Limited {
        inner: source,
        left: Rc::new(Cell::new(sent)),
        limit: sent,
        counted: "as sent",
    };
    let mut decoder = zstd::stream::read::Decoder::new(source)?.single_frame();
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let left = Rc::new(Cell::new(limit));
    let mut archive = tar::Archive::new(Limited {
        inner: decoder,
        left: Rc::clone(&left),
        limit,
        counted: "once decompressed",
    });
    let mut members = Members {
        seen: Vec::new(),
        listed: None,
        long_name: None,
        now: SystemTime::now(),
        left,
        files,
    };
    // Raw, so that tar-rs reads no GNU long name or PAX header of its own, each of which it would read whole.
    for entry in archive.entries()?.raw(true) {
        members.take(&mut entry?, &mut member)?;
    }
    if members.long_name.is_some() {
        return Err(member_refused("the outputs archive ends with a long name".to_owned()));
    }
    // To the end of the zstd frame, so a stream cut short after the tar's end is refused too.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    let listed = members
        .listed
        .take()
        .ok_or_else(|| member_refused("the outputs archive holds no list of its files".to_owned()))?;
    agree(&members.seen, &listed, held)?;
    members.fit_copies(&listed)?;
    Ok(listed)
}

/// What the reader has taken of an archive so far: the files, the list once it is read, the long name the next member
/// takes, the clock each member is held to, the decompressed bytes left of the archive's limit, which the reader under
/// the tar counts down and each copy then takes its size of, and the most paths the archive may bring back.
struct Members {
    seen: Vec<Entry>,
    listed: Option<Vec<Item>>,
    long_name: Option<Vec<u8>>,
    now: SystemTime,
    left: Rc<Cell<u64>>,
    files: usize,
}

impl Members {
    /// Takes the raw tar entry `entry`: a GNU long name for the next member, the list, or a file handed to `member`;
    /// else the refusal.
    fn take(
        &mut self,
        entry: &mut tar::Entry<'_, impl Read>,
        member: &mut impl FnMut(&str, SystemTime, &mut dyn Read) -> Result<(), UnpackError>,
    ) -> Result<(), UnpackError> {
        if self.listed.is_some() {
            return Err(member_refused("the outputs archive has a member after its list".to_owned()));
        }
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            return self.take_long_name(entry);
        }
        let name = self.long_name.take().unwrap_or_else(|| entry.path_bytes().into_owned());
        if kind != EntryType::Regular {
            let shown = String::from_utf8_lossy(&name);
            return Err(member_refused(format!("the outputs archive member '{}' is not a file", escaped(&shown))));
        }
        let path = member_path(&name)?;
        if path.eq_ignore_ascii_case(LIST_NAME) {
            self.listed = Some(read_list(entry, self.now)?);
            return Ok(());
        }
        self.check_room(&path, entry.size())?;
        let modified = last_write(entry.header(), self.now)
            .map_err(|reason| member_refused(format!("the outputs archive member '{}' has a last write {reason}", escaped(&path))))?;
        let mut hashing = Hashing::new(entry);
        member(&path, modified, &mut hashing)?;
        let (sha256, size) = hashing.finish();
        self.seen.push(Entry { path, size, sha256 });
        Ok(())
    }

    /// Whether one more file, `path` of `size` bytes, fits: within the archive's file limit, and no larger than
    /// [`MEMBER_SIZE_LIMIT`] or what is left of the archive's byte limit; else the refusal.
    fn check_room(&self, path: &str, size: u64) -> Result<(), UnpackError> {
        if self.seen.len() == self.files {
            return Err(self.too_many());
        }
        self.check_size("member", path, size)
    }

    /// The refusal of an archive holding more paths than its file limit.
    fn too_many(&self) -> UnpackError {
        member_refused(format!("the outputs archive holds more than {} files", self.files))
    }

    /// Whether the copies of `listed` fit once every member is read: the list's paths, copies included, within the
    /// archive's file limit, and each copy a plain path, not the list's own name, no larger than [`MEMBER_SIZE_LIMIT`]
    /// or what is left of the byte limit, which it then takes its size of; else the refusal, before any copy is made.
    fn fit_copies(&self, listed: &[Item]) -> Result<(), UnpackError> {
        if listed.len() > self.files {
            return Err(self.too_many());
        }
        for Item { entry, .. } in listed.iter().filter(|item| item.copy.is_some()) {
            let path = member_path(entry.path.as_bytes())?;
            if path.eq_ignore_ascii_case(LIST_NAME) {
                return Err(member_refused(format!("the outputs archive lists a copy named as its own list, {path}")));
            }
            self.check_size("copy", &path, entry.size)?;
            self.left.set(self.left.get().saturating_sub(entry.size));
        }
        Ok(())
    }

    /// Whether the file `path` of `size` bytes, a member or a copy as `kind` says, is no larger than
    /// [`MEMBER_SIZE_LIMIT`] or what is left of the archive's byte limit; else the refusal.
    fn check_size(&self, kind: &str, path: &str, size: u64) -> Result<(), UnpackError> {
        let most = MEMBER_SIZE_LIMIT.min(self.left.get());
        if size > most {
            return Err(UnpackError::Fault(refused(format!(
                "the outputs archive {kind} '{}' is {size} bytes, more than the {most} it may be",
                escaped(path)
            ))));
        }
        Ok(())
    }

    /// Takes the GNU long name `entry` holds, of at most [`LONG_NAME_LIMIT`] bytes, for the next member; else the
    /// refusal.
    fn take_long_name(&mut self, entry: &mut tar::Entry<'_, impl Read>) -> Result<(), UnpackError> {
        if self.long_name.is_some() {
            return Err(member_refused("the outputs archive has two long names in a row".to_owned()));
        }
        if entry.size() > LONG_NAME_LIMIT {
            return Err(member_refused(format!(
                "the outputs archive has a long name of more than {LONG_NAME_LIMIT} bytes"
            )));
        }
        let mut name = Vec::new();
        entry.read_to_end(&mut name)?;
        // A GNU long name ends with a NUL, which tar-rs drops too.
        if name.last() == Some(&0) {
            name.pop();
        }
        self.long_name = Some(name);
        Ok(())
    }
}

/// The last write `header` holds, as a time no more than [`CLOCK_SLACK`] past `now`; else what is wrong with it. A
/// base-256 time, which this writer never writes, is before 1970 or past what an octal field holds.
fn last_write(header: &Header, now: SystemTime) -> Result<SystemTime, String> {
    if header.as_old().mtime[0] & 0x80 != 0 {
        return Err("in base-256, before 1970 or far past this machine's clock".to_owned());
    }
    let seconds = header.mtime().map_err(|error| format!("that cannot be read: {error}"))?;
    time_at(seconds, now)
}

/// The time `seconds` after 1970, no more than [`CLOCK_SLACK`] past `now`; else what is wrong with it.
fn time_at(seconds: u64, now: SystemTime) -> Result<SystemTime, String> {
    let written = UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| format!("{seconds} seconds after 1970, past what this machine's clock holds"))?;
    if now.checked_add(CLOCK_SLACK).is_some_and(|latest| written > latest) {
        return Err(format!("{seconds} seconds after 1970, more than a day past this machine's clock"));
    }
    Ok(written)
}

/// The member name `bytes` as a `/`-separated path inside the destination: UTF-8 and a plain path (see
/// [`is_plain_path`]); else the refusal.
fn member_path(bytes: &[u8]) -> Result<String, UnpackError> {
    let outside = |shown: &str| {
        member_refused(format!(
            "the outputs archive member '{}' is not a relative path inside the destination",
            escaped(shown)
        ))
    };
    let path = std::str::from_utf8(bytes).map_err(|_| outside(&String::from_utf8_lossy(bytes)))?;
    if is_plain_path(path) { Ok(path.to_owned()) } else { Err(outside(path)) }
}

/// The items of the list member `data`, a copy marked by `"copy":true` and carrying its own last write as
/// `"modified"`, in seconds since 1970, and every other item carrying neither key; else why it is not a list the reader
/// takes ([`check_listed`]), its paths checked against each other and a copy's last write against `now`.
fn read_list(data: &mut impl Read, now: SystemTime) -> Result<Vec<Item>, UnpackError> {
    let mut text = String::new();
    data.take(LIST_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|error| refused(format!("the outputs archive's list cannot be read: {error}")))?;
    if text.len() as u64 > LIST_LIMIT {
        return Err(member_refused(format!("the outputs archive's list is longer than {LIST_LIMIT} bytes")));
    }
    let not_a_list = || member_refused("the outputs archive's list is not a list of paths, sizes and SHA-256s".to_owned());
    let value: Value = serde_json::from_str(&text).map_err(|_| not_a_list())?;
    let values = value.as_array().ok_or_else(not_a_list)?;
    let mut paths = HashSet::new();
    let mut items = Vec::with_capacity(values.len());
    for value in values {
        let item = list_item(value).ok_or_else(not_a_list)?;
        check_listed(&item, &mut paths, now)?;
        items.push(item);
    }
    Ok(items)
}

/// The list item `value` holds; `None` when it holds none.
fn list_item(value: &Value) -> Option<Item> {
    let path = value.get("path")?.as_str()?;
    let size = value.get("size")?.as_u64()?;
    let sha256 = value.get("sha256")?.as_str().filter(|sha256| is_lower_hex(sha256, SHA256_DIGITS))?;
    let copy = match (value.get("copy"), value.get("modified")) {
        (None, None) => None,
        (Some(Value::Bool(true)), Some(modified)) => Some(modified.as_u64()?),
        _ => return None,
    };
    Some(Item {
        entry: Entry {
            path: path.to_owned(),
            size,
            sha256: sha256.to_owned(),
        },
        copy,
    })
}

/// Whether the list may hold `item` after the items whose paths' simple uppercase `paths` holds, which it then joins:
/// a path of at most [`LONG_NAME_LIMIT`] bytes that no item before it has in any case, and for a copy a last write no
/// more than [`CLOCK_SLACK`] past `now`; else the refusal, which shows the path's first [`SHOWN_PATH_CHARS`] characters.
fn check_listed(item: &Item, paths: &mut HashSet<String>, now: SystemTime) -> Result<(), UnpackError> {
    let path = item.entry.path.as_str();
    if path.len() as u64 > LONG_NAME_LIMIT {
        return Err(member_refused(format!(
            "the outputs archive lists a path of more than {LONG_NAME_LIMIT} bytes: '{}'",
            shown(path)
        )));
    }
    if !paths.insert(ordinal_upper(path)) {
        return Err(member_refused(format!("the outputs archive lists '{}' twice", shown(path))));
    }
    if let Some(seconds) = item.copy {
        copy_last_write(path, seconds, now)?;
    }
    Ok(())
}

/// The last write of the copy `path`, `seconds` after 1970; else the refusal of one more than [`CLOCK_SLACK`] past
/// `now`, or past what this machine's clock holds.
fn copy_last_write(path: &str, seconds: u64, now: SystemTime) -> Result<SystemTime, UnpackError> {
    time_at(seconds, now).map_err(|reason| member_refused(format!("the outputs archive copy '{}' has a last write {reason}", shown(path))))
}

/// `path` as a refusal of the list shows it: its first [`SHOWN_PATH_CHARS`] characters, escaped, a cut marked.
fn shown(path: &str) -> String {
    match path.char_indices().nth(SHOWN_PATH_CHARS) {
        Some((cut, _)) => format!("{}\u{2026}", escaped(&path[..cut])),
        None => escaped(path),
    }
}

/// The list member's text for `items`: a JSON array of `{"path","size","sha256"}` objects, a copy's with `"copy":true`
/// and its own last write, `"modified"`.
fn list_json(items: &[Item]) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|Item { entry, copy }| {
            let mut item = json!({ "path": entry.path, "size": entry.size, "sha256": entry.sha256 });
            if let Some(modified) = copy {
                item["copy"] = Value::Bool(true);
                item["modified"] = json!(modified);
            }
            item
        })
        .collect();
    Value::Array(items).to_string()
}

/// Whether the members `seen` are exactly the items `listed` that are not copies, in order, and each copy's SHA-256 and
/// size are those of a member listed before it or of a file the PC holds (`held`); else the refusal naming the first
/// difference.
fn agree(seen: &[Entry], listed: &[Item], held: &HeldBy<'_>) -> io::Result<()> {
    let mut members = seen.iter();
    let mut sent: HashMap<&str, u64> = HashMap::new();
    for Item { entry, copy } in listed {
        if copy.is_some() {
            copy_known(entry, &sent, held)?;
            continue;
        }
        let Some(member) = members.next() else {
            return Err(refused(format!(
                "the outputs archive lacks '{}', which its list holds",
                escaped(&entry.path)
            )));
        };
        agree_member(member, entry)?;
        sent.entry(member.sha256.as_str()).or_insert(member.size);
    }
    match members.next() {
        Some(member) => Err(not_next(member)),
        None => Ok(()),
    }
}

/// Whether the member `member` is the list's item `entry`; else the refusal.
fn agree_member(member: &Entry, entry: &Entry) -> io::Result<()> {
    if entry == member {
        Ok(())
    } else if entry.path == member.path {
        Err(refused(format!(
            "the outputs archive member '{}' does not match its list: {} bytes with SHA-256 {}, listed as {} bytes with \
             SHA-256 {}",
            escaped(&member.path),
            member.size,
            member.sha256,
            entry.size,
            entry.sha256
        )))
    } else {
        Err(not_next(member))
    }
}

/// The refusal of the member `member`, which is not the item the list holds next.
fn not_next(member: &Entry) -> io::Error {
    refused(format!(
        "the outputs archive member '{}' is not the file its list holds next",
        escaped(&member.path)
    ))
}

/// Whether the copy `entry` has the SHA-256 and size of a member `sent` before it, or of a file the PC holds (`held`);
/// else the refusal.
fn copy_known(entry: &Entry, sent: &HashMap<&str, u64>, held: &HeldBy<'_>) -> io::Result<()> {
    let sha256 = entry.sha256.as_str();
    let size = sent.get(sha256).copied().or_else(|| held_size(held, sha256));
    if size == Some(entry.size) {
        Ok(())
    } else {
        Err(refused(format!(
            "the outputs archive lists '{}' as a copy of {} bytes with SHA-256 {}, which no member before it and no file \
             the PC holds has",
            escaped(&entry.path),
            entry.size,
            entry.sha256
        )))
    }
}

fn refused(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Why an archive could not be unpacked: whether its own members are refused for what they are, which a refetch of the
/// same archive would be refused the same way, or the stream could not be read, which a refetch may clear.
#[derive(Debug)]
pub(crate) enum UnpackError {
    /// A member of the archive is refused for what it is: its path or its kind, a last write it cannot hold, a name the
    /// reader never takes, or the file or folder limit it passes.
    Members(io::Error),
    /// The stream could not be read: an I/O fault, a bad frame or one cut short, a limit it passed as it was read, or a
    /// member that does not match the archive's own list.
    Fault(io::Error),
}

impl UnpackError {
    /// Whether the archive's own members are refused for what they are, which a refetch would refuse the same way.
    pub(crate) fn members(&self) -> bool {
        matches!(self, UnpackError::Members(_))
    }

    /// Whether this is the fault `kind` names, for a caller that tells one fault from another.
    pub(crate) fn is_fault(&self, kind: io::ErrorKind) -> bool {
        matches!(self, UnpackError::Fault(error) if error.kind() == kind)
    }

    /// This error, with `removal`'s failure to remove `destination` said after it, of the same kind.
    fn and_removal(self, destination: &Path, removal: &io::Error) -> UnpackError {
        let said = format!("{self}; and {} could not be removed: {removal}", destination.display());
        match self {
            UnpackError::Members(_) => UnpackError::Members(refused(said)),
            UnpackError::Fault(error) => UnpackError::Fault(io::Error::new(error.kind(), said)),
        }
    }
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnpackError::Members(error) | UnpackError::Fault(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for UnpackError {
    fn from(error: io::Error) -> UnpackError {
        UnpackError::Fault(error)
    }
}

/// The refusal of a member of the archive for what it is, which a refetch of the same archive would refuse the same way.
fn member_refused(reason: String) -> UnpackError {
    UnpackError::Members(refused(reason))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files or archives must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tar::{EntryType, Header};

    use super::{
        Entry, Item, LIST_NAME, LONG_NAME_LIMIT, MEMBER_LIMIT, Packer, PcFiles, UnpackError, WINDOW_LOG_MAX, extract_into, list_json, member_path,
        pack, pack_file, read_members, sent_limit, unpack,
    };
    use crate::boxside::outputs::OUTPUTS_BUDGET;
    use crate::outputs::glob::Globs;
    use crate::outputs::hash::sha256_of;
    use crate::outputs::manifest::{Chosen, Held, HeldBy, Manifest};
    use crate::outputs::names::is_device;
    use crate::outputs::walk::{Found, Stamp, walk};
    use crate::test_support::junction;
    use gate_router::escaped;

    /// The byte limit the cases unpack under, far above what any of them holds.
    const LIMIT: u64 = 1 << 30;

    /// `unpack` of `source` into `destination` under `limit`, with no file of the PC's to make a copy from.
    fn unpack_alone(source: impl Read, destination: &Path, limit: u64) -> Result<Vec<Entry>, UnpackError> {
        let manifest = Manifest::default();
        unpack(
            source,
            destination,
            limit,
            &PcFiles {
                root: Path::new(r"C:\no-pc-files"),
                manifest: &manifest,
                go_on: &|| true,
            },
        )
    }

    /// `extract_into` of `source` into `destination` under `limit`, making at most `folders` folders, with no file of
    /// the PC's to make a copy from.
    fn extract_alone(source: impl Read, destination: &Path, limit: u64, folders: usize) -> Result<Vec<Entry>, UnpackError> {
        let manifest = Manifest::default();
        extract_into(
            source,
            destination,
            limit,
            folders,
            &PcFiles {
                root: Path::new(r"C:\no-pc-files"),
                manifest: &manifest,
                go_on: &|| true,
            },
        )
    }

    /// A scratch folder for one test, emptied first.
    fn scratch(test: &str) -> PathBuf {
        let folder = crate::test_support::temp_dir().join(format!("gate-archive-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        folder
    }

    /// The relative paths of every file below `root`, `/`-separated and sorted; none when it does not exist.
    fn files_below(root: &Path) -> Vec<String> {
        if !root.exists() {
            return Vec::new();
        }
        let found = walk(root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the folder can be walked");
        found.into_iter().map(|file| file.path).collect()
    }

    fn entry(path: &str, data: &[u8]) -> Entry {
        let (sha256, size) = sha256_of(data).expect("bytes hash");
        Entry {
            path: path.to_owned(),
            size,
            sha256,
        }
    }

    /// One member of a hand-made archive: its name's bytes, its type and its data.
    type Member<'a> = (&'a [u8], EntryType, &'a [u8]);

    /// The list item of the member `path` holding `data`.
    fn member(path: &str, data: &[u8]) -> Item {
        Item {
            entry: entry(path, data),
            copy: None,
        }
    }

    /// The last write a hand-made copy lists, in seconds since 1970.
    const COPY_WRITTEN: u64 = 1_700_000_000;

    /// The list item of a copy at `path` of `data`, last written [`COPY_WRITTEN`].
    fn copy_of(path: &str, data: &[u8]) -> Item {
        Item {
            entry: entry(path, data),
            copy: Some(COPY_WRITTEN),
        }
    }

    /// A `.tar.zst` stream of `members` as given, no name checked, then a list of `listed`, none of them a copy.
    fn raw(members: &[Member<'_>], listed: &[Entry]) -> Vec<u8> {
        let items: Vec<Item> = listed
            .iter()
            .map(|entry| Item {
                entry: entry.clone(),
                copy: None,
            })
            .collect();
        raw_items(members, &items)
    }

    /// A `.tar.zst` stream of `members` as given, no name checked, then a list of `listed`, copies among them.
    fn raw_items(members: &[Member<'_>], listed: &[Item]) -> Vec<u8> {
        let list = list_json(listed);
        let mut all = members.to_vec();
        all.push((LIST_NAME.as_bytes(), EntryType::Regular, list.as_bytes()));
        unlisted(&all)
    }

    /// A `.tar.zst` stream of `members` as given, no name checked and no list added.
    fn unlisted(members: &[Member<'_>]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for &(name, kind, data) in members {
            let mut header = Header::new_gnu();
            header.as_gnu_mut().expect("a GNU header").name[..name.len()].copy_from_slice(name);
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, data).expect("the member can be appended");
        }
        zstd::encode_all(&tar.into_inner().expect("the tar ends")[..], 3).expect("the tar compresses")
    }

    /// The error `unpack` gives for `archive`, and the files left of what it extracted before it: none, as it removes
    /// the destination it made.
    fn unpack_error(test: &str, archive: &[u8]) -> (UnpackError, Vec<String>) {
        let destination = scratch(test);
        let error = unpack_alone(archive, &destination, LIMIT).expect_err("the archive is refused");
        assert!(!destination.exists(), "{test}: the destination is removed");
        (error, files_below(&destination))
    }

    /// The refusal `unpack` gives for `archive`, and the files left of what it extracted before it: none, as it removes
    /// the destination it made.
    fn refusal(test: &str, archive: &[u8]) -> (String, Vec<String>) {
        let (error, files) = unpack_error(test, archive);
        (error.to_string(), files)
    }

    /// The refusal `unpack` gives for `archive`, which it refused before it extracted anything.
    fn refusal_before_any_file(test: &str, archive: &[u8]) -> String {
        let (reason, extracted) = refusal(test, archive);
        assert_eq!(extracted, Vec::<String>::new(), "{test}: nothing was extracted");
        reason
    }

    #[test]
    fn packed_files_come_back_whole_with_their_hashes_names_and_last_writes() {
        let source = scratch("round-trip-source");
        let long = format!("bin/{}.dll", "x".repeat(120));
        let files: [(&str, &[u8]); 4] = [
            ("bin/a.dll", b"abc"),
            ("bin/sub/empty.pdb", b""),
            ("bin/Ünïcødé ✓.dll", "ünï".as_bytes()),
            (&long, b"long"),
        ];
        let written = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for (path, data) in files {
            let full = source.join(path);
            fs::create_dir_all(full.parent().expect("a folder")).expect("the folder can be made");
            fs::write(&full, data).expect("the file can be written");
            fs::File::options()
                .write(true)
                .open(&full)
                .and_then(|file| file.set_modified(written))
                .expect("the last write can be set");
        }
        let found = walk(&source, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        let mut archive = Vec::new();
        let packed = pack(&mut archive, &source, &chosen, &HeldBy::new()).expect("the files pack");
        assert_eq!(packed.compressed, archive.len() as u64);
        let mut expected: Vec<Entry> = files.iter().map(|(path, data)| entry(path, data)).collect();
        expected.sort_by(|first, second| first.path.cmp(&second.path));
        assert_eq!(packed.entries, expected);

        let destination = scratch("round-trip-destination");
        assert_eq!(unpack_alone(&archive[..], &destination, LIMIT).expect("the archive extracts"), expected);
        for (path, data) in files {
            let full = destination.join(path);
            assert_eq!(fs::read(&full).expect("the file was extracted"), data, "{path}");
            let modified = fs::metadata(&full).and_then(|metadata| metadata.modified()).expect("a last write");
            assert_eq!(modified, written, "{path}");
        }
        assert_eq!(files_below(&destination).len(), files.len(), "the list is not extracted");
    }

    #[test]
    fn a_file_named_as_the_list_is_not_packed() {
        let mut packer = Packer::new(io::sink(), WINDOW_LOG_MAX).expect("a packer");
        let error = packer.add(".GATE-outputs.json", 1, 0, &b"x"[..]).expect_err("the list's name is refused");
        assert_eq!(error.to_string(), "the output .GATE-outputs.json has the name of the archive's own list");
        assert!(packer.add("bin/.gate-outputs.json", 1, 0, &b"x"[..]).is_ok(), "only at the root");
    }

    #[test]
    fn a_file_that_ends_before_its_size_fails_the_pack() {
        let mut packer = Packer::new(io::sink(), WINDOW_LOG_MAX).expect("a packer");
        let error = packer.add("bin/a.dll", 5, 0, &b"abc"[..]).expect_err("a short file is refused");
        assert_eq!(error.to_string(), "bin/a.dll changed while it was packed: 3 of its 5 bytes were read");
    }

    #[test]
    fn a_member_whose_bytes_differ_from_the_list_is_refused() {
        let archive = raw(&[(b"bin/a.dll", EntryType::Regular, b"abc")], &[entry("bin/a.dll", b"abd")]);
        let (reason, _) = refusal("hash", &archive);
        assert!(
            reason.starts_with("the outputs archive member 'bin/a.dll' does not match its list: 3 bytes with SHA-256 ba78"),
            "{reason}"
        );
    }

    #[test]
    fn a_member_path_that_could_leave_the_destination_is_refused_before_anything_is_written() {
        for name in [
            "/abs.dll",
            "C:/x.dll",
            "C:x.dll",
            "../x.dll",
            "bin/../../x.dll",
            r"bin\..\x.dll",
            r"\\server\x.dll",
            "./x.dll",
            "bin//x.dll",
            "bin/",
            "bin/CON",
            "nul.txt",
            "bin/com1.dll",
            "lpt9 .x",
            "bin/\u{1b}x",
        ] {
            let archive = raw(
                &[(b"bin/ok.dll", EntryType::Regular, b"ok"), (name.as_bytes(), EntryType::Regular, b"x")],
                &[entry("bin/ok.dll", b"ok"), entry(name, b"x")],
            );
            let (reason, extracted) = refusal("escape", &archive);
            assert!(reason.ends_with("is not a relative path inside the destination"), "{name:?}: {reason}");
            assert_eq!(extracted, Vec::<String>::new(), "{name:?}: not even the member before it is left");
        }
        let not_utf8 = unlisted(&[(b"bin/\xff.dll", EntryType::Regular, b"x")]);
        let reason = refusal_before_any_file("not-utf8", &not_utf8);
        assert_eq!(
            reason,
            "the outputs archive member 'bin/\u{fffd}.dll' is not a relative path inside the destination"
        );
    }

    #[test]
    fn a_link_a_folder_or_another_entry_that_is_not_a_file_is_refused() {
        for kind in [
            EntryType::Symlink,
            EntryType::Link,
            EntryType::Directory,
            EntryType::Fifo,
            EntryType::Continuous,
        ] {
            let archive = raw(&[(b"bin/link", kind, b"")], &[]);
            assert_eq!(
                refusal_before_any_file("not-a-file", &archive),
                "the outputs archive member 'bin/link' is not a file",
                "{kind:?}"
            );
        }
    }

    /// A member the archive's own reader refuses for what it is is a member error, which a refetch of the same archive
    /// would be refused the same way; a stream cut short is a fault, which a refetch may clear.
    #[test]
    fn a_refused_member_is_a_member_error_and_a_cut_stream_is_a_fault() {
        let link = raw(&[(b"bin/link", EntryType::Symlink, b"")], &[]);
        assert!(
            unpack_error("members-error", &link).0.members(),
            "a link member is refused for what it is"
        );

        let whole = raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]);
        let cut = &whole[..whole.len() / 2];
        assert!(
            !unpack_error("fault-error", cut).0.members(),
            "a stream cut short is not a member refusal"
        );
    }

    #[test]
    fn members_and_their_list_must_agree_exactly() {
        let a = entry("a", b"a");
        let b = entry("b", b"b");
        let members: [Member<'_>; 2] = [(b"a", EntryType::Regular, b"a"), (b"b", EntryType::Regular, b"b")];
        let reason = |test: &str, archive: &[u8]| refusal(test, archive).0;
        assert_eq!(
            reason("unlisted", &raw(&members, std::slice::from_ref(&a))),
            "the outputs archive member 'b' is not the file its list holds next"
        );
        assert_eq!(
            reason("out-of-order", &raw(&members, &[b.clone(), a.clone()])),
            "the outputs archive member 'a' is not the file its list holds next"
        );
        assert_eq!(
            reason("missing", &raw(&members[..1], &[a, b])),
            "the outputs archive lacks 'b', which its list holds"
        );
        assert_eq!(reason("no-list", &unlisted(&members)), "the outputs archive holds no list of its files");
        let after = unlisted(&[(LIST_NAME.as_bytes(), EntryType::Regular, b"[]"), (b"a", EntryType::Regular, b"a")]);
        assert_eq!(
            refusal_before_any_file("after-list", &after),
            "the outputs archive has a member after its list"
        );
        let bad_list = unlisted(&[(LIST_NAME.as_bytes(), EntryType::Regular, b"{\"a\":1}")]);
        assert_eq!(
            refusal_before_any_file("bad-list", &bad_list),
            "the outputs archive's list is not a list of paths, sizes and SHA-256s"
        );
    }

    #[test]
    fn a_stream_that_is_not_an_archive_or_is_cut_short_is_refused() {
        refusal_before_any_file("garbage", b"not an archive at all");
        let whole = raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]);
        for cut in [whole.len() / 2, whole.len() - 1] {
            refusal("cut", &whole[..cut]);
        }
    }

    /// Checks `member_path` refuses each of `names` as a path outside the destination.
    fn assert_refused_names(names: &[&str]) {
        for name in names {
            let reason = member_path(name.as_bytes()).expect_err(name).to_string();
            assert!(reason.ends_with("is not a relative path inside the destination"), "{name:?}: {reason}");
        }
    }

    /// Checks `member_path` takes each of `names` as it is.
    fn assert_taken_names(names: &[&str]) {
        for name in names {
            assert_eq!(member_path(name.as_bytes()).ok().as_deref(), Some(*name), "{name:?}");
        }
    }

    #[test]
    fn a_member_named_dot_git_in_any_form_is_refused() {
        assert_refused_names(&[".git/x", ".GIT/x", ".git./x", ".git /x", "git~1/x", "a/.git/x", "a/.Git/hooks/x", ".git"]);
        assert_taken_names(&[".gitignore", "bin/x.git", "bin/.github/x", "bin/git/x"]);
    }

    #[test]
    fn a_member_with_a_short_name_or_trailing_dot_is_refused() {
        assert_refused_names(&[
            "GIT~1/x", "x.dll.", "x.dll ", "a~1b/x", "bin./x", "bin /x", "bin/a<b", "bin/a>b", "bin/a\"b", "bin/a|b", "bin/a?b", "bin/a*b",
        ]);
        assert_taken_names(&["bin/a~b.dll", "bin/~x", "bin/x~", "bin/.x", "bin/a b.dll", " bin/x"]);
    }

    #[test]
    fn a_bidi_control_in_a_member_name_is_refused() {
        let marks = [
            '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{200e}', '\u{200f}',
        ];
        let names: Vec<String> = marks.iter().map(|mark| format!("bin/a{mark}b.dll")).collect();
        assert_refused_names(&names.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn com0_and_lpt0_are_devices() {
        for device in ["COM0", "lpt0", "com0.txt", "LPT0 .x", "COM¹", "LPT³"] {
            assert!(is_device(device), "{device}");
        }
        for name in ["COM10", "LPT", "COMA", "com0x"] {
            assert!(!is_device(name), "{name}");
        }
        assert_refused_names(&["bin/COM0", "bin/lpt0.dll"]);
    }

    #[test]
    fn a_list_member_name_matches_case_insensitively() {
        let a = entry("a", b"a");
        let list = list_json(&[member("a", b"a")]);
        let archive = unlisted(&[
            (b"a", EntryType::Regular, b"a"),
            (b".GATE-Outputs.JSON", EntryType::Regular, list.as_bytes()),
        ]);
        let destination = scratch("list-case");
        assert_eq!(unpack_alone(&archive[..], &destination, LIMIT).expect("the archive extracts"), [a]);
        assert_eq!(files_below(&destination), ["a"], "the list is not extracted");
    }

    #[test]
    fn a_long_name_header_past_the_cap_is_refused() {
        let long = vec![b'a'; 4096 + 1];
        let archive = unlisted(&[(b"././@LongLink", EntryType::GNULongName, &long), (b"x", EntryType::Regular, b"x")]);
        assert_eq!(
            refusal_before_any_file("long-name", &archive),
            "the outputs archive has a long name of more than 4096 bytes"
        );
        let two = unlisted(&[
            (b"././@LongLink", EntryType::GNULongName, b"bin/a.dll"),
            (b"././@LongLink", EntryType::GNULongName, b"bin/b.dll"),
            (b"x", EntryType::Regular, b"x"),
        ]);
        assert_eq!(
            refusal_before_any_file("two-long-names", &two),
            "the outputs archive has two long names in a row"
        );
        let last = unlisted(&[(b"././@LongLink", EntryType::GNULongName, b"bin/a.dll")]);
        assert_eq!(
            refusal_before_any_file("long-name-last", &last),
            "the outputs archive ends with a long name"
        );
    }

    #[test]
    fn a_pax_header_is_refused() {
        let pax = b"30 path=bin/aaaaaaaaaaaaaaa.dll\n";
        for kind in [EntryType::XHeader, EntryType::XGlobalHeader, EntryType::GNULongLink, EntryType::GNUSparse] {
            let archive = raw(
                &[(b"PaxHeaders/x", kind, pax), (b"bin/a.dll", EntryType::Regular, b"a")],
                &[entry("bin/a.dll", b"a")],
            );
            assert_eq!(
                refusal_before_any_file("pax", &archive),
                "the outputs archive member 'PaxHeaders/x' is not a file",
                "{kind:?}"
            );
        }
    }

    #[test]
    fn an_archive_past_the_byte_limit_is_refused() {
        let data = [7_u8; 4096];
        let archive = raw(&[(b"a", EntryType::Regular, &data)], &[entry("a", &data)]);
        let destination = scratch("limit-member");
        let reason = unpack_alone(&archive[..], &destination, 2048)
            .expect_err("a member past the limit")
            .to_string();
        assert_eq!(
            reason, "the outputs archive member 'a' is 4096 bytes, more than the 1536 it may be",
            "the limit less the member's header"
        );
        assert_eq!(files_below(&destination), Vec::<String>::new(), "nothing of it is written");
        let reason = unpack_alone(&archive[..], &scratch("limit-total"), 5000)
            .expect_err("an archive past the limit")
            .to_string();
        assert!(
            reason.ends_with("the outputs archive holds more than 5000 bytes once decompressed"),
            "{reason}"
        );
        let whole = u64::try_from(zstd::decode_all(&archive[..]).expect("the archive decompresses").len()).expect("a short archive");
        assert!(
            unpack_alone(&archive[..], &scratch("limit-exact"), whole).is_ok(),
            "an archive of exactly the limit extracts"
        );
    }

    #[test]
    fn an_archive_with_too_many_members_is_refused() {
        let names: Vec<String> = (0..=MEMBER_LIMIT).map(|index| index.to_string()).collect();
        let members: Vec<Member<'_>> = names.iter().map(|name| (name.as_bytes(), EntryType::Regular, &b""[..])).collect();
        let error = read_members(&unlisted(&members)[..], LIMIT, MEMBER_LIMIT, &HeldBy::new(), |_, _, _| Ok(())).expect_err("too many files");
        assert_eq!(error.to_string(), format!("the outputs archive holds more than {MEMBER_LIMIT} files"));
    }

    #[test]
    fn a_listed_copy_of_an_earlier_member_comes_back_as_its_own_file() {
        let data = b"the same bytes";
        let archive = raw_items(
            &[(b"bin/a.dll", EntryType::Regular, data)],
            &[member("bin/a.dll", data), copy_of("bin/sub/b.dll", data)],
        );
        let destination = scratch("copy-of-member");

        let entries = unpack_alone(&archive[..], &destination, LIMIT).expect("the archive extracts");

        assert_eq!(entries, [entry("bin/a.dll", data), entry("bin/sub/b.dll", data)], "every listed path");
        assert_eq!(files_below(&destination), ["bin/a.dll", "bin/sub/b.dll"]);
        let copy = destination.join("bin").join("sub").join("b.dll");
        assert_eq!(fs::read(&copy).expect("the copy was made"), data);
        fs::write(&copy, b"changed").expect("the copy can be written");
        assert_eq!(
            fs::read(destination.join("bin").join("a.dll")).expect("the member is there"),
            data,
            "the copy is a file of its own"
        );
    }

    #[test]
    fn a_copy_whose_hash_no_member_or_manifest_holds_is_refused() {
        let data = b"the same bytes";
        let members: [Member<'_>; 1] = [(b"bin/a.dll", EntryType::Regular, data)];
        for (test, listed) in [
            ("copy-unknown", vec![member("bin/a.dll", data), copy_of("bin/b.dll", b"other bytes")]),
            ("copy-before-member", vec![copy_of("bin/b.dll", data), member("bin/a.dll", data)]),
        ] {
            let (reason, extracted) = refusal(test, &raw_items(&members, &listed));
            assert!(
                reason.starts_with("the outputs archive lists 'bin/b.dll' as a copy of "),
                "{test}: {reason}"
            );
            assert!(
                reason.ends_with("which no member before it and no file the PC holds has"),
                "{test}: {reason}"
            );
            assert_eq!(extracted, Vec::<String>::new(), "{test}");
        }
        let marked = unlisted(&[
            (b"bin/a.dll", EntryType::Regular, data),
            (
                LIST_NAME.as_bytes(),
                EntryType::Regular,
                list_json(&[member("bin/a.dll", data)]).replace("}]", ",\"copy\":false}]").as_bytes(),
            ),
        ]);
        assert_eq!(
            refusal("copy-false", &marked).0,
            "the outputs archive's list is not a list of paths, sizes and SHA-256s",
            "only a copy carries the key"
        );
    }

    #[test]
    fn copies_count_against_the_byte_limit_so_a_small_archive_cannot_expand_past_it() {
        let data = [7_u8; 4096];
        let archive = raw_items(
            &[(b"a", EntryType::Regular, &data)],
            &[member("a", &data), copy_of("b", &data), copy_of("c", &data)],
        );
        let whole = u64::try_from(zstd::decode_all(&archive[..]).expect("the archive decompresses").len()).expect("a short archive");
        let reason = unpack_alone(&archive[..], &scratch("copies-limit"), whole + 4096 + 100)
            .expect_err("the second copy passes the limit")
            .to_string();
        assert_eq!(reason, "the outputs archive copy 'c' is 4096 bytes, more than the 100 it may be");
        assert!(
            unpack_alone(&archive[..], &scratch("copies-limit-exact"), whole + 2 * 4096).is_ok(),
            "copies of exactly the limit extract"
        );
    }

    #[test]
    fn copies_count_against_the_member_limit() {
        let archive = raw_items(
            &[(b"a", EntryType::Regular, b"a")],
            &[member("a", b"a"), copy_of("b", b"a"), copy_of("c", b"a")],
        );
        let read = |files: usize| {
            read_members(&archive[..], LIMIT, files, &HeldBy::new(), |_, _, data| {
                io::copy(data, &mut io::sink()).map(|_| ()).map_err(UnpackError::from)
            })
        };
        let error = read(2).expect_err("three paths are past a limit of two");
        assert_eq!(error.to_string(), "the outputs archive holds more than 2 files");
        assert_eq!(read(3).expect("three paths fit a limit of three").len(), 3);
    }

    /// A `.tar.zst` stream of the one file `a` and its list, compressed with long-distance matching over a window of
    /// `2^window_log` bytes.
    fn windowed(window_log: u32) -> Vec<u8> {
        let tar = zstd::decode_all(&raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")])[..]).expect("the tar decompresses");
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("an encoder");
        encoder.long_distance_matching(true).expect("long-distance matching is set");
        encoder.window_log(window_log).expect("the window is set");
        encoder.write_all(&tar).expect("the tar compresses");
        encoder.finish().expect("the frame ends")
    }

    #[test]
    fn a_window_27_archive_unpacks_and_a_window_28_one_is_refused() {
        let destination = scratch("window-27");
        let entries = unpack_alone(&windowed(27)[..], &destination, LIMIT).expect("a 128 MiB window is taken");
        assert_eq!(entries, [entry("a", b"a")]);
        let (error, extracted) = unpack_error("window-28", &windowed(28));
        assert!(!error.members(), "a window past the reader's is a fault of the stream: {error}");
        assert!(error.to_string().contains("memory"), "{error}");
        assert_eq!(extracted, Vec::<String>::new());
    }

    /// What a walk of every file below `root` finds.
    fn walked(root: &Path) -> Vec<Found> {
        walk(root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk")
    }

    /// Each of `found`, hashed before the pack as the box hashes a file that may be a copy.
    fn pre_hashed(found: &[Found]) -> Vec<Chosen<'_>> {
        found
            .iter()
            .map(|file| {
                let (sha256, _) = sha256_of(fs::File::open(&file.full).expect("the file opens")).expect("the file hashes");
                Chosen { file, sha256: Some(sha256) }
            })
            .collect()
    }

    /// A PC manifest holding each of `files`, a path and its bytes, at a placeholder last write.
    fn held_at(files: &[(&str, &[u8])]) -> Manifest {
        let mut manifest = Manifest::default();
        for (path, data) in files {
            let (sha256, size) = sha256_of(*data).expect("bytes hash");
            let stamp = Stamp { size, modified: 1 };
            manifest.insert((*path).to_owned(), Held { stamp, sha256 });
        }
        manifest
    }

    /// `unpack` of `source` into `destination`, each copy no member holds made from a file below `root` that
    /// `manifest` holds with its bytes.
    fn unpack_from_pc(source: &[u8], destination: &Path, root: &Path, manifest: &Manifest) -> Result<Vec<Entry>, UnpackError> {
        unpack(
            source,
            destination,
            LIMIT,
            &PcFiles {
                root,
                manifest,
                go_on: &|| true,
            },
        )
    }

    /// Writes `data` to the file `name` in the folder `folder`, last written `seconds` after 1970.
    fn written_at(folder: &Path, name: &str, data: &[u8], seconds: u64) {
        let full = folder.join(name);
        fs::write(&full, data).expect("the file can be written");
        fs::File::options()
            .write(true)
            .open(&full)
            .and_then(|file| file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .expect("the last write can be set");
    }

    #[test]
    fn a_copy_candidate_that_changed_after_its_hash_is_packed_as_a_member() {
        let source = scratch("candidate-changed");
        fs::create_dir_all(&source).expect("the folder can be made");
        written_at(&source, "a.dll", b"same", 1_600_000_000);
        written_at(&source, "b.dll", b"same", 1_600_000_000);
        let found = walked(&source);
        let chosen = pre_hashed(&found);
        written_at(&source, "b.dll", b"changed", 1_600_000_100);
        let mut archive = Vec::new();

        let packed = pack(&mut archive, &source, &chosen, &HeldBy::new()).expect("the files pack");

        assert_eq!(packed.entries, [entry("a.dll", b"same"), entry("b.dll", b"changed")]);
        let destination = scratch("candidate-changed-out");
        unpack_alone(&archive[..], &destination, LIMIT).expect("the archive extracts");
        assert_eq!(
            fs::read(destination.join("b.dll")).expect("b.dll came back"),
            b"changed",
            "b.dll is a member holding the bytes the pack read"
        );
    }

    #[test]
    fn a_member_that_changed_after_its_pre_hash_is_packed_with_its_new_content() {
        let source = scratch("member-changed");
        fs::create_dir_all(&source).expect("the folder can be made");
        written_at(&source, "a.dll", b"before", 1_600_000_000);
        let found = walked(&source);
        let chosen = pre_hashed(&found);
        written_at(&source, "a.dll", b"after", 1_600_000_100);

        let packed = pack(io::sink(), &source, &chosen, &HeldBy::new()).expect("a file changed after its pre-hash packs");

        assert_eq!(packed.entries, [entry("a.dll", b"after")]);
    }

    #[test]
    fn a_copy_keeps_the_box_file_s_last_write() {
        let source = scratch("copy-last-write");
        fs::create_dir_all(&source).expect("the folder can be made");
        written_at(&source, "a.dll", b"same", 1_600_000_000);
        written_at(&source, "b.dll", b"same", 1_700_000_000);
        let found = walked(&source);
        let mut archive = Vec::new();
        pack(&mut archive, &source, &pre_hashed(&found), &HeldBy::new()).expect("the files pack");
        let tar = zstd::decode_all(&archive[..]).expect("the archive decompresses");
        assert!(tar.windows(11).any(|bytes| bytes == br#""copy":true"#), "b.dll is listed as a copy");
        let destination = scratch("copy-last-write-out");

        unpack_alone(&archive[..], &destination, LIMIT).expect("the archive extracts");

        let modified = |name: &str| {
            fs::metadata(destination.join(name))
                .and_then(|metadata| metadata.modified())
                .expect("its last write")
        };
        assert_eq!(modified("a.dll"), UNIX_EPOCH + Duration::from_secs(1_600_000_000));
        assert_eq!(
            modified("b.dll"),
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            "the copy has its own last write, not its member's"
        );
    }

    #[test]
    fn a_list_naming_a_path_twice_in_other_case_is_refused() {
        let data = b"the same bytes";
        let members: [Member<'_>; 1] = [(b"bin/a.dll", EntryType::Regular, data)];
        for (test, listed, twice) in [
            ("twice-member", vec![member("bin/a.dll", data), copy_of("BIN/A.dll", data)], "BIN/A.dll"),
            (
                "twice-copy",
                vec![member("bin/a.dll", data), copy_of("bin/b.dll", data), copy_of("Bin/B.dll", data)],
                "Bin/B.dll",
            ),
        ] {
            let (error, extracted) = unpack_error(test, &raw_items(&members, &listed));
            assert!(error.members(), "{test}: a refetch would be refused the same way: {error}");
            assert_eq!(error.to_string(), format!("the outputs archive lists '{twice}' twice"), "{test}");
            assert_eq!(extracted, Vec::<String>::new(), "{test}");
        }
    }

    #[test]
    fn a_list_path_past_the_long_name_limit_is_refused() {
        let data = b"the same bytes";
        let long = format!("bin/{}", "a".repeat(usize::try_from(LONG_NAME_LIMIT).expect("a small limit")));
        let archive = raw_items(
            &[(b"bin/a.dll", EntryType::Regular, data)],
            &[member("bin/a.dll", data), copy_of(&long, data)],
        );

        let (error, _) = unpack_error("long-list-path", &archive);

        assert!(error.members(), "{error}");
        assert_eq!(
            error.to_string(),
            format!(
                "the outputs archive lists a path of more than {LONG_NAME_LIMIT} bytes: '{}\u{2026}'",
                &long[..200]
            )
        );
    }

    #[test]
    fn a_copy_named_as_the_list_or_escaping_the_stage_is_refused() {
        let data = b"the same bytes";
        let members: [Member<'_>; 1] = [(b"bin/a.dll", EntryType::Regular, data)];
        let named = raw_items(&members, &[member("bin/a.dll", data), copy_of(".GATE-outputs.json", data)]);
        let (error, extracted) = unpack_error("copy-named-as-list", &named);
        assert!(error.members(), "{error}");
        assert_eq!(
            error.to_string(),
            "the outputs archive lists a copy named as its own list, .GATE-outputs.json"
        );
        assert_eq!(extracted, Vec::<String>::new());
        for (test, path) in [("copy-up", "../a.dll"), ("copy-up-deep", "bin/../../a.dll")] {
            let escaping = raw_items(&members, &[member("bin/a.dll", data), copy_of(path, data)]);
            let (error, extracted) = unpack_error(test, &escaping);
            assert!(error.members(), "{test}: {error}");
            assert_eq!(
                error.to_string(),
                format!("the outputs archive member '{path}' is not a relative path inside the destination"),
                "{test}"
            );
            assert_eq!(extracted, Vec::<String>::new(), "{test}");
        }
    }

    #[test]
    fn a_pc_source_outside_the_root_through_a_junction_is_passed_over() {
        let data: &[u8] = b"held";
        let root = scratch("pc-junction");
        let outside = scratch("pc-junction-outside");
        fs::create_dir_all(root.join("lib")).expect("the folder can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        fs::write(outside.join("held.dll"), data).expect("the outside file can be written");
        junction(&root.join("bin"), &outside);
        fs::write(root.join("lib").join("held.dll"), data).expect("the file can be written");
        let manifest = held_at(&[("bin/held.dll", data), ("lib/held.dll", data)]);
        let archive = raw_items(&[], &[copy_of("new.dll", data)]);
        let destination = scratch("pc-junction-taken");
        unpack_from_pc(&archive, &destination, &root, &manifest).expect("the file inside the root is a source");
        assert_eq!(fs::read(destination.join("new.dll")).expect("the copy was made"), data);
        fs::write(root.join("lib").join("held.dll"), b"gone").expect("the file can be changed");

        let error = unpack_from_pc(&archive, &scratch("pc-junction-refused"), &root, &manifest).expect_err("no source inside holds it");

        assert!(
            error
                .to_string()
                .ends_with(": tried bin/held.dll (outside the root), lib/held.dll (other content)"),
            "{error}"
        );
    }

    #[test]
    fn the_refusal_names_each_pc_source_tried_and_why() {
        let data: &[u8] = b"wanted";
        let root = scratch("pc-tried");
        let outside = scratch("pc-tried-outside");
        for folder in ["b", "c", "d", "f", "g"] {
            fs::create_dir_all(root.join(folder)).expect("the folder can be made");
        }
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        std::os::windows::fs::symlink_file(root.join("c").join("locked.dll"), root.join("b").join("link.dll"))
            .expect("this machine lets a test make a symbolic link");
        fs::write(root.join("c").join("locked.dll"), data).expect("the file can be written");
        for other in ["d/other.dll", "f/other.dll", "g/other.dll"] {
            fs::write(root.join(other), b"other").expect("the file can be written");
        }
        fs::write(outside.join("held.dll"), data).expect("the outside file can be written");
        junction(&root.join("e"), &outside);
        let paths = [
            "a/missing.dll",
            "b/link.dll",
            "c/locked.dll",
            "d/other.dll",
            "e/held.dll",
            "f/other.dll",
            "g/other.dll",
        ];
        let manifest = held_at(&paths.map(|path| (path, data)));
        let archive = raw_items(&[], &[copy_of("new.dll", data)]);
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("c").join("locked.dll"))
            .expect("the file can be held");

        let error = unpack_from_pc(&archive, &scratch("pc-tried-out"), &root, &manifest).expect_err("no source holds it");

        drop(locked);
        let (sha256, _) = sha256_of(data).expect("bytes hash");
        let in_use = io::Error::from_raw_os_error(32);
        assert!(!error.members(), "a refetch may clear it: {error}");
        assert_eq!(
            error.to_string(),
            format!(
                "no PC file still holds the content of new.dll, SHA-256 {sha256}: tried a/missing.dll (missing), b/link.dll (not a plain \
                 file), c/locked.dll (unreadable: {in_use}), d/other.dll (other content), e/held.dll (outside the root) and 2 more"
            )
        );
    }

    #[test]
    fn a_small_archive_uses_a_small_window() {
        let source = scratch("small-window");
        fs::create_dir_all(&source).expect("the folder can be made");
        fs::write(source.join("a.bin"), vec![7_u8; 1 << 20]).expect("the file can be written");
        let found = walked(&source);
        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        let mut archive = Vec::new();
        pack(&mut archive, &source, &chosen, &HeldBy::new()).expect("the file packs");

        let mut decoder = zstd::stream::read::Decoder::new(&archive[..]).expect("a decoder");
        decoder.window_log_max(20).expect("the window limit is set");
        let mut tar = Vec::new();

        decoder.read_to_end(&mut tar).expect("a 1 MiB archive decodes within a 1 MiB window");
    }

    #[test]
    fn a_stop_during_a_large_copy_ends_it_before_it_finishes() {
        let data = vec![7_u8; 1 << 20];
        let archive = raw_items(&[(b"a", EntryType::Regular, &data)], &[member("a", &data), copy_of("b", &data)]);
        let destination = scratch("stop-in-copy");
        let asked = Cell::new(0_u32);
        let copied = Cell::new(None);
        let manifest = Manifest::default();
        let go_on = || {
            asked.set(asked.get() + 1);
            if asked.get() < 2 {
                return true;
            }
            copied.set(fs::metadata(destination.join("b")).ok().map(|metadata| metadata.len()));
            false
        };
        let pc = PcFiles {
            root: Path::new(r"C:\no-pc-files"),
            manifest: &manifest,
            go_on: &go_on,
        };

        let error = unpack(&archive[..], &destination, LIMIT, &pc).expect_err("the stop ends the copy");

        assert!(error.is_fault(io::ErrorKind::Interrupted), "{error}");
        assert_eq!(asked.get(), 2, "nothing more is asked once it said no");
        let copied = copied.get().expect("the copy was being written when it said no");
        assert!(copied < data.len() as u64, "the copy stopped at {copied} bytes");
        assert!(!destination.exists(), "the destination is removed");
    }

    /// A `.tar.zst` stream of the one file `a`, its header's last write the 12 bytes `mtime`, and its list.
    fn stamped(mtime: [u8; 12]) -> Vec<u8> {
        let mut header = Header::new_gnu();
        let gnu = header.as_gnu_mut().expect("a GNU header");
        gnu.name[..1].copy_from_slice(b"a");
        gnu.mtime = mtime;
        header.set_entry_type(EntryType::Regular);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        let mut tar = tar::Builder::new(Vec::new());
        tar.append(&header, &b"a"[..]).expect("the member can be appended");
        let list = list_json(&[member("a", b"a")]);
        let mut list_header = Header::new_gnu();
        list_header.set_size(list.len() as u64);
        list_header.set_mode(0o644);
        tar.append_data(&mut list_header, LIST_NAME, list.as_bytes())
            .expect("the list can be appended");
        zstd::encode_all(&tar.into_inner().expect("the tar ends")[..], 3).expect("the tar compresses")
    }

    /// The 12 bytes of a header's last write `seconds` after 1970, in octal.
    fn octal(seconds: u64) -> [u8; 12] {
        let mut mtime = [0; 12];
        mtime[..11].copy_from_slice(format!("{seconds:011o}").as_bytes());
        mtime
    }

    #[test]
    fn a_base256_mtime_is_refused_without_a_panic() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("the clock is past 1970").as_secs();
        let mut far = [0; 12];
        far[0] = 0x80;
        far[4..].copy_from_slice(&u64::MAX.to_be_bytes());
        for (what, mtime) in [
            ("-1 in base-256", [0xff; 12]),
            ("the most base-256 holds", far),
            ("two days ahead", octal(now + 2 * 86_400)),
        ] {
            let reason = refusal_before_any_file("mtime", &stamped(mtime));
            assert!(reason.starts_with("the outputs archive member 'a' has a last write"), "{what}: {reason}");
        }
        assert!(
            unpack_alone(&stamped(octal(now))[..], &scratch("mtime-now"), LIMIT).is_ok(),
            "a last write of now"
        );
    }

    #[test]
    fn the_sent_limit_is_zstd_s_worst_case_for_the_limit_and_saturates() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(sent_limit(50 * GIB).get(), 50 * GIB + (50 * GIB) / 64 + 1024 * 1024, "the box's budget");
        assert_eq!(
            sent_limit(OUTPUTS_BUDGET).get(),
            50 * GIB + (50 * GIB) / 64 + 1024 * 1024,
            "the shared constant"
        );
        assert_eq!(sent_limit(0).get(), 1024 * 1024, "the slack alone");
        assert_eq!(sent_limit(u64::MAX).get(), u64::MAX, "it saturates rather than wrapping");
        assert_eq!(sent_limit(u64::MAX - 1024).get(), u64::MAX, "past what the slack can be added to");
    }

    #[test]
    fn unpack_refuses_an_existing_destination() {
        let destination = scratch("existing");
        fs::create_dir_all(&destination).expect("the folder can be made");
        let archive = raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]);
        let error = unpack_alone(&archive[..], &destination, LIMIT).expect_err("an existing destination is refused");
        assert!(error.is_fault(io::ErrorKind::AlreadyExists), "{error}");
        assert!(destination.exists(), "a folder unpack did not make is not removed");
        assert_eq!(files_below(&destination), Vec::<String>::new());
    }

    /// A fresh scratch folder for `test`, made.
    fn made(test: &str) -> PathBuf {
        let folder = scratch(test);
        fs::create_dir_all(&folder).expect("the folder can be made");
        folder
    }

    #[test]
    fn an_archive_creating_too_many_folders_is_refused() {
        let archive = raw(
            &[(b"a/b/x", EntryType::Regular, b"x"), (b"a/c/y", EntryType::Regular, b"y")],
            &[entry("a/b/x", b"x"), entry("a/c/y", b"y")],
        );
        let destination = made("folders");
        let reason = extract_alone(&archive[..], &destination, LIMIT, 2)
            .expect_err("a third folder is refused")
            .to_string();
        assert_eq!(reason, "the outputs archive makes more than 2 folders");
        assert_eq!(files_below(&destination), ["a/b/x"]);
        assert!(!destination.join("a").join("c").exists(), "the folder past the cap is not made");
        assert!(
            extract_alone(&archive[..], &made("folders-fit"), LIMIT, 3).is_ok(),
            "three folders fit a cap of three"
        );
    }

    #[test]
    fn a_member_path_deeper_than_64_parts_is_refused() {
        let deep = |parts: usize| vec!["a"; parts].join("/");
        assert_taken_names(&[&deep(64)]);
        assert_refused_names(&[&deep(65)]);
    }

    #[test]
    fn a_skippable_frame_past_the_limit_is_refused() {
        let limit = 1024 * 1024;
        let skipped = 3 * 1024 * 1024;
        let mut archive = Vec::new();
        archive.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
        archive.extend_from_slice(&u32::try_from(skipped).expect("a u32 size").to_le_bytes());
        archive.resize(archive.len() + skipped, 0);
        archive.extend(raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]));
        let error = read_members(&archive[..], limit, MEMBER_LIMIT, &HeldBy::new(), |_, _, _| Ok(())).expect_err("the skippable frame is refused");
        let most = limit + limit / 64 + 1024 * 1024;
        let reason = error.to_string();
        assert!(
            reason.contains(&format!("the outputs archive holds more than {most} bytes as sent")),
            "{reason}"
        );
    }

    #[test]
    fn a_member_larger_than_the_budget_left_is_refused_before_writing() {
        let data = [7_u8; 1000];
        let archive = raw(
            &[(b"a", EntryType::Regular, &data), (b"b", EntryType::Regular, &data)],
            &[entry("a", &data), entry("b", &data)],
        );
        let destination = made("budget");
        // `a` takes 512 bytes of header and 1024 of data and padding; `b`'s header leaves 2500 - 2048 = 452.
        let reason = extract_alone(&archive[..], &destination, 2500, MEMBER_LIMIT)
            .expect_err("b is refused")
            .to_string();
        assert_eq!(reason, "the outputs archive member 'b' is 1000 bytes, more than the 452 it may be");
        assert!(destination.join("a").exists(), "a fits");
        assert!(!destination.join("b").exists(), "nothing of b is written");
    }

    #[test]
    fn pack_refuses_a_name_the_reader_would_refuse() {
        let deep = vec!["a"; 65].join("/");
        for name in [
            "bin/x~1.log",
            "bin/a\u{202e}b.dll",
            "bin/x.dll.",
            ".git/config",
            "bin/COM0",
            deep.as_str(),
        ] {
            let mut packer = Packer::new(io::sink(), WINDOW_LOG_MAX).expect("a packer");
            let error = packer.add(name, 1, 0, &b"x"[..]).expect_err(name);
            assert_eq!(
                error.to_string(),
                format!("the output '{}' has a name the outputs archive's reader refuses", escaped(name))
            );
        }
    }

    #[test]
    fn unpack_refuses_a_junction_in_its_path() {
        let destination = scratch("junction");
        let outside = scratch("junction-outside");
        fs::create_dir_all(&destination).expect("the destination can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        junction(&destination.join("bin"), &outside);
        let archive = raw(&[(b"bin/a.dll", EntryType::Regular, b"a")], &[entry("bin/a.dll", b"a")]);
        let reason = extract_alone(&archive[..], &destination, LIMIT, MEMBER_LIMIT)
            .expect_err("a junction is refused")
            .to_string();
        assert!(reason.ends_with("is a link or a junction, not a folder"), "{reason}");
        assert_eq!(
            fs::read_dir(&outside).expect("the outside folder").count(),
            0,
            "nothing is written through it"
        );
    }

    /// A reader of the file `inner` that, at its first read, appends a byte to the file at `path`.
    struct Growing<'a> {
        inner: &'a fs::File,
        path: &'a Path,
        grown: bool,
    }

    impl Read for Growing<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.grown {
                self.grown = true;
                fs::OpenOptions::new().append(true).open(self.path)?.write_all(b"d")?;
            }
            self.inner.read(buffer)
        }
    }

    /// A file at `name` in a fresh scratch folder for `test`, holding `data`, and what a walk finds of it.
    fn one_file(test: &str, name: &str, data: &[u8]) -> Found {
        let folder = scratch(test);
        fs::create_dir_all(&folder).expect("the folder can be made");
        let full = folder.join(name);
        fs::write(&full, data).expect("the file can be written");
        Found {
            path: name.to_owned(),
            full,
            stamp: Stamp { size: 0, modified: 0 },
        }
    }

    #[test]
    fn a_file_that_grows_while_packed_fails_the_pack() {
        let found = one_file("grows", "a.dll", b"abc");
        let opened = fs::File::open(&found.full).expect("the file opens");
        let mut packer = Packer::new(io::sink(), WINDOW_LOG_MAX).expect("a packer");
        let growing = Growing {
            inner: &opened,
            path: &found.full,
            grown: false,
        };
        let error = pack_file(&mut packer, &found, &opened, growing).expect_err("a file that grew is refused");
        assert_eq!(
            error.to_string(),
            "a.dll changed while it was packed: its size or last write differs from when it was opened"
        );
    }

    #[test]
    fn a_file_last_written_before_1970_fails_the_pack() {
        let found = one_file("before-1970", "a.dll", b"a");
        fs::File::options()
            .write(true)
            .open(&found.full)
            .and_then(|file| file.set_modified(UNIX_EPOCH - Duration::from_hours(24)))
            .expect("the last write can be set");
        let root = found.full.parent().expect("the file has a folder");
        let error = pack(io::sink(), root, &[Chosen::from(&found)], &HeldBy::new()).expect_err("a file from before 1970 is refused");
        assert!(error.to_string().ends_with("a.dll was last written before 1970"), "{error}");
    }

    #[test]
    fn pack_refuses_a_file_that_is_a_link() {
        let mut found = one_file("link", "secret", b"s");
        let link = found.full.with_file_name("a.dll");
        std::os::windows::fs::symlink_file(&found.full, &link).expect("this machine lets a test make a symbolic link");
        found.full = link;
        let root = found.full.parent().expect("the link has a folder").to_owned();
        let error = pack(io::sink(), &root, &[Chosen::from(&found)], &HeldBy::new()).expect_err("a link is refused");
        assert!(error.to_string().ends_with("is a link or a junction, not a file"), "{error}");
    }

    #[test]
    fn pack_refuses_a_file_reached_through_a_junction_swapped_in_after_the_walk() {
        let root = scratch("swapped");
        let outside = scratch("swapped-outside");
        fs::create_dir_all(root.join("bin")).expect("the folder can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        fs::write(root.join("bin").join("a.dll"), b"inside").expect("the file can be written");
        fs::write(outside.join("a.dll"), b"outside").expect("the outside file can be written");
        let found = walk(&root, &Globs::new(&["**".to_owned()]).expect("a glob")).expect("the walk");
        fs::rename(root.join("bin"), root.join("moved")).expect("the walked folder can be moved away");
        junction(&root.join("bin"), &outside);

        let chosen: Vec<Chosen<'_>> = found.iter().map(Chosen::from).collect();
        let error = pack(io::sink(), &root, &chosen, &HeldBy::new()).expect_err("a file outside the work folder is refused");

        assert!(error.to_string().ends_with("bin/a.dll resolves outside the work folder"), "{error}");
    }

    /// Pseudo-random bytes, so zstd cannot shrink them, `left` more of them, counting each one given in `given`.
    struct Generated {
        left: u64,
        state: u64,
        given: Rc<Cell<u64>>,
    }

    impl Generated {
        fn new(size: u64, given: Rc<Cell<u64>>) -> Generated {
            Generated {
                left: size,
                state: 0x9E37_79B9_7F4A_7C15,
                given,
            }
        }
    }

    impl Read for Generated {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = usize::try_from(self.left).map_or(buffer.len(), |left| left.min(buffer.len()));
            for chunk in buffer[..count].chunks_mut(8) {
                self.state ^= self.state << 13;
                self.state ^= self.state >> 7;
                self.state ^= self.state << 17;
                chunk.copy_from_slice(&self.state.to_le_bytes()[..chunk.len()]);
            }
            self.left -= count as u64;
            self.given.set(self.given.get() + count as u64);
            Ok(count)
        }
    }

    /// A reader counting the bytes read through it in `taken`.
    struct Taking<R> {
        inner: R,
        taken: Rc<Cell<u64>>,
    }

    impl<R: Read> Read for Taking<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.taken.set(self.taken.get() + read as u64);
            Ok(read)
        }
    }

    /// A sink noting, at each write, how many bytes its source gave since the last write: the widest such gap is the
    /// most the stream between them held back.
    struct Watching {
        given: Rc<Cell<u64>>,
        at_last_write: u64,
        widest: Rc<Cell<u64>>,
    }

    impl Write for Watching {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let now = self.given.get();
            self.widest.set(self.widest.get().max(now - self.at_last_write));
            self.at_last_write = now;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The size streamed through the flat-memory cases.
    const LARGE: u64 = 256 * 1024 * 1024;
    /// The most a stream may hold back between its source and its sink: a sixteenth of [`LARGE`]. A writer or reader
    /// that held a whole file would hold back all of it.
    const HELD_BACK: u64 = 16 * 1024 * 1024;

    #[test]
    fn the_writer_holds_back_a_bounded_part_of_a_large_file_whatever_its_size() {
        let given = Rc::new(Cell::new(0));
        let widest = Rc::new(Cell::new(0));
        let sink = Watching {
            given: Rc::clone(&given),
            at_last_write: 0,
            widest: Rc::clone(&widest),
        };
        let mut packer = Packer::new(sink, WINDOW_LOG_MAX).expect("a packer");
        packer
            .add("bin/large.bin", LARGE, 0, Generated::new(LARGE, Rc::clone(&given)))
            .expect("the file packs");
        let packed = packer.finish().expect("the archive ends");
        assert_eq!(given.get(), LARGE);
        assert_eq!(packed.entries.len(), 1);
        assert!(packed.compressed > LARGE / 2, "random bytes do not shrink: {}", packed.compressed);
        assert!(widest.get() <= HELD_BACK, "the writer held back {} bytes", widest.get());
    }

    #[test]
    fn the_reader_holds_back_a_bounded_part_of_a_large_member_whatever_its_size() {
        let (pipe_reader, pipe_writer) = io::pipe().expect("a pipe");
        let writer = std::thread::spawn(move || {
            let mut packer = Packer::new(pipe_writer, WINDOW_LOG_MAX)?;
            packer.add("bin/large.bin", LARGE, 0, Generated::new(LARGE, Rc::new(Cell::new(0))))?;
            packer.finish()
        });
        let taken = Rc::new(Cell::new(0));
        let widest = Rc::new(Cell::new(0));
        let source = Taking {
            inner: pipe_reader,
            taken: Rc::clone(&taken),
        };
        let read = read_members(source, LIMIT, MEMBER_LIMIT, &HeldBy::new(), |_, _, data| {
            let mut sink = Watching {
                given: Rc::clone(&taken),
                at_last_write: taken.get(),
                widest: Rc::clone(&widest),
            };
            io::copy(data, &mut sink).map(|_| ()).map_err(UnpackError::from)
        });
        let packed = writer.join().expect("the writer ends").expect("the archive packs");
        let read: Vec<Entry> = read.expect("the archive extracts").into_iter().map(|item| item.entry).collect();
        assert_eq!(read, packed.entries);
        assert!(taken.get() > LARGE / 2, "random bytes do not shrink: {}", taken.get());
        assert!(widest.get() <= HELD_BACK, "the reader held back {} bytes", widest.get());
    }
}

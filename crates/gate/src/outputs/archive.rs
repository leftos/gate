//! The archive a run's outputs travel in: a tar stream compressed with zstd, named `.tar.zst`. Each file is read once,
//! hashed with SHA-256 as it is packed, and the list of what was packed (path, size, SHA-256) follows the files as the
//! last member, [`LIST_NAME`]. Reading checks each member's path before anything is written, extracts it while hashing
//! it, and checks the members against the list at the end. Writer and reader stream: what either holds in memory does
//! not grow with a file's size.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tar::{EntryType, Header};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
};

use crate::boxside::payload::{escaped, is_reordering_mark};
use crate::outputs::hash::{Hashing, SHA256_DIGITS, is_lower_hex};
use crate::outputs::walk::{Found, in_context};

/// The extension of an outputs archive's name.
pub(crate) const SUFFIX: &str = ".tar.zst";
/// The name of the archive's last member, the list of the files before it.
pub(crate) const LIST_NAME: &str = ".gate-outputs.json";
/// The zstd level the archive is compressed at: zstd's default, which keeps pace with a gigabit link.
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
/// The most `/`-separated parts a member's path may have, its file's name included.
const PATH_DEPTH_LIMIT: usize = 64;
/// What the stream as sent may hold beyond zstd's worst-case expansion of the limit's bytes: frame and block headers.
const SENT_SLACK: u64 = 1024 * 1024;
/// The largest zstd window the reader accepts, as a power of two: 8 MiB, four times what [`LEVEL`] uses.
const WINDOW_LOG_MAX: u32 = 23;
/// How far past the reader's clock a member's last write may be: a day, for clocks that disagree.
const CLOCK_SLACK: Duration = Duration::from_hours(24);

/// One packed file: its `/`-separated path relative to the root it was packed from, its size and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// What a pack wrote: its entries in order, and the archive's size, compressed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Packed {
    pub entries: Vec<Entry>,
    pub compressed: u64,
}

/// Packs `files` into `sink` as a `.tar.zst` stream, in their order, each read once: hashed as it is packed, its header
/// holding the size and last write it had when it was opened. Returns the entries and the compressed size; else why
/// the pack failed, the file's path named. A link or a junction, a file last written before 1970, and a file whose
/// size or last write changes while it is packed fail the pack, and so does a file found below `root` whose final path,
/// as the system resolves the open file, is not inside `root`'s: one reached through a folder swapped for a junction
/// after the walk.
pub(crate) fn pack(sink: impl Write, root: &Path, files: &[&Found]) -> io::Result<Packed> {
    // Resolved once, as each file's final path is: `canonicalize` is `GetFinalPathNameByHandleW` on the opened folder.
    let root = fs::canonicalize(root).map_err(|error| in_context(root, &error))?;
    let mut packer = Packer::new(sink)?;
    for file in files {
        let opened = open_plain(&file.full).map_err(|error| in_context(&file.full, &error))?;
        check_inside(&opened, &root, file).map_err(|error| in_context(&file.full, &error))?;
        pack_file(&mut packer, file, &opened, &opened).map_err(|error| in_context(&file.full, &error))?;
    }
    packer.finish()
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
fn final_path(opened: &File) -> io::Result<PathBuf> {
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
    let modified = written
        .duration_since(UNIX_EPOCH)
        .map_err(|_| refused(format!("{} was last written before 1970", file.path)))?
        .as_secs();
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
/// Nothing is written outside the destination, and on a refusal `unpack` removes the destination it made, and all it
/// extracted there; when that removal fails too, the refusal says so.
pub(crate) fn unpack(source: impl Read, destination: &Path, limit: u64) -> io::Result<Vec<Entry>> {
    fs::create_dir(destination).map_err(|error| in_context(destination, &error))?;
    extract_into(source, destination, limit, MEMBER_LIMIT).map_err(|error| match fs::remove_dir_all(destination) {
        Ok(()) => error,
        Err(removal) => io::Error::new(
            error.kind(),
            format!("{error}; and {} could not be removed: {removal}", destination.display()),
        ),
    })
}

/// Extracts `source` into the folder `destination`, which exists, as [`unpack`] does, making at most `folder_limit`
/// folders.
fn extract_into(source: impl Read, destination: &Path, limit: u64, folder_limit: usize) -> io::Result<Vec<Entry>> {
    let root = std::path::absolute(destination).map_err(|error| in_context(destination, &error))?;
    check_folder(&root)?;
    let mut folders = Folders {
        made: 0,
        limit: folder_limit,
    };
    read_members(source, limit, |path, modified, data| {
        let target = target_below(&root, path, &mut folders)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| in_context(&target, &error))?;
        io::copy(data, &mut file).map_err(|error| in_context(&target, &error))?;
        file.set_modified(modified).map_err(|error| in_context(&target, &error))
    })
}

/// The full path of the member `path` below the folder `root`, making each folder between them that does not exist;
/// else the refusal, when the full path leaves `root`, a folder on the way is a link, a junction or not a folder, or
/// `folders` may make no more.
fn target_below(root: &Path, path: &str, folders: &mut Folders) -> io::Result<PathBuf> {
    let target = path.split('/').fold(root.to_path_buf(), |target, part| target.join(part));
    let full = std::path::absolute(&target).map_err(|error| in_context(&target, &error))?;
    if !full.starts_with(root) {
        return Err(refused(format!(
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
    fn make(&mut self, folder: &Path) -> io::Result<()> {
        match fs::symlink_metadata(folder) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Ok(()),
        }
        if self.made == self.limit {
            return Err(refused(format!("the outputs archive makes more than {} folders", self.limit)));
        }
        match fs::create_dir(folder) {
            Ok(()) => {
                self.made += 1;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(in_context(folder, &error)),
        }
    }
}

/// Whether `folder` is a folder itself, not a link or a junction; else the refusal.
fn check_folder(folder: &Path) -> io::Result<()> {
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

/// An archive being written: the tar stream over the zstd encoder over the sink, and the entries packed so far.
struct Packer<W: Write> {
    tar: tar::Builder<zstd::stream::write::Encoder<'static, Counting<W>>>,
    entries: Vec<Entry>,
}

impl<W: Write> Packer<W> {
    fn new(sink: W) -> io::Result<Packer<W>> {
        let encoder = zstd::stream::write::Encoder::new(Counting { inner: sink, count: 0 }, LEVEL)?;
        Ok(Packer {
            tar: tar::Builder::new(encoder),
            entries: Vec::new(),
        })
    }

    /// Packs the `size` bytes of `data` as the file `path`, last written `modified` (seconds since 1970), hashing them
    /// as they pass. Data that ends before `size` bytes fails the pack; bytes past it are left unread.
    fn add(&mut self, path: &str, size: u64, modified: u64, data: impl Read) -> io::Result<()> {
        if path.eq_ignore_ascii_case(LIST_NAME) {
            return Err(refused(format!("the output {path} has the name of the archive's own list")));
        }
        if !is_plain_path(path) {
            return Err(refused(format!(
                "the output '{}' has a name the outputs archive's reader refuses",
                escaped(path)
            )));
        }
        let mut hashing = Hashing::new(data.take(size));
        self.tar.append_data(&mut file_header(size, modified), path, &mut hashing)?;
        let (sha256, count) = hashing.finish();
        if count != size {
            return Err(refused(format!(
                "{path} changed while it was packed: {count} of its {size} bytes were read"
            )));
        }
        let path = path.to_owned();
        self.entries.push(Entry { path, size, sha256 });
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
            entries: self.entries,
            compressed: sink.count,
        })
    }
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
/// write and data to `member` and hashing the data as `member` reads it, then checks the members against the list;
/// returns the entries, else why the archive was refused.
fn read_members(source: impl Read, limit: u64, mut member: impl FnMut(&str, SystemTime, &mut dyn Read) -> io::Result<()>) -> io::Result<Vec<Entry>> {
    // zstd's worst case for `limit` bytes, so a skippable frame or empty blocks cannot be read without end either.
    let sent = limit.saturating_add(limit / 64).saturating_add(SENT_SLACK);
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
    };
    // Raw, so that tar-rs reads no GNU long name or PAX header of its own, each of which it would read whole.
    for entry in archive.entries()?.raw(true) {
        members.take(&mut entry?, &mut member)?;
    }
    if members.long_name.is_some() {
        return Err(refused("the outputs archive ends with a long name".to_owned()));
    }
    // To the end of the zstd frame, so a stream cut short after the tar's end is refused too.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    let listed = members
        .listed
        .ok_or_else(|| refused("the outputs archive holds no list of its files".to_owned()))?;
    agree(&members.seen, &listed)?;
    Ok(members.seen)
}

/// What the reader has taken of an archive so far: the files, the list once it is read, the long name the next member
/// takes, the clock each member is held to, and the decompressed bytes left of the archive's limit, which the reader
/// under the tar counts down.
struct Members {
    seen: Vec<Entry>,
    listed: Option<Vec<Entry>>,
    long_name: Option<Vec<u8>>,
    now: SystemTime,
    left: Rc<Cell<u64>>,
}

impl Members {
    /// Takes the raw tar entry `entry`: a GNU long name for the next member, the list, or a file handed to `member`;
    /// else the refusal.
    fn take(
        &mut self,
        entry: &mut tar::Entry<'_, impl Read>,
        member: &mut impl FnMut(&str, SystemTime, &mut dyn Read) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.listed.is_some() {
            return Err(refused("the outputs archive has a member after its list".to_owned()));
        }
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            return self.take_long_name(entry);
        }
        let name = self.long_name.take().unwrap_or_else(|| entry.path_bytes().into_owned());
        if kind != EntryType::Regular {
            let shown = String::from_utf8_lossy(&name);
            return Err(refused(format!("the outputs archive member '{}' is not a file", escaped(&shown))));
        }
        let path = member_path(&name)?;
        if path.eq_ignore_ascii_case(LIST_NAME) {
            self.listed = Some(read_list(entry)?);
            return Ok(());
        }
        self.check_room(&path, entry.size())?;
        let modified = last_write(entry.header(), self.now)
            .map_err(|reason| refused(format!("the outputs archive member '{}' has a last write {reason}", escaped(&path))))?;
        let mut hashing = Hashing::new(entry);
        member(&path, modified, &mut hashing)?;
        let (sha256, size) = hashing.finish();
        self.seen.push(Entry { path, size, sha256 });
        Ok(())
    }

    /// Whether one more file, `path` of `size` bytes, fits: within [`MEMBER_LIMIT`] files, and no larger than
    /// [`MEMBER_SIZE_LIMIT`] or what is left of the archive's byte limit; else the refusal.
    fn check_room(&self, path: &str, size: u64) -> io::Result<()> {
        if self.seen.len() == MEMBER_LIMIT {
            return Err(refused(format!("the outputs archive holds more than {MEMBER_LIMIT} files")));
        }
        let most = MEMBER_SIZE_LIMIT.min(self.left.get());
        if size > most {
            return Err(refused(format!(
                "the outputs archive member '{}' is {size} bytes, more than the {most} it may be",
                escaped(path)
            )));
        }
        Ok(())
    }

    /// Takes the GNU long name `entry` holds, of at most [`LONG_NAME_LIMIT`] bytes, for the next member; else the
    /// refusal.
    fn take_long_name(&mut self, entry: &mut tar::Entry<'_, impl Read>) -> io::Result<()> {
        if self.long_name.is_some() {
            return Err(refused("the outputs archive has two long names in a row".to_owned()));
        }
        if entry.size() > LONG_NAME_LIMIT {
            return Err(refused(format!(
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
fn member_path(bytes: &[u8]) -> io::Result<String> {
    let outside = |shown: &str| {
        refused(format!(
            "the outputs archive member '{}' is not a relative path inside the destination",
            escaped(shown)
        ))
    };
    let path = std::str::from_utf8(bytes).map_err(|_| outside(&String::from_utf8_lossy(bytes)))?;
    if is_plain_path(path) { Ok(path.to_owned()) } else { Err(outside(path)) }
}

/// Whether the `/`-separated `path` is relative, of at most [`PATH_DEPTH_LIMIT`] parts, each one [`is_plain_part`].
fn is_plain_path(path: &str) -> bool {
    path.split('/').count() <= PATH_DEPTH_LIMIT && path.split('/').all(is_plain_part)
}

/// Whether `part` names a file or a folder that Windows would create under that very name and git does not own: not
/// empty, not ending in `.` or a space (so neither `.` nor `..`), with no `\ : < > " | ? *`, control character or
/// bidirectional mark, no `~` before a digit (an 8.3 short name), not `.git` in any case, and not a device name.
fn is_plain_part(part: &str) -> bool {
    !part.is_empty()
        && !part.ends_with(['.', ' '])
        && !part.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        && !part.chars().any(|character| character.is_control() || is_reordering_mark(character))
        && !part.as_bytes().windows(2).any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
        && !part.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git")
        && !is_device(part)
}

/// Whether the name `part` opens a Windows device, whatever its extension: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`,
/// `CONOUT$`, or `COM` or `LPT` and one digit, `0` to `9`, `¹`, `²` or `³`.
fn is_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|digit| matches!(digit, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

/// The entries of the list member `data`; else why it is not a list.
fn read_list(data: &mut impl Read) -> io::Result<Vec<Entry>> {
    let mut text = String::new();
    data.take(LIST_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|error| refused(format!("the outputs archive's list cannot be read: {error}")))?;
    if text.len() as u64 > LIST_LIMIT {
        return Err(refused(format!("the outputs archive's list is longer than {LIST_LIMIT} bytes")));
    }
    let not_a_list = || refused("the outputs archive's list is not a list of paths, sizes and SHA-256s".to_owned());
    let value: Value = serde_json::from_str(&text).map_err(|_| not_a_list())?;
    let items = value.as_array().ok_or_else(not_a_list)?;
    items
        .iter()
        .map(|item| {
            let path = item.get("path").and_then(Value::as_str);
            let size = item.get("size").and_then(Value::as_u64);
            let sha256 = item
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|sha256| is_lower_hex(sha256, SHA256_DIGITS));
            match (path, size, sha256) {
                (Some(path), Some(size), Some(sha256)) => Ok(Entry {
                    path: path.to_owned(),
                    size,
                    sha256: sha256.to_owned(),
                }),
                _ => Err(not_a_list()),
            }
        })
        .collect()
}

/// The list member's text for `entries`: a JSON array of `{"path","size","sha256"}` objects.
fn list_json(entries: &[Entry]) -> String {
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| json!({ "path": entry.path, "size": entry.size, "sha256": entry.sha256 }))
        .collect();
    Value::Array(items).to_string()
}

/// Whether the members `seen` are exactly the files `listed`, in order; else the refusal naming the first difference.
fn agree(seen: &[Entry], listed: &[Entry]) -> io::Result<()> {
    for (index, member) in seen.iter().enumerate() {
        match listed.get(index) {
            Some(entry) if entry == member => {}
            Some(entry) if entry.path == member.path => {
                return Err(refused(format!(
                    "the outputs archive member '{}' does not match its list: {} bytes with SHA-256 {}, listed as {} bytes \
                     with SHA-256 {}",
                    escaped(&member.path),
                    member.size,
                    member.sha256,
                    entry.size,
                    entry.sha256
                )));
            }
            _ => {
                return Err(refused(format!(
                    "the outputs archive member '{}' is not the file its list holds next",
                    escaped(&member.path)
                )));
            }
        }
    }
    match listed.get(seen.len()) {
        Some(missing) => Err(refused(format!(
            "the outputs archive lacks '{}', which its list holds",
            escaped(&missing.path)
        ))),
        None => Ok(()),
    }
}

fn refused(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its files or archives must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tar::{EntryType, Header};

    use super::{Entry, LIST_NAME, MEMBER_LIMIT, Packer, extract_into, is_device, list_json, member_path, pack, pack_file, read_members, unpack};
    use crate::boxside::payload::escaped;
    use crate::outputs::glob::Globs;
    use crate::outputs::hash::sha256_of;
    use crate::outputs::walk::{Found, Stamp, walk};

    /// The byte limit the cases unpack under, far above what any of them holds.
    const LIMIT: u64 = 1 << 30;

    /// A scratch folder for one test, emptied first.
    fn scratch(test: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("gate-archive-{}-{test}", std::process::id()));
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

    /// A `.tar.zst` stream of `members` as given, no name checked, then a list of `listed`.
    fn raw(members: &[Member<'_>], listed: &[Entry]) -> Vec<u8> {
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

    /// The refusal `unpack` gives for `archive`, and the files left of what it extracted before it: none, as it removes
    /// the destination it made.
    fn refusal(test: &str, archive: &[u8]) -> (String, Vec<String>) {
        let destination = scratch(test);
        let error = unpack(archive, &destination, LIMIT).expect_err("the archive is refused");
        assert!(!destination.exists(), "{test}: the destination is removed");
        (error.to_string(), files_below(&destination))
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
        let chosen: Vec<_> = found.iter().collect();
        let mut archive = Vec::new();
        let packed = pack(&mut archive, &source, &chosen).expect("the files pack");
        assert_eq!(packed.compressed, archive.len() as u64);
        let mut expected: Vec<Entry> = files.iter().map(|(path, data)| entry(path, data)).collect();
        expected.sort_by(|first, second| first.path.cmp(&second.path));
        assert_eq!(packed.entries, expected);

        let destination = scratch("round-trip-destination");
        assert_eq!(unpack(&archive[..], &destination, LIMIT).expect("the archive extracts"), expected);
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
        let mut packer = Packer::new(io::sink()).expect("a packer");
        let error = packer.add(".GATE-outputs.json", 1, 0, &b"x"[..]).expect_err("the list's name is refused");
        assert_eq!(error.to_string(), "the output .GATE-outputs.json has the name of the archive's own list");
        assert!(packer.add("bin/.gate-outputs.json", 1, 0, &b"x"[..]).is_ok(), "only at the root");
    }

    #[test]
    fn a_file_that_ends_before_its_size_fails_the_pack() {
        let mut packer = Packer::new(io::sink()).expect("a packer");
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
        let list = list_json(std::slice::from_ref(&a));
        let archive = unlisted(&[
            (b"a", EntryType::Regular, b"a"),
            (b".GATE-Outputs.JSON", EntryType::Regular, list.as_bytes()),
        ]);
        let destination = scratch("list-case");
        assert_eq!(unpack(&archive[..], &destination, LIMIT).expect("the archive extracts"), [a]);
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
        let reason = unpack(&archive[..], &destination, 2048).expect_err("a member past the limit").to_string();
        assert_eq!(
            reason, "the outputs archive member 'a' is 4096 bytes, more than the 1536 it may be",
            "the limit less the member's header"
        );
        assert_eq!(files_below(&destination), Vec::<String>::new(), "nothing of it is written");
        let reason = unpack(&archive[..], &scratch("limit-total"), 5000)
            .expect_err("an archive past the limit")
            .to_string();
        assert!(
            reason.ends_with("the outputs archive holds more than 5000 bytes once decompressed"),
            "{reason}"
        );
        let whole = u64::try_from(zstd::decode_all(&archive[..]).expect("the archive decompresses").len()).expect("a short archive");
        assert!(
            unpack(&archive[..], &scratch("limit-exact"), whole).is_ok(),
            "an archive of exactly the limit extracts"
        );
    }

    #[test]
    fn an_archive_with_too_many_members_is_refused() {
        let names: Vec<String> = (0..=MEMBER_LIMIT).map(|index| index.to_string()).collect();
        let members: Vec<Member<'_>> = names.iter().map(|name| (name.as_bytes(), EntryType::Regular, &b""[..])).collect();
        let error = read_members(&unlisted(&members)[..], LIMIT, |_, _, _| Ok(())).expect_err("too many files");
        assert_eq!(error.to_string(), format!("the outputs archive holds more than {MEMBER_LIMIT} files"));
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
        let list = list_json(&[entry("a", b"a")]);
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
            unpack(&stamped(octal(now))[..], &scratch("mtime-now"), LIMIT).is_ok(),
            "a last write of now"
        );
    }

    #[test]
    fn unpack_refuses_an_existing_destination() {
        let destination = scratch("existing");
        fs::create_dir_all(&destination).expect("the folder can be made");
        let archive = raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]);
        let error = unpack(&archive[..], &destination, LIMIT).expect_err("an existing destination is refused");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
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
        let reason = extract_into(&archive[..], &destination, LIMIT, 2)
            .expect_err("a third folder is refused")
            .to_string();
        assert_eq!(reason, "the outputs archive makes more than 2 folders");
        assert_eq!(files_below(&destination), ["a/b/x"]);
        assert!(!destination.join("a").join("c").exists(), "the folder past the cap is not made");
        assert!(
            extract_into(&archive[..], &made("folders-fit"), LIMIT, 3).is_ok(),
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
        let error = read_members(&archive[..], limit, |_, _, _| Ok(())).expect_err("the skippable frame is refused");
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
        let reason = extract_into(&archive[..], &destination, 2500, MEMBER_LIMIT)
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
            let mut packer = Packer::new(io::sink()).expect("a packer");
            let error = packer.add(name, 1, 0, &b"x"[..]).expect_err(name);
            assert_eq!(
                error.to_string(),
                format!("the output '{}' has a name the outputs archive's reader refuses", escaped(name))
            );
        }
    }

    /// Makes `link` a junction to the folder `target`.
    #[expect(clippy::disallowed_methods, reason = "a test's one mklink takes no spawn lock")]
    fn junction(link: &Path, target: &Path) {
        let made = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd.exe runs");
        assert!(made.status.success(), "a junction needs no privilege: {made:?}");
    }

    #[test]
    fn unpack_refuses_a_junction_in_its_path() {
        let destination = scratch("junction");
        let outside = scratch("junction-outside");
        fs::create_dir_all(&destination).expect("the destination can be made");
        fs::create_dir_all(&outside).expect("the outside folder can be made");
        junction(&destination.join("bin"), &outside);
        let archive = raw(&[(b"bin/a.dll", EntryType::Regular, b"a")], &[entry("bin/a.dll", b"a")]);
        let reason = extract_into(&archive[..], &destination, LIMIT, MEMBER_LIMIT)
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
        let mut packer = Packer::new(io::sink()).expect("a packer");
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
        let error = pack(io::sink(), root, &[&found]).expect_err("a file from before 1970 is refused");
        assert!(error.to_string().ends_with("a.dll was last written before 1970"), "{error}");
    }

    #[test]
    fn pack_refuses_a_file_that_is_a_link() {
        let mut found = one_file("link", "secret", b"s");
        let link = found.full.with_file_name("a.dll");
        std::os::windows::fs::symlink_file(&found.full, &link).expect("this machine lets a test make a symbolic link");
        found.full = link;
        let root = found.full.parent().expect("the link has a folder").to_owned();
        let error = pack(io::sink(), &root, &[&found]).expect_err("a link is refused");
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

        let chosen: Vec<_> = found.iter().collect();
        let error = pack(io::sink(), &root, &chosen).expect_err("a file outside the work folder is refused");

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
    /// The most a stream may hold back between its source and its sink: eight times the 2 MiB window zstd keeps at
    /// level 3, and a sixteenth of [`LARGE`]. A writer or reader that held a whole file would hold back all of it.
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
        let mut packer = Packer::new(sink).expect("a packer");
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
            let mut packer = Packer::new(pipe_writer)?;
            packer.add("bin/large.bin", LARGE, 0, Generated::new(LARGE, Rc::new(Cell::new(0))))?;
            packer.finish()
        });
        let taken = Rc::new(Cell::new(0));
        let widest = Rc::new(Cell::new(0));
        let source = Taking {
            inner: pipe_reader,
            taken: Rc::clone(&taken),
        };
        let read = read_members(source, LIMIT, |_, _, data| {
            let mut sink = Watching {
                given: Rc::clone(&taken),
                at_last_write: taken.get(),
                widest: Rc::clone(&widest),
            };
            io::copy(data, &mut sink).map(|_| ())
        });
        let packed = writer.join().expect("the writer ends").expect("the archive packs");
        assert_eq!(read.expect("the archive extracts"), packed.entries);
        assert!(taken.get() > LARGE / 2, "random bytes do not shrink: {}", taken.get());
        assert!(widest.get() <= HELD_BACK, "the reader held back {} bytes", widest.get());
    }
}

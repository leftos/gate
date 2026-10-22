//! The reader of the archive a run's outputs and inputs travel in: a tar stream compressed with zstd, named `.tar.zst`,
//! whose last member, [`LIST_NAME`], lists every path it brings back (path, size, SHA-256). A path whose content an
//! earlier member or a file the PC holds already has is listed as a copy and not packed. [`extract`] checks each
//! member's path before anything is written, hands it to the box's [`Place`] while hashing it, checks the members
//! against the list at the end, then hands the place each copy, with where its bytes are. Every check of the stream,
//! the list and the limits lives here. Each box's [`Place`] does its own writes and the three checks only a writer can
//! make, each with the refusal this module gives for it: a target outside the destination ([`outside_destination`]),
//! one folder past the archive's budget ([`FolderBudget`]), and a copy whose bytes as written differ from its item
//! ([`copy_mismatch`]).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate_router::escaped;
use gate_wire::{SHA256_DIGITS, is_lower_hex, ordinal_upper};
use serde_json::{Value, json};
use tar::{EntryType, Header};

use crate::hash::Hashing;
use crate::manifest::HeldBy;
use crate::names::is_plain_path;

/// The name of the archive's last member, the list of every path the archive brings back.
pub const LIST_NAME: &str = ".gate-outputs.json";
/// The most bytes the list member is read to.
const LIST_LIMIT: u64 = 4 * 1024 * 1024;
/// The most files an archive may hold: more than a list of `LIST_LIMIT` (4 MiB) bytes can name, each of its items
/// taking at least 98 bytes.
pub const MEMBER_LIMIT: usize = 50_000;
/// The most bytes one member may hold.
pub const MEMBER_SIZE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;
/// The most bytes a GNU long name member may hold.
pub const LONG_NAME_LIMIT: u64 = 4 * 1024;
/// What the stream as sent may hold beyond zstd's worst-case expansion of the limit's bytes: frame and block headers.
const SENT_SLACK: u64 = 1024 * 1024;

/// The most bytes a `.tar.zst` stream may hold for an archive of a `limit`-byte limit: zstd's worst case expansion of
/// the limit's bytes, the ratio a decoder could reach through a skippable frame or empty blocks, plus `SENT_SLACK`
/// (1 MiB). The reader refuses a longer stream, and the client fetching one caps its fetch at the same figure, so every
/// archive the box can legitimately send is one the reader would take. Saturating, and never 0: `SENT_SLACK` alone is
/// above it, so the cap the client writes under can never drop every byte.
#[must_use]
pub fn sent_limit(limit: u64) -> NonZeroU64 {
    NonZeroU64::new(limit.saturating_add(limit / 64).saturating_add(SENT_SLACK)).unwrap_or(NonZeroU64::MIN)
}
/// The largest zstd window the writer compresses with and the reader accepts, as a power of two: 128 MiB.
pub const WINDOW_LOG_MAX: u32 = 27;
/// The most characters of a path a refusal of the list shows.
const SHOWN_PATH_CHARS: usize = 200;
/// The most PC files a refusal of a copy names among those it tried.
const TRIED_NAMED: usize = 5;
/// How far past the reader's clock a member's last write may be: a day, for clocks that disagree.
const CLOCK_SLACK: Duration = Duration::from_hours(24);

/// One packed file: its `/`-separated path relative to the root it was packed from, its size and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// One item of an archive's list: a file packed as a member, or a copy, whose bytes the archive does not hold, as an
/// earlier member or a file the PC holds has them. `copy` is a copy's own last write, in seconds since 1970 as a
/// member's header holds it; `None` for a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub entry: Entry,
    pub copy: Option<u64>,
}

/// Where a box writes what an archive brings back. The reader has checked each member's name, kind, size and last write
/// and each copy's item before it hands it on; what only the writer can see, the place must check itself, giving this
/// module's refusal for it:
///
/// - each target it writes must resolve inside the destination, else [`outside_destination`] of the path;
/// - before each folder it makes, it must ask its [`FolderBudget`] ([`FolderBudget::check`]) and count the folder once
///   made;
/// - each copy's bytes as written must be its item's size and SHA-256, else [`copy_mismatch`].
pub trait Place {
    /// Writes the member `path`, a plain `/`-separated path, last written `modified`, its bytes read from `data`, which
    /// hashes them as they pass; else why not. It must read `data` to its end: the reader holds the member to the size
    /// and SHA-256 of what was read, so a short read is refused as a member that does not match its list, a fault a
    /// refetch would only meet again.
    fn member(&mut self, path: &str, modified: SystemTime, data: &mut dyn Read) -> Result<(), UnpackError>;

    /// Writes the copy `entry`, last written `modified`, its bytes taken `from` an earlier member or a file the PC
    /// holds; else why not. It must check the bytes it wrote against `entry`'s size and SHA-256, else refuse the copy
    /// with [`copy_mismatch`]; a copy no file of its own still holds is refused with [`no_pc_file`].
    fn copy(&mut self, entry: &Entry, modified: SystemTime, from: CopyFrom<'_>) -> Result<(), UnpackError>;
}

/// Where a copy's bytes are: the path of the first member before it holding its SHA-256, or a file the PC holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFrom<'a> {
    Member(&'a str),
    Pc,
}

/// The most an archive may bring back: `bytes` once decompressed, copies included, and `files` paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub bytes: u64,
    pub files: usize,
}

/// Reads the `.tar.zst` stream `source` as [`read_members`] does, within `limits`, each copy's SHA-256 a member's
/// before it or one `held` holds, handing each member to `place`; then hands `place` each copy, asking `go_on` before
/// each, with the last write its item lists and the first member holding its SHA-256, else the PC's file. Returns the
/// entries of every path listed; else the refusal, or the interrupt once `go_on` says no.
pub fn extract(
    source: impl Read,
    limits: Limits,
    held: &HeldBy<'_>,
    go_on: &dyn Fn() -> bool,
    place: &mut impl Place,
) -> Result<Vec<Entry>, UnpackError> {
    let items = read_members(source, limits.bytes, limits.files, held, |path, modified, data| {
        place.member(path, modified, data)
    })?;
    hand_copies(&items, go_on, place)?;
    Ok(items.into_iter().map(|item| item.entry).collect())
}

/// Hands `place` each copy of `items`, asking `go_on` before each: from the first member holding its SHA-256, else from
/// the PC. Each copy takes the last write its item lists. Else the refusal, or the interrupt once `go_on` says no.
fn hand_copies(items: &[Item], go_on: &dyn Fn() -> bool, place: &mut impl Place) -> Result<(), UnpackError> {
    let now = SystemTime::now();
    let mut members: HashMap<&str, &str> = HashMap::new();
    for Item { entry, copy } in items {
        let Some(seconds) = copy else {
            members.entry(entry.sha256.as_str()).or_insert(entry.path.as_str());
            continue;
        };
        if !go_on() {
            return Err(stopped());
        }
        let modified = copy_last_write(&entry.path, *seconds, now)?;
        let from = match members.get(entry.sha256.as_str()) {
            Some(member) => CopyFrom::Member(member),
            None => CopyFrom::Pc,
        };
        place.copy(entry, modified, from)?;
    }
    Ok(())
}

/// The interrupt of an unpack its `go_on` stopped while its copies were made.
#[must_use]
pub fn stopped() -> UnpackError {
    io::Error::new(io::ErrorKind::Interrupted, "the unpack was stopped while its copies were made").into()
}

/// The refusal of the copy `entry`, whose bytes as they were written are `size` bytes with SHA-256 `sha256`.
#[must_use]
pub fn copy_mismatch(entry: &Entry, size: u64, sha256: &str) -> UnpackError {
    refused(format!(
        "the outputs archive copy '{}' does not match its list: {size} bytes with SHA-256 {sha256}, listed as {} bytes \
         with SHA-256 {}",
        escaped(&entry.path),
        entry.size,
        entry.sha256
    ))
    .into()
}

/// The refusal of the copy `entry`, which no PC file still holds, naming each file `tried` and why it was passed over
/// (`said_tried`).
#[must_use]
pub fn no_pc_file(entry: &Entry, tried: &[(&str, String)]) -> UnpackError {
    refused(format!(
        "no PC file still holds the content of {}, SHA-256 {}{}",
        escaped(&entry.path),
        entry.sha256,
        said_tried(tried)
    ))
    .into()
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

/// The size of the first file `held` holds with the SHA-256 `sha256`; `None` when it holds none.
#[must_use]
pub fn held_size(held: &HeldBy<'_>, sha256: &str) -> Option<u64> {
    held.get(sha256).and_then(|paths| paths.first()).map(|(_, file)| file.stamp.size)
}

/// The refusal of the member `path`, shown escaped, as a path that is not a relative path inside the destination.
#[must_use]
pub fn outside_destination(path: &str) -> UnpackError {
    member_refused(format!(
        "the outputs archive member '{}' is not a relative path inside the destination",
        escaped(path)
    ))
}

/// The folders an extraction has made, and the most it may make.
#[derive(Debug)]
pub struct FolderBudget {
    made: usize,
    limit: usize,
}

impl FolderBudget {
    /// A budget of `limit` folders, none made yet.
    #[must_use]
    pub fn new(limit: usize) -> FolderBudget {
        FolderBudget { made: 0, limit }
    }

    /// Whether one more folder may be made; else the refusal, before the folder is made.
    pub fn check(&self) -> Result<(), UnpackError> {
        if self.made == self.limit {
            return Err(member_refused(format!("the outputs archive makes more than {} folders", self.limit)));
        }
        Ok(())
    }

    /// Counts one folder made.
    pub fn count(&mut self) {
        self.made += 1;
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

/// Whether the output `path` may be listed: not the list's own name, and a name the reader takes; else the refusal.
pub fn check_name(path: &str) -> io::Result<()> {
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

/// Reads the `.tar.zst` stream `source`, decompressed to at most `limit` bytes, handing each file member's path, last
/// write and data to `member` and hashing the data as `member` reads it, then checks the members against the list
/// (`agree`, each copy's SHA-256 a member's before it or one `held` holds) and the list against the limits: at most
/// `files` paths, and each copy's bytes within what is left of `limit` (`Members::fit_copies`). Returns the list's
/// items, else why the archive was refused.
pub fn read_members(
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
    let path = std::str::from_utf8(bytes).map_err(|_| outside_destination(&String::from_utf8_lossy(bytes)))?;
    if is_plain_path(path) {
        Ok(path.to_owned())
    } else {
        Err(outside_destination(path))
    }
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
#[must_use]
pub fn list_json(items: &[Item]) -> String {
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

/// The refusal `reason`, as the error of data a reader or writer of the archive does not take.
#[must_use]
pub fn refused(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Why an archive could not be unpacked: whether its own members are refused for what they are, which a refetch of the
/// same archive would be refused the same way, or the stream could not be read, which a refetch may clear.
#[derive(Debug)]
pub enum UnpackError {
    /// A member of the archive is refused for what it is: its path or its kind, a last write it cannot hold, a name the
    /// reader never takes, or the file or folder limit it passes.
    Members(io::Error),
    /// The stream could not be read: an I/O fault, a bad frame or one cut short, a limit it passed as it was read, or a
    /// member that does not match the archive's own list.
    Fault(io::Error),
}

impl UnpackError {
    /// Whether the archive's own members are refused for what they are, which a refetch would refuse the same way.
    #[must_use]
    pub fn members(&self) -> bool {
        matches!(self, UnpackError::Members(_))
    }

    /// Whether this is the fault `kind` names, for a caller that tells one fault from another.
    #[must_use]
    pub fn is_fault(&self, kind: io::ErrorKind) -> bool {
        matches!(self, UnpackError::Fault(error) if error.kind() == kind)
    }

    /// This error, with `removal`'s failure to remove `destination` said after it, of the same kind.
    #[must_use]
    pub fn with_removal(self, destination: &Path, removal: &io::Error) -> UnpackError {
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
#[expect(clippy::expect_used, reason = "a test that cannot make its archives must fail loudly")]
pub(crate) mod tests {
    use std::io::{self, Read, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tar::{EntryType, Header};

    use super::{
        CopyFrom, Entry, Item, LIST_NAME, LONG_NAME_LIMIT, Limits, MEMBER_LIMIT, Place, UnpackError, extract, list_json, member_path, read_members,
    };
    use crate::hash::sha256_of;
    use crate::manifest::{Held, HeldBy, Manifest, Stamp};
    use crate::names::is_device;

    /// The byte limit the cases read under, far above what any of them holds.
    pub(crate) const LIMIT: u64 = 1 << 30;

    /// The limits of `bytes` bytes and the archive's own file limit.
    pub(crate) fn within(bytes: u64) -> Limits {
        Limits { bytes, files: MEMBER_LIMIT }
    }

    /// A [`Place`] that reads only the first byte of each member, as a writer that stopped short would.
    #[derive(Default)]
    struct Skimming {
        members: Vec<String>,
    }

    impl Place for Skimming {
        fn member(&mut self, path: &str, _modified: SystemTime, data: &mut dyn Read) -> Result<(), UnpackError> {
            let mut first = [0_u8; 1];
            data.read_exact(&mut first)?;
            self.members.push(path.to_owned());
            Ok(())
        }

        fn copy(&mut self, _entry: &Entry, _modified: SystemTime, _from: CopyFrom<'_>) -> Result<(), UnpackError> {
            Ok(())
        }
    }

    /// A [`Place`] in memory: each member's path and bytes, each member's path and last write, and each copy's path and
    /// where its bytes are; the pack's own cases read their archives back through it too.
    #[derive(Default)]
    pub(crate) struct Memory {
        pub(crate) members: Vec<(String, Vec<u8>)>,
        pub(crate) written: Vec<(String, SystemTime)>,
        pub(crate) copies: Vec<(String, String)>,
    }

    impl Place for Memory {
        fn member(&mut self, path: &str, modified: SystemTime, data: &mut dyn Read) -> Result<(), UnpackError> {
            let mut bytes = Vec::new();
            data.read_to_end(&mut bytes)?;
            self.members.push((path.to_owned(), bytes));
            self.written.push((path.to_owned(), modified));
            Ok(())
        }

        fn copy(&mut self, entry: &Entry, _modified: SystemTime, from: CopyFrom<'_>) -> Result<(), UnpackError> {
            let from = match from {
                CopyFrom::Member(member) => member.to_owned(),
                CopyFrom::Pc => "the PC".to_owned(),
            };
            self.copies.push((entry.path.clone(), from));
            Ok(())
        }
    }

    /// What [`extract`] of `archive` under `limit` gives, no file of the PC's held and `go_on` always saying yes, and
    /// what it handed its place.
    fn extracted(archive: &[u8], limit: u64) -> (Result<Vec<Entry>, UnpackError>, Memory) {
        let mut memory = Memory::default();
        let result = extract(archive, within(limit), &HeldBy::new(), &|| true, &mut memory);
        (result, memory)
    }

    /// The refusal [`extract`] gives for `archive`.
    fn refusal(archive: &[u8]) -> UnpackError {
        extracted(archive, LIMIT).0.expect_err("the archive is refused")
    }

    /// The refusal [`extract`] gives for `archive`, which it gave before it handed its place any member.
    fn refusal_before_any_member(archive: &[u8]) -> UnpackError {
        let (result, memory) = extracted(archive, LIMIT);
        assert!(memory.members.is_empty(), "nothing was handed on: {:?}", memory.members);
        result.expect_err("the archive is refused")
    }

    /// Checks `error` is a member error saying `reason`: one a refetch of the same archive would be refused the same way.
    fn assert_members(error: &UnpackError, reason: &str) {
        assert!(error.members(), "a member error: {error}");
        assert_eq!(error.to_string(), reason);
    }

    /// Checks `error` is a fault of the stream, which a refetch may clear, saying `reason`.
    fn assert_fault(error: &UnpackError, reason: &str) {
        assert!(!error.members(), "a fault: {error}");
        assert_eq!(error.to_string(), reason);
    }

    pub(crate) fn entry(path: &str, data: &[u8]) -> Entry {
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
    fn a_member_path_deeper_than_64_parts_is_refused() {
        let deep = |parts: usize| vec!["a"; parts].join("/");
        assert_taken_names(&[&deep(64)]);
        assert_refused_names(&[&deep(65)]);
    }

    #[test]
    fn a_link_a_folder_or_another_entry_that_is_not_a_file_is_a_member_error() {
        for kind in [
            EntryType::Symlink,
            EntryType::Link,
            EntryType::Directory,
            EntryType::Fifo,
            EntryType::Continuous,
        ] {
            let archive = raw(&[(b"bin/link", kind, b"")], &[]);
            assert_members(
                &refusal_before_any_member(&archive),
                "the outputs archive member 'bin/link' is not a file",
            );
        }
    }

    #[test]
    fn a_pax_header_is_refused() {
        let pax = b"30 path=bin/aaaaaaaaaaaaaaa.dll\n";
        for kind in [EntryType::XHeader, EntryType::XGlobalHeader, EntryType::GNULongLink, EntryType::GNUSparse] {
            let archive = raw(
                &[(b"PaxHeaders/x", kind, pax), (b"bin/a.dll", EntryType::Regular, b"a")],
                &[entry("bin/a.dll", b"a")],
            );
            assert_members(
                &refusal_before_any_member(&archive),
                "the outputs archive member 'PaxHeaders/x' is not a file",
            );
        }
    }

    #[test]
    fn a_long_name_past_the_cap_two_in_a_row_or_one_last_is_refused() {
        let long = vec![b'a'; 4096 + 1];
        let archive = unlisted(&[(b"././@LongLink", EntryType::GNULongName, &long), (b"x", EntryType::Regular, b"x")]);
        assert_members(
            &refusal_before_any_member(&archive),
            "the outputs archive has a long name of more than 4096 bytes",
        );
        let two = unlisted(&[
            (b"././@LongLink", EntryType::GNULongName, b"bin/a.dll"),
            (b"././@LongLink", EntryType::GNULongName, b"bin/b.dll"),
            (b"x", EntryType::Regular, b"x"),
        ]);
        assert_members(&refusal_before_any_member(&two), "the outputs archive has two long names in a row");
        let last = unlisted(&[(b"././@LongLink", EntryType::GNULongName, b"bin/a.dll")]);
        assert_members(&refusal_before_any_member(&last), "the outputs archive ends with a long name");
    }

    #[test]
    fn a_member_after_the_list_is_refused() {
        let after = unlisted(&[(LIST_NAME.as_bytes(), EntryType::Regular, b"[]"), (b"a", EntryType::Regular, b"a")]);
        assert_members(&refusal_before_any_member(&after), "the outputs archive has a member after its list");
    }

    #[test]
    fn an_archive_without_a_list_is_refused() {
        let members: [Member<'_>; 2] = [(b"a", EntryType::Regular, b"a"), (b"b", EntryType::Regular, b"b")];
        assert_members(&refusal(&unlisted(&members)), "the outputs archive holds no list of its files");
    }

    #[test]
    fn a_list_naming_a_path_twice_in_other_case_is_refused() {
        let data = b"the same bytes";
        let members: [Member<'_>; 1] = [(b"bin/a.dll", EntryType::Regular, data)];
        for (listed, twice) in [
            (vec![member("bin/a.dll", data), copy_of("BIN/A.dll", data)], "BIN/A.dll"),
            (
                vec![member("bin/a.dll", data), copy_of("bin/b.dll", data), copy_of("Bin/B.dll", data)],
                "Bin/B.dll",
            ),
        ] {
            let (result, memory) = extracted(&raw_items(&members, &listed), LIMIT);
            assert_members(
                &result.expect_err("the list is refused"),
                &format!("the outputs archive lists '{twice}' twice"),
            );
            assert!(memory.copies.is_empty(), "no copy is handed on");
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
        assert_members(
            &refusal(&archive),
            &format!(
                "the outputs archive lists a path of more than {LONG_NAME_LIMIT} bytes: '{}\u{2026}'",
                &long[..200]
            ),
        );
    }

    #[test]
    fn a_copy_whose_hash_no_member_or_manifest_holds_is_refused() {
        let data = b"the same bytes";
        let members: [Member<'_>; 1] = [(b"bin/a.dll", EntryType::Regular, data)];
        for listed in [
            vec![member("bin/a.dll", data), copy_of("bin/b.dll", b"other bytes")],
            vec![copy_of("bin/b.dll", data), member("bin/a.dll", data)],
        ] {
            let error = refusal(&raw_items(&members, &listed));
            assert!(!error.members(), "a fault: {error}");
            let reason = error.to_string();
            assert!(reason.starts_with("the outputs archive lists 'bin/b.dll' as a copy of "), "{reason}");
            assert!(reason.ends_with("which no member before it and no file the PC holds has"), "{reason}");
        }
    }

    #[test]
    fn copies_count_against_the_byte_limit_so_a_small_archive_cannot_expand_past_it() {
        let data = [7_u8; 4096];
        let archive = raw_items(
            &[(b"a", EntryType::Regular, &data)],
            &[member("a", &data), copy_of("b", &data), copy_of("c", &data)],
        );
        let whole = u64::try_from(zstd::decode_all(&archive[..]).expect("the archive decompresses").len()).expect("a short archive");
        let (result, _) = extracted(&archive, whole + 4096 + 100);
        assert_fault(
            &result.expect_err("the second copy passes the limit"),
            "the outputs archive copy 'c' is 4096 bytes, more than the 100 it may be",
        );
        assert!(extracted(&archive, whole + 2 * 4096).0.is_ok(), "copies of exactly the limit extract");
    }

    #[test]
    fn copies_count_against_the_member_limit() {
        let archive = raw_items(
            &[(b"a", EntryType::Regular, b"a")],
            &[member("a", b"a"), copy_of("b", b"a"), copy_of("c", b"a")],
        );
        let read = |files: usize| {
            let limits = Limits { bytes: LIMIT, files };
            extract(&archive[..], limits, &HeldBy::new(), &|| true, &mut Memory::default())
        };
        assert_members(
            &read(2).expect_err("three paths are past a limit of two"),
            "the outputs archive holds more than 2 files",
        );
        assert_eq!(read(3).expect("three paths fit a limit of three").len(), 3);
    }

    #[test]
    fn an_archive_past_the_byte_limit_is_refused() {
        let data = [7_u8; 4096];
        let archive = raw(&[(b"a", EntryType::Regular, &data)], &[entry("a", &data)]);
        let (result, memory) = extracted(&archive, 2048);
        assert_fault(
            &result.expect_err("a member past the limit"),
            "the outputs archive member 'a' is 4096 bytes, more than the 1536 it may be",
        );
        assert!(memory.members.is_empty(), "nothing of it is handed on");
        let error = extracted(&archive, 5000).0.expect_err("an archive past the limit");
        assert!(!error.members(), "a fault: {error}");
        assert!(
            error
                .to_string()
                .ends_with("the outputs archive holds more than 5000 bytes once decompressed"),
            "{error}"
        );
        let whole = u64::try_from(zstd::decode_all(&archive[..]).expect("the archive decompresses").len()).expect("a short archive");
        assert!(extracted(&archive, whole).0.is_ok(), "an archive of exactly the limit extracts");
    }

    #[test]
    fn an_archive_with_too_many_members_is_refused() {
        let names: Vec<String> = (0..=MEMBER_LIMIT).map(|index| index.to_string()).collect();
        let members: Vec<Member<'_>> = names.iter().map(|name| (name.as_bytes(), EntryType::Regular, &b""[..])).collect();
        let error = read_members(&unlisted(&members)[..], LIMIT, MEMBER_LIMIT, &HeldBy::new(), |_, _, _| Ok(())).expect_err("too many files");
        assert_members(&error, &format!("the outputs archive holds more than {MEMBER_LIMIT} files"));
    }

    #[test]
    fn a_window_27_archive_is_read_and_a_window_28_one_is_refused() {
        let (result, memory) = extracted(&windowed(27), LIMIT);
        assert_eq!(result.expect("a 128 MiB window is taken"), [entry("a", b"a")]);
        assert_eq!(memory.members, [("a".to_owned(), b"a".to_vec())]);
        let error = refusal_before_any_member(&windowed(28));
        assert!(!error.members(), "a window past the reader's is a fault of the stream: {error}");
        assert!(error.to_string().contains("memory"), "{error}");
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
        let (result, _) = extracted(&archive, limit);
        let error = result.expect_err("the skippable frame is refused");
        assert!(!error.members(), "a fault: {error}");
        let most = limit + limit / 64 + 1024 * 1024;
        assert!(
            error
                .to_string()
                .contains(&format!("the outputs archive holds more than {most} bytes as sent")),
            "{error}"
        );
    }

    #[test]
    fn a_base256_mtime_or_one_past_the_clock_is_refused() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("the clock is past 1970").as_secs();
        let mut far = [0; 12];
        far[0] = 0x80;
        far[4..].copy_from_slice(&u64::MAX.to_be_bytes());
        for (what, mtime) in [
            ("-1 in base-256", [0xff; 12]),
            ("the most base-256 holds", far),
            ("two days ahead", octal(now + 2 * 86_400)),
        ] {
            let error = refusal_before_any_member(&stamped(mtime));
            assert!(error.members(), "{what}: {error}");
            assert!(
                error.to_string().starts_with("the outputs archive member 'a' has a last write"),
                "{what}: {error}"
            );
        }
        assert!(extracted(&stamped(octal(now)), LIMIT).0.is_ok(), "a last write of now");
    }

    #[test]
    fn a_stream_cut_short_is_a_fault() {
        let whole = raw(&[(b"a", EntryType::Regular, b"a")], &[entry("a", b"a")]);
        for cut in [whole.len() / 2, whole.len() - 1] {
            let error = refusal(&whole[..cut]);
            assert!(!error.members(), "a stream cut at {cut} is not a member refusal: {error}");
        }
        let error = refusal_before_any_member(b"not an archive at all");
        assert!(!error.members(), "{error}");
    }

    #[test]
    fn each_copy_is_handed_on_with_its_member_unless_go_on_says_no() {
        let data = b"the same bytes";
        let archive = raw_items(
            &[(b"bin/a.dll", EntryType::Regular, data)],
            &[member("bin/a.dll", data), copy_of("bin/b.dll", data)],
        );
        let mut memory = Memory::default();
        extract(&archive[..], within(LIMIT), &HeldBy::new(), &|| true, &mut memory).expect("the archive is read");
        assert_eq!(memory.copies, [("bin/b.dll".to_owned(), "bin/a.dll".to_owned())]);

        let mut memory = Memory::default();
        let error = extract(&archive[..], within(LIMIT), &HeldBy::new(), &|| false, &mut memory).expect_err("the stop ends it");
        assert!(error.is_fault(io::ErrorKind::Interrupted), "{error}");
        assert_eq!(error.to_string(), "the unpack was stopped while its copies were made");
        assert_eq!(memory.members.len(), 1, "the members were read");
        assert!(memory.copies.is_empty(), "no copy is handed on once it said no");
    }

    #[test]
    fn a_member_the_place_reads_short_is_a_fault() {
        let archive = raw(&[(b"a", EntryType::Regular, b"abc")], &[entry("a", b"abc")]);
        let mut place = Skimming::default();
        let error = extract(&archive[..], within(LIMIT), &HeldBy::new(), &|| true, &mut place).expect_err("a short read is refused");
        assert_eq!(place.members, ["a"], "the member was handed on");
        let read = entry("a", b"a");
        let listed = entry("a", b"abc");
        assert_fault(
            &error,
            &format!(
                "the outputs archive member 'a' does not match its list: 1 bytes with SHA-256 {}, listed as 3 bytes with SHA-256 {}",
                read.sha256, listed.sha256
            ),
        );
    }

    #[test]
    fn a_copy_the_pc_holds_is_handed_on_as_pc() {
        let data = b"held by the PC";
        let held_entry = entry("lib/held.dll", data);
        let mut manifest = Manifest::default();
        manifest.insert(
            held_entry.path.clone(),
            Held {
                stamp: Stamp {
                    size: held_entry.size,
                    modified: 1,
                },
                sha256: held_entry.sha256.clone(),
            },
        );
        let archive = raw_items(
            &[(b"bin/a.dll", EntryType::Regular, b"other")],
            &[member("bin/a.dll", b"other"), copy_of("bin/new.dll", data)],
        );
        let mut memory = Memory::default();
        let entries = extract(&archive[..], within(LIMIT), &manifest.by_hash(&[]), &|| true, &mut memory).expect("the archive is read");
        assert_eq!(entries, [entry("bin/a.dll", b"other"), entry("bin/new.dll", data)]);
        assert_eq!(memory.copies, [("bin/new.dll".to_owned(), "the PC".to_owned())]);
    }
}

//! The writer of the archive a run's outputs travel in: a tar stream compressed with zstd, named `.tar.zst`, which
//! [`crate::archive`] reads. Each member is hashed with SHA-256 as it is packed, and the list of every path brought back
//! (path, size, SHA-256) follows the files as the last member, [`LIST_NAME`]. A path whose content an earlier member or
//! a file the PC holds already has is listed as a copy and not packed. Each box opens its own files and hands their
//! bytes to the [`Packer`]; what the writer holds in memory does not grow with a file's size.

use std::io::{self, Read, Write};

use tar::{EntryType, Header};

use crate::archive::{Entry, Item, LIST_NAME, WINDOW_LOG_MAX, check_name, list_json, refused};
use crate::hash::Hashing;

/// The zstd level the archive is compressed at: zstd's default, which keeps pace with a gigabit link. The writer adds
/// long-distance matching over a window sized to the archive's files ([`window_log`]), which finds a repeat that far
/// back in the stream.
const LEVEL: i32 = 3;
/// The smallest zstd window the writer compresses with, as a power of two: 1 MiB.
const WINDOW_LOG_MIN: u32 = 20;

/// What a pack wrote: its entries in order, and the archive's size, compressed.
#[derive(Debug, PartialEq, Eq)]
pub struct Packed {
    pub entries: Vec<Entry>,
    pub compressed: u64,
}

/// The zstd window, as a power of two, for an archive of files of `total` bytes: the smallest that holds them, within
/// [`WINDOW_LOG_MIN`] and [`WINDOW_LOG_MAX`].
#[must_use]
pub fn window_log(total: u64) -> u32 {
    total.saturating_sub(1).bit_width().clamp(WINDOW_LOG_MIN, WINDOW_LOG_MAX)
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
pub struct Packer<W: Write> {
    tar: tar::Builder<zstd::stream::write::Encoder<'static, Counting<W>>>,
    entries: Vec<Item>,
}

impl<W: Write> Packer<W> {
    /// An archive written to `sink`, compressed over a window of `2^window_log` bytes.
    pub fn new(sink: W, window_log: u32) -> io::Result<Packer<W>> {
        let mut encoder = zstd::stream::write::Encoder::new(Counting { inner: sink, count: 0 }, LEVEL)?;
        encoder.long_distance_matching(true)?;
        encoder.window_log(window_log)?;
        Ok(Packer {
            tar: tar::Builder::new(encoder),
            entries: Vec::new(),
        })
    }

    /// The items listed so far, members and copies, in their order.
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.entries
    }

    /// Packs the `size` bytes of `data` as the file `path`, last written `modified` (seconds since 1970), hashing them
    /// as they pass. Data that ends before `size` bytes fails the pack; bytes past it are left unread.
    pub fn add(&mut self, path: &str, size: u64, modified: u64, data: impl Read) -> io::Result<()> {
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
    pub fn copy(&mut self, path: &str, size: u64, modified: u64, sha256: &str) -> io::Result<()> {
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
    pub fn finish(mut self) -> io::Result<Packed> {
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

/// The header of a plain file of `size` bytes last written `modified`, its path still to be set.
fn file_header(size: u64, modified: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(modified);
    header
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test that cannot make its archives must fail loudly")]
mod tests {
    use std::cell::Cell;
    use std::io::{self, Read, Write};
    use std::rc::Rc;
    use std::time::{Duration, UNIX_EPOCH};

    use gate_router::escaped;

    use super::Packer;
    use crate::archive::tests::{LIMIT, Memory, entry, within};
    use crate::archive::{Entry, Item, LIST_NAME, MEMBER_LIMIT, UnpackError, WINDOW_LOG_MAX, extract, list_json, read_members};
    use crate::manifest::HeldBy;

    /// The last write the round trip packs its members at, in seconds since 1970.
    const PACKED_AT: u64 = 1_600_000_000;

    #[test]
    fn two_members_and_a_copy_packed_come_back_through_the_reader() {
        let (a, b) = (entry("bin/a.dll", b"abc"), entry("bin/sub/b.pdb", b"hello"));
        let copy = Entry {
            path: "bin/c.dll".to_owned(),
            ..a.clone()
        };
        let mut archive = Vec::new();
        let mut packer = Packer::new(&mut archive, WINDOW_LOG_MAX).expect("a packer");
        packer.add(&a.path, 3, PACKED_AT, &b"abc"[..]).expect("a packs");
        packer.add(&b.path, 5, PACKED_AT, &b"hello"[..]).expect("b packs");
        packer.copy(&copy.path, a.size, 1_700_000_000, &a.sha256).expect("the copy is listed");
        let packed = packer.finish().expect("the archive ends");
        let listed = [a.clone(), b.clone(), copy.clone()];
        assert_eq!(packed.entries, listed);
        assert_eq!(packed.compressed, archive.len() as u64);

        let mut memory = Memory::default();
        let read = extract(&archive[..], within(LIMIT), &HeldBy::new(), &|| true, &mut memory).expect("the archive reads back");
        assert_eq!(read, listed, "every path, size and SHA-256 listed");
        assert_eq!(
            memory.members,
            [(a.path.clone(), b"abc".to_vec()), (b.path.clone(), b"hello".to_vec())],
            "each member whole, the list not handed on"
        );
        let packed_at = UNIX_EPOCH + Duration::from_secs(PACKED_AT);
        assert_eq!(
            memory.written,
            [(a.path.clone(), packed_at), (b.path.clone(), packed_at)],
            "each member arrives with the last write it was packed with"
        );
        assert_eq!(memory.copies, [(copy.path.clone(), a.path.clone())], "the copy is made from its member");

        let tar = zstd::decode_all(&archive[..]).expect("the stream decompresses");
        let mut members = tar::Archive::new(&tar[..]);
        let mut read_back = Vec::new();
        for member in members.entries().expect("the tar reads") {
            let mut member = member.expect("the member reads");
            let path = member.path().expect("a path").to_string_lossy().into_owned();
            let mut text = String::new();
            member.read_to_string(&mut text).expect("the member reads whole");
            read_back.push((path, text));
        }
        let (last, list) = read_back.pop().expect("a member");
        assert_eq!(last, LIST_NAME, "the list is the last member");
        assert_eq!(read_back.len(), 2, "the copy is not packed as a member");
        let items = [
            Item { entry: a, copy: None },
            Item { entry: b, copy: None },
            Item {
                entry: copy,
                copy: Some(1_700_000_000),
            },
        ];
        assert_eq!(list, list_json(&items), "the list names each member and the copy with its last write");
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

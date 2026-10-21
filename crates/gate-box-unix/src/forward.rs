//! The readers of a program's pipes, one thread per pipe: each reads its pipe to the end and does with what it read
//! as its [`Take`] says, forwarding each line itself through the box's output under the output lock, one write per
//! line, so the thread that waits for the program never takes that lock and never waits on a client that is not
//! reading. Nothing is queued between a reader and that thread: kept bytes go to a bounded buffer.
//!
//! The drain once the program's group has ended waits [`DRAIN`] for the readers to reach their pipes' ends, then, while
//! a reader is only behind, until [`HANG_GUARD`] from its start, then stops them: a reader still blocked in a read is a
//! leftover process's, and is left behind, never joined; a reader still in a write is held by a client that has
//! stopped reading, which is taken for gone. A stopped reader writes no line more ([`Forwarding::say_unless`]).

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::console::{Forwarding, Stream};

/// How long the drain waits for every reader to reach its pipe's end.
pub const DRAIN: Duration = Duration::from_secs(2);
/// The drain's whole bound, from its start: past [`DRAIN`] it waits only while a reader is still behind.
pub const HANG_GUARD: Duration = Duration::from_secs(30);
/// How often the drain past [`DRAIN`] looks again whether a reader is still behind.
const SLICE: Duration = Duration::from_millis(50);

/// The most bytes of one forwarded line handed on: the rest of a longer line is dropped.
pub const LINE_CAP: usize = 64 * 1024;

/// What a reader does with what it reads.
pub enum Take {
    /// Says each line ([`LineSplitter`]) on the stream through the output.
    Lines(Arc<dyn Forwarding>, Stream),
    /// Keeps the first bytes, this many, and notes whether more came.
    Capped(usize),
    /// Keeps the last bytes, this many.
    Tail(usize),
}

/// What one reader kept: its bytes and whether more came than it keeps; else why its pipe could not be read.
pub type Kept = io::Result<(Vec<u8>, bool)>;

/// The state a reader shares with the drain.
#[derive(Default)]
struct Shared {
    /// The bytes kept, at most the cap.
    kept: Mutex<Vec<u8>>,
    /// Set once more bytes came than [`Take::Capped`] keeps.
    over: AtomicBool,
    /// Why a read failed, when one did.
    failed: Mutex<Option<io::Error>>,
    /// Set once the reader reached its pipe's end or a failed read.
    ended: AtomicBool,
    /// Set while the reader is in a read of its pipe.
    reading: AtomicBool,
    /// Set while the reader holds bytes it read and has not yet kept, written or dropped.
    in_flight: AtomicBool,
    /// Set while the reader is saying a line through the output: waiting on its lock, or in its write.
    writing: AtomicBool,
}

impl Shared {
    /// Whether the reader has not ended and is only behind: it holds bytes it read, or it is not in a read. A reader
    /// in a read of a pipe that holds nothing waits on a process that keeps the pipe open, and is not behind.
    fn behind(&self) -> bool {
        // `in_flight` is set before `reading` is cleared, so a reader leaving its read is seen in one or the other.
        !self.ended.load(Ordering::SeqCst) && (self.in_flight.load(Ordering::SeqCst) || !self.reading.load(Ordering::SeqCst))
    }

    /// What the reader kept so far, or why its read failed.
    fn kept(&self) -> Kept {
        if let Some(error) = self.failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(error);
        }
        let kept = std::mem::take(&mut *self.kept.lock().unwrap_or_else(PoisonError::into_inner));
        Ok((kept, self.over.load(Ordering::SeqCst)))
    }
}

/// The readers of one program's pipes.
pub struct Readers {
    /// Set when the drain gives up on the readers; each looks at it again under the output lock before a line.
    stopped: Arc<AtomicBool>,
    /// Each reader's state, in the order they were started.
    shared: Vec<Arc<Shared>>,
    /// The output the readers forward lines through, when one does: a drain that gives up on a reader in a write marks
    /// its client gone.
    output: Option<Arc<dyn Forwarding>>,
    ended: mpsc::Receiver<()>,
    sender: mpsc::Sender<()>,
}

impl Default for Readers {
    fn default() -> Readers {
        let (sender, ended) = mpsc::channel();
        Readers {
            stopped: Arc::new(AtomicBool::new(false)),
            shared: Vec::new(),
            output: None,
            ended,
            sender,
        }
    }
}

/// What a drain found.
pub struct Drained {
    /// Whether every reader reached its pipe's end; `false` when one was left behind, a leftover process holding its
    /// pipe open, or a stalled client holding its write.
    pub all_ended: bool,
    /// Whether a reader was still in a write when the drain gave up: its client, connected but no longer reading, was
    /// marked gone ([`Forwarding::mark_gone`]).
    pub stalled: bool,
    /// What each reader kept, in the order they were started.
    pub kept: Vec<Kept>,
}

impl Readers {
    /// Starts a reader of `pipe` on a thread of its own, doing with what it reads as `take` says.
    ///
    /// # Errors
    ///
    /// Why the thread could not be started.
    pub fn read(&mut self, pipe: impl Read + Send + 'static, take: Take) -> io::Result<()> {
        let shared = Arc::new(Shared::default());
        if let Take::Lines(output, _) = &take {
            self.output = Some(Arc::clone(output));
        }
        let reader = Reader {
            shared: Arc::clone(&shared),
            stopped: Arc::clone(&self.stopped),
            ended: self.sender.clone(),
            take,
            write_failed: false,
        };
        // The handle is dropped: a reader left blocked in a read is detached, never joined.
        thread::Builder::new()
            .name("gate-box-reader".to_owned())
            .spawn(move || reader.pump(pipe))?;
        self.shared.push(shared);
        Ok(())
    }

    /// Waits [`DRAIN`] for every reader to reach its pipe's end, then, while a reader is still behind, until
    /// [`HANG_GUARD`] from the start; then stops the readers, so none writes a line more, marks the client gone when a
    /// reader is still in a write, and gives what each kept.
    #[must_use]
    pub fn drain(self) -> Drained {
        self.drain_within(DRAIN, HANG_GUARD)
    }

    /// Stops the readers at once, without waiting for any: none writes a line more.
    pub fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// [`Readers::drain`] with `limit` and `guard` in place of [`DRAIN`] and [`HANG_GUARD`].
    fn drain_within(self, limit: Duration, guard: Duration) -> Drained {
        let start = Instant::now();
        let (limit_end, guard_end) = (start + limit, start + guard);
        while !self.all_ended() {
            let left = limit_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // Every reader holds a sender, as does this, so the wait ends by a reader's end or the time.
            let _ = self.ended.recv_timeout(left);
        }
        while !self.all_ended() && self.shared.iter().any(|reader| reader.behind()) {
            let left = guard_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let _ = self.ended.recv_timeout(left.min(SLICE));
        }
        self.stopped.store(true, Ordering::SeqCst);
        let stalled = self
            .shared
            .iter()
            .any(|reader| !reader.ended.load(Ordering::SeqCst) && reader.writing.load(Ordering::SeqCst));
        if stalled && let Some(output) = &self.output {
            output.mark_gone();
        }
        Drained {
            all_ended: self.all_ended(),
            stalled,
            kept: self.shared.iter().map(|reader| reader.kept()).collect(),
        }
    }

    fn all_ended(&self) -> bool {
        self.shared.iter().all(|reader| reader.ended.load(Ordering::SeqCst))
    }
}

/// One pipe's reader.
struct Reader {
    shared: Arc<Shared>,
    stopped: Arc<AtomicBool>,
    ended: mpsc::Sender<()>,
    take: Take,
    /// Set by the first forwarded line whose write failed: no later line is written by this reader.
    write_failed: bool,
}

impl Reader {
    /// Reads `pipe` to its end, doing with each chunk as the reader's [`Take`] says.
    fn pump(mut self, mut pipe: impl Read) {
        let mut lines = LineSplitter::default();
        let mut chunk = [0_u8; 8192];
        loop {
            self.shared.reading.store(true, Ordering::SeqCst);
            let read = pipe.read(&mut chunk);
            self.shared.in_flight.store(true, Ordering::SeqCst);
            self.shared.reading.store(false, Ordering::SeqCst);
            match read {
                Ok(0) => break,
                Ok(read) => self.took(&chunk[..read], &mut lines),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    *self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
                    break;
                }
            }
            self.shared.in_flight.store(false, Ordering::SeqCst);
        }
        lines.finish(|line| self.forward(line));
        self.shared.ended.store(true, Ordering::SeqCst);
        self.shared.in_flight.store(false, Ordering::SeqCst);
        // A drain that stopped waiting has dropped its receiver; nothing is left to tell.
        let _ = self.ended.send(());
    }

    /// Does with `bytes` as the reader's [`Take`] says.
    fn took(&mut self, bytes: &[u8], lines: &mut LineSplitter) {
        match self.take {
            Take::Lines(..) => lines.feed(bytes, |line| self.forward(line)),
            Take::Capped(cap) => {
                let mut kept = self.shared.kept.lock().unwrap_or_else(PoisonError::into_inner);
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
                if bytes.len() > room {
                    self.shared.over.store(true, Ordering::SeqCst);
                }
            }
            Take::Tail(cap) => {
                let mut kept = self.shared.kept.lock().unwrap_or_else(PoisonError::into_inner);
                kept.extend_from_slice(bytes);
                if kept.len() > cap {
                    let cut = kept.len() - cap;
                    kept.drain(..cut);
                }
            }
        }
    }

    /// Says `line` through the output, unless the readers were stopped, a write of this reader's failed before, or the
    /// client is gone, looked at before the output lock is taken, so a reader never waits on the lock to write to no
    /// one; a write that fails marks the client gone in the output and drops every later line.
    fn forward(&mut self, line: &[u8]) {
        let Take::Lines(output, stream) = &self.take else {
            return;
        };
        if self.write_failed || self.stopped.load(Ordering::SeqCst) || output.client_gone() {
            return;
        }
        self.shared.writing.store(true, Ordering::SeqCst);
        let said = output.say_unless(&self.stopped, *stream, &String::from_utf8_lossy(line));
        self.shared.writing.store(false, Ordering::SeqCst);
        if said.is_err() {
            self.write_failed = true;
        }
    }
}

/// Cuts bytes into lines as the Windows box's forwarder does (.NET's `ReadLine`): CR LF, LF and a lone CR each end a
/// line, and the bytes after the last end are a line of their own when there are any. A line keeps its first
/// [`LINE_CAP`] bytes, the rest dropped.
#[derive(Default)]
pub struct LineSplitter {
    line: Vec<u8>,
    after_cr: bool,
}

impl LineSplitter {
    /// Takes `bytes`, giving `emit` each line they end, without its end.
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in bytes {
            let after_cr = std::mem::take(&mut self.after_cr);
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    emit(&self.line);
                    self.line.clear();
                    self.after_cr = byte == b'\r';
                }
                _ if self.line.len() < LINE_CAP => self.line.push(byte),
                _ => {}
            }
        }
    }

    /// Gives `emit` the bytes after the last line end, when there are any.
    pub fn finish(self, mut emit: impl FnMut(&[u8])) {
        if !self.line.is_empty() {
            emit(&self.line);
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test whose program or pipe cannot be made must fail loudly")]
mod tests {
    use std::io::{self, Read};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{LINE_CAP, LineSplitter, Readers, Take};
    use crate::console::{FailingSink, Forwarding, LineSink, Output, Stream};

    /// The lines `bytes`, fed in `chunks` of the given sizes, are cut into.
    fn split(bytes: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::default();
        for part in bytes.chunks(chunk) {
            splitter.feed(part, |line| lines.push(line.to_vec()));
        }
        splitter.finish(|line| lines.push(line.to_vec()));
        lines
    }

    #[test]
    fn lines_end_at_a_line_feed_a_carriage_return_or_both_as_the_windows_box_cuts_them() {
        for chunk in [1, 3, 64] {
            assert_eq!(
                split(b"a\r\nb\nc\rd", chunk),
                [b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
            );
            assert_eq!(split(b"\r\r\n", chunk), [Vec::<u8>::new(), Vec::new()], "a lone CR, then CR LF");
            assert_eq!(split(&[0xff, b'\n'], chunk), [vec![0xff]], "bytes are handed on as they are");
        }
    }

    #[test]
    fn a_line_past_the_line_cap_keeps_its_first_bytes_and_the_next_line_is_whole() {
        let mut bytes = vec![b'x'; LINE_CAP + 10];
        bytes.extend_from_slice(b"\nnext\n");
        assert_eq!(split(&bytes, 8192), [vec![b'x'; LINE_CAP], b"next".to_vec()]);
    }

    /// A sink that writes each line in two halves to one byte stream, yielding between them, so two writers not kept
    /// apart by the output lock would interleave their halves.
    #[derive(Default)]
    struct HalvesSink {
        bytes: Mutex<Vec<u8>>,
    }

    impl LineSink for HalvesSink {
        fn write_line(&self, _stream: Stream, line: &str) -> io::Result<()> {
            let (first, second) = line.split_at(line.len() / 2);
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(first.as_bytes());
            thread::yield_now();
            let mut bytes = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
            bytes.extend_from_slice(second.as_bytes());
            bytes.push(b'\n');
            Ok(())
        }
    }

    /// A pipe that gives `lines` lines naming `tag`, a few bytes per read.
    struct Lines {
        bytes: Vec<u8>,
        at: usize,
    }

    impl Lines {
        fn new(tag: &str, lines: usize) -> Lines {
            let bytes = (0..lines).flat_map(|n| format!("{tag} {n} {}\n", tag.repeat(20)).into_bytes()).collect();
            Lines { bytes, at: 0 }
        }
    }

    impl Read for Lines {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let end = (self.at + 7).min(self.bytes.len()).min(self.at + buffer.len());
            let read = end - self.at;
            buffer[..read].copy_from_slice(&self.bytes[self.at..end]);
            self.at = end;
            Ok(read)
        }
    }

    #[test]
    fn two_readers_forwarding_at_once_write_each_line_whole() {
        const LINES: usize = 2000;
        let output = Arc::new(Output::new(HalvesSink::default()));
        let mut readers = Readers::default();
        for (tag, stream) in [("out", Stream::Out), ("err", Stream::Err)] {
            let forwarding: Arc<dyn Forwarding> = output.clone();
            readers
                .read(Lines::new(tag, LINES), Take::Lines(forwarding, stream))
                .expect("the reader starts");
        }
        let drained = readers.drain();
        assert!(drained.all_ended);

        let bytes = output.sink().bytes.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let text = String::from_utf8(bytes).expect("whole lines are UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2 * LINES);
        for line in lines {
            let tag = &line[..3];
            let rest = line.split_whitespace().nth(2).expect("a line has three words");
            assert_eq!(rest, tag.repeat(20), "a line is whole: {line}");
        }
    }

    /// A pipe that gives one line, then blocks in its read until `release` is told.
    struct Held {
        given: bool,
        release: mpsc::Receiver<()>,
    }

    impl Read for Held {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !std::mem::replace(&mut self.given, true) {
                buffer[..2].copy_from_slice(b"a\n");
                return Ok(2);
            }
            // Blocks as a pipe a leftover holds open does; a dropped sender ends it, as the leftover's end would.
            let _ = self.release.recv();
            buffer[..2].copy_from_slice(b"b\n");
            Ok(2)
        }
    }

    #[test]
    fn a_reader_held_in_its_read_is_left_behind_and_writes_nothing_after_the_drain() {
        let output = Arc::new(Output::new(crate::console::RecordingSink::default()));
        let (release, held) = mpsc::channel();
        let mut readers = Readers::default();
        let forwarding: Arc<dyn Forwarding> = output.clone();
        readers
            .read(Held { given: false, release: held }, Take::Lines(forwarding, Stream::Err))
            .expect("the reader starts");
        let started = Instant::now();
        let drained = readers.drain_within(Duration::from_millis(300), Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(20), "a reader in its read is not behind");
        assert!(!drained.all_ended);

        release.send(()).expect("the reader still waits");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(output.sink().lines(), [(Stream::Err, "a".to_owned())], "no line after the drain");
    }

    /// A sink whose every write waits until `release` is told or dropped: a client that has stopped reading.
    struct Stuck {
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl LineSink for Stuck {
        fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
            let _ = self.release.lock().unwrap_or_else(PoisonError::into_inner).recv();
            Ok(())
        }
    }

    #[test]
    fn a_reader_still_in_a_write_when_the_drain_gives_up_marks_the_client_gone_within_the_whole_bound() {
        let (release, held) = mpsc::channel();
        let output = Arc::new(Output::new(Stuck { release: Mutex::new(held) }));
        let mut readers = Readers::default();
        let forwarding: Arc<dyn Forwarding> = output.clone();
        readers
            .read(&b"a\nb\n"[..], Take::Lines(forwarding, Stream::Err))
            .expect("the reader starts");
        let started = Instant::now();
        let drained = readers.drain_within(Duration::from_millis(100), Duration::from_millis(500));
        assert!(started.elapsed() < Duration::from_secs(20), "the whole bound runs from the drain's start");
        assert!(drained.stalled && !drained.all_ended);
        assert!(output.client_gone());
        drop(release);
    }

    #[test]
    fn a_failed_forward_marks_the_client_gone_and_the_reader_reads_on_to_its_end() {
        let output = Arc::new(Output::new(FailingSink::new(io::ErrorKind::ConnectionReset)));
        let mut readers = Readers::default();
        let forwarding: Arc<dyn Forwarding> = output.clone();
        readers
            .read(Lines::new("err", 50), Take::Lines(forwarding, Stream::Err))
            .expect("the reader starts");
        assert!(readers.drain().all_ended);
        assert!(output.client_gone());
    }

    /// An output whose client is gone and whose lock a test holds: a line said through it waits for the test.
    #[derive(Default)]
    struct GoneAndHeld {
        lock: Mutex<()>,
        said: AtomicUsize,
    }

    impl Forwarding for GoneAndHeld {
        fn say_unless(&self, _stopped: &AtomicBool, _stream: Stream, _line: &str) -> io::Result<()> {
            let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.said.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn client_gone(&self) -> bool {
            true
        }

        fn client_gone_probe(&self) -> bool {
            true
        }

        fn mark_gone(&self) {}
    }

    #[test]
    fn a_reader_whose_client_is_gone_forwards_nothing_and_never_waits_on_the_output_lock() {
        let output = Arc::new(GoneAndHeld::default());
        let held = output.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut readers = Readers::default();
        let forwarding: Arc<dyn Forwarding> = output.clone();
        readers
            .read(&b"a\nb\n"[..], Take::Lines(forwarding, Stream::Err))
            .expect("the reader starts");
        // Far past what a reader that takes no lock needs to reach its pipe's end.
        let drained = readers.drain_within(Duration::from_secs(10), Duration::from_secs(10));
        drop(held);
        assert!(drained.all_ended, "the reader reached its pipe's end while the test held the lock");
        assert!(!drained.stalled);
        assert_eq!(output.said.load(Ordering::SeqCst), 0, "no line is said to a gone client");
    }

    #[test]
    fn a_capped_reader_keeps_its_first_bytes_and_a_tail_reader_its_last() {
        let mut readers = Readers::default();
        readers.read(&b"0123456789"[..], Take::Capped(4)).expect("the reader starts");
        readers.read(&b"0123456789"[..], Take::Tail(4)).expect("the reader starts");
        readers.read(&b"0123"[..], Take::Capped(4)).expect("the reader starts");
        let kept: Vec<(Vec<u8>, bool)> = readers.drain().kept.into_iter().map(|kept| kept.expect("read")).collect();
        assert_eq!(kept, [(b"0123".to_vec(), true), (b"6789".to_vec(), false), (b"0123".to_vec(), false)]);
    }
}

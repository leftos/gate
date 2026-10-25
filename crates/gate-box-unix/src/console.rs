//! Where the box says its lines: stdout for the lines the client parses, stderr for failures and notes. A captured
//! console keeps them instead, so a test reads exactly what a verb said.
//!
//! An [`Output`] may be shared by threads: each line, or each group said together, is written whole under one lock, so
//! no other thread's line splits it, and a write that finds the client gone is remembered.

use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use gate_wire::stamp::{self, Tag};
use jiff::Timestamp;

use crate::forward::HANG_GUARD;

/// How a line is said: as it is, or stamped `<stamp> <tag> <text>` ([`stamp::stamped`]). The caller of each say picks
/// it for the line's source; nothing looks at a line's text to pick it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    /// As it is: a protocol line, and every line of a verb other than `box run`.
    Bare,
    /// Stamped with the moment it is written, taken under the output lock, and this tag.
    Now(Tag),
    /// Stamped with this tag and moment: a process line whose moment is the process's own, such as its start.
    At(Tag, Timestamp),
}

/// The stamp of the box's own lines in `box run`: `gat`, stamped as each is written.
pub const GAT: Stamp = Stamp::Now(Tag::Gat);

impl Stamp {
    /// `line` as this stamp says it.
    fn applied(self, line: &str) -> Cow<'_, str> {
        match self {
            Stamp::Bare => Cow::Borrowed(line),
            Stamp::Now(tag) => Cow::Owned(stamp::stamped(&stamp::now(), tag, line)),
            Stamp::At(tag, at) => Cow::Owned(stamp::stamped(&stamp::local(at), tag, line)),
        }
    }
}

/// How often a say waiting for the output lock tries it again ([`Output::held_within`]).
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// The box's two output streams, written at once or kept for a test.
pub struct Console {
    echo: bool,
    /// The lines said on stdout, when captured.
    pub out: Vec<String>,
    /// The lines said on stderr, when captured.
    pub err: Vec<String>,
}

impl Console {
    /// A console that writes each line to the process's stdout or stderr as it is said.
    #[must_use]
    pub fn stdio() -> Console {
        Console {
            echo: true,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// A console that keeps each line in [`Console::out`] or [`Console::err`].
    #[must_use]
    pub fn captured() -> Console {
        Console {
            echo: false,
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// Says `line` on stdout.
    pub fn out(&mut self, line: &str) {
        if self.echo {
            said(&mut io::stdout().lock(), line);
        } else {
            self.out.push(line.to_owned());
        }
    }

    /// Says `line` on stderr.
    pub fn err(&mut self, line: &str) {
        if self.echo {
            said(&mut io::stderr().lock(), line);
        } else {
            self.err.push(line.to_owned());
        }
    }
}

/// Writes `line` and a newline to `stream` and flushes it, so the client sees each line as it is said.
fn said(stream: &mut impl Write, line: &str) {
    // A stream that cannot be written is a client that has gone: there is no one left to tell, and the verb's status
    // still reaches the ssh session's end.
    let _ = written(stream, line);
}

/// Writes `line` and a newline to `stream` in one write of one buffer, so nothing cuts the line from its end, and
/// flushes it; else why not.
fn written(stream: &mut impl Write, line: &str) -> io::Result<()> {
    let mut whole = String::with_capacity(line.len() + 1);
    whole.push_str(line);
    whole.push('\n');
    stream.write_all(whole.as_bytes())?;
    stream.flush()
}

/// One of the box's two output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

/// Where an [`Output`] writes its lines, from any thread.
pub trait LineSink: Send + Sync {
    /// Writes `line` and a newline to `stream`, flushed, so the client sees it as it is said.
    ///
    /// # Errors
    ///
    /// Why the line could not be written; a client that has gone is [`io::ErrorKind::BrokenPipe`].
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()>;

    /// Whether the sink's reader is seen to have hung up, looked at without writing and without waiting. A sink that
    /// cannot tell answers `false`.
    fn hung_up(&self) -> bool {
        false
    }
}

/// The process's stdout and stderr. A write to a closed pipe fails with `EPIPE` ([`io::ErrorKind::BrokenPipe`]), as
/// a Rust program ignores `SIGPIPE`.
#[derive(Debug, Default)]
pub struct StdioSink;

impl LineSink for StdioSink {
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()> {
        match stream {
            Stream::Out => written(&mut io::stdout().lock(), line),
            Stream::Err => written(&mut io::stderr().lock(), line),
        }
    }

    /// Polls fds 1 and 2 with timeout 0 for `POLLHUP` or `POLLERR`: sshd gives a command pipes, and once the client's
    /// connection is gone it closes their reading ends, which `poll` reports at once though nothing is written.
    #[cfg(unix)]
    fn hung_up(&self) -> bool {
        let mut fds = [1, 2].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLHUP | libc::POLLERR,
            revents: 0,
        });
        // SAFETY: `fds` is an array of two `pollfd` that lives across the call, and its length is passed with it.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, 0) };
        ready > 0 && fds.iter().any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR) != 0)
    }
}

/// What a reader thread forwards each line through: the box's [`Output`], behind a pointer every thread may hold.
pub trait Forwarding: Send + Sync {
    /// Says `line` on `stream` as `stamp` says, unless `stopped` is set or the client is gone, both looked at under the
    /// output lock, so no line is written after the one who set `stopped` says its last.
    ///
    /// # Errors
    ///
    /// Why the line could not be written.
    fn say_unless(&self, stopped: &AtomicBool, stream: Stream, stamp: Stamp, line: &str) -> io::Result<()>;

    /// Whether the client has gone, a write having failed or [`Output::client_gone_probe`] having seen it.
    fn client_gone(&self) -> bool;

    /// [`Output::client_gone_probe`].
    fn client_gone_probe(&self) -> bool;

    /// Marks the client gone without a write: one that is still connected but takes no line.
    fn mark_gone(&self);
}

impl<S: LineSink> Forwarding for Output<S> {
    fn say_unless(&self, stopped: &AtomicBool, stream: Stream, stamp: Stamp, line: &str) -> io::Result<()> {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        if stopped.load(Ordering::SeqCst) || self.gone.is_set() {
            return Ok(());
        }
        self.write_held(stream, stamp, line)
    }

    fn client_gone(&self) -> bool {
        Output::client_gone(self)
    }

    fn client_gone_probe(&self) -> bool {
        Output::client_gone_probe(self)
    }

    fn mark_gone(&self) {
        self.gone.set();
    }
}

/// Whether the client has gone: set by the first write that failed, or by a probe that saw its pipes hung up, and
/// never cleared.
#[derive(Debug, Default)]
pub struct ClientGone(AtomicBool);

impl ClientGone {
    /// Whether a write found the client gone.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Remembers that a write found the client gone.
    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The box's output, shared by its threads: each line, or each group of lines said together, is written whole to the
/// sink under one lock, so no other thread's line comes between them; a write that fails in any way sets
/// [`ClientGone`].
#[derive(Debug)]
pub struct Output<S: LineSink> {
    sink: S,
    lock: Mutex<()>,
    gone: ClientGone,
    /// How long [`Output::say_together`] waits for the lock: [`HANG_GUARD`], shorter in a test.
    lock_bound: Duration,
}

impl<S: LineSink> Output<S> {
    /// An output writing to `sink`, its client not gone.
    #[must_use]
    pub fn new(sink: S) -> Output<S> {
        Output::with_lock_bound(sink, HANG_GUARD)
    }

    /// An output writing to `sink` whose says wait at most `lock_bound` for the lock.
    fn with_lock_bound(sink: S, lock_bound: Duration) -> Output<S> {
        Output {
            sink,
            lock: Mutex::new(()),
            gone: ClientGone::default(),
            lock_bound,
        }
    }

    /// An output writing to `sink` whose says wait at most `lock_bound` for the lock, for a test of a stuck writer.
    #[cfg(test)]
    #[must_use]
    pub fn bounded(sink: S, lock_bound: Duration) -> Output<S> {
        Output::with_lock_bound(sink, lock_bound)
    }

    /// Says `line` on `stream` as `stamp` says.
    ///
    /// # Errors
    ///
    /// Why the line could not be written.
    pub fn say(&self, stream: Stream, stamp: Stamp, line: &str) -> io::Result<()> {
        self.say_together(&[(stream, stamp, line)])
    }

    /// Says `lines` in order, each on its stream as its stamp says, under one hold of the lock, so no other line comes
    /// between them; the first line that
    /// cannot be written ends the group. Once the client is gone, nothing is written: looked at before the lock is
    /// taken, so a gone client never waits on it, and again under it. The lock is waited for at most the drain's
    /// [`HANG_GUARD`]: a thread stuck in a write to a client that is not reading holds it, and once the guard has
    /// passed the client is marked gone and nothing is written ([`Output::held_within`]).
    ///
    /// # Errors
    ///
    /// Why a line could not be written.
    pub fn say_together(&self, lines: &[(Stream, Stamp, &str)]) -> io::Result<()> {
        if self.gone.is_set() {
            return Ok(());
        }
        let Some(_held) = self.held_within(self.lock_bound) else {
            return Ok(());
        };
        if self.gone.is_set() {
            return Ok(());
        }
        for (stream, stamp, line) in lines {
            self.write_held(*stream, *stamp, line)?;
        }
        Ok(())
    }

    /// The output lock, tried every [`LOCK_RETRY`] for at most `bound`, so a thread that decides the run's end never
    /// waits for good behind a write the client is not taking (the heartbeat's or a reader's): past `bound` the client
    /// is marked gone and `None` is given, as it is once the client is seen gone while waiting.
    fn held_within(&self, bound: Duration) -> Option<MutexGuard<'_, ()>> {
        let until = Instant::now() + bound;
        loop {
            match self.lock.try_lock() {
                Ok(held) => return Some(held),
                // A thread that panicked while holding the lock left no state behind it: the lock guards only the writes.
                Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if self.gone.is_set() {
                return None;
            }
            if Instant::now() >= until {
                self.gone.set();
                return None;
            }
            thread::sleep(LOCK_RETRY);
        }
    }

    /// Writes `line` to `stream` as `stamp` says, the lock held by the caller; any failure, a broken pipe or another,
    /// sets [`ClientGone`]: a stream that cannot be written has no client left on it.
    fn write_held(&self, stream: Stream, stamp: Stamp, line: &str) -> io::Result<()> {
        self.sink.write_line(stream, &stamp.applied(line)).inspect_err(|_| self.gone.set())
    }

    /// Whether a write found the client gone, or a probe saw it ([`Output::client_gone_probe`]).
    #[must_use]
    pub fn client_gone(&self) -> bool {
        self.gone.is_set()
    }

    /// Looks whether the sink's reader has hung up ([`LineSink::hung_up`]), without taking the output lock, so a
    /// write the client is not taking never holds the look up; sets [`ClientGone`] when it has. Whether the client is
    /// gone.
    pub fn client_gone_probe(&self) -> bool {
        if self.sink.hung_up() {
            self.gone.set();
        }
        self.gone.is_set()
    }

    /// The sink the lines go to.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }
}

/// A sink that keeps every line and its stream, in the order written, for a test to read.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingSink {
    lines: Mutex<Vec<(Stream, String)>>,
}

#[cfg(test)]
impl RecordingSink {
    /// The lines written so far, with their streams.
    #[must_use]
    pub fn lines(&self) -> Vec<(Stream, String)> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The lines written so far with their streams, each unstamped, the process lines and their note left out: what a
    /// case that is not about stamps or process lines reads.
    #[must_use]
    pub fn said(&self) -> Vec<(Stream, String)> {
        self.lines()
            .into_iter()
            .map(|(stream, line)| (stream, stamp::unstamped(&line).text.to_owned()))
            .filter(|(_, text)| !stamp::is_process_line(text) && !text.starts_with("process lines: "))
            .collect()
    }
}

#[cfg(test)]
impl LineSink for RecordingSink {
    fn write_line(&self, stream: Stream, line: &str) -> io::Result<()> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner).push((stream, line.to_owned()));
        Ok(())
    }
}

/// A sink whose every write fails with an error of its kind, for a test of a client that has gone or a stream that
/// fails otherwise.
#[cfg(test)]
#[derive(Debug)]
pub struct FailingSink {
    kind: io::ErrorKind,
}

#[cfg(test)]
impl FailingSink {
    /// A sink failing every write with `kind`.
    #[must_use]
    pub fn new(kind: io::ErrorKind) -> FailingSink {
        FailingSink { kind }
    }
}

#[cfg(test)]
impl LineSink for FailingSink {
    fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
        Err(io::Error::from(self.kind))
    }
}

/// A sink whose reader has hung up, seen by a probe only: its writes still succeed, as a quiet run never writes.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct HungUpSink;

#[cfg(test)]
impl LineSink for HungUpSink {
    fn write_line(&self, _stream: Stream, _line: &str) -> io::Result<()> {
        Ok(())
    }

    fn hung_up(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    use gate_wire::stamp::{Tag, local, stamped, unstamped};
    use jiff::Timestamp;

    use super::{FailingSink, Forwarding, GAT, HungUpSink, LineSink, Output, RecordingSink, Stamp, Stream, written};

    /// How long a test waits for an event before it calls the case hung: a hang guard only.
    const GUARD: Duration = Duration::from_secs(120);

    /// A stream that keeps the bytes of each write call apart.
    #[derive(Default)]
    struct Calls(Vec<Vec<u8>>);

    impl Write for Calls {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_line_and_its_line_end_go_out_in_one_write() {
        let mut calls = Calls::default();
        assert!(written(&mut calls, "gate-remote-heartbeat: 5").is_ok());
        assert_eq!(calls.0, [b"gate-remote-heartbeat: 5\n".to_vec()]);
    }

    #[test]
    fn a_probe_that_sees_the_reader_hung_up_sets_client_gone_without_a_write() {
        let quiet = Output::new(RecordingSink::default());
        assert!(!quiet.client_gone_probe());
        assert!(!quiet.client_gone());
        let hung_up = Output::new(HungUpSink);
        assert!(!hung_up.client_gone(), "nothing is seen before the probe");
        assert!(hung_up.client_gone_probe());
        assert!(hung_up.client_gone());
    }

    #[test]
    fn a_line_forwarded_once_stopped_is_dropped() {
        let output = Output::new(RecordingSink::default());
        let stopped = AtomicBool::new(false);
        assert!(output.say_unless(&stopped, Stream::Err, Stamp::Bare, "before").is_ok());
        stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(output.say_unless(&stopped, Stream::Err, Stamp::Bare, "after").is_ok());
        assert_eq!(output.sink().lines(), [(Stream::Err, "before".to_owned())]);
    }

    #[test]
    fn a_recording_sink_keeps_each_line_and_its_stream_in_order() {
        let output = Output::new(RecordingSink::default());
        assert!(output.say(Stream::Err, Stamp::Bare, "gate-remote-started: n").is_ok());
        assert!(output.say(Stream::Out, Stamp::Bare, "a note").is_ok());
        assert_eq!(
            output.sink().lines(),
            [(Stream::Err, "gate-remote-started: n".to_owned()), (Stream::Out, "a note".to_owned())]
        );
        assert!(!output.client_gone());
    }

    #[test]
    fn each_say_stamps_its_line_as_its_caller_says_and_a_bare_line_is_written_as_it_is() {
        let output = Output::new(RecordingSink::default());
        let at = Timestamp::constant(1_790_000_000, 0);
        assert!(output.say(Stream::Err, Stamp::Bare, "gate-remote-started: n").is_ok());
        assert!(output.say(Stream::Err, Stamp::At(Tag::Gat, at), "spawn pid=1 ppid=2 sh").is_ok());
        let together = [
            (Stream::Out, Stamp::Now(Tag::Out), "hi"),
            (Stream::Err, Stamp::Bare, "gate-remote-heartbeat: 5"),
        ];
        assert!(output.say_together(&together).is_ok());
        assert!(output.say_unless(&AtomicBool::new(false), Stream::Err, GAT, "gate: a note").is_ok());

        let lines = output.sink().lines();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[0], (Stream::Err, "gate-remote-started: n".to_owned()), "a protocol line is bare");
        assert_eq!(lines[1], (Stream::Err, stamped(&local(at), Tag::Gat, "spawn pid=1 ppid=2 sh")));
        let hi = unstamped(&lines[2].1);
        assert_eq!((lines[2].0, hi.tag, hi.text), (Stream::Out, Some(Tag::Out), "hi"));
        assert_eq!(
            lines[3],
            (Stream::Err, "gate-remote-heartbeat: 5".to_owned()),
            "a bare line said with a stamped one stays bare"
        );
        let note = unstamped(&lines[4].1);
        assert_eq!((note.tag, note.text), (Some(Tag::Gat), "gate: a note"));
    }

    #[test]
    fn lines_said_together_are_never_split_by_another_thread_s_line() {
        const THREADS: usize = 8;
        const GROUPS: usize = 200;
        let output = Output::new(RecordingSink::default());
        thread::scope(|scope| {
            for thread in 0..THREADS {
                let output = &output;
                scope.spawn(move || {
                    for group in 0..GROUPS {
                        let (tag, human) = (format!("tag {thread} {group}"), format!("human {thread} {group}"));
                        assert!(
                            output
                                .say_together(&[(Stream::Err, Stamp::Bare, &tag), (Stream::Err, Stamp::Bare, &human)])
                                .is_ok()
                        );
                    }
                });
            }
        });
        let lines = output.sink().lines();
        assert_eq!(lines.len(), THREADS * GROUPS * 2);
        for pair in lines.chunks(2) {
            let tag = pair[0].1.strip_prefix("tag ");
            assert!(tag.is_some(), "{pair:?}");
            assert_eq!(pair[1].1.strip_prefix("human "), tag, "{pair:?}");
        }
    }

    #[test]
    fn nothing_is_said_once_the_client_is_gone() {
        let output = Output::new(RecordingSink::default());
        output.mark_gone();
        assert!(
            output
                .say_together(&[(Stream::Out, Stamp::Bare, "a"), (Stream::Err, Stamp::Bare, "b")])
                .is_ok()
        );
        assert!(output.say(Stream::Err, Stamp::Bare, "c").is_ok());
        assert!(output.say_unless(&AtomicBool::new(false), Stream::Err, Stamp::Bare, "d").is_ok());
        assert!(output.sink().lines().is_empty(), "{:?}", output.sink().lines());
    }

    /// A sink whose first write says it is inside it and then blocks until the test lets it go; it keeps the lines
    /// written.
    struct HeldSink {
        entered: Mutex<Sender<()>>,
        release: Mutex<Receiver<()>>,
        blocked: AtomicBool,
        lines: Mutex<Vec<String>>,
    }

    impl LineSink for HeldSink {
        fn write_line(&self, _stream: Stream, line: &str) -> io::Result<()> {
            if !self.blocked.swap(true, Ordering::SeqCst) {
                let _ = self.entered.lock().unwrap_or_else(PoisonError::into_inner).send(());
                let _ = self.release.lock().unwrap_or_else(PoisonError::into_inner).recv();
            }
            self.lines.lock().unwrap_or_else(PoisonError::into_inner).push(line.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_say_that_cannot_take_the_lock_in_the_bound_marks_the_client_gone_and_returns() {
        let (entered, inside) = channel();
        let (release, held) = channel();
        let sink = HeldSink {
            entered: Mutex::new(entered),
            release: Mutex::new(held),
            blocked: AtomicBool::new(false),
            lines: Mutex::new(Vec::new()),
        };
        let output = Output::bounded(sink, Duration::from_millis(50));
        thread::scope(|scope| {
            let stuck = scope.spawn(|| output.say_unless(&AtomicBool::new(false), Stream::Err, Stamp::Bare, "stuck"));
            assert!(inside.recv_timeout(GUARD).is_ok(), "the other thread reached its write");
            // A wait for the stuck write would never end: the write is let go only after this returns.
            let said = output.say_together(&[(Stream::Err, Stamp::Bare, "main")]);
            assert!(said.is_ok(), "{said:?}");
            assert!(output.client_gone(), "the client is marked gone");
            let _ = release.send(());
            assert!(stuck.join().is_ok_and(|written| written.is_ok()));
        });
        let lines = output.sink().lines.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(lines, ["stuck"], "nothing of the main thread's is written");
    }

    #[test]
    fn a_write_that_finds_a_broken_pipe_sets_client_gone() {
        let output = Output::new(FailingSink::new(io::ErrorKind::BrokenPipe));
        let failed = output.say(Stream::Err, Stamp::Bare, "gate-remote-heartbeat: 5");
        assert_eq!(failed.map_err(|error| error.kind()), Err(io::ErrorKind::BrokenPipe));
        assert!(output.client_gone());
    }

    #[test]
    fn a_write_that_fails_any_other_way_sets_client_gone_too() {
        for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::NotConnected, io::ErrorKind::Other] {
            let output = Output::new(FailingSink::new(kind));
            assert!(
                output
                    .say_together(&[(Stream::Out, Stamp::Bare, "a"), (Stream::Err, Stamp::Bare, "b")])
                    .is_err()
            );
            assert!(output.client_gone(), "{kind:?}");
        }
    }
}

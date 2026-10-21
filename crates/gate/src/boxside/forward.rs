//! The output of the command and of the processes the box starts around it forwarded: one thread per pipe reads it
//! line by line (CR LF, LF and a lone CR each end a line, the bytes read as UTF-8 with U+FFFD for what is not) and
//! writes each line, LF-terminated, to the box's own stdout or stderr under one lock every such line takes, so no line
//! lands inside another. A write that fails ends that stream's writing and sets the flag a process's readers share, the
//! run's signal that the client closed the connection; its reader goes on draining the pipe, so the process never
//! blocks on a full one. Once the readers are stopped they drop every later line, one already waiting for the lock
//! included. A reader that has not reached the end of its pipe when a drain's limit passes is not taken for one a
//! leftover process still holds open while it is only behind — it holds bytes it has read and not written or dropped,
//! or it is not in a read at all, between reads or still starting — and is waited for up to a hang guard. Only a reader
//! that says it is in its read is ever cancelled: a cancel landing in a thread's start-up can wedge the drain for good. A
//! read still pending on a pipe holding nothing is a leftover's, and is cancelled; the reader is taken for a leftover only if
//! that read ended through the cancellation, a read that completes anyway forwarding its bytes and being judged
//! again, or if its thread exits without saying it reached the end. The threads start no process.

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;

/// How long the readers get, once the command's main process has exited, to reach the end of its output.
pub(crate) const DRAIN: Duration = Duration::from_secs(2);

/// How long, from the start of a drain, a reader that is only behind is still waited for after the drain's limit: the
/// guard that ends a drain still waiting on a reader that would never finish.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// How often, after the limit, a drain looks whether the readers that have not ended are still behind.
const SLICE: Duration = Duration::from_millis(50);

/// Whether `error` is a read the drain cancelled, which is not a read that failed.
fn was_cancelled(error: &io::Error) -> bool {
    error.raw_os_error() == i32::try_from(ERROR_OPERATION_ABORTED).ok()
}

/// The lock every line written to the box's own stdout or stderr from a thread other than the run's own takes: the
/// lines forwarded from the command, the restore and git, and the heartbeat, each written whole under it; the run holds
/// it while the box is away on the command's UTF-8 console, so no such line goes there. The run's own console lines need
/// not take it: each is said on the run's own thread, never while that thread is away, and written with its line feed
/// under std's lock on the stream, which every line written under this lock also takes, so no locked line lands inside
/// one. git, a process of its own, could write inside a heartbeat if it shared the box's stderr, so its output is
/// forwarded through [`Forwarders::start_git`] instead.
pub(crate) type OutputLock = Arc<Mutex<()>>;

/// The run's one client-gone flag: set once a write to the box's own output fails, a forwarded line's or the
/// heartbeat's line or idle probe, since the client's connection is then gone; every wait of the run that watches for a
/// gone client reads it. A box verb with no heartbeat makes one of its own for its readers.
pub(crate) type ClientGone = Arc<AtomicBool>;

/// Which of the box's own streams a pipe's lines go to.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Out => "stdout",
            Stream::Err => "stderr",
        }
    }
}

/// The readers of one process's output, one per pipe.
pub(crate) struct Forwarders {
    stopped: Arc<AtomicBool>,
    /// The run's [`ClientGone`], which a reader whose write to the box's own output failed sets, as the heartbeat does.
    client_gone: ClientGone,
    ended: Receiver<()>,
    /// Set when a reader was taken for a leftover once the limit passed: a read of it ended through the cancellation
    /// the drain asked for, or the reader could not be asked. The drain's answer is not that every reader ended.
    leftover: AtomicBool,
    /// One per reader, in the order the readers started.
    behind: Vec<Behind>,
}

impl Forwarders {
    /// Starts a reader for the command's `stdout` pipe and one for its `stderr` pipe, writing under `lock`, a failed
    /// write setting `gone`.
    ///
    /// # Errors
    ///
    /// The system's error when a thread cannot be started; a reader started before it goes on.
    pub(crate) fn start(stdout: File, stderr: File, lock: &OutputLock, gone: &ClientGone) -> io::Result<Forwarders> {
        Forwarders::start_each([(stdout, Stream::Out), (stderr, Stream::Err)], "the command's", lock, gone)
    }

    /// Starts a reader for each of git's `stdout` and `stderr` pipes that is given, writing its lines to the box's own
    /// stream of its name under `lock`, a failed write setting `gone`; a read that fails is said naming git's pipe. No
    /// pipe given is no reader, and a drain of none finds every reader ended.
    ///
    /// # Errors
    ///
    /// The system's error when a thread cannot be started; a reader started before it goes on.
    pub(crate) fn start_git(stdout: Option<File>, stderr: Option<File>, lock: &OutputLock, gone: &ClientGone) -> io::Result<Forwarders> {
        let pipes = stdout
            .map(|pipe| (pipe, Stream::Out))
            .into_iter()
            .chain(stderr.map(|pipe| (pipe, Stream::Err)));
        Forwarders::start_each(pipes, "git's", lock, gone)
    }

    /// Starts a reader for each of `pipes`, writing its lines to the box's stream it names under `lock`, a failed write
    /// setting `gone`; a read that fails is said naming the pipe `source`'s, as in `the command's stdout`.
    fn start_each(
        pipes: impl IntoIterator<Item = (File, Stream)>,
        source: &'static str,
        lock: &OutputLock,
        gone: &ClientGone,
    ) -> io::Result<Forwarders> {
        let stopped = Arc::new(AtomicBool::new(false));
        let client_gone = Arc::clone(gone);
        let (sender, ended) = mpsc::channel();
        let mut behind = Vec::new();
        for (pipe, stream) in pipes {
            let in_flight = Arc::new(AtomicBool::new(false));
            let reading = Arc::new(AtomicBool::new(false));
            let ended_reader = Arc::new(AtomicBool::new(false));
            let aborted = Arc::new(AtomicBool::new(false));
            let sink: Box<dyn Write + Send> = match stream {
                Stream::Out => Box::new(io::stdout()),
                Stream::Err => Box::new(io::stderr()),
            };
            let forwarder = Forwarder {
                lock: Arc::clone(lock),
                stopped: Arc::clone(&stopped),
                client_gone: Arc::clone(&client_gone),
                stream,
                source,
                sink,
                write_failed: false,
                in_flight: Arc::clone(&in_flight),
                reading: Arc::clone(&reading),
                aborted: Arc::clone(&aborted),
            };
            let sender = sender.clone();
            let reader_ended = Arc::clone(&ended_reader);
            let thread = thread::Builder::new().name(format!("forward {}", stream.name())).spawn(move || {
                forwarder.pump(pipe);
                // Looked at before the send, so a drain asking after the send sees a reader that ended.
                ended_reader.store(true, Ordering::SeqCst);
                // The run stops listening once its drain is over; a reader that ends later has no one to tell.
                let _ = sender.send(());
            })?;
            behind.push(Behind {
                thread,
                in_flight,
                reading,
                ended: reader_ended,
                aborted,
            });
        }
        Ok(Forwarders {
            stopped,
            client_gone,
            ended,
            leftover: AtomicBool::new(false),
            behind,
        })
    }

    /// Whether the run's [`ClientGone`] is set, by a reader's write to the box's own output that failed or by the
    /// heartbeat's: the client's connection is gone, and the run stops the command's job for it.
    pub(crate) fn client_gone(&self) -> bool {
        self.client_gone.load(Ordering::SeqCst)
    }

    /// Waits at most `limit` for every reader to reach the end of its pipe, then stops them, so they drop every later
    /// line; what they reached, and whether a write to the box's own output failed meanwhile, the last moment a
    /// reader can still write.
    ///
    /// A reader that has not ended when `limit` passes is waited for further, up to [`HANG_GUARD`] from the start of
    /// the drain, while it is only behind: it holds bytes it has read and has not written or dropped, or it is not in
    /// a read (see [`Forwarders::any_behind`]). A read still pending on a pipe holding nothing is a leftover's, so
    /// the drain cancels it; the reader is taken for a leftover only if that read ended through the cancellation, a
    /// read that completes anyway forwarding its bytes and being judged again. Once no reader is still behind, or the
    /// guard passes, they are stopped as above.
    pub(crate) fn drain(self, limit: Duration) -> Drained {
        self.drain_within(limit, HANG_GUARD)
    }

    /// [`Forwarders::drain`] with a `guard` in place of [`HANG_GUARD`], so a case can watch a reader that is kept
    /// behind stopped by it.
    fn drain_within(self, limit: Duration, guard: Duration) -> Drained {
        let start = Instant::now();
        let limit_end = start + limit;
        let guard_end = start + guard;
        while !self.all_ended() {
            let left = limit_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.ended.recv_timeout(left) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every reader's sender is gone, so no reader will ever say it ended again.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        while !self.all_ended() {
            let left = guard_end.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.any_behind() {
                break;
            }
            match self.ended.recv_timeout(left.min(SLICE)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.stopped.store(true, Ordering::SeqCst);
        Drained {
            all_ended: self.all_ended(),
            client_gone: self.client_gone.load(Ordering::SeqCst),
        }
    }

    /// Whether every reader reached the end of its pipe, none of them ending through a cancellation the drain asked
    /// for and none left unresolved.
    fn all_ended(&self) -> bool {
        !self.leftover.load(Ordering::SeqCst) && self.behind.iter().all(Behind::reached_end)
    }

    /// Whether a reader that has not ended is only behind, telling a reader that holds bytes or is not in a read
    /// (between reads, or its thread still starting) from one in its read: a read still pending there on a pipe holding
    /// nothing is one only a process the exited command left keeps open, so it is cancelled, and the pump records
    /// whether it ended through the cancellation. A reader not in its read is never cancelled, since a cancel landing
    /// in a thread's start-up or in a write can wedge the drain or cut the write short. `false` once every reader has
    /// ended or been taken for a leftover, which is when the drain stops.
    fn any_behind(&self) -> bool {
        let mut behind = false;
        for reader in &self.behind {
            if reader.resolved() {
                continue;
            }
            if reader.in_flight.load(Ordering::SeqCst) || !reader.reading.load(Ordering::SeqCst) {
                behind = true;
                continue;
            }
            // The reader is in its read, or just past it: the read can still complete with bytes to forward, or end
            // through the cancellation, which the pump marks. Either way the reader is waited for, and judged again. A
            // reader that cannot be asked is taken for a leftover, as one that did not end in time was before.
            if gate_win32::cancel_synchronous_io(&reader.thread).is_err() {
                self.leftover.store(true, Ordering::SeqCst);
                return false;
            }
            behind = true;
        }
        behind
    }
}

/// A reader a drain that has passed its limit watches: its thread, to tell a pending read from one not yet made, and
/// what it holds.
struct Behind {
    /// The reader's thread, whose handle the drain cancels a pending read on, and whose settling says the reader
    /// ended even if it never said so.
    thread: JoinHandle<()>,
    /// Set while the reader holds bytes it has read and has not written or dropped yet.
    in_flight: Arc<AtomicBool>,
    /// Set while the reader is in a read of its pipe: the only time the drain may cancel it.
    reading: Arc<AtomicBool>,
    /// Set once the reader reached the end of its pipe, or failed to read it, and is about to say so.
    ended: Arc<AtomicBool>,
    /// Set by the pump when a read ended through the drain's cancellation: a leftover held its pipe open.
    aborted: Arc<AtomicBool>,
}

impl Behind {
    /// Whether the reader ended, it having said so or its thread having exited without saying anything: either way
    /// the drain stops waiting on it.
    fn resolved(&self) -> bool {
        self.ended.load(Ordering::SeqCst) || self.thread.is_finished()
    }

    /// Whether the reader said it reached the end of its pipe, not having ended through a read the drain cancelled.
    /// A reader whose thread exited without saying so reached no end the drain can see.
    fn reached_end(&self) -> bool {
        !self.aborted.load(Ordering::SeqCst) && self.ended.load(Ordering::SeqCst)
    }
}

/// What one [`Forwarders::drain`] found.
pub(crate) struct Drained {
    /// Whether every reader reached the end of its pipe, within the limit or, behind, before the hang guard: it does
    /// not when a read ended through a cancellation the drain asked for — a process the forwarded process left
    /// running holding its pipe open — when a reader's thread exited without saying it reached the end, or when a
    /// reader could not be asked.
    pub(crate) all_ended: bool,
    /// Whether the run's [`ClientGone`] was set meanwhile, by a reader's write to the box's own output that failed or
    /// by the heartbeat's: the client's connection is gone.
    pub(crate) client_gone: bool,
}

/// One pipe's reader: where its lines go, and whether its writing has ended.
struct Forwarder {
    lock: OutputLock,
    stopped: Arc<AtomicBool>,
    /// The run's [`ClientGone`]: set when a write to the box's own output fails.
    client_gone: ClientGone,
    stream: Stream,
    /// Whose pipe it reads, as a read failure names it: `the command's`, `git's`.
    source: &'static str,
    /// The box's stream the lines go to, `stream`'s.
    sink: Box<dyn Write + Send>,
    /// Set by the first write to the box's stream that fails; no later line is written by this reader.
    write_failed: bool,
    /// Shared with the drain: set while this reader holds bytes it has read and has not written or dropped yet, and
    /// across every write it makes to the box's own output, so the drain never cancels one.
    in_flight: Arc<AtomicBool>,
    /// Shared with the drain: set just before each read of the pipe and cleared as soon as it returns, so the drain
    /// cancels nothing but a read.
    reading: Arc<AtomicBool>,
    /// Shared with the drain: set when a read ended through the drain's cancellation, which is not a read failure.
    aborted: Arc<AtomicBool>,
}

impl Forwarder {
    /// Reads `pipe` to its end, writing each line as it is read. A read that fails ends the reading, said on stderr;
    /// a read the drain cancelled is the drain's own doing and is not said.
    fn pump(mut self, mut pipe: File) {
        let mut lines = LineSplitter::default();
        let mut chunk = [0_u8; 4096];
        loop {
            self.reading.store(true, Ordering::SeqCst);
            let result = pipe.read(&mut chunk);
            self.reading.store(false, Ordering::SeqCst);
            match result {
                Ok(0) => break,
                Ok(read) => {
                    self.in_flight.store(true, Ordering::SeqCst);
                    lines.feed(&chunk[..read], |line| self.write(line));
                    self.in_flight.store(false, Ordering::SeqCst);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The drain cancelled this read: only a read that ended through it is a leftover's, and there is
                // nothing more to read of it.
                Err(error) if was_cancelled(&error) => {
                    self.aborted.store(true, Ordering::SeqCst);
                    break;
                }
                Err(error) => {
                    // Held while the failure is written, so the drain never cancels a write of the reader's.
                    self.in_flight.store(true, Ordering::SeqCst);
                    self.say_read_failure(&error);
                    self.in_flight.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
        self.in_flight.store(true, Ordering::SeqCst);
        lines.finish(|line| self.write(line));
        self.in_flight.store(false, Ordering::SeqCst);
    }

    /// Writes `line` and a line feed to the box's stream and flushes it, unless the readers were stopped or a write
    /// failed before.
    fn write(&mut self, line: &[u8]) {
        if self.write_failed || self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut text = String::from_utf8_lossy(line).into_owned();
        text.push('\n');
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        // Looked at again under the lock: the readers may have been stopped while this one waited for it.
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        if write_flushed(&mut self.sink, text.as_bytes()).is_err() {
            self.write_failed = true;
            self.client_gone.store(true, Ordering::SeqCst);
        }
    }

    /// Says on stderr that the pipe could not be read; when stderr cannot take it either, nothing is left to say it on.
    fn say_read_failure(&self, error: &io::Error) {
        let line = format!("gate: remote could not read {} {}: {error}\n", self.source, self.stream.name());
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = write_flushed(&mut io::stderr().lock(), line.as_bytes());
    }
}

fn write_flushed(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)?;
    stream.flush()
}

/// Cuts bytes into lines as .NET's `ReadLine` does: CR LF, LF and a lone CR each end a line, and the bytes after the
/// last end are a line of their own when there are any.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    after_cr: bool,
}

impl LineSplitter {
    /// Takes `bytes`, giving `emit` each line they end, without its end.
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in bytes {
            let after_cr = std::mem::take(&mut self.after_cr);
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    emit(&self.line);
                    self.line.clear();
                    self.after_cr = byte == b'\r';
                }
                _ => self.line.push(byte),
            }
        }
    }

    /// Gives `emit` the bytes after the last line end, when there are any.
    fn finish(self, mut emit: impl FnMut(&[u8])) {
        if !self.line.is_empty() {
            emit(&self.line);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{self, PipeReader, PipeWriter, Read, Write};
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    use windows_sys::Win32::System::Threading::{ResumeThread, SuspendThread};

    use super::{Behind, ClientGone, Forwarder, Forwarders, LineSplitter, OutputLock, Stream};

    /// How long the stop case lets its reader run before it stops the readers, so the reader is past its first look at
    /// the stop and waits for the lock: only the case's power to go red rests on it. A reader slower than this looks
    /// after the stop and writes nothing either way, so no delay can turn the case red.
    const HEAD_START: Duration = Duration::from_millis(200);

    /// A sink that keeps every byte written to it.
    #[derive(Clone, Default)]
    struct Recording(Arc<Mutex<Vec<u8>>>);

    impl Write for Recording {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// One reader of `pipe`, its lines written to `sink`, wired as [`Forwarders::start_each`] wires a reader: the
    /// drain can be watched on it, and the flags it reports through are its own.
    fn one_reader(pipe: File, sink: Box<dyn Write + Send>, lock: &OutputLock) -> Forwarders {
        let stopped = Arc::new(AtomicBool::new(false));
        let client_gone = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicBool::new(false));
        let reading = Arc::new(AtomicBool::new(false));
        let reader_ended = Arc::new(AtomicBool::new(false));
        let aborted = Arc::new(AtomicBool::new(false));
        let forwarder = Forwarder {
            lock: Arc::clone(lock),
            stopped: Arc::clone(&stopped),
            client_gone: Arc::clone(&client_gone),
            stream: Stream::Err,
            source: "the test's",
            sink,
            write_failed: false,
            in_flight: Arc::clone(&in_flight),
            reading: Arc::clone(&reading),
            aborted: Arc::clone(&aborted),
        };
        let (sender, ended_rx) = mpsc::channel();
        let ended_by_close = Arc::clone(&reader_ended);
        let thread = thread::spawn(move || {
            forwarder.pump(pipe);
            ended_by_close.store(true, Ordering::SeqCst);
            let _ = sender.send(());
        });
        Forwarders {
            stopped,
            client_gone,
            ended: ended_rx,
            leftover: AtomicBool::new(false),
            behind: vec![Behind {
                thread,
                in_flight,
                reading,
                ended: reader_ended,
                aborted,
            }],
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose reader thread fails must fail loudly")]
    fn a_reader_waiting_for_the_lock_when_the_readers_stop_writes_nothing() {
        let lock = OutputLock::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let written = Recording::default();
        let mut forwarder = Forwarder {
            lock: Arc::clone(&lock),
            stopped: Arc::clone(&stopped),
            client_gone: Arc::default(),
            stream: Stream::Err,
            source: "the test's",
            sink: Box::new(written.clone()),
            write_failed: false,
            in_flight: Arc::default(),
            reading: Arc::default(),
            aborted: Arc::default(),
        };
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (writing, started) = mpsc::channel();
        let reader = thread::spawn(move || {
            writing.send(()).expect("the case listens until the reader starts");
            forwarder.write(b"late");
        });
        started.recv().expect("the reader starts");
        thread::sleep(HEAD_START);

        stopped.store(true, Ordering::SeqCst);
        drop(held);
        reader.join().expect("the reader does not panic");

        assert!(
            written.0.lock().unwrap_or_else(PoisonError::into_inner).is_empty(),
            "a line that waited for the lock while the readers stopped is dropped"
        );
    }

    /// The drain's limit in the cases: far shorter than [`HOLD`], so the reader is still behind when it passes.
    const LIMIT: Duration = Duration::from_millis(100);

    /// How long the case keeps the output lock from the reader, which has by then read the line and waits to write it.
    const HOLD: Duration = Duration::from_secs(1);

    /// The guard the case gives [`Forwarders::drain_within`] in place of [`super::HANG_GUARD`].
    const GUARD: Duration = Duration::from_secs(1);

    /// The longest the guard case waits for its reader to hold the line it read, and then for its drain to return: it
    /// never runs out unless the reader never reads the line or the drain waits for it past the guard.
    const HOLDS_LINE_GUARD: Duration = Duration::from_secs(300);

    /// How long the starved case keeps its reader suspended, so the read the kernel completed for it is not picked up
    /// before the drain looks.
    const SUSPENDED: Duration = Duration::from_secs(1);

    /// The bound every wait in the starved case gets, so a regression there fails rather than hangs.
    const CASE_GUARD: Duration = Duration::from_secs(10);

    /// How long the blocking-write case lets the reader wait in its write before the case reads the sink, letting the
    /// write finish.
    const RELEASE: Duration = Duration::from_secs(1);

    /// How long the line the blocking-write case writes is: far more than any pipe holds, so the reader's write of it
    /// blocks in the kernel.
    const LONG_LINE: usize = 1 << 20;

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_still_behind_at_the_drain_limit_on_a_closed_pipe_is_waited_for_not_taken_for_a_leftover() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        writer.write_all(b"last\n").expect("the process's last line is written");
        // The only write end is closed: no process can hold the pipe open, as with a command that exited alone.
        drop(writer);
        let lock = OutputLock::default();
        let (held_tx, held_rx) = mpsc::channel();
        let holder = {
            let lock = lock.clone();
            thread::spawn(move || {
                let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
                held_tx.send(()).expect("the case waits for the hold");
                thread::sleep(HOLD);
            })
        };
        held_rx.recv().expect("the lock is held");
        let pipe = File::from(OwnedHandle::from(reader));
        let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock, &ClientGone::default()).expect("the reader starts");

        let drained = forwarders.drain(LIMIT);

        holder.join().expect("the holder does not panic");
        assert!(
            drained.all_ended,
            "a reader that was only behind, its pipe's write end closed, reaches the end of what the process wrote"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_idle_on_a_pipe_a_stand_in_holds_open_is_still_taken_for_a_leftover() {
        // The drain runs on a thread of its own, so a drain that never returns fails the case rather than hanging it.
        let (finished, verdict) = mpsc::channel();
        thread::spawn(move || {
            let (reader, writer) = io::pipe().expect("the pipe is made");
            let lock = OutputLock::default();
            let pipe = File::from(OwnedHandle::from(reader));
            let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock, &ClientGone::default()).expect("the reader starts");
            let aborted = Arc::clone(&forwarders.behind[0].aborted);

            let drained = forwarders.drain(LIMIT);

            drop(writer);
            let _ = finished.send((drained.all_ended, aborted.load(Ordering::SeqCst)));
        });
        let (all_ended, aborted) = verdict.recv_timeout(START_UP_GUARD).expect("the drain returns");
        assert!(!all_ended, "a pipe a leftover still holds open with nothing to read is a leftover");
        assert!(aborted, "the reader ended through the drain's cancellation of its read");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_kept_behind_past_the_hang_guard_is_stopped_not_waited_for_forever() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        let peek = reader.try_clone().expect("the pipe's read end can be shared");
        let lock = OutputLock::default();
        // Held for the whole drain, so the reader, once it has read the line, waits to write it: it never finishes.
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // The write end stays open until the drain is over, as a process still running holds it.
        writer.write_all(b"more\n").expect("the line is written");
        let pipe = File::from(OwnedHandle::from(reader));
        let forwarders = Forwarders::start_each([(pipe, Stream::Err)], "the test's", &lock, &ClientGone::default()).expect("the reader starts");
        // A reader not yet past its read is not behind but in its read, which the drain cancels after the limit: the
        // case drains only once the reader holds the line, read out of the pipe (a failed read takes nothing from it),
        // and waits to write it.
        let holding = Instant::now() + HOLDS_LINE_GUARD;
        while !(forwarders.behind[0].in_flight.load(Ordering::SeqCst) && bytes_waiting(&peek) == 0) {
            assert!(!forwarders.behind[0].thread.is_finished(), "the reader ended before holding the line");
            assert!(Instant::now() < holding, "the reader reads the line and holds it");
            thread::sleep(Duration::from_millis(10));
        }
        // The drain runs on a thread of its own, so a drain that waited for the reader forever fails the case rather
        // than hanging it.
        let (finished, verdict) = mpsc::channel();
        let started = Instant::now();
        thread::spawn(move || {
            let drained = forwarders.drain_within(LIMIT, GUARD);
            let _ = finished.send((drained.all_ended, started.elapsed()));
        });
        let (all_ended, elapsed) = verdict.recv_timeout(HOLDS_LINE_GUARD).expect("the guard ends the drain");

        drop(writer);
        drop(held);
        assert!(!all_ended, "a reader kept behind past the guard is stopped with its bytes unread");
        assert!(
            elapsed >= GUARD,
            "the drain waits for a reader that is behind until the guard passes: {elapsed:?}"
        );
    }

    /// How many bytes the pipe whose read end `peek` shares holds unread.
    fn bytes_waiting(peek: &PipeReader) -> u32 {
        let mut available = 0_u32;
        // SAFETY: `peek`'s handle is open for the whole call, which reads no data and writes only `available`.
        let peeked = unsafe {
            PeekNamedPipe(
                peek.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(peeked, 0, "the pipe can be peeked: {}", io::Error::last_os_error());
        available
    }

    /// How many readers each drain of the start-up case judges: they start together, so its first look finds several
    /// still in their thread's start-up, where a cancel can wedge the drain.
    const READERS_PER_DRAIN: usize = 64;

    /// How many such drains each driver thread runs.
    const DRAINS_PER_DRIVER: usize = 25;

    /// How many threads run drains side by side, so the case's wall time stays short.
    const DRIVERS: usize = 8;

    /// The longest the start-up case waits for its drains: it never runs out unless a drain is wedged.
    const START_UP_GUARD: Duration = Duration::from_secs(600);

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_drain_begun_as_its_readers_start_cancels_their_reads_and_never_wedges() {
        let (done, finished) = mpsc::channel();
        for _ in 0..DRIVERS {
            let done = done.clone();
            thread::spawn(move || {
                for _ in 0..DRAINS_PER_DRIVER {
                    let mut writers: Vec<PipeWriter> = Vec::new();
                    let mut pipes = Vec::new();
                    for _ in 0..READERS_PER_DRAIN {
                        let (reader, writer) = io::pipe().expect("the pipe is made");
                        writers.push(writer);
                        pipes.push((File::from(OwnedHandle::from(reader)), Stream::Err));
                    }
                    let lock = OutputLock::default();
                    let forwarders = Forwarders::start_each(pipes, "the test's", &lock, &ClientGone::default()).expect("the readers start");
                    // No limit: the drain judges the readers, and cancels a read one waits on, from their start on.
                    let drained = forwarders.drain_within(Duration::ZERO, START_UP_GUARD);
                    drop(writers);
                    assert!(!drained.all_ended, "readers idle on pipes a stand-in holds open are leftovers");
                }
                // The case listens until every driver has said it finished, or the guard passes.
                let _ = done.send(());
            });
        }
        drop(done);
        for _ in 0..DRIVERS {
            finished
                .recv_timeout(START_UP_GUARD)
                .expect("every drain returns: none is left inside the cancel of a reader that had not reached its first read");
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_starved_after_its_read_completed_is_waited_for_and_forwards_the_line() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        // The channel has been used once before the reader starts, so nothing the case sends once the reader may
        // hold the heap allocates.
        let (go, go_rx) = mpsc::channel();
        go.send(()).expect("the channel is primed");
        let lock = OutputLock::default();
        let written = Recording::default();
        let forwarders = one_reader(File::from(OwnedHandle::from(reader)), Box::new(written.clone()), &lock);
        // The resumer holds the reader and lets it go itself: nothing it does between the two allocates, so a reader
        // suspended inside the heap cannot keep it from resuming. It says which it did in `held`.
        let handle_number = forwarders.behind[0].thread.as_raw_handle() as usize;
        let held = Arc::new(AtomicU8::new(0));
        let resumer = {
            let held = Arc::clone(&held);
            thread::spawn(move || {
                go_rx.recv().expect("the channel's prime is taken");
                go_rx.recv().expect("the case says the first line is forwarded");
                let handle = std::ptr::with_exposed_provenance_mut::<std::ffi::c_void>(handle_number);
                // SAFETY: the join handle keeping this thread's handle open lives in the forwarders on the test
                // thread until the drain returns, well after this resume.
                let suspended = unsafe { SuspendThread(handle) } != u32::MAX;
                held.store(if suspended { 1 } else { 2 }, Ordering::SeqCst);
                thread::sleep(SUSPENDED);
                // SAFETY: as above.
                unsafe { ResumeThread(handle) };
            })
        };
        // The reader forwards one line first, so the read it is held in is the one the second line completes.
        writer.write_all(b"first\n").expect("the first line is written");
        let forwarded = Instant::now() + CASE_GUARD;
        while written.0.lock().unwrap_or_else(PoisonError::into_inner).as_slice() != b"first\n".as_slice() {
            assert!(Instant::now() < forwarded, "the first line is forwarded");
            thread::sleep(Duration::from_millis(1));
        }
        go.send(()).expect("the resumer is told");
        let holding = Instant::now() + CASE_GUARD;
        while held.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < holding, "the reader is held");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(held.load(Ordering::SeqCst), 1, "the reader is suspended");
        writer.write_all(b"second\n").expect("the second line is written");
        // The command exited: no process holds the pipe open.
        drop(writer);

        let drained = forwarders.drain_within(LIMIT, CASE_GUARD);

        resumer.join().expect("the resumer does not panic");
        assert!(
            drained.all_ended,
            "a reader starved before writing what it read is waited for, not taken for a leftover"
        );
        assert_eq!(
            written.0.lock().unwrap_or_else(PoisonError::into_inner).as_slice(),
            b"first\nsecond\n",
            "both lines the reader forwarded are written"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_blocked_in_a_write_is_waited_for_not_cancelled() {
        let (line_reader, mut line_writer) = io::pipe().expect("the line's pipe is made");
        let (sink_reader, sink_writer) = io::pipe().expect("the sink's pipe is made");
        let lock = OutputLock::default();
        let forwarders = one_reader(File::from(OwnedHandle::from(line_reader)), Box::new(sink_writer), &lock);
        // Far more than any pipe holds, so the reader's write of it blocks in the kernel.
        let mut line = vec![b'x'; LONG_LINE];
        line.push(b'\n');
        line_writer.write_all(&line).expect("the line is written");
        // The command exited: its pipe's write end is closed, so the reader blocks in writing the line on, only.
        drop(line_writer);
        // Reads the sink from the release on, so the reader's blocked write finishes once the drain has waited a while.
        let released = thread::spawn(move || {
            thread::sleep(RELEASE);
            let mut sink_reader = sink_reader;
            let mut got = Vec::new();
            let mut chunk = vec![0_u8; 64 * 1024];
            loop {
                match sink_reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => got.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            got
        });

        let drained = forwarders.drain(LIMIT);

        let got = released.join().expect("the reader of the sink does not panic");
        assert!(drained.all_ended, "a reader blocked in a write is waited for, not taken for a leftover");
        assert!(
            !drained.client_gone,
            "the drain waited for the reader's write instead of cutting it short"
        );
        assert_eq!(got, line, "the line the reader waited to write is forwarded whole");
    }

    /// How long the not-reading case lets its writer get into its blocked write before the drain judges it: only the
    /// case's power to go red rests on it, since a drain that judges the writer sooner cancels nothing either way.
    const INTO_THE_WRITE: Duration = Duration::from_millis(200);

    #[test]
    #[expect(clippy::expect_used, reason = "a test whose pipe or thread cannot be made must fail loudly")]
    fn a_reader_past_its_read_and_blocked_in_a_write_is_not_cancelled_and_the_client_is_not_taken_for_gone() {
        let (mut sink_reader, sink_writer) = io::pipe().expect("the sink's pipe is made");
        let client_gone = Arc::new(AtomicBool::new(false));
        let mut forwarder = Forwarder {
            lock: OutputLock::default(),
            stopped: Arc::default(),
            client_gone: Arc::clone(&client_gone),
            stream: Stream::Err,
            source: "the test's",
            sink: Box::new(sink_writer),
            write_failed: false,
            in_flight: Arc::default(),
            reading: Arc::default(),
            aborted: Arc::default(),
        };
        let line = vec![b'x'; LONG_LINE];
        let writing = line.clone();
        // Its read done, the reader writes a line far longer than the sink's pipe holds, so the write blocks in the
        // kernel; neither its `in_flight` nor its `reading` is set, the moment between the two a drain can land in.
        let thread = thread::spawn(move || forwarder.write(&writing));
        thread::sleep(INTO_THE_WRITE);
        let (_, ended) = mpsc::channel::<()>();
        let forwarders = Forwarders {
            stopped: Arc::default(),
            client_gone: Arc::clone(&client_gone),
            ended,
            leftover: AtomicBool::new(false),
            behind: vec![Behind {
                thread,
                in_flight: Arc::default(),
                reading: Arc::default(),
                ended: Arc::default(),
                aborted: Arc::default(),
            }],
        };

        assert!(forwarders.any_behind(), "a reader that is not in its read is waited for");

        let mut got = Vec::new();
        sink_reader.read_to_end(&mut got).expect("the sink is read to the writer's end");
        assert!(
            !client_gone.load(Ordering::SeqCst),
            "the drain cut no write short, so the client is not taken for gone"
        );
        let mut expected = line;
        expected.push(b'\n');
        assert_eq!(got, expected, "the line the reader was writing is forwarded whole");
    }

    #[test]
    fn a_reader_whose_thread_exited_without_saying_it_ended_is_not_counted_as_ended() {
        let thread = thread::spawn(|| {});
        let deadline = Instant::now() + CASE_GUARD;
        while !thread.is_finished() {
            assert!(Instant::now() < deadline, "the reader's thread exits");
            thread::sleep(Duration::from_millis(1));
        }
        // The channel is disconnected and no end signal is set: the drain stops waiting on the reader, and, its
        // thread having exited without saying it reached the end, counts it as not ended.
        let (_, ended) = mpsc::channel::<()>();
        let forwarders = Forwarders {
            stopped: Arc::default(),
            client_gone: Arc::default(),
            ended,
            leftover: AtomicBool::new(false),
            behind: vec![Behind {
                thread,
                in_flight: Arc::default(),
                reading: Arc::default(),
                ended: Arc::default(),
                aborted: Arc::default(),
            }],
        };
        let started = Instant::now();

        let drained = forwarders.drain(LIMIT);

        assert!(
            !drained.all_ended,
            "a reader whose thread exited without saying it ended is not counted as ended"
        );
        assert!(started.elapsed() < CASE_GUARD, "the drain returns without spinning");
    }

    /// The lines `chunks` read one after another give, each read as UTF-8 with U+FFFD for what is not.
    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::default();
        for chunk in chunks {
            splitter.feed(chunk, |line| lines.push(String::from_utf8_lossy(line).into_owned()));
        }
        splitter.finish(|line| lines.push(String::from_utf8_lossy(line).into_owned()));
        lines
    }

    #[test]
    fn a_cr_lf_split_across_two_reads_ends_one_line() {
        assert_eq!(split(&[b"a\r", b"\nb"]), ["a", "b"]);
    }

    #[test]
    fn a_cr_then_a_cr_lf_end_two_lines() {
        assert_eq!(split(&[b"\r\r\n"]), ["", ""]);
    }

    #[test]
    fn the_bytes_after_the_last_end_are_a_line() {
        assert_eq!(split(&[b"x"]), ["x"]);
    }

    #[test]
    fn an_invalid_byte_reads_as_the_replacement_character() {
        assert_eq!(split(&[&[0xff, b'\n']]), ["\u{FFFD}"]);
    }
}

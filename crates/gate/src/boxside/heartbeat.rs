//! `gate box run`'s heartbeat: `gate-remote-heartbeat: <cpu ms>` on stderr every period, on `gate-watchdog`'s
//! [`Pump`], so the client knows a quiet command is still working. While no command is watched the pump writes every
//! period; while one is, only when the CPU its job used since the last beat reaches 1% of the time since it, as the
//! PowerShell wrapper's heartbeat counts it, or, for a WSL command, the WSL VM's CPU reaches a tenth of it: the VM's
//! work runs outside the job. The command's build servers are its run's own and run in its job, so the job's CPU counts
//! them. Each line takes the box's output lock, so none lands inside a forwarded line. On a period with no line due the
//! pump flushes, and the box's flush probes stderr with a zero-byte write under the same lock; a line or a probe that
//! fails because stderr's reader has gone sets the run's [`ClientGone`], so the run sees a gone client within a period
//! even while its command writes nothing. A probe that fails any other way ends the probing for the rest of the run and
//! the lines go on; any other failed line or flush ends the heartbeats. Neither sets the flag. The heartbeat starts no
//! process.

use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_watchdog::{CpuSource, Pump, Ticker};
use gate_win32::Job;
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::WriteFile;

use crate::boxside::forward::{ClientGone, OutputLock};

/// How often the heartbeat looks, and so the most often it writes.
const PERIOD: Duration = Duration::from_secs(5);

/// The nanoseconds in one 100 ns tick.
const NANOS_PER_TICK: u128 = 100;

/// The heartbeat's period: the seam's milliseconds when given, else [`PERIOD`].
pub(crate) fn period(seams: &BoxSeams) -> Duration {
    seams.heartbeat_ms.map_or(PERIOD, Duration::from_millis)
}

/// Starts the heartbeat on stderr, every line and probe under `lock`, looking every `period`; a line or a probe that
/// fails sets `gone`.
///
/// # Errors
///
/// Why the heartbeat's thread could not be started.
pub(crate) fn start(lock: &OutputLock, gone: &ClientGone, period: Duration) -> io::Result<Pump> {
    let writer = Locked {
        lock: Arc::clone(lock),
        gone: Arc::clone(gone),
        sink: io::stderr(),
        probing: true,
    };
    Pump::start(writer, Arc::new(BoxClock { started: Instant::now() }), period)
}

/// The CPU source of the command running in `job`: the job's own CPU, its processes that ended included, and for a
/// `wsl` command the WSL VM's, where its work runs.
pub(crate) fn job_cpu(job: Arc<Job>, wsl: bool) -> Arc<dyn CpuSource> {
    Arc::new(JobCpu { job, wsl })
}

/// The box's monotonic clock in 100 ns ticks since the heartbeat started.
struct BoxClock {
    started: Instant,
}

impl Ticker for BoxClock {
    fn now_ticks(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos() / NANOS_PER_TICK).unwrap_or(u64::MAX)
    }
}

/// A writer that holds the box's output lock for each write and gives `sink` the whole buffer in one `write_all`: on
/// [`io::Stderr`], one write of the whole line on the locked stream, so a heartbeat never lands inside a line the forwarders
/// write under the same lock. Its flush, under the same lock, flushes `sink` and then probes it. A write, a flush or a
/// probe that fails because the reader has gone ([`reader_gone`]) sets `gone`: the client's connection is gone. A probe
/// that fails any other way is the last: the flush passes and no later flush probes.
struct Locked<W> {
    lock: OutputLock,
    gone: ClientGone,
    sink: W,
    /// Whether the flush still probes `sink`: true until a probe fails other than for a gone reader.
    probing: bool,
}

impl<W: Probed> Write for Locked<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.sink.write_all(buf).inspect_err(|error| mark_if_gone(&self.gone, error))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.sink.flush().inspect_err(|error| mark_if_gone(&self.gone, error))?;
        if !self.probing {
            return Ok(());
        }
        match self.sink.probe() {
            Err(error) if reader_gone(&error) => {
                mark_if_gone(&self.gone, &error);
                Err(error)
            }
            // A stream that cannot be asked is no gone client: the lines go on, and a gone reader is seen by a line.
            Err(_) => {
                self.probing = false;
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }
}

/// The Win32 errors a write to a pipe gives once its reader has gone: closed (`ERROR_NO_DATA`), broken
/// (`ERROR_BROKEN_PIPE`) or never there (`ERROR_PIPE_NOT_CONNECTED`).
const READER_GONE: [u32; 3] = [ERROR_NO_DATA, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED];

/// Whether `error` says the stream's reader has gone ([`READER_GONE`]), rather than any other failure.
fn reader_gone(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| READER_GONE.contains(&code))
}

/// Sets `gone` when `error` says the stream's reader has gone ([`reader_gone`]); any other failure leaves it alone.
fn mark_if_gone(gone: &AtomicBool, error: &io::Error) {
    if reader_gone(error) {
        gone.store(true, Ordering::SeqCst);
    }
}

/// A stream the heartbeat writes that can be asked, without writing a byte, whether its reader still takes writes.
trait Probed: Write {
    /// `Err` once the stream's reader has gone, or when the stream cannot be asked.
    fn probe(&mut self) -> io::Result<()>;
}

impl Probed for io::Stderr {
    /// [`probe`] on stderr's raw handle, read afresh on each call. It is called only under the box's output lock, which
    /// the box also holds while `Utf8Console` points the process's standard handles at a donor's console, so the handle
    /// is always the box's own stderr.
    fn probe(&mut self) -> io::Result<()> {
        probe(self.as_raw_handle())
    }
}

/// A zero-byte `WriteFile` on `handle`, which std would skip as an empty write: it fails on a pipe whose reader has
/// closed it, and succeeds while the reader is there. A null or invalid handle, a process with no stderr, is not
/// probed: there is no client to lose.
fn probe(handle: RawHandle) -> io::Result<()> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Ok(());
    }
    let empty: [u8; 0] = [];
    let mut written = 0;
    // SAFETY: the buffer is valid for the zero bytes asked, `written` is a valid place for the count, and no overlapped
    // structure is given. Stderr's handle is the box's own and open for the call: the box's output lock, held around
    // every probe, is also held while `Utf8Console` swaps the process's standard handles.
    let wrote = unsafe { WriteFile(handle, empty.as_ptr(), 0, &raw mut written, std::ptr::null_mut()) };
    if wrote == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The heartbeat's CPU source for one command: its job, and the WSL VM when `wsl` says the command is a WSL one.
struct JobCpu {
    job: Arc<Job>,
    wsl: bool,
}

impl CpuSource for JobCpu {
    /// The job's CPU, in 100 ns ticks; it only rises, as the job's accounting keeps what its ended processes used.
    fn job_cpu_ticks(&self) -> Result<u64, String> {
        self.job
            .accounting()
            .map(|accounting| accounting.cpu)
            .map_err(|error| format!("cannot read the CPU of job {}: {error}", self.job.name()))
    }

    /// The WSL VM's CPU, in 100 ns ticks, for a WSL command, whose work runs in the VM outside its job; `None` for any
    /// other command.
    fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
        if !self.wsl {
            return Ok(None);
        }
        gate_win32::wsl_vm_cpu().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, PipeWriter, Write};
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    use gate_win32::{EnvironmentBlock, Launch, LaunchOutput};
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NO_DATA, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Pipes::{PIPE_NOWAIT, PIPE_WAIT, SetNamedPipeHandleState};

    use super::{Locked, Probed, job_cpu, probe, reader_gone};
    use crate::boxside::forward::{ClientGone, OutputLock};

    /// How many bytes the full-pipe case writes at a time while it fills the pipe, before it tops it up a byte at a time.
    const FILL_CHUNK: usize = 4096;

    /// The CPU the job-alone test burns outside the job: 50 ms in 100 ns ticks.
    const BURN: u64 = 500_000;

    /// How many reads of its own CPU the job-alone test makes before it calls the burn broken rather than spin forever.
    const READ_GUARD: u64 = 100_000_000;

    /// How long the job-alone test sleeps between two looks for its burner's CPU.
    const RISE_STEP: Duration = Duration::from_millis(10);

    /// How many looks for its burner's CPU the job-alone test makes before it calls the job's accounting broken: 600 s of
    /// them, a hang guard a starved burner never reaches.
    const RISE_LOOKS: u64 = 60_000;

    /// How long a test waits for an event before it calls the run hung.
    const GUARD: Duration = Duration::from_secs(30);

    /// How long the locked-writer test lets a write that must wait sit before it looks that it has not written.
    const HELD: Duration = Duration::from_millis(200);

    /// A sink that records each write it is given whole.
    struct Recording(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Write for Recording {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Probed for Recording {
        fn probe(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A pipe's write end as the heartbeat's sink, probed as stderr is.
    struct Piped(PipeWriter);

    impl Write for Piped {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl Probed for Piped {
        fn probe(&mut self) -> io::Result<()> {
            probe(self.0.as_raw_handle())
        }
    }

    /// The heartbeat's writer on `writer`, and the client-gone flag it sets.
    fn locked(writer: PipeWriter) -> (Locked<Piped>, ClientGone) {
        let gone = ClientGone::default();
        let locked = Locked {
            lock: OutputLock::default(),
            gone: Arc::clone(&gone),
            sink: Piped(writer),
            probing: true,
        };
        (locked, gone)
    }

    /// A sink whose probe fails with the Win32 error `probe`, and whose write and flush fail with `write` when it is
    /// given; it records each write it takes and counts its probes.
    struct Failing {
        write: Option<u32>,
        probe: u32,
        written: Vec<Vec<u8>>,
        probes: u32,
    }

    /// The `io::Error` of the Win32 error `code`.
    #[expect(clippy::expect_used, reason = "a Win32 error code that does not fit fails the case loudly")]
    fn os_error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(i32::try_from(code).expect("a Win32 error code fits an i32"))
    }

    impl Write for Failing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(code) = self.write {
                return Err(os_error(code));
            }
            self.written.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.write.map_or(Ok(()), |code| Err(os_error(code)))
        }
    }

    impl Probed for Failing {
        fn probe(&mut self) -> io::Result<()> {
            self.probes += 1;
            Err(os_error(self.probe))
        }
    }

    /// The heartbeat's writer on a [`Failing`] sink, and the client-gone flag it sets.
    fn failing(write: Option<u32>, probe: u32) -> (Locked<Failing>, ClientGone) {
        let gone = ClientGone::default();
        let sink = Failing {
            write,
            probe,
            written: Vec::new(),
            probes: 0,
        };
        let locked = Locked {
            lock: OutputLock::default(),
            gone: Arc::clone(&gone),
            sink,
            probing: true,
        };
        (locked, gone)
    }

    #[test]
    fn a_probe_that_fails_while_the_reader_is_there_stops_the_probing_and_the_beats_go_on() {
        let (mut locked, gone) = failing(None, ERROR_ACCESS_DENIED);

        let probed = locked.flush();
        let beat = locked.write_all(b"gate-remote-heartbeat: 12\n").and_then(|()| locked.flush());

        assert!(probed.is_ok(), "a probe that cannot ask is no end: {probed:?}");
        assert!(beat.is_ok(), "the next beat is written: {beat:?}");
        assert_eq!(locked.sink.written, [b"gate-remote-heartbeat: 12\n".to_vec()], "the beat's line, whole");
        assert_eq!(locked.sink.probes, 1, "no probe after the one that failed");
        assert!(!gone.load(Ordering::SeqCst), "the client is not taken for gone");
    }

    #[test]
    fn a_write_or_flush_that_fails_sets_the_flag_only_for_a_gone_reader() {
        let (mut denied, denied_gone) = failing(Some(ERROR_ACCESS_DENIED), ERROR_ACCESS_DENIED);
        let wrote = denied.write(b"x");
        let flushed = denied.flush();

        assert!(wrote.is_err(), "the write fails: {wrote:?}");
        assert!(flushed.is_err(), "the flush fails: {flushed:?}");
        assert!(!denied_gone.load(Ordering::SeqCst), "a denied write is not a gone client");

        let (mut closed, closed_gone) = failing(Some(ERROR_NO_DATA), ERROR_NO_DATA);
        let wrote = closed.write(b"x");

        assert!(wrote.as_ref().is_err_and(reader_gone), "the write finds the reader gone: {wrote:?}");
        assert!(closed_gone.load(Ordering::SeqCst), "the write alone takes the client for gone");
    }

    /// Sets the wait mode `mode` (`PIPE_WAIT` or `PIPE_NOWAIT`) on the pipe end `writer`.
    fn set_wait(writer: &PipeWriter, mode: u32) -> io::Result<()> {
        // SAFETY: the handle is open while `writer` lives; the mode is read only, and the two limits are not given.
        let set = unsafe { SetNamedPipeHandleState(writer.as_raw_handle(), &raw const mode, std::ptr::null(), std::ptr::null()) };
        if set == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Writes `chunk` to `writer`, a pipe end that does not wait, until a write takes less than all of it.
    fn fill_with(writer: &mut PipeWriter, chunk: &[u8]) -> io::Result<()> {
        while writer.write(chunk)? == chunk.len() {}
        Ok(())
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make its pipe must fail loudly")]
    fn a_probe_of_a_pipe_whose_reader_has_gone_fails_and_sets_the_flag() {
        let (reader, writer) = io::pipe().expect("the pipe is made");
        let (mut locked, gone) = locked(writer);
        drop(reader);

        let flushed = locked.flush();

        assert!(flushed.as_ref().is_err_and(reader_gone), "the probe finds the reader gone: {flushed:?}");
        assert!(gone.load(Ordering::SeqCst), "the client is gone");
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make its pipe must fail loudly")]
    fn a_probe_of_a_pipe_whose_reader_is_there_passes() {
        let (reader, writer) = io::pipe().expect("the pipe is made");
        let (mut locked, gone) = locked(writer);

        let flushed = locked.flush();
        drop(reader);

        assert!(flushed.is_ok(), "the probe passes: {flushed:?}");
        assert!(!gone.load(Ordering::SeqCst), "the client is there");
    }

    /// A pipe its reader has stopped reading, its buffer full, still has its reader: the probe neither waits for room
    /// nor finds the reader gone. Run on a thread, so a probe that waits fails the case at the guard.
    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot make or fill its pipe must fail loudly")]
    fn a_probe_of_a_full_pipe_whose_reader_is_there_returns_and_passes() {
        let (reader, mut writer) = io::pipe().expect("the pipe is made");
        set_wait(&writer, PIPE_NOWAIT).expect("the pipe stops waiting for room");
        fill_with(&mut writer, &[b'x'; FILL_CHUNK]).expect("the pipe is filled");
        fill_with(&mut writer, b"x").expect("the pipe is topped up");
        set_wait(&writer, PIPE_WAIT).expect("the pipe waits for room again");
        let (mut locked, gone) = locked(writer);
        let (done, finished) = channel();
        let probing = thread::spawn(move || {
            let _ = done.send(locked.flush().map_err(|error| error.to_string()));
        });

        let flushed = finished.recv_timeout(GUARD);
        drop(reader);

        assert_eq!(flushed, Ok(Ok(())), "the probe returns within the guard and passes");
        assert!(!gone.load(Ordering::SeqCst), "the client is there");
        assert!(probing.join().is_ok());
    }

    #[test]
    fn no_handle_is_not_probed() {
        assert!(probe(std::ptr::null_mut()).is_ok(), "a null handle has no client to lose");
        assert!(probe(INVALID_HANDLE_VALUE).is_ok(), "an invalid handle has no client to lose");
    }

    /// The CPU this test process has used so far, in 100 ns ticks.
    #[expect(clippy::expect_used, reason = "a test that cannot read its own CPU must fail loudly")]
    fn own_cpu() -> u64 {
        gate_win32::process_times(std::process::id())
            .expect("this process's times can be read")
            .expect("this process runs")
            .cpu
    }

    /// A `cmd.exe` that spins on the CPU until it is ended, writing nothing.
    #[expect(clippy::expect_used, reason = "a test that cannot find the system folder must fail loudly")]
    fn burner() -> Launch {
        let system = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot is set"));
        Launch {
            application: system.join(r"System32\cmd.exe"),
            command_line: r#"cmd.exe /d /c "for /l %i in (0,0,1) do @rem""#.encode_utf16().collect(),
            directory: system,
            environment: EnvironmentBlock::new(std::env::vars_os()).expect("the block is built"),
            output: LaunchOutput::Piped,
            priority: None,
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "a test that cannot create its job or start its burner must fail loudly")]
    fn heartbeat_counts_the_job_alone() {
        let name = format!("gate-heartbeat-test-{}", std::process::id());
        // Kill-on-close, so a failing case leaves no burner spinning.
        let job = Arc::new(gate_win32::Job::create_kill_on_close(&name).expect("the job is created"));
        let source = job_cpu(Arc::clone(&job), false);
        let first = source.job_cpu_ticks();

        let start = own_cpu();
        let mut reads: u64 = 0;
        while own_cpu() < start + BURN {
            reads += 1;
            assert!(
                reads < READ_GUARD,
                "the process's own CPU did not rise by {BURN} ticks in {READ_GUARD} reads"
            );
        }
        let second = source.job_cpu_ticks();

        assert_eq!((first, second), (Ok(0), Ok(0)), "CPU outside the empty job counts nothing");

        let burning = burner()
            .start_suspended()
            .expect("the burner starts")
            .assign_and_resume(&job)
            .expect("the burner is put in the job and resumed");
        let mut looks: u64 = 0;
        while source.job_cpu_ticks().expect("the job's CPU can be read") == 0 {
            looks += 1;
            assert!(
                looks < RISE_LOOKS,
                "the job's CPU did not rise in {RISE_LOOKS} looks while a process in it burned"
            );
            thread::sleep(RISE_STEP);
        }
        job.terminate(1).expect("the job is terminated");

        assert_eq!(
            burning.wait(GUARD).expect("the burner is waited for"),
            Some(1),
            "the burner ends with its job"
        );
    }

    #[test]
    fn heartbeat_write_waits_for_the_output_lock_and_writes_its_line_whole() {
        let lock = OutputLock::default();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut writer = Locked {
            lock: Arc::clone(&lock),
            gone: Arc::default(),
            sink: Recording(Arc::clone(&writes)),
            probing: true,
        };
        let held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (done, finished) = channel();
        let thread = thread::spawn(move || {
            let written = writer.write_all(b"gate-remote-heartbeat: 12\n").and_then(|()| writer.flush());
            let _ = done.send(written.is_ok());
        });

        assert!(
            finished.recv_timeout(HELD).is_err(),
            "the write waits while a forwarded line holds the lock"
        );
        assert!(
            writes.lock().unwrap_or_else(PoisonError::into_inner).is_empty(),
            "nothing is written while the lock is held"
        );
        drop(held);

        assert_eq!(finished.recv_timeout(GUARD), Ok(true), "the write goes through once the lock is free");
        assert!(thread.join().is_ok());
        assert_eq!(
            *writes.lock().unwrap_or_else(PoisonError::into_inner),
            [b"gate-remote-heartbeat: 12\n".to_vec()],
            "one write, the whole line"
        );
    }
}

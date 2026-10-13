//! `gate box run`'s heartbeat: `gate-remote-heartbeat: <cpu ms>` on stderr every period, on `gate-watchdog`'s
//! [`Pump`], so the client knows a quiet command is still working. While no command is watched the pump writes every
//! period; while one is, only when the CPU its job used since the last beat reaches 1% of the time since it, as the
//! PowerShell wrapper's heartbeat counts it. The command's build servers are its run's own and run in its job, so the
//! job's CPU counts them. Each line takes the box's output lock, so none lands inside a forwarded line. The heartbeat
//! starts no process.

use std::io::{self, Write};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_watchdog::{CpuSource, Pump, Ticker};
use gate_win32::Job;

use crate::boxside::forward::OutputLock;

/// How often the heartbeat looks, and so the most often it writes.
const PERIOD: Duration = Duration::from_secs(5);

/// The nanoseconds in one 100 ns tick.
const NANOS_PER_TICK: u128 = 100;

/// The heartbeat's period: the seam's milliseconds when given, else [`PERIOD`].
pub(crate) fn period(seams: &BoxSeams) -> Duration {
    seams.heartbeat_ms.map_or(PERIOD, Duration::from_millis)
}

/// Starts the heartbeat on stderr, every line under `lock`, looking every `period`.
///
/// # Errors
///
/// Why the heartbeat's thread could not be started.
pub(crate) fn start(lock: &OutputLock, period: Duration) -> io::Result<Pump> {
    let writer = Locked {
        lock: Arc::clone(lock),
        sink: io::stderr(),
    };
    Pump::start(writer, Arc::new(BoxClock { started: Instant::now() }), period)
}

/// The CPU source of the command running in `job`: the job's own CPU, its processes that ended included.
pub(crate) fn job_cpu(job: Arc<Job>) -> Arc<dyn CpuSource> {
    Arc::new(JobCpu { job })
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
/// write under the same lock.
struct Locked<W> {
    lock: OutputLock,
    sink: W,
}

impl<W: Write> Write for Locked<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _held = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.sink.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

/// The heartbeat's CPU source for one command: its job.
struct JobCpu {
    job: Arc<Job>,
}

impl CpuSource for JobCpu {
    /// The job's CPU, in 100 ns ticks; it only rises, as the job's accounting keeps what its ended processes used.
    fn job_cpu_ticks(&self) -> Result<u64, String> {
        self.job
            .accounting()
            .map(|accounting| accounting.cpu)
            .map_err(|error| format!("cannot read the CPU of job {}: {error}", self.job.name()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    use gate_win32::{EnvironmentBlock, Launch, LaunchOutput};

    use super::{Locked, job_cpu};
    use crate::boxside::forward::OutputLock;

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
        let source = job_cpu(Arc::clone(&job));
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
            sink: Recording(Arc::clone(&writes)),
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

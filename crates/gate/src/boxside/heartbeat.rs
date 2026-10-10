//! `gate box run`'s heartbeat: `gate-remote-heartbeat: <cpu ms>` on stderr every period, on `gate-watchdog`'s
//! [`Pump`], so the client knows a quiet command is still working. While no command is watched the pump writes every
//! period; while one is, only when the CPU its job and every build server on the box used since the last beat reaches
//! 1% of the time since it, as the PowerShell wrapper's heartbeat counts it. The box keeps build servers warm, so a
//! quiet `dotnet build` works in a compiler server or `MSBuild` node outside its job. Each line takes the box's output
//! lock, so none lands inside a forwarded line. The heartbeat starts no process.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gate_args::BoxSeams;
use gate_watchdog::{CpuSource, Pump, Ticker};
use gate_win32::Job;
use proc_table::Win32ProcessTable;

use crate::boxside::forward::OutputLock;
use crate::servers::{Scope, Servers};

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

/// The CPU source of the command running in `job`: the job's own CPU plus every build server's on the box outside it.
pub(crate) fn job_cpu(job: Arc<Job>) -> Arc<dyn CpuSource> {
    Arc::new(JobAndServers {
        job,
        counted: Mutex::new(Counted::default()),
    })
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

/// The heartbeat's CPU source for one command: its job, and what the reads so far have counted.
struct JobAndServers {
    job: Arc<Job>,
    counted: Mutex<Counted>,
}

/// The build servers' command lines read so far, and the running sum.
#[derive(Default)]
struct Counted {
    servers: Servers,
    sum: RunningSum,
}

impl CpuSource for JobAndServers {
    /// The running sum of the job's CPU and the build servers' outside it. Every build server on the box counts, warm
    /// ones started before the command included; the job's own processes are left to the job's CPU, which counts them
    /// already. A server whose CPU or command line cannot be read is skipped; a job that cannot be read is the error.
    fn job_cpu_ticks(&self) -> Result<u64, String> {
        let name = self.job.name();
        let members: BTreeSet<u32> = self
            .job
            .process_ids()
            .map_err(|error| format!("cannot list the processes of job {name}: {error}"))?
            .into_iter()
            .collect();
        let processes = Win32ProcessTable.snapshot_without_command_lines();
        let listed: BTreeSet<u32> = processes.iter().map(|process| process.pid).collect();
        // Every server counts as new from a start of 0, so warm MSBuild nodes count as well as compiler servers.
        let scope = Scope {
            members: &members,
            since: 0,
            under: None,
        };
        let mut counted = self.counted.lock().unwrap_or_else(PoisonError::into_inner);
        let found = counted
            .servers
            .read(&processes, &scope, gate_win32::process_times, proc_table::command_line_of);
        let job = self
            .job
            .accounting()
            .map_err(|error| format!("cannot read the CPU of job {name}: {error}"))?
            .cpu;
        Ok(counted.sum.add(job, found.new_servers, &listed))
    }
}

/// The CPU counted so far, in 100 ns ticks, and the last read it was counted from. It only rises, as [`Pump`] takes
/// the rise between two reads as the CPU used.
#[derive(Debug, Default)]
struct RunningSum {
    total: u64,
    job: u64,
    servers: BTreeMap<u32, u64>,
}

impl RunningSum {
    /// Adds what the job's CPU `job` and each build server's in `servers`, by pid, rose since the last read, and
    /// returns the sum so far; `listed` is every pid the read's process snapshot held. A server new since the last
    /// read, or whose total fell (its id now another process's), adds all its CPU. A read is no reason to forget a
    /// server: an empty snapshot, which is a failed one, keeps every server's last total, and so does a pid it still
    /// lists whose times could not be read; only a pid a good snapshot no longer lists is dropped, adding nothing.
    fn add(&mut self, job: u64, servers: BTreeMap<u32, u64>, listed: &BTreeSet<u32>) -> u64 {
        let mut used = job.saturating_sub(self.job);
        self.job = job;
        if !listed.is_empty() {
            for (pid, &cpu) in &servers {
                let rise = match self.servers.get(pid) {
                    Some(&last) if cpu >= last => cpu - last,
                    _ => cpu,
                };
                used = used.saturating_add(rise);
            }
            let kept: Vec<(u32, u64)> = std::mem::take(&mut self.servers)
                .into_iter()
                .filter(|(pid, _)| listed.contains(pid) && !servers.contains_key(pid))
                .collect();
            self.servers = servers;
            self.servers.extend(kept);
        }
        self.total = self.total.saturating_add(used);
        self.total
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{self, Write};
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

    use super::{Locked, RunningSum};
    use crate::boxside::forward::OutputLock;

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

    /// A process every good snapshot lists that is no build server.
    const OTHER: u32 = 4;

    /// An hour of CPU in 100 ns ticks: what a warm server may have used before the run.
    const HOUR: u64 = 36_000_000_000;

    fn servers(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
        pairs.iter().copied().collect()
    }

    fn listed(pids: &[u32]) -> BTreeSet<u32> {
        pids.iter().copied().collect()
    }

    /// One good read: a snapshot listing [`OTHER`] and every server in `servers`, each one's times read.
    fn read(sum: &mut RunningSum, job: u64, servers: BTreeMap<u32, u64>) -> u64 {
        let listed = servers.keys().copied().chain([OTHER]).collect();
        sum.add(job, servers, &listed)
    }

    #[test]
    fn heartbeat_sum_keeps_every_server_over_a_failed_snapshot() {
        let mut sum = RunningSum::default();
        let first = read(&mut sum, 0, servers(&[(7, HOUR)]));
        assert_eq!(
            sum.add(10, servers(&[]), &listed(&[])),
            first + 10,
            "an empty snapshot adds the job's rise alone"
        );
        assert_eq!(
            read(&mut sum, 10, servers(&[(7, HOUR + 5)])),
            first + 15,
            "the server is still known after it: its 5 more ticks, no hour-long jump"
        );
    }

    #[test]
    fn heartbeat_sum_keeps_a_listed_server_whose_times_could_not_be_read() {
        let mut sum = RunningSum::default();
        let first = read(&mut sum, 0, servers(&[(7, HOUR)]));
        assert_eq!(
            sum.add(0, servers(&[]), &listed(&[OTHER, 7])),
            first,
            "a listed server not read adds nothing"
        );
        assert_eq!(
            read(&mut sum, 0, servers(&[(7, HOUR + 5)])),
            first + 5,
            "its last total was kept: 5 more ticks, no hour-long jump"
        );
    }

    #[test]
    fn heartbeat_sum_drops_a_server_only_once_a_good_snapshot_no_longer_lists_it() {
        let mut sum = RunningSum::default();
        let first = read(&mut sum, 0, servers(&[(7, HOUR)]));
        assert_eq!(
            sum.add(0, servers(&[]), &listed(&[OTHER])),
            first,
            "gone from a good snapshot, it adds nothing"
        );
        assert_eq!(
            read(&mut sum, 0, servers(&[(7, HOUR + 5)])),
            first + HOUR + 5,
            "the id back again is a new process: all its CPU"
        );
    }

    #[test]
    fn heartbeat_sum_first_read_counts_the_job_and_every_server_whole() {
        let mut sum = RunningSum::default();
        assert_eq!(read(&mut sum, 100, servers(&[(7, 30), (9, 5)])), 135);
    }

    #[test]
    fn heartbeat_sum_adds_the_job_delta() {
        let mut sum = RunningSum::default();
        read(&mut sum, 100, servers(&[]));
        assert_eq!(read(&mut sum, 130, servers(&[])), 130, "the job's 30 more ticks");
        assert_eq!(read(&mut sum, 130, servers(&[])), 130, "a job that used nothing adds nothing");
    }

    #[test]
    fn heartbeat_sum_adds_a_rising_servers_delta() {
        let mut sum = RunningSum::default();
        read(&mut sum, 0, servers(&[(7, 10)]));
        assert_eq!(read(&mut sum, 0, servers(&[(7, 25)])), 25, "the server's 15 more ticks");
    }

    #[test]
    fn heartbeat_sum_adds_a_new_server_whole() {
        let mut sum = RunningSum::default();
        read(&mut sum, 0, servers(&[(7, 10)]));
        assert_eq!(
            read(&mut sum, 0, servers(&[(7, 10), (8, 40)])),
            50,
            "the server new since the last read, all 40"
        );
    }

    #[test]
    fn heartbeat_sum_adds_a_server_whose_total_fell_whole() {
        let mut sum = RunningSum::default();
        read(&mut sum, 0, servers(&[(7, 50)]));
        assert_eq!(read(&mut sum, 0, servers(&[(7, 20)])), 70, "the id reused by another process, all 20");
    }

    #[test]
    fn heartbeat_sum_adds_nothing_for_a_vanished_server_and_all_of_one_back_again() {
        let mut sum = RunningSum::default();
        read(&mut sum, 0, servers(&[(7, 50)]));
        assert_eq!(read(&mut sum, 0, servers(&[])), 50, "a server gone adds nothing");
        assert_eq!(read(&mut sum, 0, servers(&[(7, 60)])), 110, "back again it is new, all 60");
    }

    #[test]
    fn heartbeat_sum_never_falls() {
        let mut sum = RunningSum::default();
        let reads: [(u64, &[(u32, u64)]); 6] = [
            (10, &[(1, 100), (2, 5)]),
            (12, &[(1, 40)]),
            (12, &[]),
            (20, &[(2, 1), (3, 9)]),
            (20, &[(2, 0), (3, 9)]),
            (25, &[(3, 12)]),
        ];
        let mut last = 0;
        for (job, pairs) in reads {
            let total = read(&mut sum, job, servers(pairs));
            assert!(total >= last, "{total} after {last} for {job} {pairs:?}");
            last = total;
        }
        assert_eq!(last, 10 + 105 + 2 + 40 + 8 + 10 + 5 + 3, "every read's rise, summed");
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

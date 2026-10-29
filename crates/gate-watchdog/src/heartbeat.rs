//! The box heartbeat: the rule that decides when a watched command's CPU is worth a line, and the pump that writes
//! those lines from a thread of its own, so a write a full pipe blocks holds up no one else.

use std::any::Any;
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The prefix of every heartbeat line.
const PREFIX: &str = "gate-remote-heartbeat:";

/// The prefix of the line written once the heartbeats stop, with why.
const STOPPED_PREFIX: &str = "gate: remote heartbeat stopped:";

/// The prefix of the line written once when the WSL VM's CPU cannot be read, with why.
const VM_UNREAD_PREFIX: &str = "gate: remote heartbeat cannot read the WSL VM's CPU:";

/// The 100 ns ticks in one millisecond.
const TICKS_PER_MS: u64 = 10_000;

/// The pump thread's name, for a debugger.
const THREAD_NAME: &str = "gate-heartbeat";

/// The CPU a watched command has used, read on demand from whatever the caller samples.
pub trait CpuSource: Send + Sync {
    /// The watched job's total CPU time in 100 ns ticks.
    ///
    /// # Errors
    ///
    /// The message the heartbeats report once and stop for, when the job's CPU cannot be read.
    fn job_cpu_ticks(&self) -> Result<u64, String>;

    /// The WSL VM's total CPU time in 100 ns ticks, for a command whose work runs in the VM, outside its job; `None`
    /// for a command that is not a WSL one, or when no VM runs.
    ///
    /// # Errors
    ///
    /// The message the heartbeats report once before going on from the job's CPU alone, when the VM's CPU cannot be read.
    fn vm_cpu_ticks(&self) -> Result<Option<u64>, String>;
}

/// A monotonic clock in 100 ns ticks, injected so a test can drive the beat rule.
pub trait Ticker: Send + Sync {
    /// The current time in 100 ns ticks.
    fn now_ticks(&self) -> u64;
}

/// The beat rule: a command's CPU is worth a heartbeat once its job has used at least 1% of one core since the last
/// beat, or the WSL VM it works in a tenth of one, the CPU of each carried forward between samples until one is written.
#[derive(Clone, Copy, Debug, Default)]
pub struct BeatClock {
    last_beat: u64,
    job_carried: u64,
    vm_carried: u64,
}

impl BeatClock {
    /// A clock that has beaten nowhere yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restarts the clock at `now_ticks`, with no CPU carried forward.
    pub fn reset(&mut self, now_ticks: u64) {
        self.last_beat = now_ticks;
        self.job_carried = 0;
        self.vm_carried = 0;
    }

    /// Adds the job's and the WSL VM's CPU ticks used since the last sample and says whether a beat is due at
    /// `now_ticks`; a due one restarts the clock there.
    pub fn sample(&mut self, cpu_ticks_used: u64, vm_ticks_used: u64, now_ticks: u64) -> bool {
        self.job_carried = self.job_carried.saturating_add(cpu_ticks_used);
        self.vm_carried = self.vm_carried.saturating_add(vm_ticks_used);
        let elapsed = u128::from(now_ticks.saturating_sub(self.last_beat));
        let job_due = u128::from(self.job_carried) * 100 >= elapsed;
        let vm_due = u128::from(self.vm_carried) * 10 >= elapsed;
        if elapsed == 0 || !(job_due || vm_due) {
            return false;
        }
        self.reset(now_ticks);
        true
    }
}

/// The heartbeat pump: every period it writes a heartbeat whatever the CPU while no run is watched — the CPU total
/// summed over every run watched so far, as the wrapper's `cpuTotal` is, and zero before any — and only when the
/// watched command's CPU earns one while a run is. Its own thread does the sampling and the writing, so a write a full
/// pipe blocks holds up no one else. A period with no heartbeat due flushes the writer instead. A WSL VM whose CPU
/// cannot be read is said once in the pump's life, through the same writer; that period counts the job's CPU alone,
/// and every period still reads the VM again. A write or a flush that fails, a job CPU read that fails and a panic in
/// the thread all end the heartbeats for the rest of the run, and the pump never ends the run itself; a writer may act
/// on its own failure first, as the box's sets its run's client-gone flag, which then ends the run.
pub struct Pump {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// What the pump's thread and its callers share.
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    ticker: Arc<dyn Ticker>,
}

/// The pump's own state, all of it under the one lock.
struct State {
    cpu: Option<Arc<dyn CpuSource>>,
    job_ticks: u64,
    /// The WSL VM's CPU at the last reading, `None` when there was none to count from.
    vm_ticks: Option<u64>,
    /// Set once a failed VM read has been said, so a later failure is not said again.
    vm_said: bool,
    cpu_total: u64,
    clock: BeatClock,
    /// A line the next tick says before its heartbeat, the heartbeats going on after it.
    said: Option<String>,
    notice: Option<String>,
    stopped: bool,
    /// Set once the pump's thread has ended, however it ended.
    finished: bool,
    panicked: Option<String>,
}

impl State {
    /// The state of a pump that watches no run and has said nothing.
    fn new() -> Self {
        Self {
            cpu: None,
            job_ticks: 0,
            vm_ticks: None,
            vm_said: false,
            cpu_total: 0,
            clock: BeatClock::new(),
            said: None,
            notice: None,
            stopped: false,
            finished: false,
            panicked: None,
        }
    }

    /// Watches `cpu` from now on: its job's CPU and its VM's are baselined, and the beat clock restarted at `ticker`'s
    /// time. A job CPU read that fails is the notice the heartbeats stop for; a VM read that fails is said once.
    fn watch(&mut self, cpu: Arc<dyn CpuSource>, ticker: &dyn Ticker) {
        match cpu.job_cpu_ticks() {
            Ok(ticks) => {
                self.job_ticks = ticks;
                self.vm_ticks = None;
                self.vm_used(cpu.as_ref());
                self.clock.reset(ticker.now_ticks());
                self.cpu = Some(cpu);
            }
            Err(message) => self.notice = Some(stopped_line(&message)),
        }
    }

    /// The WSL VM CPU ticks `cpu` used since its last reading. A reading of none, or one lower than the last, which a
    /// VM that restarted gives, is a fresh baseline and counts nothing; a read that fails counts nothing and leaves no
    /// baseline, so the next good reading is one, and the first failure of the pump's life is said.
    fn vm_used(&mut self, cpu: &dyn CpuSource) -> u64 {
        match cpu.vm_cpu_ticks() {
            Ok(reading) => {
                let used = match (self.vm_ticks, reading) {
                    (Some(last), Some(now)) if now >= last => now - last,
                    _ => 0,
                };
                self.vm_ticks = reading;
                used
            }
            Err(message) => {
                self.vm_ticks = None;
                if !self.vm_said {
                    self.vm_said = true;
                    self.said = Some(format!("{VM_UNREAD_PREFIX} {message}"));
                }
                0
            }
        }
    }
}

/// What one tick decided: a line said before its heartbeat, the heartbeat line due now, if any, and whether the
/// heartbeats end after them.
struct Tick {
    said: Option<String>,
    line: Option<String>,
    ended: bool,
}

impl Pump {
    /// Starts the pump over `writer`, reading the time from `ticker` once every `period`.
    ///
    /// # Errors
    ///
    /// Why the pump's thread could not be started.
    pub fn start<W: Write + Send + 'static>(writer: W, ticker: Arc<dyn Ticker>, period: Duration) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            wake: Condvar::new(),
            ticker,
        });
        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new().name(THREAD_NAME.to_string()).spawn(move || {
                let _finished = Finished(&shared);
                pump(writer, period, &shared);
            })?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Samples `cpu`, the run watched from now on: its job's CPU and its WSL VM's are baselined there, the beat clock
    /// restarted, and a heartbeat is written only when that CPU earns one. A job CPU read that fails here reports why
    /// the heartbeats stopped, as one during the pump does; a VM read that fails is said once, as one during the pump is.
    pub fn watch(&self, cpu: Arc<dyn CpuSource>) {
        lock(&self.shared.state).watch(cpu, self.shared.ticker.as_ref());
    }

    /// Drops the watched run: the pump writes unconditionally again.
    pub fn unwatch(&self) {
        lock(&self.shared.state).cpu = None;
    }

    /// Signals the pump to stop and joins its thread, so it blocks for as long as a write is stuck.
    ///
    /// # Errors
    ///
    /// What the pump's thread panicked with, whether the pump caught it or it ended the thread; the line saying so has
    /// been written to the pump's writer whenever the pump could still write.
    #[must_use = "the panic a pump hit is reported only by this Result"]
    pub fn stop(mut self) -> Result<(), String> {
        self.signal_stop();
        self.joined()
    }

    /// Signals the pump to stop and waits at most `limit` for its thread to end; a thread still stuck in a write then
    /// is left to end on its own, or with the process, so a reader that stopped reading cannot hold up the caller.
    ///
    /// # Errors
    ///
    /// What the pump's thread panicked with, as [`Pump::stop`] reports it, or that it did not end within `limit`.
    #[must_use = "the panic a pump hit is reported only by this Result"]
    pub fn stop_within(mut self, limit: Duration) -> Result<(), String> {
        self.signal_stop();
        let finished = {
            let state = lock(&self.shared.state);
            let (state, _) = self
                .shared
                .wake
                .wait_timeout_while(state, limit, |state| !state.finished)
                .unwrap_or_else(PoisonError::into_inner);
            state.finished
        };
        if !finished {
            // Dropping the handle detaches the thread, still blocked in its write.
            drop(self.thread.take());
            return Err(format!(
                "the heartbeat pump did not stop within {} ms and was left to end on its own",
                limit.as_millis()
            ));
        }
        self.joined()
    }

    /// Joins the pump's thread, already told to stop, and reports what it panicked with.
    fn joined(&mut self) -> Result<(), String> {
        let joined = match self.thread.take().map(JoinHandle::join) {
            Some(Err(payload)) => Some(panic_message(payload.as_ref())),
            _ => None,
        };
        let recorded = lock(&self.shared.state).panicked.clone();
        match recorded.or(joined) {
            Some(message) => Err(message),
            None => Ok(()),
        }
    }

    /// Tells the pump's thread to stop without waiting for it.
    fn signal_stop(&self) {
        lock(&self.shared.state).stopped = true;
        self.shared.wake.notify_all();
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        // The thread is detached, so a write a stuck pipe blocks cannot hold up the process ending.
        self.signal_stop();
    }
}

/// Marks the pump's thread finished when dropped, on its way out whether it returned or unwound, and wakes a caller
/// waiting for it.
struct Finished<'a>(&'a Shared);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).finished = true;
        self.0.wake.notify_all();
    }
}

/// The pump's thread: its loop, and the one line a panic in it is worth.
fn pump<W: Write>(mut writer: W, period: Duration, shared: &Shared) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| pump_loop(&mut writer, period, shared))) {
        let message = panic_message(payload.as_ref());
        // Recorded before the line is written, so a writer that panics again still reports the pump's own panic.
        lock(&shared.state).panicked = Some(message.clone());
        let line = format!("{STOPPED_PREFIX} the heartbeat pump panicked: {message}");
        let _ = write_line(&mut writer, &line);
    }
}

/// The loop: wait a period, decide the line under the state lock, and write it outside the lock; a period with no line
/// due flushes the writer instead, so a writer whose flush asks whether its reader is still there asks once a period.
fn pump_loop<W: Write>(writer: &mut W, period: Duration, shared: &Shared) {
    loop {
        {
            let state = lock(&shared.state);
            let (guard, _) = shared
                .wake
                .wait_timeout_while(state, period, |state| !state.stopped)
                .unwrap_or_else(PoisonError::into_inner);
            if guard.stopped {
                return;
            }
        }
        let Tick { said, line, ended } = {
            let mut state = lock(&shared.state);
            tick(&mut state, shared.ticker.as_ref())
        };
        let lines: Vec<String> = said.into_iter().chain(line).collect();
        let written = if lines.is_empty() {
            writer.flush()
        } else {
            lines.iter().try_for_each(|line| write_line(writer, line))
        };
        if written.is_err() {
            return;
        }
        if ended {
            return;
        }
    }
}

/// One tick under the state lock: the lines it decided, and whether the heartbeats end after them.
fn tick(state: &mut State, ticker: &dyn Ticker) -> Tick {
    if let Some(notice) = state.notice.take() {
        return Tick {
            said: state.said.take(),
            line: Some(notice),
            ended: true,
        };
    }
    let Some(cpu) = state.cpu.clone() else {
        return Tick {
            said: state.said.take(),
            line: Some(heartbeat(state.cpu_total)),
            ended: false,
        };
    };
    match cpu.job_cpu_ticks() {
        Ok(ticks) => {
            let used = ticks.saturating_sub(state.job_ticks);
            state.job_ticks = ticks;
            state.cpu_total = state.cpu_total.saturating_add(used);
            let vm_used = state.vm_used(cpu.as_ref());
            let beat = state.clock.sample(used, vm_used, ticker.now_ticks());
            Tick {
                said: state.said.take(),
                line: beat.then(|| heartbeat(state.cpu_total)),
                ended: false,
            }
        }
        Err(message) => Tick {
            said: state.said.take(),
            line: Some(stopped_line(&message)),
            ended: true,
        },
    }
}

/// The heartbeat line for a run that has used `cpu_total` ticks.
#[must_use]
fn heartbeat(cpu_total: u64) -> String {
    format!("{PREFIX} {}", cpu_total / TICKS_PER_MS)
}

/// The line saying the heartbeats stopped, with the reason a CPU read gave.
#[must_use]
fn stopped_line(message: &str) -> String {
    format!("{STOPPED_PREFIX} {message}")
}

/// The line a box says once its heartbeats cannot start at all, with the reason the start failed.
#[must_use]
pub fn start_failed_line(error: &dyn std::fmt::Display) -> String {
    format!("{STOPPED_PREFIX} cannot start the heartbeat: {error}")
}

/// What a panic payload says, or that it said nothing readable.
#[must_use]
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic".to_string()
}

/// Writes `line` and its newline, and flushes them: one call per line, and a failure of either ends the heartbeats.
fn write_line<W: Write>(writer: &mut W, line: &str) -> io::Result<()> {
    writer.write_all(format!("{line}\n").as_bytes())?;
    writer.flush()
}

/// Locks `mutex`, taking the state back from a writer that panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![expect(clippy::expect_used, reason = "a test that cannot start its pump must fail loudly")]

    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    /// How long a test waits for an event before it calls the run hung: no test asserts on how long anything took,
    /// only on the order its channels give. A pump that never delivers fails the test after this guard, except in
    /// `a_stuck_write_does_not_block_unwatch`, where a `watch` or `unwatch` that waited for the stuck writer hangs and
    /// only the gate's own timeout ends it.
    const GUARD: Duration = Duration::from_secs(30);

    /// The ticks in one millisecond.
    const MS: u64 = 10_000;

    /// The ticks in one second.
    const SECOND: u64 = 10_000_000;

    /// How often the pump ticks in a test.
    const PERIOD: Duration = Duration::from_millis(5);

    /// The line an unwatched pump of a machine that has not run anything writes.
    const IDLE_LINE: &str = "gate-remote-heartbeat: 0";

    /// The line a CPU read failure of the test's job is reported with.
    const STOPPED_LINE: &str = "gate: remote heartbeat stopped: the job is gone";

    /// A CPU source that reads one value forever.
    struct FixedCpu(Arc<AtomicU64>);

    /// The CPU source of a job that has used no CPU.
    fn idle_cpu() -> Arc<dyn CpuSource> {
        Arc::new(FixedCpu(Arc::new(AtomicU64::new(0))))
    }

    impl CpuSource for FixedCpu {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            Ok(self.0.load(Ordering::SeqCst))
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// A CPU source that cannot be read at all.
    struct AlwaysFailingCpu;

    impl CpuSource for AlwaysFailingCpu {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            Err("the job is gone".to_string())
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// A CPU source whose job reads what the case stored, and whose WSL VM gives the readings the case scripted, in order.
    struct ScriptedVm {
        job: AtomicU64,
        vm: Mutex<VecDeque<Result<Option<u64>, String>>>,
    }

    impl CpuSource for ScriptedVm {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            Ok(self.job.load(Ordering::SeqCst))
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            lock(&self.vm).pop_front().expect("the case scripted every VM read it makes")
        }
    }

    /// A clock that reads what the case stored.
    struct SetTicker(AtomicU64);

    impl Ticker for SetTicker {
        fn now_ticks(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// What a pump says on each tick of a run watched at 0 with the WSL VM read as `baseline`, one tick 5 s after the
    /// last, each step the job's total CPU in ms and the VM's reading at that tick: the line said, and the heartbeat.
    fn vm_ticks(baseline: Result<Option<u64>, String>, steps: &[(u64, Result<Option<u64>, String>)]) -> Vec<(Option<String>, Option<String>)> {
        let readings = std::iter::once(baseline).chain(steps.iter().map(|(_, vm)| vm.clone())).collect();
        let source = Arc::new(ScriptedVm {
            job: AtomicU64::new(0),
            vm: Mutex::new(readings),
        });
        let ticker = SetTicker(AtomicU64::new(0));
        let mut state = State::new();
        state.watch(Arc::clone(&source) as Arc<dyn CpuSource>, &ticker);
        (1_u64..)
            .zip(steps)
            .map(|(at, (job_ms, _))| {
                source.job.store(job_ms * MS, Ordering::SeqCst);
                ticker.0.store(at * 5 * SECOND, Ordering::SeqCst);
                let decided = tick(&mut state, &ticker);
                assert!(!decided.ended, "the heartbeats go on");
                (decided.said, decided.line)
            })
            .collect()
    }

    /// The line a failed read of the WSL VM's CPU is said with, in the VM cases.
    const VM_UNREAD_LINE: &str = "gate: remote heartbeat cannot read the WSL VM's CPU: the list is gone";

    #[test]
    fn start_failed_line_is_the_stopped_prefix_plus_why() {
        assert_eq!(
            start_failed_line(&"no thread"),
            "gate: remote heartbeat stopped: cannot start the heartbeat: no thread"
        );
    }

    #[test]
    fn a_tenth_of_a_core_in_the_wsl_vm_beats_while_the_job_is_under_one_percent() {
        assert_eq!(
            vm_ticks(Ok(Some(0)), &[(49, Ok(Some(500 * MS)))]),
            [(None, Some("gate-remote-heartbeat: 49".to_string()))],
            "500 ms of VM CPU over 5 s is a tenth of a core"
        );
    }

    #[test]
    fn a_twentieth_of_a_core_in_the_wsl_vm_does_not_beat() {
        assert_eq!(
            vm_ticks(Ok(Some(0)), &[(49, Ok(Some(250 * MS)))]),
            [(None, None)],
            "250 ms of VM CPU over 5 s is under a tenth of a core"
        );
    }

    #[test]
    fn a_vm_reading_of_none_never_beats_from_the_vm() {
        assert_eq!(
            vm_ticks(Ok(None), &[(0, Ok(Some(10 * SECOND))), (0, Ok(None)), (0, Ok(Some(20 * SECOND)))]),
            [(None, None), (None, None), (None, None)],
            "a reading after none is a fresh baseline, so nothing counts across the gap"
        );
    }

    #[test]
    fn a_vm_reading_lower_than_the_last_is_a_fresh_baseline() {
        assert_eq!(
            vm_ticks(Ok(Some(10 * SECOND)), &[(0, Ok(Some(0))), (0, Ok(Some(SECOND)))]),
            [(None, None), (None, Some("gate-remote-heartbeat: 0".to_string()))],
            "the VM restarted at 0, and its 1 s since then over 10 s is a tenth of a core"
        );
    }

    #[test]
    fn a_failed_vm_read_is_said_once_and_the_beats_go_on_from_the_job() {
        let failed = || Err("the list is gone".to_string());
        assert_eq!(
            vm_ticks(Ok(Some(0)), &[(0, failed()), (100, failed()), (200, failed())]),
            [
                (Some(VM_UNREAD_LINE.to_string()), None),
                (None, Some("gate-remote-heartbeat: 100".to_string())),
                (None, Some("gate-remote-heartbeat: 200".to_string())),
            ],
            "the failure is said once, and the job's CPU still beats"
        );
    }

    #[test]
    fn the_vm_is_read_again_after_a_failed_read_and_its_next_good_reading_is_a_baseline() {
        let failed = || Err("the list is gone".to_string());
        assert_eq!(
            vm_ticks(
                Ok(Some(0)),
                &[(0, failed()), (100, failed()), (100, Ok(Some(10 * SECOND))), (100, Ok(Some(12 * SECOND)))]
            ),
            [
                (Some(VM_UNREAD_LINE.to_string()), None),
                (None, Some("gate-remote-heartbeat: 100".to_string())),
                (None, None),
                (None, Some("gate-remote-heartbeat: 100".to_string())),
            ],
            "the failure is said once; the first good reading after it counts nothing, and the next one's 2 s over 10 s beats"
        );
    }

    #[test]
    fn a_vm_read_that_fails_at_the_watch_is_said_by_the_first_tick_with_its_beat() {
        assert_eq!(
            vm_ticks(Err("the list is gone".to_string()), &[(100, Ok(Some(0))), (100, Ok(Some(SECOND)))]),
            [
                (Some(VM_UNREAD_LINE.to_string()), Some("gate-remote-heartbeat: 100".to_string())),
                (None, Some("gate-remote-heartbeat: 100".to_string())),
            ],
            "the watch's failure is said by the first tick, whose job CPU beats; its good reading is a baseline, and the \
             next one's 1 s over 5 s beats"
        );
    }

    /// A CPU source whose first reads, the watch's, give no job CPU and no VM, and whose later ones give a second of
    /// job CPU and a VM that cannot be read.
    struct VmFailsAfterTheWatch {
        job_reads: AtomicUsize,
        vm_reads: AtomicUsize,
    }

    impl CpuSource for VmFailsAfterTheWatch {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            Ok(if self.job_reads.fetch_add(1, Ordering::SeqCst) == 0 { 0 } else { SECOND })
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            if self.vm_reads.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(None)
            } else {
                Err("the list is gone".to_string())
            }
        }
    }

    #[test]
    fn pump_writes_a_failed_vm_read_before_the_beat_of_the_same_tick() {
        let (events, seen) = channel();
        let (reads, _read) = channel();
        let pump = Pump::start(ReportingWriter { events }, Arc::new(StepTicker::new(5 * SECOND, reads)), PERIOD).expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        pump.watch(Arc::new(VmFailsAfterTheWatch {
            job_reads: AtomicUsize::new(0),
            vm_reads: AtomicUsize::new(0),
        }));
        assert_eq!(next_beat(&seen), VM_UNREAD_LINE, "the failed VM read is written first");
        assert_eq!(next_line(&seen), "gate-remote-heartbeat: 1000", "then the beat its tick's job CPU earned");
        assert!(pump.stop().is_ok());
    }

    /// A CPU source that reads once, baselining the run, and then cannot be read.
    struct FailAfterFirst(AtomicUsize);

    impl CpuSource for FailAfterFirst {
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(0)
            } else {
                Err("the job is gone".to_string())
            }
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// A CPU source that reads once, baselining the run, and then panics inside the pump's thread.
    struct PanicAfterFirst(AtomicUsize);

    impl CpuSource for PanicAfterFirst {
        #[expect(clippy::panic, clippy::panic_in_result_fn, reason = "the pump must catch a panic in the code it samples")]
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(0)
            } else {
                panic!("the CPU source blew up")
            }
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// A clock that never moves.
    struct ZeroTicker;

    impl Ticker for ZeroTicker {
        fn now_ticks(&self) -> u64 {
            0
        }
    }

    /// A clock that advances a fixed step on every read, so a watched run's elapsed time grows, and reports every read
    /// on a channel.
    struct StepTicker {
        step: u64,
        reads: AtomicU64,
        read: Sender<()>,
    }

    impl StepTicker {
        /// A clock stepping `step` ticks a read, reporting each read on `read`.
        fn new(step: u64, read: Sender<()>) -> Self {
            Self {
                step,
                reads: AtomicU64::new(0),
                read,
            }
        }
    }

    impl Ticker for StepTicker {
        fn now_ticks(&self) -> u64 {
            let _ = self.read.send(());
            self.step.saturating_mul(self.reads.fetch_add(1, Ordering::SeqCst).saturating_add(1))
        }
    }

    /// An event a test's writer reports: a line the pump wrote, or the writer being dropped, which is the pump's
    /// thread having ended.
    #[derive(Debug)]
    enum Event {
        Line(String),
        Ended,
    }

    /// A writer that reports every line it is given on a channel.
    struct ReportingWriter {
        events: Sender<Event>,
    }

    impl Write for ReportingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let line = String::from_utf8_lossy(buf).trim_end().to_string();
            let _ = self.events.send(Event::Line(line));
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every write fails.
    struct FailingWrite {
        attempts: Arc<AtomicUsize>,
    }

    impl Write for FailingWrite {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("the pipe is closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose writes succeed and whose every flush fails.
    struct FailingFlush {
        attempts: Arc<AtomicUsize>,
    }

    /// A writer whose every write panics.
    struct PanickingWriter;

    impl Write for PanickingWriter {
        #[expect(
            clippy::panic,
            clippy::panic_in_result_fn,
            reason = "the pump must report a writer that panics even inside its own panic line"
        )]
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            panic!("the pipe blew up")
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for FailingFlush {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("the pipe is closed"))
        }
    }

    /// A writer that reports its own drop on a channel, so a test can wait on the pump's thread ending without ever
    /// looking at a clock.
    struct DropReporter<W> {
        writer: W,
        events: Sender<Event>,
    }

    impl<W: Write> Write for DropReporter<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writer.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }

    impl<W> Drop for DropReporter<W> {
        fn drop(&mut self) {
            let _ = self.events.send(Event::Ended);
        }
    }

    /// A writer whose first write logs that it is inside it and then blocks until the test releases it.
    struct StuckWriter {
        entered: Sender<()>,
        held: Receiver<()>,
        log: Arc<Mutex<Vec<&'static str>>>,
        blocked: bool,
    }

    impl Write for StuckWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                lock(&self.log).push("entered");
                let _ = self.entered.send(());
                let _ = self.held.recv();
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A call the pump made on its writer.
    enum Call {
        Write,
        Flush,
    }

    /// A writer that reports every write and flush it is given on a channel, in order.
    struct CallReporter {
        calls: Sender<Call>,
    }

    impl Write for CallReporter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.calls.send(Call::Write);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let _ = self.calls.send(Call::Flush);
            Ok(())
        }
    }

    /// The next line the pump wrote, waiting for one however long it takes; an empty string once its thread ended.
    fn next_line(events: &Receiver<Event>) -> String {
        match events.recv_timeout(GUARD) {
            Ok(Event::Line(line)) => line,
            Ok(Event::Ended) | Err(_) => String::new(),
        }
    }

    /// The next line a watched pump wrote, skipping the lines an unwatched one wrote before it was given a run.
    fn next_beat(events: &Receiver<Event>) -> String {
        loop {
            let line = next_line(events);
            if line != IDLE_LINE {
                return line;
            }
        }
    }

    /// Every line the writer was given until the pump's thread ended, and whether it ended.
    fn drain(events: &Receiver<Event>) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        while let Ok(event) = events.recv_timeout(GUARD) {
            match event {
                Event::Line(line) => lines.push(line),
                Event::Ended => return (lines, true),
            }
        }
        (lines, false)
    }

    /// Whether the pump's writer was dropped, which is its thread having ended.
    fn ended(events: &Receiver<Event>) -> bool {
        while let Ok(event) = events.recv_timeout(GUARD) {
            if matches!(event, Event::Ended) {
                return true;
            }
        }
        false
    }

    /// The four rows of `Test-HeartbeatBeatClock` (`remote.selftest.ps1`), whose samples are 5 s apart.
    #[test]
    fn beat_clock_rows_match_the_remote_self_test() {
        let period = 5 * SECOND;
        let rows: [(&str, &[u64], &[bool]); 4] = [
            ("51 ms per 5 s", &[51], &[true]),
            ("49 ms per 5 s over 15 s", &[49, 49, 49], &[false, false, false]),
            ("0 then 120 ms", &[0, 120], &[false, true]),
            ("60, 49, 51 ms", &[60, 49, 51], &[true, false, true]),
        ];
        for (name, cpu_ms, want) in rows {
            let mut clock = BeatClock::new();
            clock.reset(0);
            let mut now = 0;
            let mut got = Vec::new();
            for ms in cpu_ms {
                now += period;
                got.push(clock.sample(ms * MS, 0, now));
            }
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn elapsed_zero_is_no_beat() {
        let mut clock = BeatClock::new();
        clock.reset(1000);
        assert!(!clock.sample(1_000_000, 1_000_000, 1000), "no time passed, so no beat");
    }

    #[test]
    fn pump_writes_unconditionally_when_unwatched() {
        let (events, seen) = channel();
        let pump = Pump::start(ReportingWriter { events }, Arc::new(ZeroTicker), PERIOD).expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        assert!(pump.stop().is_ok(), "a pump that only wrote heartbeats reports nothing");
    }

    #[test]
    fn an_unwatched_pump_reports_the_last_run_cpu() {
        let (events, seen) = channel();
        let (reads, _read) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: ReportingWriter { events: events.clone() },
                events,
            },
            Arc::new(StepTicker::new(5 * SECOND, reads)),
            PERIOD,
        )
        .expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);

        let value = Arc::new(AtomicU64::new(0));
        let cpu: Arc<dyn CpuSource> = Arc::new(FixedCpu(Arc::clone(&value)));
        pump.watch(cpu);
        value.store(1_000_000_000, Ordering::SeqCst);
        assert_eq!(next_beat(&seen), "gate-remote-heartbeat: 100000", "the watched run's beat");
        pump.unwatch();
        assert_eq!(
            next_line(&seen),
            "gate-remote-heartbeat: 100000",
            "with no run watched the pump reports the last run's CPU total"
        );
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_stops_after_a_failed_write() {
        let (events, seen) = channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let pump = Pump::start(
            DropReporter {
                writer: FailingWrite {
                    attempts: Arc::clone(&attempts),
                },
                events,
            },
            Arc::new(ZeroTicker),
            Duration::ZERO,
        )
        .expect("the pump starts");
        assert!(ended(&seen), "the pump ended after its write failed");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "the failed write stopped the pump from trying again");
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_stops_after_a_failed_flush() {
        let (events, seen) = channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let pump = Pump::start(
            DropReporter {
                writer: FailingFlush {
                    attempts: Arc::clone(&attempts),
                },
                events,
            },
            Arc::new(ZeroTicker),
            Duration::ZERO,
        )
        .expect("the pump starts");
        assert!(ended(&seen), "the pump ended after its flush failed");
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "the failed flush stopped the pump from trying again");
        assert!(pump.stop().is_ok());
    }

    /// A watched run that uses no CPU on a clock that never moves earns no line, so every tick from the watch on has
    /// none due: the pump flushes its writer on such a tick. A line is one write and its flush, so a flush with no write
    /// before it is a tick with no line.
    #[test]
    fn an_idle_tick_flushes_the_writer() {
        let (calls, made) = channel();
        let pump = Pump::start(CallReporter { calls }, Arc::new(ZeroTicker), PERIOD).expect("the pump starts");
        pump.watch(idle_cpu());
        let mut after_write = false;
        let mut idle_flush = false;
        while !idle_flush {
            match made.recv_timeout(GUARD).expect("the pump calls its writer again within the guard") {
                Call::Write => after_write = true,
                Call::Flush => {
                    idle_flush = !after_write;
                    after_write = false;
                }
            }
        }
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_reports_a_cpu_read_failure_once() {
        let (events, seen) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: ReportingWriter { events: events.clone() },
                events,
            },
            Arc::new(ZeroTicker),
            PERIOD,
        )
        .expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        pump.watch(Arc::new(AlwaysFailingCpu));
        let (lines, finished) = drain(&seen);
        assert!(finished, "the pump ended after the read failed");
        assert_eq!(
            lines.last(),
            Some(&STOPPED_LINE.to_string()),
            "the read failure is the last line written: {lines:?}"
        );
        for line in &lines[..lines.len() - 1] {
            assert_eq!(line, IDLE_LINE, "only the unwatched pump's own beats come before it: {lines:?}");
        }
        assert_eq!(
            lines.iter().filter(|line| line.as_str() == STOPPED_LINE).count(),
            1,
            "the read failure is reported once: {lines:?}"
        );
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_reports_a_mid_run_cpu_read_failure_once() {
        let (events, seen) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: ReportingWriter { events: events.clone() },
                events,
            },
            Arc::new(ZeroTicker),
            PERIOD,
        )
        .expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        pump.watch(Arc::new(FailAfterFirst(AtomicUsize::new(0))));
        let (lines, finished) = drain(&seen);
        assert!(finished, "the pump ended after the read failed mid-run");
        assert_eq!(
            lines.last(),
            Some(&STOPPED_LINE.to_string()),
            "the mid-run read failure is the last line: {lines:?}"
        );
        for line in &lines[..lines.len() - 1] {
            assert_eq!(line, IDLE_LINE, "only the unwatched pump's own beats come before it: {lines:?}");
        }
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_beats_only_when_the_beat_clock_says_so() {
        let (events, seen) = channel();
        let (reads, read) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: ReportingWriter { events: events.clone() },
                events,
            },
            Arc::new(StepTicker::new(5 * SECOND, reads)),
            PERIOD,
        )
        .expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);

        let value = Arc::new(AtomicU64::new(0));
        let cpu: Arc<dyn CpuSource> = Arc::new(FixedCpu(Arc::clone(&value)));
        pump.watch(cpu);
        value.store(1_000_000_000, Ordering::SeqCst);
        assert_eq!(
            next_beat(&seen),
            "gate-remote-heartbeat: 100000",
            "the 100 s of CPU the watched run used, in ms"
        );

        // The reads queued so far are the ones the beat itself took. Every read from here is one more period watched
        // with a run that has used no more CPU, and none of them may write: a beat starts the clock over.
        while read.try_recv().is_ok() {}
        for _ in 0..3 {
            assert!(read.recv_timeout(GUARD).is_ok(), "the pump read its clock again");
        }
        assert!(
            seen.try_iter().all(|event| !matches!(event, Event::Line(_))),
            "a run that used no more CPU beats no more"
        );
        assert!(pump.stop().is_ok());
    }

    #[test]
    fn pump_reports_a_panicking_cpu_source() {
        let (events, seen) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: ReportingWriter { events: events.clone() },
                events,
            },
            Arc::new(ZeroTicker),
            PERIOD,
        )
        .expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        pump.watch(Arc::new(PanicAfterFirst(AtomicUsize::new(0))));
        let (lines, finished) = drain(&seen);
        assert!(finished, "the pump's thread ended after the panic");
        assert_eq!(
            lines.last(),
            Some(&"gate: remote heartbeat stopped: the heartbeat pump panicked: the CPU source blew up".to_string()),
            "the panic is reported as a stopped heartbeat: {lines:?}"
        );
        assert_eq!(
            pump.stop(),
            Err("the CPU source blew up".to_string()),
            "stop reports what the pump's thread panicked with"
        );
    }

    /// A writer that panics on every write: the pump's thread dies inside its own panic line, and `stop` still says
    /// what the pump panicked with.
    #[test]
    fn pump_reports_a_writer_that_panics() {
        let (events, seen) = channel();
        let pump = Pump::start(
            DropReporter {
                writer: PanickingWriter,
                events,
            },
            Arc::new(ZeroTicker),
            PERIOD,
        )
        .expect("the pump starts");
        assert!(ended(&seen), "the pump's thread died inside its panic line");
        assert_eq!(
            pump.stop(),
            Err("the pipe blew up".to_string()),
            "a thread that died inside the panic line is still reported"
        );
    }

    #[test]
    fn heartbeat_stop_within_returns_once_a_free_pump_ends() {
        let (events, seen) = channel();
        let pump = Pump::start(ReportingWriter { events }, Arc::new(ZeroTicker), PERIOD).expect("the pump starts");
        assert_eq!(next_line(&seen), IDLE_LINE);
        assert_eq!(pump.stop_within(GUARD), Ok(()), "a pump free to end ends, and reports nothing");
    }

    /// A writer blocked for good must not hold up a bounded stop: it returns saying so, and leaves the thread behind.
    #[test]
    fn heartbeat_stop_within_leaves_a_pump_stuck_in_its_writer() {
        let (entered, entered_seen) = channel();
        let (release, held) = channel::<()>();
        let writer = StuckWriter {
            entered,
            held,
            log: Arc::new(Mutex::new(Vec::new())),
            blocked: false,
        };
        let pump = Pump::start(writer, Arc::new(ZeroTicker), PERIOD).expect("the pump starts");
        assert!(entered_seen.recv_timeout(GUARD).is_ok(), "the pump reached its write");
        assert_eq!(
            pump.stop_within(Duration::from_millis(20)),
            Err("the heartbeat pump did not stop within 20 ms and was left to end on its own".to_string())
        );
        let _ = release.send(());
    }

    /// A write that blocks must not hold up `watch` or `unwatch`. A regression here shows as a hang the gate's own
    /// timeout ends, not as a different event order: the calls below return, and the log then reads in order.
    #[test]
    fn a_stuck_write_does_not_block_unwatch() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_seen) = channel();
        let (release, held) = channel::<()>();
        let writer = StuckWriter {
            entered,
            held,
            log: Arc::clone(&log),
            blocked: false,
        };
        let pump = Pump::start(writer, Arc::new(ZeroTicker), PERIOD).expect("the pump starts");
        assert!(entered_seen.recv_timeout(GUARD).is_ok(), "the pump reached its write");
        lock(&log).push("watch");
        pump.watch(idle_cpu());
        lock(&log).push("unwatch");
        pump.unwatch();
        let _ = release.send(());
        lock(&log).push("stop");
        assert!(pump.stop().is_ok());
        assert_eq!(*lock(&log), ["entered", "watch", "unwatch", "stop"]);
    }
}

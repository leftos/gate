//! The CPU a run's command has used, for the heartbeat: each process of its process group, and each member that left
//! it, read with `proc_pid_rusage(RUSAGE_INFO_V4)` for its own CPU and the CPU of the children it reaped, its parent
//! and group read with `proc_pidinfo(PROC_PIDTBSDINFO)`. A process that vanishes is dropped when a member reaped it,
//! since its figure is then in that member's children, and folded into a retired sum when its reaper is outside the
//! group (launchd for an orphan, the box for the leader), so each process's CPU is counted once. The figures come in
//! mach ticks, converted to 100 ns ticks by the timebase (125/3 ns a tick on Apple Silicon, 1/1 on Intel).
//!
//! On macOS a zombie is still listed in its group and `proc_pid_rusage` still reads its final figures, while
//! `proc_pidinfo` fails on it; a reap moves the reaped process's final own and children's CPU into its reaper's
//! children's CPU at once.
//!
//! A tracked process whose read fails other than with `ESRCH` (no such process), say while a setuid program it runs
//! cannot be read, is held at its last value and parent until it reads again or is gone. The figure still loses or
//! doubles some CPU:
//! - a child whose parent ended since the child's last read, then reaped by launchd, is dropped with its parent's
//!   chain, so the CPU it used is lost;
//! - a zombie first seen with no parent ever read is dropped, as if a member reaped it, so if launchd reaps it its CPU
//!   is lost;
//! - in a group past the listing's cap of 4096 pids, a child whose parent is admitted only later can be folded and then
//!   counted again in that parent's children.
//!
//! The conversion and the tally read no operating system state, so their tests run on every target; the reads are
//! macOS only.

use std::collections::BTreeMap;

/// The nanoseconds in one 100 ns tick.
const NANOS_PER_TICK: u128 = 100;

/// The parent of a process whose parent could never be read: a zombie first seen after it ended.
pub const UNKNOWN_PARENT: i32 = 0;

/// `mach` ticks in 100 ns ticks under the timebase `numer`/`denom` (nanoseconds a mach tick is), saturating; a zero
/// `denom`, which no timebase read gives, reads as 1.
#[must_use]
pub fn mach_to_ticks(mach: u64, numer: u32, denom: u32) -> u64 {
    let nanos = u128::from(mach) * u128::from(numer) / u128::from(denom.max(1));
    u64::try_from(nanos / NANOS_PER_TICK).unwrap_or(u64::MAX)
}

/// One process as one tick's read of it found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcRead {
    /// Its pid.
    pub pid: i32,
    /// Its start, in mach ticks: with the pid, what tells a reused pid from the process before it.
    pub start: u64,
    /// Its parent, the last one known; [`UNKNOWN_PARENT`] when none could be read.
    pub ppid: i32,
    /// The CPU it has used itself, user and system, in mach ticks.
    pub own: u64,
    /// The CPU, user and system in mach ticks, of the children it has reaped.
    pub children: u64,
}

impl ProcRead {
    /// Its own CPU and its reaped children's, saturating.
    fn value(&self) -> u64 {
        self.own.saturating_add(self.children)
    }
}

/// What a tally keeps of a process read at the last tick.
#[derive(Clone, Copy, Debug)]
struct Live {
    ppid: i32,
    value: u64,
}

/// A process read at the last tick that this tick's reads miss.
#[derive(Clone, Copy, Debug)]
struct Gone {
    start: u64,
    ppid: i32,
    value: u64,
}

/// The CPU a group's processes have used, each counted once: the processes read at the last tick, by pid and start,
/// and the retired sum of those that vanished with their reaper outside the tracked set; the figure only rises.
#[derive(Debug, Default)]
pub struct Tally {
    live: BTreeMap<(i32, u64), Live>,
    retired: u64,
    reported: u64,
}

impl Tally {
    /// Counts one tick's `reads` and gives the figure in mach ticks: the retired sum, each read's own and children's
    /// CPU and each `held` process's last value, never lower than the figure the tick before gave. A `held` process,
    /// by pid and start, is one kept at the last tick that could not be read this tick but is still there; it keeps its
    /// last value and parent. A kept process neither read nor held has vanished.
    pub fn tick(&mut self, reads: &[ProcRead], held: &[(i32, u64)]) -> u64 {
        let mut next: BTreeMap<(i32, u64), Live> = reads
            .iter()
            .map(|each| {
                (
                    (each.pid, each.start),
                    Live {
                        ppid: each.ppid,
                        value: each.value(),
                    },
                )
            })
            .collect();
        for key in held {
            if let Some(live) = self.live.get(key) {
                next.entry(*key).or_insert(*live);
            }
        }
        let present: BTreeMap<i32, u64> = next.keys().copied().collect();
        let gone: BTreeMap<i32, Gone> = self
            .live
            .iter()
            .filter(|(key, _)| !next.contains_key(key))
            .map(|(&(pid, start), live)| {
                let Live { ppid, value } = *live;
                (pid, Gone { start, ppid, value })
            })
            .collect();
        for vanished in gone.values() {
            if !reaped_in_tree(vanished, &present, &gone) {
                self.retired = self.retired.saturating_add(vanished.value);
            }
        }
        let candidate = next.values().fold(self.retired, |sum, live| sum.saturating_add(live.value));
        self.live = next;
        self.reported = self.reported.max(candidate);
        self.reported
    }

    /// How many processes the tally keeps: those read or held at the last tick.
    #[cfg(test)]
    pub(crate) fn live_len(&self) -> usize {
        self.live.len()
    }

    /// The processes kept at the last tick, read or held: each one's pid, start and last known parent.
    pub fn tracked(&self) -> impl Iterator<Item = (i32, u64, i32)> + '_ {
        self.live.iter().map(|(&(pid, start), live)| (pid, start, live.ppid))
    }
}

/// Whether `vanished`'s figure is, or will be at its reap, in the children of a process still there this tick: its
/// parent chain, through the processes that vanished with it, reaches one in `read` that started before it, or a parent
/// never known; `false` when the chain leaves the tracked set.
fn reaped_in_tree(vanished: &Gone, read: &BTreeMap<i32, u64>, gone: &BTreeMap<i32, Gone>) -> bool {
    let (mut start, mut ppid) = (vanished.start, vanished.ppid);
    loop {
        if ppid == UNKNOWN_PARENT || read.get(&ppid).is_some_and(|parent| *parent < start) {
            return true;
        }
        // Each step goes to a process started earlier, so the chain ends.
        match gone.get(&ppid) {
            Some(parent) if parent.start < start => (start, ppid) = (parent.start, parent.ppid),
            _ => return false,
        }
    }
}

#[cfg(target_os = "macos")]
pub use mac::GroupCpu;

/// A process group's CPU is read only on macOS: elsewhere every read fails, which ends the heartbeats.
#[cfg(all(unix, not(target_os = "macos")))]
pub struct GroupCpu;

#[cfg(all(unix, not(target_os = "macos")))]
impl GroupCpu {
    /// The source of the process group `_group`, which cannot be read here.
    #[must_use]
    pub fn new(_group: i32) -> GroupCpu {
        GroupCpu
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl gate_watchdog::CpuSource for GroupCpu {
    fn job_cpu_ticks(&self) -> Result<u64, String> {
        Err(format!("a process group's CPU is not read on {}", std::env::consts::OS))
    }

    fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, PoisonError};

    use gate_watchdog::CpuSource;

    use super::{ProcRead, Tally, UNKNOWN_PARENT, mach_to_ticks};

    /// How a group's processes are listed: [`crate::stop::group_pids_into`], or a test's own.
    type Listing = fn(i32, &mut Vec<i32>) -> Result<(), String>;

    /// The heartbeat's CPU source for the command whose process group is `group`, its leader the group's own pid.
    pub struct GroupCpu {
        group: i32,
        listing: Listing,
        /// The mach timebase, `numer` and `denom`, read once; why not when it could not be read.
        timebase: Result<(u32, u32), String>,
        state: Mutex<Reading>,
    }

    /// What the reads keep between calls: the tally, and the room the pids are listed into.
    #[derive(Default)]
    struct Reading {
        tally: Tally,
        pids: Vec<i32>,
    }

    /// A process admitted to a tick, as the pre-read found it.
    #[derive(Clone, Copy)]
    struct Candidate {
        pid: i32,
        start: u64,
        ppid: i32,
        /// Whether the tally kept it at the last tick, so that a read failing other than with `ESRCH` holds it.
        tracked: bool,
    }

    /// What `proc_pid_rusage` reads of a process, in mach ticks.
    #[derive(Clone, Copy)]
    struct Usage {
        start: u64,
        own: u64,
        children: u64,
    }

    /// What `proc_pidinfo(PROC_PIDTBSDINFO)` reads of a process.
    #[derive(Clone, Copy)]
    struct Bsd {
        ppid: i32,
        pgid: u32,
    }

    impl GroupCpu {
        /// The CPU source of the process group `group`, listed with `proc_listpids`.
        #[must_use]
        pub fn new(group: i32) -> GroupCpu {
            GroupCpu::listed_by(group, crate::stop::group_pids_into)
        }

        /// The CPU source of `group`, its processes listed by `listing`.
        fn listed_by(group: i32, listing: Listing) -> GroupCpu {
            GroupCpu {
                group,
                listing,
                timebase: timebase(),
                state: Mutex::new(Reading::default()),
            }
        }
    }

    impl CpuSource for GroupCpu {
        /// The group's CPU in 100 ns ticks: each listed process still in the group and each tracked one, read in start
        /// order, a process that cannot be read skipped, each process's CPU counted once ([`Tally`]).
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            let (numer, denom) = self.timebase.clone()?;
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Reading { tally, pids } = &mut *state;
            (self.listing)(self.group, pids).map_err(|why| format!("cannot list process group {}: {why}", self.group))?;
            let group = u32::try_from(self.group).unwrap_or(u32::MAX);
            let candidates = admitted(pids, tally, group);
            let (reads, held) = read_in_start_order(candidates);
            Ok(mach_to_ticks(tally.tick(&reads, &held), numer, denom))
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
    }

    /// The processes this tick reads: each one `tally` kept, by its kept start, whether still in `group` or not; and each
    /// pid of `listed` that `tally` does not keep, admitted by [`admit`].
    fn admitted(listed: &[i32], tally: &Tally, group: u32) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = tally
            .tracked()
            .map(|(pid, start, ppid)| Candidate {
                pid,
                start,
                ppid: bsd_read(pid).map_or(ppid, |read| read.ppid),
                tracked: true,
            })
            .collect();
        let kept: BTreeSet<i32> = candidates.iter().map(|candidate| candidate.pid).collect();
        candidates.extend(listed.iter().filter(|pid| !kept.contains(pid)).filter_map(|pid| admit(*pid, group)));
        candidates
    }

    /// The listed process `pid` as the pre-read finds it, when it is still in `group` (or a zombie, whose group cannot
    /// be read); `None` otherwise, or when it cannot be read.
    fn admit(pid: i32, group: u32) -> Option<Candidate> {
        let start = usage(pid).ok()?.start;
        let bsd = bsd_read(pid);
        if bsd.is_some_and(|read| read.pgid != group) {
            return None;
        }
        Some(Candidate {
            pid,
            start,
            ppid: bsd.map_or(UNKNOWN_PARENT, |read| read.ppid),
            tracked: false,
        })
    }

    /// The `candidates` read in start order, so a parent is read before its child and a reap between the two reads
    /// misses for a tick rather than counts twice; and the tracked ones whose read failed other than with `ESRCH`, by
    /// pid and start, to hold. A read whose start differs from the pre-read's, a reused pid, is dropped.
    fn read_in_start_order(mut candidates: Vec<Candidate>) -> (Vec<ProcRead>, Vec<(i32, u64)>) {
        candidates.sort_by_key(|candidate| candidate.start);
        let mut reads = Vec::with_capacity(candidates.len());
        let mut held = Vec::new();
        for candidate in &candidates {
            match usage(candidate.pid) {
                Ok(read) if read.start == candidate.start => reads.push(ProcRead {
                    pid: candidate.pid,
                    start: candidate.start,
                    ppid: candidate.ppid,
                    own: read.own,
                    children: read.children,
                }),
                Err(errno) if candidate.tracked && errno != libc::ESRCH => held.push((candidate.pid, candidate.start)),
                Ok(_) | Err(_) => {}
            }
        }
        (reads, held)
    }

    /// The mach timebase, `numer` and `denom`: the nanoseconds of one mach tick.
    #[expect(
        deprecated,
        reason = "libc's mach_timebase_info points at the mach2 crate; this one call needs no new dependency"
    )]
    fn timebase() -> Result<(u32, u32), String> {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `info` lives across the call, which writes only into it.
        let read = unsafe { libc::mach_timebase_info(&raw mut info) };
        if read != 0 || info.denom == 0 {
            return Err(format!("cannot read the mach timebase: kern_return {read}"));
        }
        Ok((info.numer, info.denom))
    }

    /// The process `pid` as `proc_pid_rusage(RUSAGE_INFO_V4)` reads it, a zombie included; the `errno` of a read that
    /// fails, `ESRCH` when no process has the pid.
    fn usage(pid: i32) -> Result<Usage, i32> {
        // SAFETY: an all-zero `rusage_info_v4` is a valid value of a plain C struct.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` lives across the call, which writes one `rusage_info_v4`, the flavor asked, into it.
        let read = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
        if read != 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        Ok(Usage {
            start: info.ri_proc_start_abstime,
            own: info.ri_user_time.saturating_add(info.ri_system_time),
            children: info.ri_child_user_time.saturating_add(info.ri_child_system_time),
        })
    }

    /// The parent and group of the process `pid` from `proc_pidinfo(PROC_PIDTBSDINFO)`; `None` when it cannot be read,
    /// as for a zombie.
    fn bsd_read(pid: i32) -> Option<Bsd> {
        // SAFETY: an all-zero `proc_bsdinfo` is a valid value of a plain C struct.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is `size` bytes and lives across the call, which writes at most that much into it.
        let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if filled != size {
            return None;
        }
        Some(Bsd {
            ppid: i32::try_from(info.pbi_ppid).ok()?,
            pgid: info.pbi_pgid,
        })
    }

    #[cfg(test)]
    #[expect(clippy::expect_used, reason = "a test whose program cannot start must fail loudly")]
    mod tests {
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        use std::sync::PoisonError;
        use std::thread;
        use std::time::{Duration, Instant};

        use gate_watchdog::CpuSource;

        use super::{GroupCpu, bsd_read, timebase, usage};
        use crate::cpu::mach_to_ticks;

        /// How long a case waits for what it expects: a hang guard only.
        const HANG_GUARD: Duration = Duration::from_secs(120);

        #[expect(
            clippy::disallowed_methods,
            reason = "the spawn lock is the Windows gate's; this test starts the group it reads"
        )]
        fn grouped(script: &str) -> Child {
            Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("sh starts")
        }

        fn group_of(child: &Child) -> i32 {
            i32::try_from(child.id()).expect("a pid is a pid_t")
        }

        /// The CPU, in mach ticks, of the children `leader` has reaped; 0 when it cannot be read.
        fn children_mach(leader: i32) -> u64 {
            usage(leader).map_or(0, |read| read.children)
        }

        /// A listing holding a pid no process has, then the real group's.
        fn with_a_vanished_pid(group: i32, pids: &mut Vec<i32>) -> Result<(), String> {
            crate::stop::group_pids_into(group, pids)?;
            pids.push(i32::MAX - 1);
            Ok(())
        }

        #[test]
        fn a_vanished_member_is_skipped_not_an_error() {
            let mut child = grouped("sleep 60");
            let cpu = GroupCpu::listed_by(group_of(&child), with_a_vanished_pid);
            let read = cpu.job_cpu_ticks();
            // Best effort: the case's own sleep.
            let _ = child.kill();
            let _ = child.wait();
            assert!(read.is_ok(), "{read:?}");
            let failing = GroupCpu::listed_by(group_of(&child), |_, _| Err("no listing".to_owned()));
            assert!(failing.job_cpu_ticks().is_err(), "a listing that fails is the source's only error");
        }

        #[test]
        fn a_real_busy_member_makes_the_total_rise() {
            let mut child = grouped("while :; do :; done");
            let cpu = GroupCpu::new(group_of(&child));
            let first = cpu.job_cpu_ticks().expect("the group is read");
            let guard = Instant::now() + HANG_GUARD;
            let mut second = first;
            while second <= first && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                second = cpu.job_cpu_ticks().expect("the group is read");
            }
            // Best effort: the case's own loop.
            let _ = child.kill();
            let _ = child.wait();
            let third = cpu.job_cpu_ticks().expect("an emptied group still reads");
            assert!(second > first, "the busy member's CPU rose: {first} then {second}");
            assert!(third >= second, "a member that exited keeps what it used: {second} then {third}");
        }

        #[test]
        fn a_known_burn_reads_within_a_factor_of_two_of_getrusage() {
            // A child of a leader that waits for it: the burn is the leader's reaped children's, and the leader is
            // left unreaped, as the box's runner leaves it, while it is read.
            let mut child = grouped("perl -e '$t = times; 1 while times - $t < 1' & wait; sleep 60");
            let leader = group_of(&child);
            let (numer, denom) = timebase().expect("the timebase is read");
            let guard = Instant::now() + HANG_GUARD;
            let mut mach = 0;
            while mach == 0 && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                mach = children_mach(leader);
            }
            // Best effort: the case's own sleep.
            let _ = child.kill();
            let _ = child.wait();
            let read = Duration::from_nanos(mach_to_ticks(mach, numer, denom).saturating_mul(100));
            // SAFETY: an all-zero `rusage` is a valid value, which `getrusage` fills.
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            // SAFETY: `usage` lives across the call, which writes only into it.
            assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut usage) }, 0);
            let micros = |time: libc::timeval| u64::try_from(time.tv_sec).unwrap_or(0) * 1_000_000 + u64::try_from(time.tv_usec).unwrap_or(0);
            let reference = Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime));
            assert!(
                reference >= Duration::from_millis(900) && read >= reference / 2 && read <= reference * 2,
                "the leader's reaped burn read {read:?}, getrusage {reference:?} (timebase {numer}/{denom})"
            );
        }

        #[test]
        fn a_burn_the_leader_reaps_after_it_was_seen_is_counted_once() {
            let mut child = grouped("perl -e '$t = times; 1 while times - $t < 1'; sleep 60");
            let leader = group_of(&child);
            let (numer, denom) = timebase().expect("the timebase is read");
            let cpu = GroupCpu::new(leader);
            let guard = Instant::now() + HANG_GUARD;
            let half_second = 5_000_000;
            let mut seen = 0;
            while seen <= half_second && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                seen = cpu.job_cpu_ticks().expect("the group is read");
            }
            let unreaped = children_mach(leader) == 0;
            let mut reaped = 0;
            while reaped == 0 && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                reaped = children_mach(leader);
            }
            let last = cpu.job_cpu_ticks();
            // Best effort: the case's own sleep.
            let _ = child.kill();
            let _ = child.wait();
            let reaped = mach_to_ticks(reaped, numer, denom);
            let last = last.expect("the group is read");
            assert!(seen > half_second && reaped > 0, "the burn was seen at {seen} and reaped at {reaped}");
            assert!(
                unreaped,
                "the read that passed half a second, {seen}, came before sh reaped the burn: it saw it alive"
            );
            assert!(
                last < reaped + seen / 2,
                "the burn seen at {seen} ticks and reaped at {reaped} read {last}: counted once, not its live figure beside its reaped one"
            );
        }

        #[test]
        fn a_member_that_left_the_group_is_still_counted() {
            let marker = std::env::temp_dir().join(format!("gate-cpu-setsid-{}", std::process::id()));
            let script = format!(
                "perl -e 'use POSIX; select(undef, undef, undef, 0.05) until -e $ARGV[0]; setsid() or die; \
                 $t = times; 1 while times - $t < 1; sleep 60' '{}' & wait",
                marker.display()
            );
            let mut child = grouped(&script);
            let leader = group_of(&child);
            let cpu = GroupCpu::new(leader);
            let guard = Instant::now() + HANG_GUARD;
            let mut burner = None;
            while burner.is_none() && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                cpu.job_cpu_ticks().expect("the group is read");
                let state = cpu.state.lock().unwrap_or_else(PoisonError::into_inner);
                burner = state.tally.tracked().map(|(pid, _, _)| pid).find(|pid| *pid != leader);
            }
            std::fs::write(&marker, b"").expect("the marker is written");
            let half_second = 5_000_000;
            let mut figure = 0;
            while figure <= half_second && Instant::now() < guard {
                thread::sleep(Duration::from_millis(50));
                figure = cpu.job_cpu_ticks().expect("the group is read");
            }
            let group = u32::try_from(leader).expect("a pid is positive");
            let left = burner.and_then(bsd_read).is_some_and(|read| read.pgid != group);
            let reaped = children_mach(leader);
            // Best effort: the case's own processes and marker.
            if let Some(pid) = burner {
                // SAFETY: `kill` takes plain integers and touches no memory of this process.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&marker);
            assert!(burner.is_some(), "the perl child was tracked while still in the group");
            assert!(left, "the perl child left the group with setsid");
            assert!(
                figure > half_second && reaped == 0,
                "its burn counted while it lived outside the group: {figure} ticks, sh's reaped children {reaped}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcRead, Tally, UNKNOWN_PARENT, mach_to_ticks};

    fn proc(pid: i32, start: u64, ppid: i32, own: u64, children: u64) -> ProcRead {
        ProcRead {
            pid,
            start,
            ppid,
            own,
            children,
        }
    }

    /// The box's pid, the leader's parent.
    const BOX: i32 = 900;

    #[test]
    fn mach_ticks_convert_by_the_timebase_to_100_ns_ticks() {
        assert_eq!(mach_to_ticks(3, 125, 3), 1, "3 Apple Silicon ticks are 125 ns");
        assert_eq!(mach_to_ticks(24_000_000, 125, 3), 10_000_000, "a second of Apple Silicon ticks");
        assert_eq!(mach_to_ticks(1_000, 1, 1), 10, "Intel's ticks are nanoseconds");
        assert_eq!(mach_to_ticks(0, 125, 3), 0);
        assert_eq!(mach_to_ticks(u64::MAX, 1_000, 1), u64::MAX, "saturates rather than wraps");
        assert_eq!(
            u128::from(mach_to_ticks(u64::MAX, 125, 3)),
            u128::from(u64::MAX) * 125 / 3 / 100,
            "no overflow of the product on the way"
        );
        assert_eq!(mach_to_ticks(u64::MAX / 2, 1, 1), u64::MAX / 200, "no overflow on the way");
        assert_eq!(mach_to_ticks(1_000, 1, 0), 10, "a zero denom reads as 1");
    }

    #[test]
    fn a_member_the_leader_reaps_is_counted_once() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 2, 10, 10, 0)], &[]), 10);
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 12)], &[]),
            12,
            "make's 12 arrive in sh's children and are counted once"
        );
    }

    #[test]
    fn a_grandchild_a_live_member_reaps_is_counted_once() {
        let mut tally = Tally::default();
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 2, 10, 1, 0), proc(12, 3, 11, 5, 0)], &[]),
            6
        );
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 2, 10, 1, 7)], &[]),
            8,
            "the compile's 7 in make's children"
        );
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 8)], &[]), 8, "and then in sh's, once");
    }

    #[test]
    fn an_orphan_launchd_reaps_keeps_its_last_figure() {
        let mut tally = Tally::default();
        tally.tick(&[proc(10, 1, BOX, 0, 0), proc(13, 4, 1, 3, 2)], &[]);
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 0)], &[]), 5, "its own and its children's, folded");
    }

    #[test]
    fn a_member_that_left_the_group_is_counted_once_when_its_parent_reaps_it() {
        let mut tally = Tally::default();
        tally.tick(&[proc(10, 1, BOX, 0, 0), proc(14, 5, 10, 3, 0)], &[]);
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(14, 5, 10, 6, 0)], &[]),
            6,
            "still read by pid after setsid"
        );
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 6)], &[]), 6);
    }

    #[test]
    fn a_reap_between_a_parent_s_read_and_its_child_s_never_lowers_the_figure() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 2, 10, 10, 0)], &[]), 10);
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0)], &[]),
            10,
            "make gone, sh read before the reap: held, not 0"
        );
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 12)], &[]), 12);
    }

    #[test]
    fn the_tally_keeps_only_live_members() {
        let mut tally = Tally::default();
        for pid in 11..10_011 {
            tally.tick(
                &[
                    proc(10, 1, BOX, 0, u64::try_from(pid - 11).unwrap_or(0)),
                    proc(pid, u64::try_from(pid).unwrap_or(0), 10, 1, 0),
                ],
                &[],
            );
        }
        assert_eq!(tally.live_len(), 2, "10 000 members came and went; only sh and the last are kept");
    }

    #[test]
    fn a_reused_pid_with_a_new_start_is_a_new_member() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 0), proc(7, 2, 1, 100, 0)], &[]), 100);
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(7, 3, 1, 10, 0)], &[]),
            110,
            "the earlier process keeps its value and the new one adds its own"
        );
    }

    #[test]
    fn a_parent_and_grandchild_that_vanish_together_under_a_live_leader_are_dropped() {
        let mut tally = Tally::default();
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 2, 10, 2, 0), proc(12, 3, 11, 5, 0)], &[]),
            7
        );
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 7)], &[]), 7, "both in sh's children, through make's");
    }

    #[test]
    fn the_leader_and_a_child_that_vanish_together_are_both_folded() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 1, 0), proc(11, 2, 10, 3, 0)], &[]), 4);
        assert_eq!(
            tally.tick(&[proc(20, 5, BOX, 2, 0)], &[]),
            6,
            "the chain leaves through the box: both folded"
        );
    }

    #[test]
    fn a_process_with_no_parent_ever_read_is_dropped_when_it_vanishes() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 0, 0), proc(15, 2, UNKNOWN_PARENT, 4, 0)], &[]), 4);
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 4)], &[]),
            4,
            "a zombie of unknown parent is taken as reaped in the tree"
        );
    }

    #[test]
    fn a_process_whose_parent_pid_was_reused_by_a_later_process_is_folded() {
        let mut tally = Tally::default();
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 5, 12, 5, 0), proc(12, 7, 10, 3, 0)], &[]),
            8
        );
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(12, 7, 10, 6, 0)], &[]),
            11,
            "pid 12 started after 11, so it is not 11's reaper"
        );
    }

    #[test]
    fn a_process_whose_vanished_parent_pid_was_reused_by_a_later_process_is_folded() {
        let mut tally = Tally::default();
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 0), proc(11, 5, 12, 5, 0), proc(12, 7, 10, 3, 0)], &[]),
            8
        );
        assert_eq!(
            tally.tick(&[proc(10, 1, BOX, 0, 6)], &[]),
            11,
            "pid 12, gone with it, started after 11: 11 folds and 12 is in sh's children"
        );
    }

    #[test]
    fn a_process_unreadable_for_a_tick_is_held_not_retired() {
        let mut tally = Tally::default();
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 5, 0)], &[]), 5);
        assert_eq!(tally.tick(&[], &[(10, 1)]), 5, "the leader cannot be read but is still there");
        assert_eq!(tally.tick(&[proc(10, 1, BOX, 8, 0)], &[]), 8, "read again, it is counted once");
    }
}

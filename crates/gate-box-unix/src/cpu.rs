//! The CPU a run's command has used, for the heartbeat: every process of its process group read with
//! `proc_pidinfo(PROC_PIDTASKALLINFO)`, kept by pid and start at the highest value seen so a member that exits keeps
//! what it used, plus the processes the leader reaped, read with `proc_pid_rusage`. The figures come in mach ticks,
//! converted to 100 ns ticks by the timebase (125/3 ns a tick on Apple Silicon, 1/1 on Intel).
//!
//! The conversion and the high-water map read no operating system state, so their tests run on every target; the reads
//! are macOS only.

use std::collections::BTreeMap;

/// The nanoseconds in one 100 ns tick.
const NANOS_PER_TICK: u128 = 100;

/// `mach` ticks in 100 ns ticks under the timebase `numer`/`denom` (nanoseconds a mach tick is), saturating; a zero
/// `denom`, which no timebase read gives, reads as 1.
#[must_use]
pub fn mach_to_ticks(mach: u64, numer: u32, denom: u32) -> u64 {
    let nanos = u128::from(mach) * u128::from(numer) / u128::from(denom.max(1));
    u64::try_from(nanos / NANOS_PER_TICK).unwrap_or(u64::MAX)
}

/// One process as one read of it found it: its pid, its start (microseconds since 1970), its process group and the
/// CPU it has used in mach ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberRead {
    /// Its pid.
    pub pid: i32,
    /// Its start, in microseconds since 1970: with the pid, what tells a reused pid from the process before it.
    pub start: u64,
    /// Its process group, read in the same read as its CPU.
    pub group: u32,
    /// The CPU it has used, user and system, in mach ticks.
    pub mach: u64,
}

/// The reads in `reads` of processes still in the process group `group`: a pid listed in it may have been reused by a
/// process outside it before it was read.
pub fn members_of(reads: &[MemberRead], group: u32) -> impl Iterator<Item = &MemberRead> {
    reads.iter().filter(move |read| read.group == group)
}

/// The CPU a group's processes have used, each kept by pid and start at the highest value seen, so a member that
/// exits keeps its last value and the total only rises; with the leader's reaped children kept the same way.
#[derive(Debug, Default)]
pub struct HighWater {
    members: BTreeMap<(i32, u64), u64>,
    children: u64,
}

impl HighWater {
    /// Counts `read`: its value, unless lower than the one its pid and start already have.
    pub fn observe(&mut self, read: &MemberRead) {
        let kept = self.members.entry((read.pid, read.start)).or_insert(0);
        *kept = (*kept).max(read.mach);
    }

    /// Counts `mach` ticks as what the leader's reaped children used, unless lower than what was seen before.
    pub fn observe_children(&mut self, mach: u64) {
        self.children = self.children.max(mach);
    }

    /// Every member's highest value and the children's, summed in mach ticks, saturating.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.members.values().fold(self.children, |sum, mach| sum.saturating_add(*mach))
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
    use std::sync::{Mutex, PoisonError};

    use gate_watchdog::CpuSource;

    use super::{HighWater, MemberRead, mach_to_ticks, members_of};

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

    /// What the reads keep between calls: the high-water map, and the room the pids are listed into.
    #[derive(Default)]
    struct Reading {
        high: HighWater,
        pids: Vec<i32>,
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
        /// The group's CPU in 100 ns ticks: each listed process still in the group, a process that cannot be read
        /// skipped, and the leader's reaped children, a read of them that fails adding nothing.
        fn job_cpu_ticks(&self) -> Result<u64, String> {
            let (numer, denom) = self.timebase.clone()?;
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Reading { high, pids } = &mut *state;
            (self.listing)(self.group, pids).map_err(|why| format!("cannot list process group {}: {why}", self.group))?;
            let reads: Vec<MemberRead> = pids.iter().filter_map(|pid| member_read(*pid)).collect();
            let group = u32::try_from(self.group).unwrap_or(u32::MAX);
            for read in members_of(&reads, group) {
                high.observe(read);
            }
            if let Some(children) = children_mach(self.group) {
                high.observe_children(children);
            }
            Ok(mach_to_ticks(high.total(), numer, denom))
        }

        fn vm_cpu_ticks(&self) -> Result<Option<u64>, String> {
            Ok(None)
        }
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

    /// The process `pid` as `proc_pidinfo(PROC_PIDTASKALLINFO)` reads it; `None` when it cannot be read, gone or not.
    fn member_read(pid: i32) -> Option<MemberRead> {
        // SAFETY: an all-zero `proc_taskallinfo` is a valid value of a plain C struct.
        let mut info: libc::proc_taskallinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_taskallinfo>()).ok()?;
        // SAFETY: `info` is `size` bytes and lives across the call, which writes at most that much into it.
        let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKALLINFO, 0, (&raw mut info).cast(), size) };
        if filled != size {
            return None;
        }
        let start = info
            .pbsd
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbsd.pbi_start_tvusec);
        Some(MemberRead {
            pid: i32::try_from(info.pbsd.pbi_pid).ok()?,
            start,
            group: info.pbsd.pbi_pgid,
            mach: info.ptinfo.pti_total_user.saturating_add(info.ptinfo.pti_total_system),
        })
    }

    /// The CPU, user and system in mach ticks, of the children `leader` has reaped, from
    /// `proc_pid_rusage(RUSAGE_INFO_V4)`; `None` when it cannot be read.
    fn children_mach(leader: i32) -> Option<u64> {
        // SAFETY: an all-zero `rusage_info_v4` is a valid value of a plain C struct.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` lives across the call, which writes one `rusage_info_v4`, the flavor asked, into it.
        let read = unsafe { libc::proc_pid_rusage(leader, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
        (read == 0).then(|| info.ri_child_user_time.saturating_add(info.ri_child_system_time))
    }

    #[cfg(test)]
    #[expect(clippy::expect_used, reason = "a test whose program cannot start must fail loudly")]
    mod tests {
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        use gate_watchdog::CpuSource;

        use super::{GroupCpu, children_mach, timebase};
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
                mach = children_mach(leader).unwrap_or(0);
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
    }
}

#[cfg(test)]
mod tests {
    use super::{HighWater, MemberRead, mach_to_ticks, members_of};

    fn read(pid: i32, start: u64, group: u32, mach: u64) -> MemberRead {
        MemberRead { pid, start, group, mach }
    }

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
    fn the_high_water_map_only_rises() {
        let mut high = HighWater::default();
        high.observe(&read(10, 1, 10, 100));
        high.observe(&read(11, 1, 10, 50));
        assert_eq!(high.total(), 150);
        high.observe(&read(10, 1, 10, 80));
        assert_eq!(high.total(), 150, "a value that falls changes nothing");
        high.observe(&read(10, 1, 10, 200));
        assert_eq!(high.total(), 250, "member 11 vanished and keeps its last value");
        high.observe_children(30);
        high.observe_children(20);
        assert_eq!(high.total(), 280, "the children's count only rises too");
    }

    #[test]
    fn a_member_of_another_group_is_not_counted() {
        let reads = [read(10, 1, 10, 100), read(11, 1, 99, 1_000), read(12, 1, 10, 5)];
        let mut high = HighWater::default();
        for member in members_of(&reads, 10) {
            high.observe(member);
        }
        assert_eq!(high.total(), 105, "pid 11, reused outside the group, adds nothing");
    }

    #[test]
    fn a_reused_pid_with_a_new_start_is_a_new_member() {
        let mut high = HighWater::default();
        high.observe(&read(7, 1, 10, 100));
        high.observe(&read(7, 2, 10, 10));
        assert_eq!(high.total(), 110, "the earlier process keeps its value and the new one adds its own");
    }
}

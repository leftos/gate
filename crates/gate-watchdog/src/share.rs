//! The machine facts a sample carries and the share of it a command could have had.

/// CPU time in 100 ns ticks, the unit the Windows system and job-accounting times come in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticks(pub u64);

impl Ticks {
    /// The ticks gained from `earlier` to `self`, saturating at zero: these readings only ever rise, so one that fell
    /// is read as no gain rather than as a wrap.
    #[must_use]
    pub fn saturating_sub(self, earlier: Self) -> Self {
        Self(self.0.saturating_sub(earlier.0))
    }
}

/// The system's CPU times in 100 ns ticks: idle, kernel (which includes idle) and user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemTimes {
    /// The ticks the processors spent idle.
    pub idle: u64,
    /// The ticks they spent in kernel mode, idle time included.
    pub kernel: u64,
    /// The ticks they spent in user mode.
    pub user: u64,
}

/// The machine a command runs on and the share of it that command may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    /// The logical processors the machine offers.
    pub cores: u32,
    /// The processors the command's share is measured against, given by the caller: the light pool's thread count for
    /// a light gate, every core for any other.
    pub demand: u32,
    /// Whether the command is High priority, which no load of others slows.
    pub high: bool,
}

impl Machine {
    /// The machine `cores` logical processors offer a command that may use `demand` of them.
    ///
    /// The demand is the caller's, as `New-Watch` computes it: `min(cores, the light pool's threads)` for a light gate
    /// and `cores` for every other. `share_of` clamps no demand itself, so the caller that knows the pool clamps.
    #[must_use]
    pub fn new(cores: u32, demand: u32, high: bool) -> Self {
        Self { cores, demand, high }
    }
}

/// The share of its demand a command could have had when others left `free_cores` of the machine free ([`free_cores`]):
/// all of it for a High-priority command, else the free cores over the demand, clamped to `[0.05, 1]`. The demand is
/// taken as given, as `Get-FreeShare` takes it; what is clamped here is the share.
#[must_use]
pub(crate) fn share_of(machine: Machine, free_cores: f64) -> f64 {
    if machine.high {
        return 1.0;
    }
    (free_cores / f64::from(machine.demand)).clamp(0.05, 1.0)
}

/// The cores other processes left free over the interval from `before` to `after`, unclamped: the machine's busy cores,
/// `busy * cores`, less the `own_gained` cores the command used itself, are the cores others held, and the rest of the
/// machine's cores are free. A High-priority command is measured the same way.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "the PowerShell gate divides these tick counts as doubles, so the same loss keeps the share equal"
)]
pub(crate) fn free_cores(machine: Machine, before: SystemTimes, after: SystemTimes, own_gained: Ticks, interval_s: f64) -> f64 {
    let idle = after.idle.saturating_sub(before.idle);
    let total = after.kernel.saturating_sub(before.kernel) + after.user.saturating_sub(before.user);
    let busy = if total > 0 { 1.0 - (idle as f64 / total as f64) } else { 0.0 };
    let own_cores = (own_gained.0 as f64 / 1e7) / interval_s;
    let others = (busy * f64::from(machine.cores) - own_cores).max(0.0);
    f64::from(machine.cores) - others
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One free-share row of the PowerShell self-test: 16 cores and a 10 s interval, the machine `busy_sixteenths`
    /// sixteenths busy, the command having used `own_cores` cores of it over that interval.
    fn share(busy_sixteenths: u64, own_cores: u64, demand: u32, high: bool) -> f64 {
        const CAPACITY: u64 = 1_600_000_000;
        const TICKS_PER_CORE: u64 = 100_000_000;
        let idle = CAPACITY * (16 - busy_sixteenths) / 16;
        let user = CAPACITY * busy_sixteenths / 16;
        let machine = Machine::new(16, demand, high);
        let before = SystemTimes { idle: 0, kernel: 0, user: 0 };
        let after = SystemTimes { idle, kernel: idle, user };
        share_of(machine, free_cores(machine, before, after, Ticks(own_cores * TICKS_PER_CORE), 10.0))
    }

    /// The rows of `Test-FreeShare` (`gate.selftest.ps1`), in its order: a light gate's demand is its pool's two
    /// threads where the machine has them.
    #[test]
    fn free_share_rows_match_the_powershell_self_test() {
        let rows: [(&str, u64, u64, u32, bool, f64); 8] = [
            ("an idle machine", 0, 0, 16, false, 1.0),
            ("half the machine busy elsewhere", 8, 0, 16, false, 0.5),
            ("the command filling the machine itself", 16, 16, 16, false, 1.0),
            ("a light gate with 8 cores busy elsewhere", 8, 0, 2, false, 1.0),
            ("a light gate with 15 cores busy elsewhere", 15, 0, 2, false, 0.5),
            ("the floor", 16, 0, 16, false, 0.05),
            ("a High-priority command on a full machine", 16, 0, 16, true, 1.0),
            ("own CPU read past the busy share", 4, 6, 16, false, 1.0),
        ];
        for (name, busy, own, demand, high, want) in rows {
            let got = share(busy, own, demand, high);
            assert!((got - want).abs() <= 1e-9, "{name}: got {got}, expected {want}");
        }
    }
}

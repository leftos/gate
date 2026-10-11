//! The cores hint's verdict: whether a passed run's busy cores fitted the slot kind it ran on.

/// The average busy cores above which a light run is told heavy would fit: the light slot's thread budget.
const LIGHT_BUDGET: f64 = 2.0;
/// The average busy cores under which a heavy run is told light would fit.
const HEAVY_FLOOR: f64 = 1.5;
/// The free cores a heavy run must have had on average before it is told light would fit.
const COLD_FREE: f64 = 4.0;

/// How many cores a command kept busy and how many the rest of the machine left it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cores {
    /// The command's CPU over the wall time it was not paused, in cores, kept to one decimal.
    pub avg: f64,
    /// The command's highest CPU over one unpaused sampler interval, in cores, unrounded: each reader rounds it as it
    /// shows it.
    pub peak: f64,
    /// The mean over the unpaused samples of the cores other processes left free, kept to one decimal.
    pub free: f64,
}

/// The slot kind a run is judged on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// A light slot.
    Light,
    /// A heavy slot.
    Heavy,
    /// The whole machine, which is never judged.
    Critical,
}

/// A run that did not fit its slot kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// A light run kept more cores busy than the light slot's budget: heavy would fit.
    Hot,
    /// A heavy run kept under 1.5 cores busy while at least 4 were free: light would fit.
    Cold,
}

/// Whether `cores` fitted `slot`: a light run averaging over 2 cores ran hot whatever was free, a heavy run averaging
/// under 1.5 ran cold when at least 4 cores were free on average, and a critical run is never judged. The peak does not
/// change the verdict.
#[must_use]
pub fn hint(cores: Cores, slot: SlotKind) -> Option<Hint> {
    match slot {
        SlotKind::Light if cores.avg > LIGHT_BUDGET => Some(Hint::Hot),
        SlotKind::Heavy if cores.avg < HEAVY_FLOOR && cores.free >= COLD_FREE => Some(Hint::Cold),
        _ => None,
    }
}

/// `value` kept to one decimal, a midpoint to the even tenth.
#[must_use]
pub(crate) fn tenths(value: f64) -> f64 {
    (value * 10.0).round_ties_even() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cores(avg: f64, free: f64) -> Cores {
        Cores { avg, peak: 16.0, free }
    }

    #[test]
    fn cores_verdict_table() {
        let rows: [(&str, Cores, SlotKind, Option<Hint>); 9] = [
            ("light at its budget", cores(2.0, 16.0), SlotKind::Light, None),
            ("light past its budget", cores(2.1, 16.0), SlotKind::Light, Some(Hint::Hot)),
            (
                "light past its budget on a full machine",
                cores(2.1, 0.0),
                SlotKind::Light,
                Some(Hint::Hot),
            ),
            ("heavy under the floor with 4 free", cores(1.4, 4.0), SlotKind::Heavy, Some(Hint::Cold)),
            ("heavy under the floor with 3.9 free", cores(1.4, 3.9), SlotKind::Heavy, None),
            ("heavy at the floor", cores(1.5, 16.0), SlotKind::Heavy, None),
            ("heavy idle on a free machine", cores(0.0, 16.0), SlotKind::Heavy, Some(Hint::Cold)),
            ("critical idle", cores(0.0, 16.0), SlotKind::Critical, None),
            ("critical busy", cores(12.0, 16.0), SlotKind::Critical, None),
        ];
        for (name, cores, slot, want) in rows {
            assert_eq!(hint(cores, slot), want, "{name}");
        }
    }

    #[test]
    fn tenths_round_halves_to_even() {
        assert!((tenths(7.44) - 7.4).abs() < 1e-9);
        assert!((tenths(14.56) - 14.6).abs() < 1e-9);
        assert!((tenths(0.25) - 0.2).abs() < 1e-9, "a midpoint goes to the even tenth");
    }
}

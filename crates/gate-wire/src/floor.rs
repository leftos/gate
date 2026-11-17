//! The free-space floor both boxes' age prunes keep a work root's volume above: the free space under which the prune
//! goes on to remove work folders however recent, and the free space at which that removal stops, each a share of the
//! volume's size with a least amount of its own.

/// A gibibyte, as the free space is said.
pub const GIB: u64 = 1 << 30;
/// The size over which both boxes' age prunes remove a work folder no run holds, however recent its last use: a folder
/// in daily use is never idle, and cargo keeps every output whose hash changes, so without a cap it grows unbounded.
pub const FOLDER_CAP: u64 = 60 * GIB;
/// The share of a volume, in hundredths, under whose free space the floor pass starts.
const LOW_PERCENT: u128 = 20;
/// The share of a volume, in hundredths, at whose free space the floor pass stops.
const HIGH_PERCENT: u128 = 30;
/// The least free space under which the floor pass starts, whatever the volume's size.
const LEAST_LOW: u64 = 100 * GIB;
/// The least free space at which the floor pass stops, whatever the volume's size.
const LEAST_HIGH: u64 = 150 * GIB;

/// The free-space floor of one volume, in bytes: the floor pass starts while the free space is under `low` and stops
/// once it is at `high`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloorMarks {
    pub low: u64,
    pub high: u64,
}

/// The floor of a volume of `total_bytes`: `low` the greater of 20% of it and 100 GiB, `high` the greater of 30% of it
/// and 150 GiB, each share rounded down to the byte.
#[must_use]
pub fn floor_marks(total_bytes: u64) -> FloorMarks {
    FloorMarks {
        low: share(total_bytes, LOW_PERCENT).max(LEAST_LOW),
        high: share(total_bytes, HIGH_PERCENT).max(LEAST_HIGH),
    }
}

/// `percent` hundredths of `total`, rounded down; wide enough that no total overflows it, and never more than `total`.
fn share(total: u64, percent: u128) -> u64 {
    u64::try_from(u128::from(total) * percent / 100).unwrap_or(total)
}

/// What the preflight's free-space line starts with: `gate: remote free <free bytes> <total bytes>`.
pub const FREE_PREFIX: &str = "gate: remote free ";

/// The preflight's free-space line, `gate: remote free <free> <total>`: the bytes the box's user may still write on the
/// work root's volume and the volume's size, each a decimal count of bytes.
#[must_use]
pub fn free_line(free: u64, total: u64) -> String {
    format!("{FREE_PREFIX}{free} {total}")
}

/// The free bytes and the volume's size of a free-space line ([`free_line`]); `None` for any other line.
#[must_use]
pub fn free_of_line(line: &str) -> Option<(u64, u64)> {
    let (free, total) = line.strip_prefix(FREE_PREFIX)?.split_once(' ')?;
    Some((free.parse().ok()?, total.parse().ok()?))
}

/// Whether a volume with `free` bytes free of `total` is under its floor's low mark ([`floor_marks`]), where the
/// preflight starts the age prune out of schedule.
#[must_use]
pub fn is_low(free: u64, total: u64) -> bool {
    free < floor_marks(total).low
}

/// The stamp file in the work root a preflight's out-of-schedule prune start writes, whose write time keeps another
/// preflight from starting one for [`PRUNE_DEBOUNCE`].
pub const PRUNE_STAMP: &str = "prune-started";

/// How long after a start the stamp ([`PRUNE_STAMP`]) keeps a preflight from starting another.
pub const PRUNE_DEBOUNCE: std::time::Duration = std::time::Duration::from_mins(10);

/// What the line saying a preflight started the age prune out of schedule says, and starts its other forms with.
const PRUNE_STARTED: &str = "gate: remote prune started";

/// What the line saying a preflight's start of the age prune failed starts with.
const PRUNE_NOT_STARTED: &str = "gate: remote prune not started: ";

/// What a preflight on a volume under its low mark did about the age prune, said on the line after its free-space line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PruneStart {
    /// It started the prune now: `gate: remote prune started`.
    Started,
    /// Another preflight started it this many whole seconds ago, within [`PRUNE_DEBOUNCE`], so it started nothing:
    /// `gate: remote prune started <n> s ago`.
    Recent(u64),
    /// Its start failed, for this reason: `gate: remote prune not started: <why>`.
    Failed(String),
}

impl PruneStart {
    /// Its line, the reason of a failed start escaped.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            PruneStart::Started => PRUNE_STARTED.to_owned(),
            PruneStart::Recent(seconds) => format!("{PRUNE_STARTED} {seconds} s ago"),
            PruneStart::Failed(why) => format!("{PRUNE_NOT_STARTED}{}", crate::escaped(why)),
        }
    }

    /// What the prune line `line` says ([`PruneStart::line`]); `None` for any other line.
    #[must_use]
    pub fn of_line(line: &str) -> Option<PruneStart> {
        if line == PRUNE_STARTED {
            return Some(PruneStart::Started);
        }
        if let Some(why) = line.strip_prefix(PRUNE_NOT_STARTED) {
            return Some(PruneStart::Failed(why.to_owned()));
        }
        let seconds = line.strip_prefix(PRUNE_STARTED)?.strip_prefix(' ')?.strip_suffix(" s ago")?;
        seconds.parse().ok().map(PruneStart::Recent)
    }
}

/// How long before `now`, in whole seconds, a stamp last written at `written` started a prune when it still keeps a
/// preflight from starting another (written under [`PRUNE_DEBOUNCE`] before `now`); `None` for no stamp, an older one,
/// or one written after `now`, so a clock set back never stops the starts.
#[must_use]
pub fn recent_start(written: Option<std::time::SystemTime>, now: std::time::SystemTime) -> Option<u64> {
    let age = now.duration_since(written?).ok()?;
    (age < PRUNE_DEBOUNCE).then_some(age.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{FloorMarks, GIB, floor_marks};

    #[test]
    fn the_floor_of_a_1863_gib_volume_is_20_and_30_percent_of_it() {
        assert_eq!(
            floor_marks(2_000_398_934_016),
            FloorMarks {
                low: 400_079_786_803,
                high: 600_119_680_204
            }
        );
    }

    #[test]
    fn the_floor_of_a_small_volume_is_100_and_150_gib() {
        let least = FloorMarks {
            low: 100 * GIB,
            high: 150 * GIB,
        };

        assert_eq!(floor_marks(256 * GIB), least);
        assert_eq!(floor_marks(0), least);
    }

    #[test]
    fn the_floor_of_a_500_gib_volume_whose_20_percent_is_100_gib_exactly_is_100_and_150_gib() {
        assert_eq!(
            floor_marks(500 * GIB),
            FloorMarks {
                low: 100 * GIB,
                high: 150 * GIB
            }
        );
    }

    #[test]
    fn the_floor_of_a_600_gib_volume_is_120_and_180_gib() {
        assert_eq!(
            floor_marks(600 * GIB),
            FloorMarks {
                low: 120 * GIB,
                high: 180 * GIB
            }
        );
    }
}

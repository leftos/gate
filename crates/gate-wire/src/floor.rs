//! The free-space floor both boxes' age prunes keep a work root's volume above: the free space under which the prune
//! goes on to remove work folders however recent, and the free space at which that removal stops, each a share of the
//! volume's size with a least amount of its own.

/// A gibibyte, as the free space is said.
pub const GIB: u64 = 1 << 30;
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

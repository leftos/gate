//! The order a routed run tries its capable builders in: by the earliest expected finish of each, read from the figures
//! the caller gives for it ([`BuilderFigures`]), a held work folder and then file order breaking an exact tie
//! ([`order`]).

/// What the PC knows of one builder for one command, each time in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuilderFigures {
    /// How many of the builder's remote slots on this PC are free.
    pub free_slots: u32,
    /// The expected wait, in seconds, for a slot of the builder's pool on this PC; 0 with a free slot.
    pub queue_wait: f64,
    /// The learned duration, in seconds, of this command on the builder; `None` when it has none.
    pub duration: Option<f64>,
    /// Whether the builder is known to hold a work folder for the worktree.
    pub holds_folder: bool,
}

/// The figures every builder gets when the caller knows nothing to tell them apart: a free slot, no wait, no learned
/// duration and a work folder held, so every builder scores alike and file order decides.
pub const ALIKE: BuilderFigures = BuilderFigures {
    free_slots: 1,
    queue_wait: 0.0,
    duration: None,
    holds_folder: true,
};

/// The indexes of `figures`, best first: by expected finish, the queue wait plus the duration plus, for a builder that
/// holds no work folder for the worktree, the duration once more. A builder without a duration is given the median of
/// the others' (for an even count the mean of the two middle ones); when no builder has one, the queue wait alone
/// decides. An exact tie goes to a builder that holds a work folder first, then keeps the given (file) order, as the
/// sort is stable. Every queue wait and duration is non-negative and finite.
#[must_use]
pub fn order(figures: &[BuilderFigures]) -> Vec<usize> {
    ranked(figures).ranks.into_iter().map(|rank| rank.index).collect()
}

/// One builder's place in the order [`ranked`] gives: its index among the figures and its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rank {
    /// The builder's index among the figures given.
    pub index: usize,
    /// Its expected finish in seconds: the queue wait, plus the duration and, without a held work folder, the duration
    /// once more; the queue wait alone when no builder has a duration ([`Ranking::has_duration`]).
    pub seconds: f64,
}

/// The builders of one decision's figures, best first, each with its score ([`ranked`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    /// Each builder's place, best first.
    pub ranks: Vec<Rank>,
    /// Whether every score includes a duration, the builder's own or the median of the others': `true` when any
    /// builder has one, as then every builder without one is given that median; `false` when none has one, as the queue
    /// wait alone then scores each.
    pub has_duration: bool,
}

/// The builders of `figures`, best first, each with its score: the order [`order`] gives, by the same rule.
#[must_use]
pub fn ranked(figures: &[BuilderFigures]) -> Ranking {
    let usable = |seconds: f64| seconds.is_finite() && seconds >= 0.0;
    debug_assert!(
        figures
            .iter()
            .all(|builder| usable(builder.queue_wait) && builder.duration.is_none_or(usable)),
        "every queue wait and duration is non-negative and finite: {figures:?}"
    );
    let known: Vec<f64> = figures.iter().filter_map(|builder| builder.duration).collect();
    let fill = median(known);
    let scores: Vec<f64> = figures.iter().map(|builder| expected_finish(builder, fill)).collect();
    let mut indexes: Vec<usize> = (0..figures.len()).collect();
    indexes.sort_by(|&left, &right| {
        let warm_first = figures[right].holds_folder.cmp(&figures[left].holds_folder);
        scores[left].total_cmp(&scores[right]).then(warm_first)
    });
    let ranks = indexes
        .into_iter()
        .map(|index| Rank {
            index,
            seconds: scores[index],
        })
        .collect();
    Ranking {
        ranks,
        has_duration: fill.is_some(),
    }
}

/// The builders a routed run's decision scored from the figures this PC read, two or more, as the run's `balanced to`
/// line and its journal's `balanced` field tell them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balanced {
    /// The builder the run goes to first, then every other in score order.
    pub entries: Vec<BalancedEntry>,
    /// Whether the scores include a duration ([`Ranking::has_duration`]); the same for every builder of a decision.
    pub has_duration: bool,
}

/// One builder a routed run's decision scored ([`Balanced`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancedEntry {
    /// The builder's alias.
    pub alias: String,
    /// Its score ([`Rank::seconds`]) in whole tenths of a second, rounded ([`tenths`]).
    pub tenths: u64,
    /// How many of the builder's remote slots on this PC were free.
    pub free: u32,
    /// Whether the builder is not known to hold a work folder for the worktree.
    pub cold: bool,
}

impl BalancedEntry {
    /// Its score in seconds, to the tenth it keeps.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a score in tenths of a second stays far below 2^53, where an f64 holds every whole number"
    )]
    pub fn seconds(&self) -> f64 {
        self.tenths as f64 / 10.0
    }
}

/// `seconds` in whole tenths of a second, rounded to the nearest; 0 for a negative or NaN span and `u64::MAX` past it,
/// as the cast saturates.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the float-to-integer cast saturates, which is the rule this function states"
)]
pub fn tenths(seconds: f64) -> u64 {
    (seconds * 10.0).round() as u64
}

/// The builder's expected finish: its queue wait, plus its duration (`fill` for a builder without one) and, when it
/// holds no work folder for the worktree, that duration once more; the queue wait alone when `fill` is `None`, as no
/// builder has a duration.
fn expected_finish(builder: &BuilderFigures, fill: Option<f64>) -> f64 {
    let Some(duration) = builder.duration.or(fill) else {
        return builder.queue_wait;
    };
    let penalty = if builder.holds_folder { 0.0 } else { duration };
    builder.queue_wait + duration + penalty
}

/// The median of `durations`, the mean of the two middle ones for an even count; `None` when there are none.
fn median(mut durations: Vec<f64>) -> Option<f64> {
    durations.sort_by(f64::total_cmp);
    let middle = durations.len() / 2;
    match durations.len() {
        0 => None,
        count if count % 2 == 0 => Some(f64::midpoint(durations[middle - 1], durations[middle])),
        _ => Some(durations[middle]),
    }
}

#[cfg(test)]
mod tests {
    use super::{BuilderFigures, order};

    fn figures(queue_wait: f64, duration: Option<f64>, holds_folder: bool) -> BuilderFigures {
        BuilderFigures {
            free_slots: u32::from(queue_wait <= 0.0),
            queue_wait,
            duration,
            holds_folder,
        }
    }

    #[test]
    fn a_free_builder_beats_a_full_first_one() {
        let given = [figures(60.0, Some(100.0), true), figures(0.0, Some(100.0), true)];
        assert_eq!(order(&given), [1, 0]);
    }

    #[test]
    fn a_faster_builder_wins() {
        let given = [figures(0.0, Some(300.0), true), figures(0.0, Some(100.0), true)];
        assert_eq!(order(&given), [1, 0]);
    }

    #[test]
    fn a_cold_builder_pays_one_more_duration() {
        let given = [figures(0.0, Some(80.0), false), figures(0.0, Some(100.0), true)];
        assert_eq!(order(&given), [1, 0], "160 s cold against 100 s warm");
    }

    #[test]
    fn a_much_faster_cold_builder_still_beats_a_warm_one() {
        let given = [figures(0.0, Some(300.0), true), figures(0.0, Some(100.0), false)];
        assert_eq!(order(&given), [1, 0], "200 s cold against 300 s warm");
    }

    #[test]
    fn an_exact_tie_keeps_file_order() {
        let given = [
            figures(10.0, Some(100.0), true),
            figures(0.0, Some(50.0), false),
            figures(10.0, Some(100.0), true),
        ];
        assert_eq!(order(&given), [1, 0, 2]);
    }

    #[test]
    fn a_builder_without_a_duration_scores_with_the_median_of_the_others() {
        let given = [
            figures(0.0, Some(100.0), true),
            figures(0.0, Some(200.0), true),
            figures(0.0, Some(60.0), true),
            figures(0.0, None, true),
        ];
        assert_eq!(
            order(&given),
            [2, 0, 3, 1],
            "the fourth scores the median 100 s, tied with the first and after it"
        );
        let given = [figures(0.0, Some(100.0), true), figures(0.0, Some(60.0), true), figures(0.0, None, true)];
        assert_eq!(order(&given), [1, 2, 0], "the even count's median is the mean 80 s");
        let given = [figures(0.0, Some(50.0), true), figures(0.0, None, false)];
        assert_eq!(order(&given), [0, 1], "a cold builder pays the median once more");
    }

    #[test]
    fn with_no_duration_anywhere_queue_wait_then_file_order_decide() {
        let given = [
            figures(30.0, None, true),
            figures(0.0, None, false),
            figures(30.0, None, false),
            figures(0.0, None, true),
        ];
        assert_eq!(
            order(&given),
            [3, 1, 0, 2],
            "no builder pays for a cold folder without a duration; a warm one wins the tie of the same wait"
        );
        let given = [figures(30.0, None, false), figures(0.0, None, false), figures(0.0, None, false)];
        assert_eq!(order(&given), [1, 2, 0], "the same wait and warmth keeps file order");
    }

    #[test]
    fn an_exact_tie_goes_to_the_warm_builder_before_file_order() {
        let given = [
            figures(0.0, Some(100.0), false),
            figures(0.0, Some(200.0), true),
            figures(0.0, Some(50.0), true),
            figures(100.0, Some(100.0), true),
        ];
        assert_eq!(order(&given), [2, 1, 3, 0], "all but the third tie at 200 s; the warm ones go first");
    }
}

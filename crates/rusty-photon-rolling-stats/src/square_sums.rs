//! [`SquareSums`]: a running count and sum of squares that is never reset,
//! for the RMS over an interval the caller picks.

/// A running count of values and the sum of their squares, never reset.
///
/// It answers the RMS of everything it has folded and, the reason it exists,
/// the RMS over an interval of the caller's own choosing: read it at both ends
/// and [`SquareSums::since`] gives what was folded in between. A sliding window
/// cannot answer that once the interval outlasts it, and an interval timed by
/// someone else, such as a camera exposure over a guider's steps, always may.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SquareSums {
    count: u64,
    sum_of_squares: f64,
}

impl SquareSums {
    /// An accumulator that has folded nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 0,
            sum_of_squares: 0.0,
        }
    }

    /// A reading taken elsewhere, such as one carried over the wire. `None`
    /// when no accumulator could have reported it: a sum of squares that is
    /// negative or not finite.
    #[must_use]
    pub fn from_parts(count: u64, sum_of_squares: f64) -> Option<Self> {
        (sum_of_squares.is_finite() && sum_of_squares >= 0.0).then_some(Self {
            count,
            sum_of_squares,
        })
    }

    /// Fold one value. A value whose square is not finite is skipped rather
    /// than counted: it would leave the sum non-finite for good.
    pub fn add(&mut self, value: f64) {
        let square = value * value;
        if square.is_finite() {
            self.count = self.count.saturating_add(1);
            self.sum_of_squares += square;
        }
    }

    /// How many values have been folded.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The sum of the folded values' squares.
    #[must_use]
    pub const fn sum_of_squares(&self) -> f64 {
        self.sum_of_squares
    }

    /// The root mean square of the folded values. `None` when nothing has
    /// been folded, and past `u32::MAX` values, beyond which the count no
    /// longer converts to `f64` without loss.
    #[must_use]
    pub fn rms(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let count = f64::from(u32::try_from(self.count).ok()?);
        Some((self.sum_of_squares / count).sqrt())
    }

    /// What was folded since `earlier`, a reading of the same accumulator
    /// taken before this one. `None` when `earlier` cannot be one, because its
    /// count or its sum is larger than this reading's. The sum only ever grows,
    /// so the difference of two genuine readings is never negative.
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Option<Self> {
        let count = self.count.checked_sub(earlier.count)?;
        let sum_of_squares = self.sum_of_squares - earlier.sum_of_squares;
        (sum_of_squares >= 0.0).then_some(Self {
            count,
            sum_of_squares,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn folded(values: &[f64]) -> SquareSums {
        let mut sums = SquareSums::new();
        for &value in values {
            sums.add(value);
        }
        sums
    }

    #[test]
    fn nothing_folded_has_no_rms() {
        assert_eq!(SquareSums::new().rms(), None);
        assert_eq!(SquareSums::new().count(), 0);
    }

    #[test]
    fn the_rms_is_the_root_of_the_mean_square() {
        let sums = folded(&[0.5, -1.0, 0.5, -1.0]);
        assert_eq!(sums.count(), 4);
        assert_eq!(sums.sum_of_squares(), 2.5);
        assert_eq!(sums.rms(), Some(0.625_f64.sqrt()));
    }

    #[test]
    fn a_value_whose_square_is_not_finite_is_skipped() {
        let sums = folded(&[1e200, 0.5, f64::NAN, f64::INFINITY]);
        assert_eq!(sums.count(), 1);
        assert_eq!(sums.sum_of_squares(), 0.25);
    }

    #[test]
    fn since_gives_what_was_folded_between_two_readings() {
        let mut sums = folded(&[3.0, 4.0]);
        let earlier = sums;
        sums.add(0.75);
        sums.add(-0.75);
        let between = sums.since(&earlier).unwrap();
        assert_eq!(between.count(), 2);
        assert_eq!(between.sum_of_squares(), 1.125);
        assert_eq!(between.rms(), Some(0.75));
    }

    #[test]
    fn since_an_unchanged_reading_folded_nothing() {
        let sums = folded(&[2.0]);
        let between = sums.since(&sums).unwrap();
        assert_eq!(between.count(), 0);
        assert_eq!(between.rms(), None);
    }

    #[test]
    fn a_later_reading_is_not_an_earlier_one() {
        let earlier = folded(&[1.0]);
        let later = folded(&[1.0, 1.0]);
        assert_eq!(earlier.since(&later), None);
    }

    #[test]
    fn a_reading_with_a_smaller_count_but_a_larger_sum_is_not_an_earlier_one() {
        let earlier = SquareSums::from_parts(1, 9.0).unwrap();
        let later = SquareSums::from_parts(2, 2.0).unwrap();
        assert_eq!(later.since(&earlier), None);
    }

    #[test]
    fn from_parts_refuses_a_sum_no_accumulator_could_report() {
        assert_eq!(SquareSums::from_parts(1, -0.5), None);
        assert_eq!(SquareSums::from_parts(1, f64::NAN), None);
        assert_eq!(SquareSums::from_parts(1, f64::INFINITY), None);
        assert_eq!(
            SquareSums::from_parts(2, 0.5).map(|s| (s.count(), s.sum_of_squares())),
            Some((2, 0.5))
        );
    }
}

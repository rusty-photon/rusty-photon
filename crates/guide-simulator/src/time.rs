//! The simulation's instants.

use std::time::Duration;

/// An instant of a simulation: the time since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimTime(Duration);

impl SimTime {
    /// The start of the simulation.
    pub const START: Self = Self(Duration::ZERO);

    /// The instant `since_start` after the start.
    #[must_use]
    pub const fn from_start(since_start: Duration) -> Self {
        Self(since_start)
    }

    /// The time since the start.
    #[must_use]
    pub const fn since_start(self) -> Duration {
        self.0
    }

    /// The instant `by` later.
    #[must_use]
    pub const fn after(self, by: Duration) -> Self {
        Self(self.0.saturating_add(by))
    }

    /// Seconds since the start, for the models' arithmetic.
    pub(crate) const fn secs(self) -> f64 {
        self.0.as_secs_f64()
    }

    /// The instant `secs` seconds after the start; a negative or
    /// non-finite count is the start.
    pub(crate) fn from_secs(secs: f64) -> Self {
        Self(Duration::try_from_secs_f64(secs).unwrap_or(Duration::ZERO))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn after_adds_the_duration() {
        let t = SimTime::from_start(Duration::from_secs(2)).after(Duration::from_millis(500));
        assert_eq!(t.since_start(), Duration::from_millis(2500));
    }

    #[test]
    fn seconds_round_trip() {
        assert_eq!(SimTime::from_secs(1.25).secs(), 1.25);
    }

    #[test]
    fn a_negative_or_non_finite_count_is_the_start() {
        assert_eq!(SimTime::from_secs(-1.0), SimTime::START);
        assert_eq!(SimTime::from_secs(f64::NAN), SimTime::START);
    }
}

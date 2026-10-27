//! What an ASCOM `ObservingConditions` driver maps onto [`SensorMean`].
//!
//! Three drivers keep three means each — temperature, humidity, dew point —
//! and answer the same two questions from them: which window serves the
//! `AveragePeriod` a client sets, and how long ago the freshest of the three
//! took a sample. Both answers were policy each driver spelled out for
//! itself, and a fix to one copy did not reach the others; they live here
//! so a change lands in every driver at once.
//!
//! The crate stays `std`-only: the policy speaks in hours and `Duration`s,
//! and the ASCOM error codes stay with the driver.

use std::ops::RangeInclusive;
use std::time::Duration;

use crate::SensorMean;

/// ASCOM's ceiling on `AveragePeriod`.
///
/// The one place the limit is written down. Each driver enforces it three
/// times and reads it from here, so none of the checks can drift: the device
/// rejects an out-of-range `SetAveragePeriod` with `INVALID_VALUE`, the
/// manager holds the same line for a caller that bypasses the device, and a
/// setup-page save refuses to persist a longer `averaging_period` than a
/// client could select at runtime.
pub const MAX_AVERAGING_PERIOD: Duration = Duration::from_hours(24);

/// The `AveragePeriod` range ASCOM allows, in hours.
///
/// Derived from [`MAX_AVERAGING_PERIOD`] rather than spelled again — the
/// conversion is not available in a `const`. There is no lower bound below
/// zero: zero is ASCOM's "not averaging". `contains` is false for NaN, so a
/// caller checking against this range rejects NaN without a separate
/// finiteness test.
#[must_use]
pub fn average_period_hours() -> RangeInclusive<f64> {
    0.0..=MAX_AVERAGING_PERIOD.as_secs_f64() / 3600.0
}

/// The shortest window the means use: the one that serves
/// `AveragePeriod = 0`, and the floor under every other period.
///
/// ASCOM reads 0 as "the device is not averaging — give me the most recent
/// value". [`SensorMean`] has no unaveraged mode, and because its window is
/// applied on read, a literally zero-length window holds nothing at all.
/// An unbounded window is no better: it would resurrect the staleness the
/// read-side window exists to prevent, and a stalled poll loop would keep
/// answering with an hours-old sample.
///
/// So 0 becomes the shortest window that still always holds the newest
/// sample under healthy polling. It is measured in poll intervals rather
/// than seconds because the poll cadence is configurable: a fixed 10 s
/// window against a 60 s cadence would leave the sensors reading "no value"
/// for 50 seconds out of every 60. Three intervals tolerates two missed
/// polls before readings degrade, which is the honest answer once the device
/// has gone quiet that long. The 10 s floor keeps a very fast poll interval
/// from making the window shorter than one client round trip.
#[must_use]
pub fn instantaneous_window(poll_interval: Duration) -> Duration {
    poll_interval.saturating_mul(3).max(Duration::from_secs(10))
}

/// The window that serves `period_hours`: the period itself, but never
/// shorter than [`instantaneous_window`].
///
/// Zero routes to the instantaneous window, and so does any positive period
/// shorter than it. A window shorter than the poll cadence, applied on read,
/// holds no sample for part of every poll cycle, so the means would answer
/// "no value" between polls while the sensor is healthy; the floor keeps
/// the newest sample inside the window whatever the client asked for. The
/// period a client set is recorded verbatim by the driver and read back
/// as-is, so the floor changes what the means average over, not what
/// `AveragePeriod` reports.
///
/// Config seeding and `SetAveragePeriod` both come through here, so a period
/// written to a config file behaves exactly like the same period set over
/// the wire. `period_hours` is never negative — the device rejects that
/// before it reaches here — so `<= 0.0` reads as "is zero" without a float
/// equality. A value that should have been rejected (NaN, or a period too
/// large for a `Duration`) also lands on the instantaneous window rather
/// than panicking: the panicking conversion would take a driver down
/// mid-session, and failing closed to the shortest window keeps sensor reads
/// honest instead.
#[must_use]
pub fn effective_window(period_hours: f64, poll_interval: Duration) -> Duration {
    let floor = instantaneous_window(poll_interval);
    if period_hours <= 0.0 {
        return floor;
    }
    Duration::try_from_secs_f64(period_hours * 3600.0).map_or(floor, |window| window.max(floor))
}

/// How long ago the freshest of `means` took a sample: what a driver
/// answers for `TimeSinceLastUpdate` with an empty sensor name.
///
/// `None` while every mean is empty, so the client reads "no data" rather
/// than "updated just now" — a sensor that was never read, or whose windows
/// were emptied, is not fresh. A mean that is empty does not hide a fresher
/// one beside it.
pub fn time_since_freshest_update<'a>(
    means: impl IntoIterator<Item = &'a SensorMean>,
) -> Option<Duration> {
    means
        .into_iter()
        .filter_map(SensorMean::time_since_last_update)
        .min()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::thread::sleep;

    /// Slept between two samples so their ages can be told apart under load.
    const SETTLE: Duration = Duration::from_millis(50);

    #[test]
    fn the_average_period_range_is_ascoms_zero_to_twenty_four_hours() {
        // The contract constant, visible: every check reads this range, so a
        // botched derivation would move all of them at once and silently.
        let allowed = average_period_hours();
        assert!((*allowed.start() - 0.0).abs() < f64::EPSILON);
        assert!((*allowed.end() - 24.0).abs() < f64::EPSILON);
        assert_eq!(MAX_AVERAGING_PERIOD, Duration::from_hours(24));
        assert!(!allowed.contains(&f64::NAN));
    }

    #[test]
    fn the_instantaneous_window_is_three_poll_intervals_with_a_ten_second_floor() {
        // A fixed window would leave a slowly-polled device reading "no
        // value" between polls; a very fast cadence must not shrink the
        // window below one client round trip.
        assert_eq!(
            instantaneous_window(Duration::from_secs(60)),
            Duration::from_secs(180)
        );
        assert_eq!(
            instantaneous_window(Duration::from_millis(100)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn effective_window_serves_zero_with_the_instantaneous_window() {
        let poll = Duration::from_secs(5);
        assert_eq!(effective_window(0.0, poll), instantaneous_window(poll));
    }

    #[test]
    fn effective_window_floors_a_period_below_the_instantaneous_window() {
        // 0.001 h is 3.6 s: shorter than a 5 s poll cadence, so a literal
        // window would hold no sample for 1.4 s of every 5. The floor keeps
        // the newest sample inside; a period above the floor is itself.
        let poll = Duration::from_secs(5);
        assert_eq!(effective_window(0.001, poll), Duration::from_secs(15));
        assert_eq!(effective_window(0.01, poll), Duration::from_secs(36));
        assert_eq!(effective_window(2.0, poll), Duration::from_hours(2));
    }

    #[test]
    fn effective_window_does_not_panic_on_a_non_finite_period() {
        // The device and the manager both reject a non-finite period before
        // it reaches here, so this pins the last line: a caller that did
        // neither must still not hit the panicking conversion.
        let poll = Duration::from_secs(5);
        let expected = instantaneous_window(poll);
        assert_eq!(effective_window(f64::NAN, poll), expected);
        assert_eq!(effective_window(f64::INFINITY, poll), expected);
        assert_eq!(effective_window(f64::MAX, poll), expected);
    }

    #[test]
    fn time_since_freshest_update_is_none_while_every_mean_is_empty() {
        let means = [SensorMean::default(), SensorMean::default()];
        assert_eq!(time_since_freshest_update(&means), None);
        assert_eq!(time_since_freshest_update([]), None);
    }

    #[test]
    fn time_since_freshest_update_is_the_newest_sample_and_skips_empty_means() {
        let mut older = SensorMean::default();
        let mut newer = SensorMean::default();
        let empty = SensorMean::default();
        older.add_sample(1.0);
        sleep(SETTLE);
        newer.add_sample(2.0);

        let freshest = time_since_freshest_update([&older, &empty, &newer])
            .expect("two means hold a sample, so this is not the no-data case");
        let newest_alone = newer.time_since_last_update().unwrap();
        let oldest_alone = older.time_since_last_update().unwrap();
        // A floor rather than a bound on scheduling: the answer is at least
        // the newer sample's age (read just after it) and never as old as the
        // older sample, which the settle keeps SETTLE behind.
        assert!(freshest <= newest_alone, "{freshest:?} > {newest_alone:?}");
        assert!(
            freshest + SETTLE <= oldest_alone,
            "{freshest:?} + {SETTLE:?} > {oldest_alone:?}"
        );
    }
}

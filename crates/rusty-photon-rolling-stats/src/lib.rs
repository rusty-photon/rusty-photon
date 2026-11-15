#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `rusty-photon-rolling-stats` — rolling statistics: time-windowed over
//! timestamped samples, and cumulative over an interval the caller picks.
//!
//! [`SensorMean`] keeps a rolling window of `f64` samples and answers the mean
//! over that window. The ASCOM `ObservingConditions` drivers serve
//! `Temperature` / `Humidity` / `DewPoint` from one each, and resize them from
//! `AveragePeriod` — but nothing here knows that. Anything sampled on a cadence
//! can use it.
//!
//! Which is why this is its own crate rather than a module in a driver crate:
//! it is `std`-only and has no reason to make a consumer inherit a device
//! library, a serialiser, or an async runtime to get a mean.
//!
//! The one behaviour worth knowing before reading further is that the window is
//! applied on **read**, not only on insert — see [`SensorMean::get_mean`]. A
//! sampler that stops sampling reads as "no value" rather than as an average of
//! readings that aged out.
//!
//! [`SensorMean`] reads no clock. The caller hands in the instant that dates
//! a sample and the instant every time-dependent question is asked at, as
//! [`std::time::Instant`]s, so the caller picks the clock and a test passes
//! the instants it means.
//!
//! [`observing_conditions`] holds the one piece of ASCOM policy the three
//! `ObservingConditions` drivers share: how `AveragePeriod` maps onto a window,
//! and what `TimeSinceLastUpdate` with an empty sensor name answers over
//! several means.
//!
//! [`SquareSums`] is the cumulative counterpart of a window: a count and sum of
//! squares that is never reset, read at both ends of an interval the caller
//! picks and subtracted to the RMS over that interval, however long it is.

pub mod observing_conditions;
mod square_sums;

pub use square_sums::SquareSums;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A sensor sample and the instant that dates it
#[derive(Debug, Clone)]
struct TimedSample {
    at: Instant,
    value: f64,
}

/// Rolling mean calculator for sensor values
///
/// Maintains a time-windowed collection of samples and calculates
/// the mean value over the configured window period.
#[derive(Debug, Clone)]
pub struct SensorMean {
    samples: VecDeque<TimedSample>,
    window: Duration,
}

impl SensorMean {
    /// Create a new sensor mean calculator with the given time window
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            window,
        }
    }

    /// Add a sample dated `at` to the rolling window
    ///
    /// Removes the samples that are outside the time window as of `at`.
    ///
    /// Samples are expected in the order of their instants. One dated before
    /// the newest held is kept, and dated as the newest: the reading still
    /// counts, and the buffer stays ordered, which eviction relies on when
    /// it stops at the first sample inside the window.
    pub fn add_sample(&mut self, value: f64, at: Instant) {
        let at = self.samples.back().map_or(at, |newest| at.max(newest.at));

        // Add new sample
        self.samples.push_back(TimedSample { at, value });

        // Remove samples outside the window
        self.cleanup_old_samples(at);
    }

    /// Remove samples that are outside the time window as of `now`
    fn cleanup_old_samples(&mut self, now: Instant) {
        while let Some(sample) = self.samples.front() {
            if self.is_in_window(sample, now) {
                break;
            }
            self.samples.pop_front();
        }
    }

    /// Whether `sample` is inside the time window as of `now`: no older than
    /// the window is long.
    ///
    /// Asked of the sample's age rather than of a cutoff instant: the
    /// cutoff, `now - window`, is an `Option` (`Instant::checked_sub`) whose
    /// `None` would need an answer of its own, and the age always has one.
    /// A sample dated after `now` has no age yet, and is inside.
    fn is_in_window(&self, sample: &TimedSample, now: Instant) -> bool {
        now.saturating_duration_since(sample.at) <= self.window
    }

    /// Get the mean of the samples inside the time window as of `now`.
    ///
    /// Returns `None` when the window holds no samples. The ASCOM
    /// `ObservingConditions` drivers report that as `VALUE_NOT_SET`.
    ///
    /// The window is applied on read, not only on insert. Eviction happens in
    /// [`add_sample`](Self::add_sample), so a reader that trusted the deque
    /// alone would keep averaging samples that had aged out for as long as
    /// nothing new arrived — and that is exactly the state a stalled poll loop
    /// produces while its session stays open. Reporting an hour-old
    /// temperature as current is the kind of quiet wrong answer that costs a
    /// night, so a stale window reads as "no value" instead.
    #[must_use]
    pub fn get_mean(&self, now: Instant) -> Option<f64> {
        let mut sum = 0.0_f64;
        // Counted rather than `len()`d because the filter decides membership;
        // saturating because the count is a `u32` for the lossless conversion
        // below, and the window is poll-rate bounded nowhere near 2^32.
        let mut count: u32 = 0;
        for sample in self
            .samples
            .iter()
            .filter(|sample| self.is_in_window(sample, now))
        {
            sum += sample.value;
            count = count.saturating_add(1);
        }

        if count == 0 {
            return None;
        }
        Some(sum / f64::from(count))
    }

    /// Time elapsed at `now` since the instant of the newest sample.
    ///
    /// `None` means the buffer is empty — either nothing has been added yet,
    /// or [`set_window`](Self::set_window) shrank the window far enough to
    /// evict everything. It does **not** mean the samples are stale: a buffer
    /// whose contents have all aged out still reports how long ago the newest
    /// one was taken, which is precisely what tells a caller how stale
    /// [`get_mean`](Self::get_mean)'s `None` is.
    ///
    /// A sample dated after `now` reports zero. A caller that reads its clock
    /// and then a buffer another task has added to in between holds one.
    /// `None` would put it in the same bucket as an empty buffer, which the
    /// ASCOM drivers surface as `f64::MAX` seconds: a fresh reading described
    /// to the client as never updated, which is the opposite of the truth. A
    /// sample dated after `now` is as new as a sample can be.
    #[must_use]
    pub fn time_since_last_update(&self, now: Instant) -> Option<Duration> {
        let newest = self.samples.back()?;
        Some(now.saturating_duration_since(newest.at))
    }

    /// Change the time window and cleanup old samples
    ///
    /// Samples outside the new window as of `now` are removed immediately.
    pub fn set_window(&mut self, window: Duration, now: Instant) {
        self.window = window;

        // Cleanup with the new window
        self.cleanup_old_samples(now);
    }

    /// Get the current time window
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// The number of samples held in the buffer.
    ///
    /// Not the same as the number *within* the window: eviction runs on
    /// insert (and on [`set_window`](Self::set_window)), so between a sample
    /// aging out and the next one arriving this still counts it. That
    /// asymmetry with [`get_mean`](Self::get_mean) — which applies the window
    /// on read — is deliberate: it is what lets a test tell whether eviction
    /// actually ran. Treat this as buffer occupancy, not window membership.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

impl Default for SensorMean {
    fn default() -> Self {
        // Default to 5 minutes
        Self::new(Duration::from_mins(5))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The instant the tests count from. `Instant` has no other constructor;
    /// every other instant in a test is this one plus a `Duration`, so no
    /// test waits and each asserts the exact answer.
    fn start() -> Instant {
        Instant::now()
    }

    const SECOND: Duration = Duration::from_secs(1);
    const NANOSECOND: Duration = Duration::from_nanos(1);

    #[test]
    fn test_new_sensor_mean() {
        let window = Duration::from_mins(1);
        let mean = SensorMean::new(window);

        assert_eq!(mean.window(), window);
        assert_eq!(mean.sample_count(), 0);
        assert_eq!(mean.get_mean(start()), None);
    }

    #[test]
    fn test_add_sample() {
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_mins(1));

        mean.add_sample(10.0, t0);
        assert_eq!(mean.sample_count(), 1);
        assert_eq!(mean.get_mean(t0), Some(10.0));

        mean.add_sample(20.0, t0 + SECOND);
        assert_eq!(mean.sample_count(), 2);
        assert_eq!(mean.get_mean(t0 + SECOND), Some(15.0));

        mean.add_sample(30.0, t0 + 2 * SECOND);
        assert_eq!(mean.sample_count(), 3);
        assert_eq!(mean.get_mean(t0 + 2 * SECOND), Some(20.0));
    }

    #[test]
    fn test_time_since_last_update() {
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_mins(1));

        assert_eq!(mean.time_since_last_update(t0), None);

        mean.add_sample(10.0, t0);
        mean.add_sample(20.0, t0 + 3 * SECOND);

        // Counted from the newest sample's instant to the one asked about:
        // neither the older sample nor the moment of the call comes into it.
        assert_eq!(
            mean.time_since_last_update(t0 + 10 * SECOND),
            Some(7 * SECOND)
        );
        assert_eq!(
            mean.time_since_last_update(t0 + 3 * SECOND),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn time_since_last_update_reports_zero_for_a_sample_dated_after_now() {
        // What a caller holds when it read its clock before the buffer and a
        // sample arrived in between.
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_mins(1));
        mean.add_sample(10.0, t0 + Duration::from_hours(1));

        let elapsed = mean
            .time_since_last_update(t0)
            .expect("a sample is present, so this is not the no-data case");
        assert_eq!(
            elapsed,
            Duration::ZERO,
            "a sample dated after now is as new as a sample gets"
        );
        assert_eq!(
            mean.get_mean(t0),
            Some(10.0),
            "a sample dated after now is inside the window"
        );
    }

    #[test]
    fn test_window_cleanup() {
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_millis(100));

        mean.add_sample(10.0, t0);
        mean.add_sample(20.0, t0);
        assert_eq!(mean.sample_count(), 2);

        // A sample dated after the first two aged out triggers cleanup
        let later = t0 + Duration::from_millis(150);
        mean.add_sample(30.0, later);
        assert_eq!(mean.sample_count(), 1);
        assert_eq!(mean.get_mean(later), Some(30.0));
    }

    #[test]
    fn a_sample_exactly_one_window_old_is_inside_the_window() {
        // The edge both the read filter and eviction share: a sample as old
        // as the window is long is the oldest one inside it.
        let t0 = start();
        let window = Duration::from_secs(10);
        let mut mean = SensorMean::new(window);
        mean.add_sample(10.0, t0);

        assert_eq!(mean.get_mean(t0 + window), Some(10.0));
        assert_eq!(mean.get_mean(t0 + window + NANOSECOND), None);

        mean.add_sample(20.0, t0 + window);
        assert_eq!(mean.sample_count(), 2, "eviction dropped a sample inside");
        mean.add_sample(30.0, t0 + window + NANOSECOND);
        assert_eq!(mean.sample_count(), 2, "eviction kept a sample outside");
    }

    #[test]
    fn get_mean_ignores_samples_that_aged_out_without_a_new_insert() {
        // The state a stalled poll loop leaves behind: samples in the deque,
        // all older than the window, and nothing arriving to evict them.
        // Eviction only runs on insert, so `get_mean` has to apply the window
        // itself or it reports a stale average as current.
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_millis(50));

        mean.add_sample(10.0, t0);
        mean.add_sample(20.0, t0);
        assert_eq!(mean.get_mean(t0), Some(15.0));

        assert_eq!(
            mean.get_mean(t0 + Duration::from_millis(100)),
            None,
            "a window holding only aged-out samples must read as no value, not as a stale mean"
        );
        // The samples are still held — this is a read-side filter, not an
        // eviction — so the next insert is what actually drops them.
        assert_eq!(mean.sample_count(), 2);
    }

    #[test]
    fn a_sample_dated_before_the_newest_is_kept_and_dated_as_the_newest() {
        let t0 = start();
        let window = 5 * SECOND;
        let newest = t0 + 10 * SECOND;
        let mut mean = SensorMean::new(window);
        mean.add_sample(10.0, newest);

        // By its own date this one is ten seconds old in a five second
        // window.
        mean.add_sample(20.0, t0);

        assert_eq!(mean.sample_count(), 2, "the late sample was dropped");
        assert_eq!(
            mean.get_mean(newest),
            Some(15.0),
            "the late sample is not in the mean"
        );
        assert_eq!(
            mean.time_since_last_update(newest),
            Some(Duration::ZERO),
            "the late sample moved the newest instant back"
        );
        // Both leave the window together, at the newest instant's age.
        assert_eq!(mean.get_mean(newest + window), Some(15.0));
        assert_eq!(mean.get_mean(newest + window + NANOSECOND), None);
        mean.add_sample(30.0, newest + window + NANOSECOND);
        assert_eq!(mean.sample_count(), 1, "eviction left an aged-out sample");
    }

    #[test]
    fn test_set_window() {
        let t0 = start();
        let mut mean = SensorMean::new(Duration::from_mins(1));

        mean.add_sample(10.0, t0);
        mean.add_sample(20.0, t0 + Duration::from_millis(50));
        mean.add_sample(30.0, t0 + Duration::from_millis(100));

        assert_eq!(mean.sample_count(), 3);

        // Shrink window to very small duration
        let now = t0 + Duration::from_millis(105);
        mean.set_window(Duration::from_millis(10), now);

        // Old samples should be removed
        assert_eq!(mean.window(), Duration::from_millis(10));
        assert_eq!(mean.sample_count(), 1);
        assert_eq!(mean.get_mean(now), Some(30.0));
    }

    #[test]
    fn time_since_last_update_still_answers_for_a_buffer_that_has_gone_stale() {
        // Staleness and emptiness are different answers. `get_mean` going
        // `None` is what a caller notices; this is how it learns how stale
        // the reading it can no longer have actually is.
        let t0 = start();
        let later = t0 + Duration::from_millis(100);
        let mut mean = SensorMean::new(Duration::from_millis(50));
        mean.add_sample(10.0, t0);

        assert_eq!(mean.get_mean(later), None, "the sample has aged out");
        let elapsed = mean
            .time_since_last_update(later)
            .expect("an aged-out sample is still a sample");
        assert_eq!(elapsed, Duration::from_millis(100));
    }

    #[test]
    fn time_since_last_update_is_none_once_a_window_shrink_empties_the_buffer() {
        // The second way the buffer empties, and the reason `None` cannot be
        // documented as "nothing was ever added": `set_window` evicts, and a
        // small enough window evicts everything.
        let t0 = start();
        let later = t0 + Duration::from_millis(50);
        let mut mean = SensorMean::new(Duration::from_mins(1));
        mean.add_sample(10.0, t0);

        mean.set_window(Duration::from_millis(10), later);

        assert_eq!(mean.sample_count(), 0, "the shrink evicted the only sample");
        assert_eq!(mean.time_since_last_update(later), None);
    }

    #[test]
    fn test_default() {
        let mean = SensorMean::default();
        assert_eq!(mean.window(), Duration::from_mins(5));
    }
}

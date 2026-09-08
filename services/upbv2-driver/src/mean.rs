//! Sensor mean calculation
//!
//! This module implements time-windowed mean calculation for sensor values.
//! It maintains a rolling window of samples and calculates the mean over that window.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

/// A single timestamped sensor sample
#[derive(Debug, Clone)]
struct TimedSample {
    timestamp: SystemTime,
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

    /// Add a new sample to the rolling window
    ///
    /// Automatically removes samples that fall outside the time window.
    pub fn add_sample(&mut self, value: f64) {
        let now = SystemTime::now();

        // Add new sample
        self.samples.push_back(TimedSample {
            timestamp: now,
            value,
        });

        // Remove samples outside the window
        self.cleanup_old_samples(now);
    }

    /// Remove samples that are older than the time window
    fn cleanup_old_samples(&mut self, now: SystemTime) {
        let cutoff = now
            .checked_sub(self.window)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        while let Some(sample) = self.samples.front() {
            if sample.timestamp < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get the mean of the samples currently inside the time window.
    ///
    /// Returns `None` when the window holds no samples — which the
    /// `ObservingConditions` device reports as `VALUE_NOT_SET`.
    ///
    /// The window is applied on read, not only on insert. Eviction happens in
    /// [`add_sample`](Self::add_sample), so a reader that trusted the deque
    /// alone would keep averaging samples that had aged out for as long as
    /// nothing new arrived — and that is exactly the state a stalled poll loop
    /// produces while the session stays open. Reporting an hour-old
    /// temperature as current is the kind of quiet wrong answer that costs a
    /// night, so a stale window reads as "no value" instead.
    #[must_use]
    pub fn get_mean(&self) -> Option<f64> {
        let cutoff = SystemTime::now()
            .checked_sub(self.window)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut sum = 0.0_f64;
        // Counted rather than `len()`d because the filter decides membership;
        // saturating because the count is a `u32` for the lossless conversion
        // below, and the window is poll-rate bounded nowhere near 2^32.
        let mut count: u32 = 0;
        for sample in self.samples.iter().filter(|s| s.timestamp >= cutoff) {
            sum += sample.value;
            count = count.saturating_add(1);
        }

        if count == 0 {
            return None;
        }
        Some(sum / f64::from(count))
    }

    /// Time elapsed since the last sample was added.
    ///
    /// `None` means exactly one thing: no samples have been added yet. A
    /// backwards clock jump — an NTP correction, a VM resuming from a
    /// snapshot — leaves the newest sample stamped in the future, and
    /// `duration_since` fails on that. Reporting it as `None` would put it in
    /// the same bucket as "no data", which the device surfaces as `f64::MAX`
    /// seconds: a fresh reading described to the client as never updated,
    /// which is the opposite of the truth. A sample stamped in the future is
    /// as new as a sample can be, so it reports zero.
    #[must_use]
    pub fn time_since_last_update(&self) -> Option<Duration> {
        let sample = self.samples.back()?;
        Some(
            SystemTime::now()
                .duration_since(sample.timestamp)
                .unwrap_or(Duration::ZERO),
        )
    }

    /// Change the time window and cleanup old samples
    ///
    /// Samples outside the new window will be removed immediately.
    pub fn set_window(&mut self, window: Duration) {
        self.window = window;

        // Cleanup with the new window
        let now = SystemTime::now();
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
    use std::thread::sleep;

    /// Slept between an event and reading a duration back, so the assertion
    /// can be a load-safe floor rather than a bound on scheduling latency.
    const SETTLE: Duration = Duration::from_millis(50);

    #[test]
    fn test_new_sensor_mean() {
        let window = Duration::from_mins(1);
        let mean = SensorMean::new(window);

        assert_eq!(mean.window(), window);
        assert_eq!(mean.sample_count(), 0);
        assert_eq!(mean.get_mean(), None);
    }

    #[test]
    fn test_add_sample() {
        let mut mean = SensorMean::new(Duration::from_mins(1));

        mean.add_sample(10.0);
        assert_eq!(mean.sample_count(), 1);
        assert_eq!(mean.get_mean(), Some(10.0));

        mean.add_sample(20.0);
        assert_eq!(mean.sample_count(), 2);
        assert_eq!(mean.get_mean(), Some(15.0));

        mean.add_sample(30.0);
        assert_eq!(mean.sample_count(), 3);
        assert_eq!(mean.get_mean(), Some(20.0));
    }

    #[test]
    fn test_time_since_last_update() {
        let mut mean = SensorMean::new(Duration::from_mins(1));

        assert_eq!(mean.time_since_last_update(), None);

        mean.add_sample(10.0);
        sleep(SETTLE);
        let elapsed = mean.time_since_last_update().unwrap();

        // A floor, because load can only push the elapsed time up: it proves
        // the answer is measured from the sample's timestamp rather than
        // returned as a constant zero, and no amount of contention makes it
        // fail. The ceiling is deliberately loose. It exists to catch a
        // timestamp that was never set — an epoch-based answer reads as
        // decades — not to assert anything about scheduling latency, which is
        // all a tight bound here would measure.
        assert!(
            elapsed >= SETTLE,
            "elapsed should be at least the {SETTLE:?} slept since the sample, got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "elapsed should be recent, got {elapsed:?} — is the timestamp set from the sample?"
        );
    }

    #[test]
    fn time_since_last_update_reports_zero_for_a_sample_stamped_in_the_future() {
        // The state a backwards clock jump leaves behind. Staged directly
        // rather than by moving the system clock, which a test cannot do.
        let mut mean = SensorMean::new(Duration::from_mins(1));
        mean.samples.push_back(TimedSample {
            timestamp: SystemTime::now() + Duration::from_secs(3600),
            value: 10.0,
        });

        let elapsed = mean
            .time_since_last_update()
            .expect("a sample is present, so this is not the no-data case");
        assert_eq!(
            elapsed,
            Duration::ZERO,
            "a future-stamped sample is as new as a sample gets"
        );
    }

    #[test]
    fn test_window_cleanup() {
        let mut mean = SensorMean::new(Duration::from_millis(100));

        mean.add_sample(10.0);
        mean.add_sample(20.0);
        assert_eq!(mean.sample_count(), 2);

        // Wait for samples to age out
        sleep(Duration::from_millis(150));

        // Add new sample, which should trigger cleanup
        mean.add_sample(30.0);
        assert_eq!(mean.sample_count(), 1);
        assert_eq!(mean.get_mean(), Some(30.0));
    }

    #[test]
    fn get_mean_ignores_samples_that_aged_out_without_a_new_insert() {
        // The state a stalled poll loop leaves behind: samples in the deque,
        // all older than the window, and nothing arriving to evict them.
        // Eviction only runs on insert, so `get_mean` has to apply the window
        // itself or it reports a stale average as current.
        let mut mean = SensorMean::new(Duration::from_millis(50));

        mean.add_sample(10.0);
        mean.add_sample(20.0);
        assert_eq!(mean.get_mean(), Some(15.0));

        sleep(Duration::from_millis(100));

        assert_eq!(
            mean.get_mean(),
            None,
            "a window holding only aged-out samples must read as no value, not as a stale mean"
        );
        // The samples are still held — this is a read-side filter, not an
        // eviction — so the next insert is what actually drops them.
        assert_eq!(mean.sample_count(), 2);
    }

    #[test]
    fn test_set_window() {
        let mut mean = SensorMean::new(Duration::from_mins(1));

        mean.add_sample(10.0);
        sleep(Duration::from_millis(50));
        mean.add_sample(20.0);
        sleep(Duration::from_millis(50));
        mean.add_sample(30.0);

        assert_eq!(mean.sample_count(), 3);

        // Shrink window to very small duration
        mean.set_window(Duration::from_millis(10));

        // Old samples should be removed
        assert!(mean.sample_count() < 3);
    }

    #[test]
    fn test_default() {
        let mean = SensorMean::default();
        assert_eq!(mean.window(), Duration::from_mins(5));
    }
}

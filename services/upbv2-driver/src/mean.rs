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

    /// Get the mean of all samples in the current window
    ///
    /// Returns None if there are no samples available.
    #[must_use]
    pub fn get_mean(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }

        let sum: f64 = self.samples.iter().map(|s| s.value).sum();
        // The u32 detour keeps the usize→f64 conversion lossless; the
        // fallback arm is unreachable (the window is poll-rate bounded,
        // nowhere near 2^32 samples).
        let count = f64::from(u32::try_from(self.samples.len()).unwrap_or(u32::MAX));
        Some(sum / count)
    }

    /// Get the time elapsed since the last sample was added
    ///
    /// Returns None if no samples have been added yet.
    #[must_use]
    pub fn time_since_last_update(&self) -> Option<Duration> {
        self.samples
            .back()
            .and_then(|sample| SystemTime::now().duration_since(sample.timestamp).ok())
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

    /// Get the number of samples currently in the window
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
        let elapsed = mean.time_since_last_update().unwrap();
        assert!(elapsed < Duration::from_millis(10));
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

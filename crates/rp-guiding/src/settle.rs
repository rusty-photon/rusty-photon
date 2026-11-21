//! Settling: whether the stars have stayed near the lock long enough
//! after guiding began or a dither moved the lock. See
//! `docs/services/guider.md` (Dither, settle and abort).

use std::time::Duration;

use tracing::debug;

/// A settle request's terms (`settle` of `guiding/start` and `dither`, or
/// the configuration's `settling`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettleParams {
    /// The distance from the lock below which a frame counts, in pixels.
    pub tolerance_px: f64,
    /// How long the frames must stay below it without a break.
    pub time: Duration,
    /// How long after the start the stars may take to settle.
    pub timeout: Duration,
}

/// Where a settle stands after a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settling {
    /// Not settled yet.
    Settling,
    /// Settled.
    Settled,
    /// Not settled within the timeout.
    TimedOut,
}

/// One wait for the stars to settle.
#[derive(Debug, Clone)]
pub struct Settle {
    params: SettleParams,
    started: Duration,
    /// The time of the first frame of the run below the tolerance that is
    /// still unbroken.
    below_since: Option<Duration>,
}

impl Settle {
    /// A wait that starts at `now`, when guiding began or the dither moved
    /// the lock: a time since an epoch of the caller's, on a clock that
    /// never runs backward.
    #[must_use]
    pub const fn new(params: SettleParams, now: Duration) -> Self {
        Self {
            params,
            started: now,
            below_since: None,
        }
    }

    /// Takes a frame at `now` with its distance from the lock, `None` for
    /// a star-lost frame.
    ///
    /// The frames have settled when they have stayed below the tolerance
    /// for the settle time: from the first frame of an unbroken run below
    /// it to this one. A frame at or above it, or a star-lost frame,
    /// breaks the run. A frame that settles settles even at the timeout.
    pub fn frame(&mut self, distance_px: Option<f64>, now: Duration) -> Settling {
        let below = distance_px.is_some_and(|distance| distance < self.params.tolerance_px);
        self.below_since = if below {
            Some(self.below_since.unwrap_or(now))
        } else {
            None
        };
        let settled = self
            .below_since
            .is_some_and(|since| now.saturating_sub(since) >= self.params.time);
        let settling = if settled {
            Settling::Settled
        } else if now.saturating_sub(self.started) > self.params.timeout {
            Settling::TimedOut
        } else {
            Settling::Settling
        };
        debug!(?distance_px, ?settling, "settle frame");
        settling
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const PARAMS: SettleParams = SettleParams {
        tolerance_px: 1.0,
        time: Duration::from_secs(10),
        timeout: Duration::from_secs(60),
    };

    fn at(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    /// Feeds `distances` to a settle started at 0, one frame each 2.5 s
    /// from 2.5 s, and returns where it stood after each.
    fn feed(params: SettleParams, distances: &[Option<f64>]) -> Vec<Settling> {
        let mut settle = Settle::new(params, Duration::ZERO);
        distances
            .iter()
            .zip(1_u32..)
            .map(|(distance, n)| settle.frame(*distance, at(2.5 * f64::from(n))))
            .collect()
    }

    #[test]
    fn frames_that_stay_below_the_tolerance_for_the_time_settle() {
        let states = feed(PARAMS, &[Some(0.5); 5]);
        assert_eq!(
            states,
            [
                Settling::Settling,
                Settling::Settling,
                Settling::Settling,
                Settling::Settling,
                Settling::Settled
            ]
        );
    }

    #[test]
    fn a_frame_at_the_tolerance_or_a_star_lost_frame_starts_the_time_again() {
        for breaker in [Some(1.0), Some(3.0), None] {
            let states = feed(
                PARAMS,
                &[
                    Some(0.5),
                    Some(0.5),
                    Some(0.5),
                    breaker,
                    Some(0.5),
                    Some(0.5),
                    Some(0.5),
                    Some(0.5),
                    Some(0.5),
                ],
            );
            assert_eq!(states[7], Settling::Settling, "{breaker:?}");
            assert_eq!(states[8], Settling::Settled, "{breaker:?}");
        }
    }

    #[test]
    fn frames_that_never_settle_time_out_after_the_timeout() {
        let mut settle = Settle::new(PARAMS, at(100.0));
        assert_eq!(settle.frame(Some(2.0), at(160.0)), Settling::Settling);
        assert_eq!(
            settle.frame(Some(2.0), at(160.0) + Duration::from_nanos(1)),
            Settling::TimedOut
        );
    }

    #[test]
    fn a_frame_that_settles_at_the_timeout_settles() {
        let mut settle = Settle::new(PARAMS, Duration::ZERO);
        assert_eq!(settle.frame(Some(0.5), at(55.0)), Settling::Settling);
        assert_eq!(settle.frame(Some(0.5), at(65.0)), Settling::Settled);
    }

    #[test]
    fn with_no_settle_time_the_first_frame_below_the_tolerance_settles() {
        let params = SettleParams {
            time: Duration::ZERO,
            ..PARAMS
        };
        assert_eq!(
            feed(params, &[Some(2.0), Some(0.9)]),
            [Settling::Settling, Settling::Settled]
        );
    }
}

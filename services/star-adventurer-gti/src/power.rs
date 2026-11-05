//! The mount controller's power, as far as the driver can tell: whether
//! it restarted while the driver served it, and whether the mount's
//! position survived the restart.
//!
//! The handshake reads each axis' init flag before its own `:F`, so a
//! clear flag says the controller restarted since it was last
//! initialised. On a restart while serving, the handshake writes the last
//! known position back ([`position_to_restore`]) and marks the position
//! lost when the mount was moving too fast for that position to be
//! trusted. See `docs/services/star-adventurer-gti.md` §Power cut while
//! serving.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::coordinates::SIDEREAL_DEG_PER_SEC;
use crate::manager::{AxisSnapshot, MountParameters, MountSnapshot};

/// The fastest tracking-mode rate whose sample is still trusted across a
/// cut, as a multiple of the axis' sidereal rate. Tracking and a guide
/// pulse on top of it stay under it; anything faster is a manual move
/// whose position at the cut is not known closely enough.
const MAX_TRUSTED_RATE_SIDEREAL: f64 = 2.0;

/// The controller's power state, shared by the handshake, the poll loop
/// and the device.
#[derive(Debug)]
pub struct PowerWatch {
    position_lost: AtomicBool,
    /// When a background poll first failed after its last success: the
    /// latest instant the link can have gone. `None` while polls succeed.
    first_failed_poll: Mutex<Option<Instant>>,
    /// Bumped once per restart the handshake finds while serving; the
    /// connected device watches it to drop what the cut ended.
    restarts: watch::Sender<u64>,
}

impl Default for PowerWatch {
    fn default() -> Self {
        Self {
            position_lost: AtomicBool::new(false),
            first_failed_poll: Mutex::new(None),
            restarts: watch::Sender::new(0),
        }
    }
}

impl PowerWatch {
    /// Whether the mount's position is lost: slews, tracking and guide
    /// pulses are refused and `Park()` parks in place until ground truth
    /// clears it.
    pub fn position_lost(&self) -> bool {
        self.position_lost.load(Ordering::SeqCst)
    }

    /// Mark the position lost.
    pub fn mark_position_lost(&self) {
        self.position_lost.store(true, Ordering::SeqCst);
    }

    /// Clear a lost position: ground truth (a sync, or the recovery from a
    /// named park) has anchored the frame again. Returns whether it was
    /// lost.
    pub fn clear_position_lost(&self) -> bool {
        self.position_lost.swap(false, Ordering::SeqCst)
    }

    /// A background poll failed at `at`. Keeps the first failure after the
    /// last success.
    pub fn note_poll_failed(&self, at: Instant) {
        let mut first = self
            .first_failed_poll
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if first.is_none() {
            *first = Some(at);
        }
    }

    /// A background poll succeeded: the link is up.
    pub fn note_poll_succeeded(&self) {
        *self
            .first_failed_poll
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The first background poll that failed since the last success.
    pub fn first_failed_poll(&self) -> Option<Instant> {
        *self
            .first_failed_poll
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a restart found while serving.
    pub fn record_restart(&self) {
        self.restarts.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Watch the restarts found while serving.
    pub fn subscribe_restarts(&self) -> watch::Receiver<u64> {
        self.restarts.subscribe()
    }
}

/// The position a restarted controller is given back, per axis, and
/// whether it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restore {
    pub ra_ticks: i32,
    pub dec_ticks: i32,
    /// The mount was moving too fast, or for too long unobserved, for
    /// the written position to be more than an estimate.
    pub lost: bool,
}

/// What a restart while serving writes back: each axis' last sample,
/// carried forward to `first_failed_poll` when the axis was tracking.
///
/// `None` when `previous` holds no sample, so there is nothing to carry
/// over (the controller restarted before the driver first read it).
///
/// An axis is trusted when it was stopped (exact), or tracking at a known
/// rate no faster than [`MAX_TRUSTED_RATE_SIDEREAL`] with a failed poll
/// no more than `max_age` after its sample to bound the motion. Anything
/// else — a goto, the fast regime, a blocked axis, an unknown rate, a
/// tracking sample nothing bounds — is written back as it was sampled,
/// the best estimate there is, and marks the position lost.
#[must_use]
pub fn position_to_restore(
    previous: &MountSnapshot,
    params: &MountParameters,
    first_failed_poll: Option<Instant>,
    max_age: Duration,
) -> Option<Restore> {
    if previous.ra.sampled_at.is_none() && previous.dec.sampled_at.is_none() {
        return None;
    }
    let ra = carry_axis(
        previous.ra,
        params.tmr_freq,
        params.high_speed_ratio_ra,
        params.cpr_ra,
        first_failed_poll,
        max_age,
    );
    let dec = carry_axis(
        previous.dec,
        params.tmr_freq,
        params.high_speed_ratio_dec,
        params.cpr_dec,
        first_failed_poll,
        max_age,
    );
    Some(Restore {
        ra_ticks: ra.0,
        dec_ticks: dec.0,
        lost: !(ra.1 && dec.1),
    })
}

/// One axis of [`position_to_restore`]: the ticks to write and whether
/// they can be trusted.
fn carry_axis(
    axis: AxisSnapshot,
    tmr_freq: u32,
    high_speed_ratio: u32,
    cpr: u32,
    first_failed_poll: Option<Instant>,
    max_age: Duration,
) -> (i32, bool) {
    let sampled = axis.position_ticks.clamp(POSITION_MIN, POSITION_MAX);
    let Some(sampled_at) = axis.sampled_at else {
        return (sampled, false);
    };
    if !axis.running() {
        return (sampled, true);
    }
    let rate = axis.rate_ticks_per_sec(tmr_freq, high_speed_ratio);
    let sidereal = f64::from(cpr) * SIDEREAL_DEG_PER_SEC / 360.0;
    let trusted_rate =
        axis.status.is_some() && rate != 0.0 && rate.abs() <= MAX_TRUSTED_RATE_SIDEREAL * sidereal;
    let bounded = first_failed_poll
        .is_some_and(|failed| failed.saturating_duration_since(sampled_at) <= max_age);
    match first_failed_poll {
        Some(failed) if trusted_rate && bounded => {
            let carried = axis.projected_to(failed, tmr_freq, high_speed_ratio, max_age);
            (
                carried.position_ticks.clamp(POSITION_MIN, POSITION_MAX),
                true,
            )
        }
        _ => (sampled, false),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use skywatcher_motor_protocol::{
        AxisStatus, Direction, InitFlags, ModeKind, MotionFlags, Speed,
    };

    use super::*;

    const CPR_RA: u32 = 3_628_800;
    const CPR_DEC: u32 = 2_903_040;
    const TMR_FREQ: u32 = 16_000_000;
    const MAX_AGE: Duration = Duration::from_millis(800);

    fn params() -> MountParameters {
        MountParameters {
            cpr_ra: CPR_RA,
            cpr_dec: CPR_DEC,
            tmr_freq: TMR_FREQ,
            high_speed_ratio_ra: 32,
            high_speed_ratio_dec: 32,
            motor_board_version: 0x000C_3003,
            ra_at_handshake_ticks: 0,
            dec_at_handshake_ticks: 0,
        }
    }

    fn status(running: bool, mode: ModeKind, speed: Speed, blocked: bool) -> AxisStatus {
        AxisStatus {
            mode,
            direction: Direction::Cw,
            speed,
            motion: MotionFlags { running, blocked },
            init: InitFlags {
                initialized: true,
                level_switch_on: false,
            },
        }
    }

    fn stopped(ticks: i32, at: Instant) -> AxisSnapshot {
        AxisSnapshot {
            position_ticks: ticks,
            sampled_at: Some(at),
            status: Some(status(false, ModeKind::Tracking, Speed::Slow, false)),
            ..AxisSnapshot::default()
        }
    }

    /// RA tracking at sidereal: the step period that turns it at the
    /// sidereal rate.
    fn tracking_sidereal(ticks: i32, at: Instant) -> AxisSnapshot {
        AxisSnapshot {
            position_ticks: ticks,
            sampled_at: Some(at),
            status: Some(status(true, ModeKind::Tracking, Speed::Slow, false)),
            step_period: params().sidereal_step_period_ra(),
            ..AxisSnapshot::default()
        }
    }

    #[test]
    fn nothing_sampled_has_nothing_to_restore() {
        let restore = position_to_restore(&MountSnapshot::default(), &params(), None, MAX_AGE);
        assert_eq!(restore, None);
    }

    #[test]
    fn a_stopped_mount_is_restored_exactly_and_trusted() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: stopped(-907_200, t),
            dec: stopped(725_760, t),
        };
        let restore = position_to_restore(&previous, &params(), None, MAX_AGE).unwrap();
        assert_eq!(
            restore,
            Restore {
                ra_ticks: -907_200,
                dec_ticks: 725_760,
                lost: false
            }
        );
    }

    #[test]
    fn a_tracking_axis_is_carried_to_the_first_failed_poll() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: tracking_sidereal(100_000, t),
            dec: stopped(725_760, t),
        };
        let failed = t + Duration::from_millis(200);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        // Sidereal on the GTi's RA is about 42 ticks/s: about 8 ticks in
        // 200 ms.
        let moved = restore.ra_ticks - 100_000;
        assert!((7..=10).contains(&moved), "carried {moved} ticks");
        assert_eq!(restore.dec_ticks, 725_760);
        assert!(!restore.lost);
    }

    #[test]
    fn a_tracking_axis_with_no_failed_poll_is_lost() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: tracking_sidereal(100_000, t),
            dec: stopped(725_760, t),
        };
        let restore = position_to_restore(&previous, &params(), None, MAX_AGE).unwrap();
        assert_eq!(restore.ra_ticks, 100_000);
        assert!(restore.lost);
    }

    #[test]
    fn a_tracking_sample_older_than_the_cap_is_lost() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: tracking_sidereal(100_000, t),
            dec: stopped(725_760, t),
        };
        let failed = t + MAX_AGE + Duration::from_millis(1);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        assert_eq!(restore.ra_ticks, 100_000);
        assert!(restore.lost);
    }

    #[test]
    fn a_goto_is_written_back_as_sampled_and_lost() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: AxisSnapshot {
                position_ticks: 400_000,
                sampled_at: Some(t),
                status: Some(status(true, ModeKind::Goto, Speed::Fast, false)),
                ..AxisSnapshot::default()
            },
            dec: stopped(725_760, t),
        };
        let failed = t + Duration::from_millis(100);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        assert_eq!(restore.ra_ticks, 400_000);
        assert!(restore.lost);
    }

    #[test]
    fn a_blocked_axis_is_lost() {
        let t = Instant::now();
        let mut ra = tracking_sidereal(100_000, t);
        ra.status = Some(status(true, ModeKind::Tracking, Speed::Slow, true));
        let previous = MountSnapshot {
            ra,
            dec: stopped(725_760, t),
        };
        let failed = t + Duration::from_millis(100);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        assert!(restore.lost);
    }

    #[test]
    fn a_tracking_axis_of_unknown_rate_is_lost() {
        let t = Instant::now();
        let mut ra = tracking_sidereal(100_000, t);
        ra.step_period = 0;
        let previous = MountSnapshot {
            ra,
            dec: stopped(725_760, t),
        };
        let failed = t + Duration::from_millis(100);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        assert!(restore.lost);
    }

    #[test]
    fn a_slow_mode_move_faster_than_twice_sidereal_is_lost() {
        let t = Instant::now();
        let mut ra = tracking_sidereal(100_000, t);
        // A third of the sidereal period: three times sidereal.
        ra.step_period = params().sidereal_step_period_ra() / 3;
        let previous = MountSnapshot {
            ra,
            dec: stopped(725_760, t),
        };
        let failed = t + Duration::from_millis(100);
        let restore = position_to_restore(&previous, &params(), Some(failed), MAX_AGE).unwrap();
        assert!(restore.lost);
    }

    #[test]
    fn the_first_failed_poll_survives_later_failures_until_a_success() {
        let watch = PowerWatch::default();
        let t = Instant::now();
        watch.note_poll_failed(t);
        watch.note_poll_failed(t + Duration::from_millis(50));
        assert_eq!(watch.first_failed_poll(), Some(t));
        watch.note_poll_succeeded();
        assert_eq!(watch.first_failed_poll(), None);
    }

    #[test]
    fn clearing_a_lost_position_says_whether_it_was_lost() {
        let watch = PowerWatch::default();
        assert!(!watch.clear_position_lost());
        watch.mark_position_lost();
        assert!(watch.position_lost());
        assert!(watch.clear_position_lost());
        assert!(!watch.position_lost());
    }
}

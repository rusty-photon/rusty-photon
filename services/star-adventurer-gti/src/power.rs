//! The mount controller's power, as far as the driver can tell: whether
//! it restarted while the driver served it, and whether the mount's
//! position survived the restart.
//!
//! The handshake reads each axis' init flag before its own `:F`, so a
//! clear flag says the controller restarted since it was last
//! initialised. On a restart while serving, the handshake writes the last
//! known position back ([`position_to_restore`]) and marks the position
//! lost when the mount was moving too fast for that position to be
//! trusted. A position the restore does not trust goes to the mount state
//! record too, so a reload or a restart of the service keeps it. See
//! `docs/services/star-adventurer-gti.md` §Power cut while serving.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::warn;

use crate::coordinates::SIDEREAL_DEG_PER_SEC;
use crate::manager::{AxisSnapshot, MountParameters, MountSnapshot};
use crate::mount_state::{self, HeldRecord, MountStateRecord, MountStateStore, ReadRecord};

/// The fastest tracking-mode rate whose sample is still trusted across a
/// cut, as a multiple of the axis' sidereal rate. Tracking and a guide
/// pulse on top of it stay under it; anything faster is a manual move
/// whose position at the cut is not known closely enough.
const MAX_TRUSTED_RATE_SIDEREAL: f64 = 2.0;

/// The longest outage after which the last sample is written back.
///
/// A blip, plus the transport's 5 s retry, fits well inside it; a longer
/// cut may be a planned one, during which someone can have moved the
/// mount by hand, so the mount state record decides instead.
pub const MAX_TRUSTED_OUTAGE: Duration = Duration::from_secs(30);

/// What the driver knows about the mount's position across power cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// The encoders say where the mount is.
    Known,
    /// The controller restarted after an outage too long to trust the
    /// last sample, and its encoders hold power-up values. The power-up
    /// check against the mount state record decides: at the next
    /// connect, or at once when a client is connected.
    AwaitingCheck,
    /// Lost: only a sync or the recovery from a named park clears it.
    Lost,
}

/// The controller's power state, shared by the handshake, the request
/// paths and the device.
#[derive(Debug)]
pub struct PowerWatch {
    position: Mutex<Position>,
    /// A restart the handshake saw but has not finished handling: its
    /// `:F` set the init flags, so a retry after a failure later in the
    /// handshake would otherwise read them set and miss it.
    restart_pending: AtomicBool,
    /// When a request first failed after the last success: the latest
    /// instant the link can have gone. `None` while requests succeed.
    first_failed_request: Mutex<Option<Instant>>,
    /// Bumped once per restart the handshake finds while serving; the
    /// connected device watches it to drop what the cut ended.
    restarts: watch::Sender<u64>,
    /// The mount state record, which keeps a position the driver does not
    /// trust across a reload and a restart of the service. `None` with no
    /// config file to keep it beside.
    record: Option<MountStateStore>,
}

impl Default for PowerWatch {
    fn default() -> Self {
        Self::with_record(None)
    }
}

/// Lock `m`, recovering the value from a poisoned lock: every value these
/// locks guard is a plain copy, whole after any panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PowerWatch {
    /// A watch that keeps the mount state record in `record`.
    pub(crate) fn with_record(record: Option<MountStateStore>) -> Self {
        Self {
            position: Mutex::new(Position::Known),
            restart_pending: AtomicBool::new(false),
            first_failed_request: Mutex::new(None),
            restarts: watch::Sender::new(0),
            record,
        }
    }

    /// The mount state record, when there is a config file to keep it
    /// beside.
    pub(crate) const fn record(&self) -> Option<&MountStateStore> {
        self.record.as_ref()
    }

    /// What the driver knows about the mount's position.
    pub fn position(&self) -> Position {
        *lock(&self.position)
    }

    /// Whether the encoders say where the mount is. While they do not,
    /// slews, tracking and guide pulses are refused and `Park()` parks in
    /// place.
    pub fn position_known(&self) -> bool {
        self.position() == Position::Known
    }

    /// Mark the position lost.
    pub fn mark_position_lost(&self) {
        *lock(&self.position) = Position::Lost;
    }

    /// Hold the position for the power-up check, unless it is already
    /// lost.
    pub fn await_power_up_check(&self) {
        let mut position = lock(&self.position);
        if *position == Position::Known {
            *position = Position::AwaitingCheck;
        }
    }

    /// The power-up check found the position (`true`, the seed ran) or
    /// lost it. A no-op unless a check was awaited.
    pub fn settle_power_up_check(&self, found: bool) {
        let mut position = lock(&self.position);
        if *position == Position::AwaitingCheck {
            *position = if found {
                Position::Known
            } else {
                Position::Lost
            };
        }
    }

    /// Ground truth (a sync, or the recovery from a named park) has
    /// anchored the frame: the position is known. Returns whether it was
    /// not.
    pub fn clear_position_lost(&self) -> bool {
        std::mem::replace(&mut *lock(&self.position), Position::Known) != Position::Known
    }

    /// Whether a restart the handshake saw is still being handled.
    pub fn restart_pending(&self) -> bool {
        self.restart_pending.load(Ordering::SeqCst)
    }

    /// Set or clear the restart being handled.
    pub fn set_restart_pending(&self, pending: bool) {
        self.restart_pending.store(pending, Ordering::SeqCst);
    }

    /// A request failed at `at`. Keeps the first failure after the last
    /// success.
    pub fn note_request_failed(&self, at: Instant) {
        lock(&self.first_failed_request).get_or_insert(at);
    }

    /// A request succeeded: the link is up.
    pub fn note_request_succeeded(&self) {
        *lock(&self.first_failed_request) = None;
    }

    /// The first request that failed since the last success.
    pub fn first_failed_request(&self) -> Option<Instant> {
        *lock(&self.first_failed_request)
    }

    /// Record a restart found while serving.
    pub fn record_restart(&self) {
        self.restarts.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Watch the restarts found while serving.
    pub fn subscribe_restarts(&self) -> watch::Receiver<u64> {
        self.restarts.subscribe()
    }

    /// The restore found the mount moving: lose the position, and say so
    /// in the record.
    pub(crate) async fn lose_position_after_restart(&self) {
        let Some(store) = &self.record else {
            self.mark_position_lost();
            return;
        };
        let record = store.lock().await;
        self.mark_position_lost();
        write_lost(&record).await;
    }

    /// After a long cut, hold the position for the power-up check, or lose
    /// it now when the record already decides the check. A record that
    /// says at rest becomes `awaiting_check` at its ticks, so the check
    /// outlives a reload and a restart of the service. Any other record,
    /// or a position already lost, loses the position, and the record says
    /// lost: the error says why.
    ///
    /// Without a record the check is held in memory.
    ///
    /// # Errors
    ///
    /// Returns why the position is lost.
    pub(crate) async fn await_check_after_long_cut(&self) -> Result<(), String> {
        let Some(store) = &self.record else {
            self.await_power_up_check();
            return Ok(());
        };
        let record = store.lock().await;
        let read = record.read().await;
        let pending = if self.position() == Position::Lost {
            Err("the mount's position was already lost".to_string())
        } else {
            mount_state::after_long_cut(&read)
        };
        match pending {
            Ok(pending) => {
                self.await_power_up_check();
                if read != ReadRecord::Record(pending) {
                    if let Err(e) = record.write(pending).await {
                        warn!(
                            "the mount state record does not hold the pending power-up \
                             check, so a reload or a restart of the service before the next \
                             connect forgets it: {e}"
                        );
                    }
                }
                Ok(())
            }
            Err(reason) => {
                self.mark_position_lost();
                write_lost(&record).await;
                Err(reason)
            }
        }
    }
}

/// Write `lost` to the held record, warning when the write does not land.
async fn write_lost(record: &HeldRecord<'_>) {
    if let Err(e) = record.write(MountStateRecord::Lost).await {
        warn!(
            "the mount state record does not say the position is lost, so a reload or a \
             restart of the service before the next connect forgets it: {e}"
        );
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
/// carried forward to `first_failed_request` when the axis was tracking.
///
/// `None` when `previous` holds no sample, so there is nothing to carry
/// over (the controller restarted before the driver first read it).
///
/// An axis is trusted when it was stopped (exact), or tracking at a known
/// rate no faster than [`MAX_TRUSTED_RATE_SIDEREAL`] with a failed
/// request no more than `max_age` after its sample to bound the motion.
/// Anything else — a goto, the fast regime, a blocked axis, an unknown
/// rate or status, a tracking sample nothing bounds — is written back as
/// it was sampled, the best estimate there is, and marks the position
/// lost.
#[must_use]
pub fn position_to_restore(
    previous: &MountSnapshot,
    params: &MountParameters,
    first_failed_request: Option<Instant>,
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
        first_failed_request,
        max_age,
    );
    let dec = carry_axis(
        previous.dec,
        params.tmr_freq,
        params.high_speed_ratio_dec,
        params.cpr_dec,
        first_failed_request,
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
    first_failed_request: Option<Instant>,
    max_age: Duration,
) -> (i32, bool) {
    let sampled = axis.position_ticks.clamp(POSITION_MIN, POSITION_MAX);
    // No sample, or no status since the last handshake: how the axis
    // was moving is not known.
    let (Some(sampled_at), Some(_)) = (axis.sampled_at, axis.status) else {
        return (sampled, false);
    };
    if !axis.running() {
        return (sampled, true);
    }
    let rate = axis.rate_ticks_per_sec(tmr_freq, high_speed_ratio);
    let sidereal = f64::from(cpr) * SIDEREAL_DEG_PER_SEC / 360.0;
    let trusted_rate = rate != 0.0 && rate.abs() <= MAX_TRUSTED_RATE_SIDEREAL * sidereal;
    // A failure that predates the sample says nothing about when the
    // link went after it.
    match first_failed_request {
        Some(failed)
            if trusted_rate
                && failed >= sampled_at
                && failed.duration_since(sampled_at) <= max_age =>
        {
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
    fn a_tracking_axis_is_carried_to_the_first_failed_request() {
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
    fn a_tracking_axis_with_no_failed_request_is_lost() {
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
    fn an_axis_with_no_status_since_the_handshake_is_lost() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: AxisSnapshot {
                position_ticks: 400_000,
                sampled_at: Some(t),
                ..AxisSnapshot::default()
            },
            dec: stopped(725_760, t),
        };
        let restore = position_to_restore(&previous, &params(), None, MAX_AGE).unwrap();
        assert_eq!(restore.ra_ticks, 400_000);
        assert!(restore.lost);
    }

    #[test]
    fn a_failure_older_than_the_tracking_sample_bounds_nothing() {
        let t = Instant::now();
        let previous = MountSnapshot {
            ra: tracking_sidereal(100_000, t + Duration::from_millis(500)),
            dec: stopped(725_760, t),
        };
        let restore = position_to_restore(&previous, &params(), Some(t), MAX_AGE).unwrap();
        assert!(restore.lost);
    }

    #[test]
    fn the_first_failed_request_survives_later_failures_until_a_success() {
        let watch = PowerWatch::default();
        let t = Instant::now();
        watch.note_request_failed(t);
        watch.note_request_failed(t + Duration::from_millis(50));
        assert_eq!(watch.first_failed_request(), Some(t));
        watch.note_request_succeeded();
        assert_eq!(watch.first_failed_request(), None);
    }

    #[test]
    fn clearing_a_lost_position_says_whether_it_was_not_known() {
        let watch = PowerWatch::default();
        assert!(!watch.clear_position_lost());
        watch.mark_position_lost();
        assert_eq!(watch.position(), Position::Lost);
        assert!(watch.clear_position_lost());
        assert!(watch.position_known());
    }

    #[test]
    fn a_power_up_check_settles_known_or_lost() {
        let watch = PowerWatch::default();
        watch.await_power_up_check();
        assert_eq!(watch.position(), Position::AwaitingCheck);
        watch.settle_power_up_check(true);
        assert_eq!(watch.position(), Position::Known);
        watch.await_power_up_check();
        watch.settle_power_up_check(false);
        assert_eq!(watch.position(), Position::Lost);
    }

    fn watch_with_record(dir: &std::path::Path) -> PowerWatch {
        PowerWatch::with_record(Some(MountStateStore::beside(&dir.join("config.json"))))
    }

    async fn record_of(watch: &PowerWatch) -> ReadRecord {
        watch.record().unwrap().lock().await.read().await
    }

    async fn put_record(watch: &PowerWatch, record: MountStateRecord) {
        watch
            .record()
            .unwrap()
            .lock()
            .await
            .write(record)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_long_cut_of_a_mount_at_rest_holds_the_check_in_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let watch = watch_with_record(dir.path());
        put_record(
            &watch,
            MountStateRecord::AtRest {
                ra_ticks: 1_000,
                dec_ticks: 2_000,
            },
        )
        .await;
        watch.await_check_after_long_cut().await.unwrap();
        assert_eq!(watch.position(), Position::AwaitingCheck);
        assert_eq!(
            record_of(&watch).await,
            ReadRecord::Record(MountStateRecord::AwaitingCheck {
                ra_ticks: 1_000,
                dec_ticks: 2_000
            })
        );
    }

    #[tokio::test]
    async fn a_long_cut_with_no_record_loses_the_position_in_the_record_too() {
        let dir = tempfile::tempdir().unwrap();
        let watch = watch_with_record(dir.path());
        watch.await_check_after_long_cut().await.unwrap_err();
        assert_eq!(watch.position(), Position::Lost);
        assert_eq!(
            record_of(&watch).await,
            ReadRecord::Record(MountStateRecord::Lost)
        );
    }

    #[tokio::test]
    async fn a_long_cut_of_a_mount_the_record_saw_moving_loses_the_position() {
        let dir = tempfile::tempdir().unwrap();
        let watch = watch_with_record(dir.path());
        put_record(&watch, MountStateRecord::Moving).await;
        watch.await_check_after_long_cut().await.unwrap_err();
        assert_eq!(watch.position(), Position::Lost);
        assert_eq!(
            record_of(&watch).await,
            ReadRecord::Record(MountStateRecord::Lost)
        );
    }

    #[tokio::test]
    async fn a_long_cut_after_the_position_was_lost_records_lost_whatever_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let watch = watch_with_record(dir.path());
        put_record(
            &watch,
            MountStateRecord::AtRest {
                ra_ticks: 1_000,
                dec_ticks: 2_000,
            },
        )
        .await;
        watch.mark_position_lost();
        watch.await_check_after_long_cut().await.unwrap_err();
        assert_eq!(watch.position(), Position::Lost);
        assert_eq!(
            record_of(&watch).await,
            ReadRecord::Record(MountStateRecord::Lost)
        );
    }

    #[tokio::test]
    async fn a_long_cut_without_a_record_holds_the_check_in_memory() {
        let watch = PowerWatch::default();
        watch.await_check_after_long_cut().await.unwrap();
        assert_eq!(watch.position(), Position::AwaitingCheck);
    }

    #[tokio::test]
    async fn a_restore_that_loses_the_position_says_so_in_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let watch = watch_with_record(dir.path());
        put_record(
            &watch,
            MountStateRecord::AtRest {
                ra_ticks: 1_000,
                dec_ticks: 2_000,
            },
        )
        .await;
        watch.lose_position_after_restart().await;
        assert_eq!(watch.position(), Position::Lost);
        assert_eq!(
            record_of(&watch).await,
            ReadRecord::Record(MountStateRecord::Lost)
        );
    }

    #[test]
    fn a_lost_position_does_not_wait_for_a_power_up_check() {
        let watch = PowerWatch::default();
        watch.mark_position_lost();
        watch.await_power_up_check();
        assert_eq!(watch.position(), Position::Lost);
        watch.settle_power_up_check(true);
        assert_eq!(
            watch.position(),
            Position::Lost,
            "only ground truth clears it"
        );
    }
}

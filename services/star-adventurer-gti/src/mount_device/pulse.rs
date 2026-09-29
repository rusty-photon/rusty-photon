//! `PulseGuide`: starting a guide pulse, and the watcher that ends it.
//!
//! An East/West pulse on a tracking RA axis changes the step period of
//! the running motor — `:I1 <shifted>`, and `:I1 <sidereal>` to end it —
//! once a live `:f1` read confirms RA is running in Tracking / Slow / CW.
//! It never stops the motor. Every other pulse starts its axis from rest
//! (`:K`, `:f` until stopped, `:G` `:I` `:J`) and ends with `:K`, or — an
//! RA pulse with Tracking on — with `:I1 <sidereal>` on the running motor.
//!
//! Every wire burst runs under `axis_ownership`, and a pulse acts only
//! while its axis still holds its [`PulseId`]. See the design doc's
//! [§"`PulseGuide` lifecycle"](../../../../docs/services/star-adventurer-gti.md#pulseguide-lifecycle)
//! for the ownership rule, the edge-step trim and the restore's failure
//! ladder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::GuideDirection;
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use rusty_photon_shared_transport::Session;
use skywatcher_motor_protocol::{
    Axis, AxisStatus, Command, Direction, ModeKind, MotionMode, ProtocolError, Response, Speed,
};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::codec::SkywatcherCodec;
use crate::config::RaPulseEdgeSteps;
use crate::error::StarAdvError;
use crate::manager::{MountManager, MountParameters};

use super::slew::AXIS_STOP_TIMEOUT;
use super::telescope::GuidePulse;
use super::{DriverState, MountDevice, PulseId};

/// Device session slot, shared with the watcher. `Some` between
/// `set_connected(true)` and `set_connected(false)`.
type SessionSlot = Arc<RwLock<Option<Session<SkywatcherCodec>>>>;

/// Attempts at a restore frame, and at rolling back a start, before the
/// failure ladder takes over.
const RESTORE_ATTEMPTS: u32 = 3;
/// Pause between restore attempts, with `axis_ownership` released so a
/// waiting `AbortSlew` or guard is not held up by a flaky link.
const RETRY_BACKOFF: Duration = Duration::from_millis(50);
/// Largest shift, either way, the edge-step trim may make to a pulse's
/// run. The trim divides a few ticks by the pulse's rate difference, so a
/// tiny guide rate would stretch it without bound (1.38 ticks at 0.001 ×
/// sidereal is 33 s); the `GTi`'s defaults trim by 66 and 153 ms.
const MAX_TRIM_SECONDS: f64 = 0.5;
/// `:f` poll cadence while waiting for a pulse's axis to stop. There is
/// no fixed wait before the first poll: on the `GTi` a `:K` from a
/// tracking rate clears the running flag in about 4 ms.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How a pulse's watcher ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Restore {
    /// Put RA back to `period` (sidereal) on the running motor, with
    /// `:I1` alone.
    Rate { period: u32 },
    /// Stop RA and restart it at `period` (sidereal): `:K1`, `:f1` until
    /// stopped, `:G110 :I1 :J1`. Only for a mount that refused a live
    /// `:I1` this connection, where a live restore would be refused too.
    Restart { period: u32 },
    /// Stop the axis.
    Stop,
}

/// A started pulse, handed to its watcher.
#[derive(Debug, Clone, Copy)]
pub(super) struct PulsePlan {
    pub(super) axis: Axis,
    pub(super) id: PulseId,
    /// When the restore goes out: the instant the rate changed, plus the
    /// requested duration, plus the edge-step trim on the live-rate path.
    pub(super) deadline: Instant,
    pub(super) restore: Restore,
}

/// `true` for a failure the firmware answered — a `!` error reply: the
/// command did not take effect. Anything else (a timeout, an I/O error, a
/// reply that does not decode) leaves it unknown whether it landed.
const fn is_refusal(error: &StarAdvError) -> bool {
    matches!(error, StarAdvError::Protocol(ProtocolError::MountError(_)))
}

/// Whether a live `:f` status shows the axis already running in `mode`,
/// so the pulse can change its period without a `:G`. `running` is
/// required, not just the mode: a stopped axis reads Tracking on the real
/// firmware.
fn admits_live_rate(status: AxisStatus, mode: MotionMode) -> bool {
    let running_as = (
        status.mode,
        status.speed,
        status.direction == Direction::Ccw,
    );
    status.motion.running
        && !status.motion.blocked
        && running_as == (mode.kind, mode.speed, mode.ccw)
}

/// Seconds to add to a live-rate RA pulse so it delivers its commanded
/// angle despite the forward steps the motor board adds at each rate
/// change: `-steps / (r_pulse - r_sidereal)`, rates in encoder ticks per
/// second, clamped to `±MAX_TRIM_SECONDS`. Negative (the pulse ends early)
/// when the pulse speeds the axis up. `0` for anything but East/West.
fn edge_step_trim_seconds(
    direction: GuideDirection,
    shifted_period: u32,
    sidereal_period: u32,
    tmr_freq: u32,
    steps: RaPulseEdgeSteps,
) -> f64 {
    let steps = match direction {
        GuideDirection::East => steps.east(),
        GuideDirection::West => steps.west(),
        _ => return 0.0,
    };
    let r_pulse = f64::from(tmr_freq) / f64::from(shifted_period);
    let r_sidereal = f64::from(tmr_freq) / f64::from(sidereal_period);
    let delta = r_pulse - r_sidereal;
    if delta.abs() < f64::EPSILON {
        return 0.0;
    }
    (-steps / delta).clamp(-MAX_TRIM_SECONDS, MAX_TRIM_SECONDS)
}

/// The pulse's run from `t0`: `duration` plus `trim_seconds`, never
/// negative — a pulse shorter than its trim runs its two edges back to
/// back.
fn run_length(duration: Duration, trim_seconds: f64) -> Duration {
    Duration::try_from_secs_f64((duration.as_secs_f64() + trim_seconds).max(0.0))
        .unwrap_or(duration)
}

/// Poll `:f<axis>` until the axis reports stopped, with no fixed wait
/// before the first poll. `Ok(false)` when it still runs at `timeout`.
async fn wait_stopped(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    axis: Axis,
    timeout: Duration,
) -> crate::error::Result<bool> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if let Response::Status(status) =
            manager.send(session, Command::InquireStatus(axis)).await?
        {
            if !status.motion.running {
                return Ok(true);
            }
        }
        if deadline.is_none_or(|d| Instant::now() >= d) {
            return Ok(false);
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
}

/// `t0 + run`, saturating at `t0` on an overflow no real pulse reaches.
fn deadline_after(t0: Instant, run: Duration) -> Instant {
    t0.checked_add(run).unwrap_or(t0)
}

fn cancelled() -> ASCOMError {
    ASCOMError::new(
        ASCOMErrorCode::INVALID_OPERATION,
        "PulseGuide cancelled: another operation took over the axis while the pulse was starting",
    )
}

impl MountDevice {
    /// Start `pulse` for `duration` and hand it to its watcher. The
    /// request has already been validated and resolved.
    pub(super) async fn start_pulse(
        &self,
        direction: GuideDirection,
        pulse: GuidePulse,
        duration: Duration,
        params: &MountParameters,
    ) -> ASCOMResult<()> {
        let shifted = pulse.step_period()?;
        // The watcher's own session comes first: a failure here leaves
        // nothing on the wire to undo, and a watcher that exists is never
        // left without a way to end its pulse.
        let session = self
            .manager
            .transport()
            .acquire()
            .await
            .map_err(StarAdvError::from)?;
        let started = self
            .begin_pulse(direction, pulse, shifted, duration, params)
            .await;
        let plan = match started {
            Ok(plan) => plan,
            Err(e) => {
                if let Err(close) = session.close().await {
                    warn!(error = %close, "pulse-guide session close failed");
                }
                return Err(e);
            }
        };
        debug!(?direction, ?duration, ?plan, "pulse_guide started");
        PulseWatcher::for_device(self).spawn(session, plan);
        Ok(())
    }

    /// Claim the pulse's axis and put its rate on the wire. On failure the
    /// claim is released and nothing is left running that no watcher will
    /// end.
    async fn begin_pulse(
        &self,
        direction: GuideDirection,
        pulse: GuidePulse,
        shifted: u32,
        duration: Duration,
        params: &MountParameters,
    ) -> ASCOMResult<PulsePlan> {
        let axis = pulse.axis;
        let lock = self.axis_ownership.lock().await;
        let (id, tracking_on) = {
            let mut s = self.state.write().await;
            if self.slew_in_progress.load(Ordering::SeqCst) {
                return Err(ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    "PulseGuide refused while slewing",
                ));
            }
            if s.pulse_guiding.get(axis).is_some() {
                return Err(ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    "PulseGuide refused while a same-axis pulse is in flight",
                ));
            }
            let id = s.allocate_pulse_id();
            s.pulse_guiding.set(axis, Some(id));
            (id, axis == Axis::Ra && s.tracking_requested)
        };
        let sidereal = params.sidereal_step_period_ra();
        let result = async move {
            let live_rate_allowed = !self.live_rate_refused.load(Ordering::SeqCst);
            if tracking_on
                && live_rate_allowed
                && self.manager.commanded_step_period(Axis::Ra) == sidereal
            {
                match self.status_now(Axis::Ra).await? {
                    status if admits_live_rate(status, MotionMode::TRACKING) => {
                        let trim = edge_step_trim_seconds(
                            direction,
                            shifted,
                            sidereal,
                            params.tmr_freq,
                            self.config.ra_pulse_edge_steps,
                        );
                        let t0 = Instant::now();
                        match self
                            .send(Command::SetStepPeriod {
                                axis,
                                period: shifted,
                            })
                            .await
                        {
                            Ok(_) => {
                                debug!(shifted, trim, "pulse_guide: live rate change");
                                return Ok(PulsePlan {
                                    axis,
                                    id,
                                    deadline: deadline_after(t0, run_length(duration, trim)),
                                    restore: Restore::Rate { period: sidereal },
                                });
                            }
                            Err(e) if is_refusal(&e) => self.note_live_rate_refused(&e),
                            Err(e) => {
                                drop(lock);
                                self.roll_back_live_shift(id, sidereal).await;
                                return Err(e.into());
                            }
                        }
                    }
                    status if status.motion.running && status.mode == ModeKind::Goto => {
                        return Err(ASCOMError::new(
                            ASCOMErrorCode::INVALID_OPERATION,
                            "PulseGuide refused: the RA axis is running a goto",
                        ));
                    }
                    status => warn!(
                        ?status,
                        "Tracking is on but RA is not tracking as the driver last set it; \
                         starting RA from rest for the pulse"
                    ),
                }
            }
            let restore = if !tracking_on {
                Restore::Stop
            } else if self.live_rate_refused.load(Ordering::SeqCst) {
                Restore::Restart { period: sidereal }
            } else {
                Restore::Rate { period: sidereal }
            };
            self.start_from_rest(lock, pulse, shifted, duration, id, restore)
                .await
        }
        .await;
        if result.is_err() {
            self.state.write().await.release_pulse(axis, id);
        }
        result
    }

    /// Stop the pulse's axis, then set its mode and period and start it.
    /// The stop-and-wait runs with `axis_ownership` released; ownership is
    /// re-checked before the axis is started again.
    async fn start_from_rest(
        &self,
        lock: tokio::sync::MutexGuard<'_, ()>,
        pulse: GuidePulse,
        shifted: u32,
        duration: Duration,
        id: PulseId,
        restore: Restore,
    ) -> ASCOMResult<PulsePlan> {
        let axis = pulse.axis;
        self.send(Command::StopMotion(axis))
            .await
            .map_err(ASCOMError::from)?;
        drop(lock);
        if !self.wait_stopped_now(axis).await? {
            return Err(StarAdvError::Transport(format!(
                "axis {axis:?} did not stop within {AXIS_STOP_TIMEOUT:?}"
            ))
            .into());
        }
        let _lock = self.axis_ownership.lock().await;
        {
            let s = self.state.read().await;
            let tracking_changed = restore != Restore::Stop && !s.tracking_requested;
            if s.pulse_guiding.get(axis) != Some(id)
                || self.slew_in_progress.load(Ordering::SeqCst)
                || tracking_changed
            {
                debug!(
                    ?axis,
                    "pulse_guide: axis taken over during the stop-and-wait"
                );
                return Err(cancelled());
            }
        }
        let mode = MotionMode {
            kind: ModeKind::Tracking,
            speed: Speed::Slow,
            ccw: pulse.ccw,
        };
        self.send(Command::SetMotionMode { axis, mode })
            .await
            .map_err(ASCOMError::from)?;
        self.send(Command::SetStepPeriod {
            axis,
            period: shifted,
        })
        .await
        .map_err(ASCOMError::from)?;
        let t0 = Instant::now();
        if let Err(e) = self.send(Command::StartMotion(axis)).await {
            if !is_refusal(&e) {
                // The `:J` may have landed: do not leave the axis moving
                // with no watcher to end it.
                self.abandon_start(axis, restore).await;
            }
            return Err(e.into());
        }
        debug!(?axis, shifted, "pulse_guide: started from rest");
        Ok(PulsePlan {
            axis,
            id,
            deadline: deadline_after(t0, duration),
            restore,
        })
    }

    /// Live `:f<axis>` read through the device session.
    async fn status_now(&self, axis: Axis) -> ASCOMResult<AxisStatus> {
        match self.send(Command::InquireStatus(axis)).await {
            Ok(Response::Status(status)) => Ok(status),
            Ok(other) => Err(StarAdvError::Transport(format!(
                "unexpected reply to :f{axis:?}: {other:?}"
            ))
            .into()),
            Err(e) => Err(e.into()),
        }
    }

    async fn wait_stopped_now(&self, axis: Axis) -> ASCOMResult<bool> {
        self.with_session(async |session| {
            wait_stopped(&self.manager, session, axis, AXIS_STOP_TIMEOUT)
                .await
                .map_err(ASCOMError::from)
        })
        .await
    }

    /// The mount refused a live `:I1`: this connection stops trying, and
    /// its pulses stop and restart RA instead.
    fn note_live_rate_refused(&self, error: &StarAdvError) {
        if !self.live_rate_refused.swap(true, Ordering::SeqCst) {
            warn!(
                error = %error,
                "the mount refused a step-period change on the running RA motor; \
                 guide pulses on this connection will stop and restart RA instead"
            );
        }
    }

    /// The live `:I1 <shifted>` failed ambiguously: it may have landed.
    /// Put sidereal back — idempotent, so safe whether or not it did — so
    /// RA is not left at a rate no watcher will restore.
    async fn roll_back_live_shift(&self, id: PulseId, sidereal: u32) {
        for attempt in 1..=RESTORE_ATTEMPTS {
            let lock = self.axis_ownership.lock().await;
            if self.state.read().await.pulse_guiding.get(Axis::Ra) != Some(id) {
                return;
            }
            match self
                .send(Command::SetStepPeriod {
                    axis: Axis::Ra,
                    period: sidereal,
                })
                .await
            {
                Ok(_) => return,
                Err(e) => debug!(attempt, error = %e, "pulse_guide: rolling back the rate failed"),
            }
            drop(lock);
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
        warn!("pulse_guide: could not put RA back to sidereal after a failed pulse start; stopping RA");
        self.stop_ra_owned_by(id).await;
    }

    /// Stop RA, still owned by pulse `id`, that is running at a rate the
    /// driver can no longer vouch for: `:K1`, then `:L1` if the stop does
    /// not show. `Tracking` goes false only on a confirmed stop, so an
    /// unconfirmed one leaves the tracking-time guard armed.
    async fn stop_ra_owned_by(&self, id: PulseId) {
        let owned = async || self.state.read().await.pulse_guiding.get(Axis::Ra) == Some(id);
        for command in [
            Command::StopMotion(Axis::Ra),
            Command::InstantStop(Axis::Ra),
        ] {
            {
                let _lock = self.axis_ownership.lock().await;
                if !owned().await {
                    return;
                }
                if let Err(e) = self.send(command.clone()).await {
                    warn!(error = %e, ?command, "pulse_guide: stopping RA failed");
                }
            }
            if self.wait_stopped_now(Axis::Ra).await.unwrap_or(false) {
                let _lock = self.axis_ownership.lock().await;
                if owned().await {
                    self.state.write().await.tracking_requested = false;
                    warn!(
                        "pulse_guide: stopped RA after a failed pulse start. Tracking is now off"
                    );
                }
                return;
            }
        }
        error!("pulse_guide: RA still reports running after :K1 and :L1");
    }

    /// A start from rest failed after its `:J` may have landed. End what
    /// may be running: sidereal on RA that was tracking, a stop otherwise.
    async fn abandon_start(&self, axis: Axis, restore: Restore) {
        let command = match restore {
            Restore::Rate { period } => Command::SetStepPeriod { axis, period },
            Restore::Restart { .. } | Restore::Stop => Command::StopMotion(axis),
        };
        if let Err(e) = self.send(command).await {
            warn!(?axis, error = %e, "pulse_guide: could not end a pulse whose start failed");
        }
    }
}

/// Whether the watcher still owns its axis when it wakes.
enum Ownership {
    /// The axis still holds this pulse's id and nothing has taken over.
    Owned,
    /// Another operation cleared or replaced the pulse: send nothing, and
    /// leave the axis' pulse slot alone.
    Superseded,
    /// Parked, slewing, or the client disconnected: send nothing, and end
    /// the pulse.
    Abandoned,
}

/// Ends one pulse. Shares the device's state, lock and session slot, and
/// talks to the mount through its own session.
struct PulseWatcher {
    state: Arc<RwLock<DriverState>>,
    manager: Arc<MountManager>,
    session_slot: SessionSlot,
    slew_in_progress: Arc<AtomicBool>,
    axis_ownership: Arc<Mutex<()>>,
}

impl PulseWatcher {
    fn for_device(device: &MountDevice) -> Self {
        Self {
            state: Arc::clone(&device.state),
            manager: Arc::clone(&device.manager),
            session_slot: Arc::clone(&device.session),
            slew_in_progress: Arc::clone(&device.slew_in_progress),
            axis_ownership: Arc::clone(&device.axis_ownership),
        }
    }

    fn spawn(self, session: Session<SkywatcherCodec>, plan: PulsePlan) {
        tokio::spawn(async move {
            tokio::time::sleep_until(plan.deadline).await;
            self.end(&session, plan).await;
            if let Err(e) = session.close().await {
                warn!(error = %e, "pulse-guide watcher session close failed");
            }
        });
    }

    async fn ownership(&self, plan: PulsePlan) -> Ownership {
        // Copy out and release `state` before touching the session slot:
        // holding one while acquiring the other can deadlock against a
        // path that takes them the other way round and a queued writer.
        let (owned, parked) = {
            let s = self.state.read().await;
            (s.pulse_guiding.get(plan.axis) == Some(plan.id), s.at_park)
        };
        if !owned {
            return Ownership::Superseded;
        }
        if parked
            || self.slew_in_progress.load(Ordering::SeqCst)
            || self.session_slot.read().await.is_none()
        {
            return Ownership::Abandoned;
        }
        Ownership::Owned
    }

    async fn release(&self, plan: PulsePlan) {
        self.state.write().await.release_pulse(plan.axis, plan.id);
    }

    /// End the pulse, then release its axis.
    async fn end(&self, session: &Session<SkywatcherCodec>, plan: PulsePlan) {
        match plan.restore {
            Restore::Restart { period } => self.restart_tracking(session, plan, period).await,
            Restore::Rate { .. } | Restore::Stop => self.restore(session, plan).await,
        }
        self.release(plan).await;
    }

    /// Send the restore, retrying an ambiguous failure; fall back to the
    /// stop ladder when it cannot be made to land.
    async fn restore(&self, session: &Session<SkywatcherCodec>, plan: PulsePlan) {
        let axis = plan.axis;
        for attempt in 1..=RESTORE_ATTEMPTS {
            let lock = self.axis_ownership.lock().await;
            match self.ownership(plan).await {
                Ownership::Owned => {}
                Ownership::Superseded => {
                    debug!(
                        ?axis,
                        "pulse-guide watcher: pulse was cancelled; nothing to restore"
                    );
                    return;
                }
                Ownership::Abandoned => {
                    debug!(
                        ?axis,
                        "pulse-guide watcher: parked, slewing or disconnected"
                    );
                    return;
                }
            }
            let command = match plan.restore {
                Restore::Rate { period } => Command::SetStepPeriod { axis, period },
                Restore::Restart { .. } | Restore::Stop => Command::StopMotion(axis),
            };
            match self.manager.send(session, command).await {
                Ok(_) => {
                    drop(lock);
                    if plan.restore == Restore::Stop && !self.stopped(session, axis).await {
                        self.escalate(session, plan).await;
                    }
                    debug!(?axis, "pulse-guide watcher: pulse ended");
                    return;
                }
                Err(e) if is_refusal(&e) => {
                    warn!(?axis, error = %e, "pulse-guide restore refused by the mount");
                    break;
                }
                Err(e) => {
                    debug!(?axis, attempt, error = %e, "pulse-guide restore failed; retrying");
                }
            }
            drop(lock);
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
        self.stop_after_failed_restore(session, plan).await;
    }

    /// The mount refused live rate changes this connection: stop RA and
    /// restart it at sidereal. Both halves re-check, under
    /// `axis_ownership`, that the pulse still owns RA and Tracking is
    /// still on, so the restart never overrides an operation that took
    /// the axis during the stop-and-wait. If the restart does not land,
    /// RA is left stopped and `Tracking` is turned off to match.
    async fn restart_tracking(
        &self,
        session: &Session<SkywatcherCodec>,
        plan: PulsePlan,
        period: u32,
    ) {
        {
            let _lock = self.axis_ownership.lock().await;
            if !self.owned_while_tracking(plan).await {
                return;
            }
            if let Err(e) = self
                .manager
                .send(session, Command::StopMotion(Axis::Ra))
                .await
            {
                warn!(error = %e, "pulse-guide restart: :K1 failed");
            }
        }
        if !self.stopped(session, Axis::Ra).await && !self.escalate(session, plan).await {
            return;
        }
        let lock = self.axis_ownership.lock().await;
        if !self.owned_while_tracking(plan).await {
            return;
        }
        let restarted = async {
            self.manager
                .send(
                    session,
                    Command::SetMotionMode {
                        axis: Axis::Ra,
                        mode: MotionMode::TRACKING,
                    },
                )
                .await?;
            self.manager
                .send(
                    session,
                    Command::SetStepPeriod {
                        axis: Axis::Ra,
                        period,
                    },
                )
                .await?;
            self.manager
                .send(session, Command::StartMotion(Axis::Ra))
                .await
        }
        .await;
        match restarted {
            Ok(_) => {}
            // Refused: nothing was started, so RA is stopped for real.
            Err(e) if is_refusal(&e) => {
                self.state.write().await.tracking_requested = false;
                warn!(error = %e, "pulse-guide restart refused; RA is stopped. Tracking is now off");
            }
            // Ambiguous: the `:J1` may have landed. Stop RA and confirm it
            // before `Tracking` is allowed to read false.
            Err(e) => {
                warn!(error = %e, "pulse-guide restart failed ambiguously; stopping RA");
                drop(lock);
                self.stop_after_failed_restore(session, plan).await;
            }
        }
    }

    async fn owned_while_tracking(&self, plan: PulsePlan) -> bool {
        matches!(self.ownership(plan).await, Ownership::Owned)
            && self.state.read().await.tracking_requested
    }

    async fn stopped(&self, session: &Session<SkywatcherCodec>, axis: Axis) -> bool {
        wait_stopped(&self.manager, session, axis, AXIS_STOP_TIMEOUT)
            .await
            .unwrap_or(false)
    }

    /// The restore could not be made to land. A live-rate pulse leaves RA
    /// at a guide rate the driver can no longer vouch for, so stop it:
    /// `:K`, then `:L` if the stop does not show. `Tracking` goes false
    /// only on a confirmed stop, so an unconfirmed one leaves the
    /// tracking-time guard armed on a mount that may still be moving.
    async fn stop_after_failed_restore(&self, session: &Session<SkywatcherCodec>, plan: PulsePlan) {
        let axis = plan.axis;
        {
            let _lock = self.axis_ownership.lock().await;
            if !matches!(self.ownership(plan).await, Ownership::Owned) {
                return;
            }
            if let Err(e) = self.manager.send(session, Command::StopMotion(axis)).await {
                warn!(?axis, error = %e, "pulse-guide restore failed and :K failed too");
            }
        }
        let confirmed = self.stopped(session, axis).await || self.escalate(session, plan).await;
        if confirmed && plan.restore != Restore::Stop {
            let _lock = self.axis_ownership.lock().await;
            if matches!(self.ownership(plan).await, Ownership::Owned) {
                self.state.write().await.tracking_requested = false;
                warn!("pulse-guide restore failed; stopped RA. Tracking is now off");
            }
        }
    }

    /// `:L` an axis a `:K` did not stop, if the pulse still owns it.
    /// Returns whether the axis is confirmed stopped afterwards.
    async fn escalate(&self, session: &Session<SkywatcherCodec>, plan: PulsePlan) -> bool {
        let axis = plan.axis;
        {
            let _lock = self.axis_ownership.lock().await;
            if !matches!(self.ownership(plan).await, Ownership::Owned) {
                return false;
            }
            if let Err(e) = self.manager.send(session, Command::InstantStop(axis)).await {
                error!(?axis, error = %e, "pulse-guide: :L failed on an axis :K did not stop");
                return false;
            }
        }
        let stopped = self.stopped(session, axis).await;
        if !stopped {
            error!(
                ?axis,
                "pulse-guide: axis still reports running after :K and :L"
            );
        }
        stopped
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::coordinates::pulse_guide_step_period;
    use skywatcher_motor_protocol::error::MountErrorCode;
    use skywatcher_motor_protocol::{InitFlags, MotionFlags};

    const TMR: u32 = 16_000_000;
    const SIDEREAL: u32 = 379_912;
    const EAST_HALF: u32 = 759_824;
    const WEST_HALF: u32 = 253_275;

    #[test]
    fn the_default_trim_ends_a_west_pulse_early_and_an_east_pulse_late() {
        // West: -3.23 ticks / (63.17 - 42.12 ticks/s) = -153 ms.
        // East: -1.38 ticks / (21.06 - 42.12 ticks/s) = +66 ms.
        let steps = RaPulseEdgeSteps::default();
        let west = edge_step_trim_seconds(GuideDirection::West, WEST_HALF, SIDEREAL, TMR, steps);
        let east = edge_step_trim_seconds(GuideDirection::East, EAST_HALF, SIDEREAL, TMR, steps);
        assert!((west + 0.1534).abs() < 0.001, "west trim {west}");
        assert!((east - 0.0655).abs() < 0.001, "east trim {east}");
    }

    #[test]
    fn dec_pulses_are_never_trimmed() {
        let steps = RaPulseEdgeSteps::default();
        for direction in [GuideDirection::North, GuideDirection::South] {
            assert_eq!(
                edge_step_trim_seconds(direction, 949_780, 474_890, TMR, steps),
                0.0
            );
        }
    }

    #[test]
    fn the_trim_is_clamped_for_tiny_guide_rates() {
        // 0.001 × sidereal: the unclamped trim would be 1.38 / 0.042 s.
        let steps = RaPulseEdgeSteps::default();
        let east_tiny = pulse_guide_step_period(SIDEREAL, 0.999);
        let trim = edge_step_trim_seconds(GuideDirection::East, east_tiny, SIDEREAL, TMR, steps);
        assert!((trim - MAX_TRIM_SECONDS).abs() < 1e-12, "trim {trim}");
    }

    #[test]
    fn a_zero_step_config_disables_the_trim() {
        let steps = RaPulseEdgeSteps::new(0.0, 0.0);
        let trim = edge_step_trim_seconds(GuideDirection::West, WEST_HALF, SIDEREAL, TMR, steps);
        assert_eq!(trim, 0.0);
    }

    #[test]
    fn the_trim_lengthens_or_shortens_the_run() {
        assert_eq!(
            run_length(Duration::from_secs(2), 0.25),
            Duration::from_millis(2_250)
        );
        assert_eq!(
            run_length(Duration::from_secs(2), -0.25),
            Duration::from_millis(1_750)
        );
    }

    #[test]
    fn a_pulse_shorter_than_its_trim_runs_its_edges_back_to_back() {
        assert_eq!(
            run_length(Duration::from_millis(100), -0.153),
            Duration::ZERO
        );
    }

    fn status(running: bool, mode: ModeKind, speed: Speed, direction: Direction) -> AxisStatus {
        AxisStatus {
            mode,
            direction,
            speed,
            motion: MotionFlags {
                running,
                blocked: false,
            },
            init: InitFlags {
                initialized: true,
                level_switch_on: false,
            },
        }
    }

    #[test]
    fn only_a_running_tracking_slow_cw_axis_admits_a_live_rate_change() {
        let tracking = status(true, ModeKind::Tracking, Speed::Slow, Direction::Cw);
        assert!(admits_live_rate(tracking, MotionMode::TRACKING));
        for (why, s) in [
            // A stopped axis reads Tracking on the real firmware.
            (
                "stopped",
                status(false, ModeKind::Tracking, Speed::Slow, Direction::Cw),
            ),
            (
                "goto",
                status(true, ModeKind::Goto, Speed::Fast, Direction::Cw),
            ),
            (
                "fast",
                status(true, ModeKind::Tracking, Speed::Fast, Direction::Cw),
            ),
            (
                "ccw",
                status(true, ModeKind::Tracking, Speed::Slow, Direction::Ccw),
            ),
        ] {
            assert!(
                !admits_live_rate(s, MotionMode::TRACKING),
                "{why} must not admit"
            );
        }
        let blocked = AxisStatus {
            motion: MotionFlags {
                running: true,
                blocked: true,
            },
            ..tracking
        };
        assert!(!admits_live_rate(blocked, MotionMode::TRACKING));
    }

    #[test]
    fn only_a_mount_error_reply_counts_as_a_refusal() {
        assert!(is_refusal(&StarAdvError::Protocol(
            ProtocolError::MountError(MountErrorCode::MotorNotStopped)
        )));
        assert!(!is_refusal(&StarAdvError::Transport("timeout".into())));
        assert!(!is_refusal(&StarAdvError::Protocol(
            ProtocolError::PayloadError("garbled".into())
        )));
    }
}

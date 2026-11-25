//! Guiding in the camera loop.
//!
//! A `guiding/start` selects the guide stars on a fresh frame, calibrates
//! when no stored record serves the run, takes the lock and guides; each
//! frame's offset becomes at most one pulse an axis, RA first, each after
//! the mount check (docs/services/guider.md § Control, § The mount: checks
//! and pulses). A start or a dither answers once the stars have settled at
//! the lock, or with `settle_timeout`, and gives up its turn then. A pause
//! holds the corrections, and a full pause the exposures too, the mount
//! checked once an exposure's time.

use std::sync::Arc;

use ascom_alpaca::api::Telescope;
use rp_guiding::{
    Calibration, ControlError, ControlParams, Controller, Offset, Settle, SettleParams, Settling,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{Cycle, End, Then, Took};
use crate::api::{ApiError, ErrorCode, SettleSpec, Size, Stop, StopReason};
use crate::config::Config;
use crate::guider::{AbortAnswer, SettledAnswer, State, Turn};
use crate::mount::{Fault, MountStart, RunMount};

/// A settle's answer.
pub type SettledReply = oneshot::Sender<Result<SettledAnswer, ApiError>>;

/// What a `guiding/start` hands the loop.
pub struct GuidingRun {
    /// The mount as a request from `idle` or `looping` read it; `None` for a
    /// start in a run state, which guides with the run's.
    pub mount: Option<(Arc<dyn Telescope>, MountStart)>,
    pub recalibrate: bool,
    pub settle: SettleSpec,
    pub reply: SettledReply,
    /// The request's turn, held until the stars have settled or the settle
    /// has timed out.
    pub turn: Turn,
}

/// What a `dither` hands the loop.
pub struct DitherRun {
    pub amount: Size,
    pub ra_only: bool,
    pub settle: SettleSpec,
    pub reply: SettledReply,
    pub turn: Turn,
}

/// The state a start was found in, which a start that does not guide
/// returns to.
pub enum Found {
    Idle,
    Looping,
    /// A run, calibrated again: a fresh frame with too few stars returns to
    /// it, and a calibration that fails stops it.
    Run(Box<Guiding>),
}

/// A start waiting for what comes before its settle.
pub struct StartAfter {
    pub settle: SettleSpec,
    pub reply: SettledReply,
    pub found: Found,
}

/// A start waiting for its fresh selection, with the calibration it guides
/// with.
pub struct Starting {
    mount: RunMount,
    start: MountStart,
    calibration: Calibration,
    after: StartAfter,
    turn: Turn,
}

/// What a run in the loop is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    Guiding,
    StarLost,
    Paused { full: bool },
}

/// A request waiting for the stars to settle.
struct SettleWait {
    settle: Settle,
    reply: SettledReply,
    /// Whether it is a dither's, which a `dither/abort` ends.
    dither: bool,
    _turn: Turn,
}

/// A start in `star_lost` waiting for its fresh selection.
struct SelectWait {
    settle: SettleSpec,
    reply: SettledReply,
    turn: Turn,
}

/// A guiding run in the loop.
pub struct Guiding {
    controller: Controller,
    mount: RunMount,
    start: MountStart,
    calibration: Calibration,
    sub: Sub,
    settle: Option<SettleWait>,
    select: Option<SelectWait>,
    /// The offset of the latest frame that had one, where an aborted dither
    /// sets the lock.
    last_offset: Option<Offset>,
}

/// The control settings of the configuration, as the core takes them.
fn control_params(config: &Config) -> ControlParams {
    ControlParams {
        ra_aggression: config.control.ra.aggression.get(),
        ra_hysteresis: config.control.ra.hysteresis.get(),
        dec_mode: match config.control.dec.mode {
            crate::config::DecMode::Auto => rp_guiding::DecMode::Auto,
            crate::config::DecMode::North => rp_guiding::DecMode::North,
            crate::config::DecMode::South => rp_guiding::DecMode::South,
            crate::config::DecMode::Off => rp_guiding::DecMode::Off,
        },
        dec_aggression: config.control.dec.aggression.get(),
        dec_reversal_frames: config.control.dec.reversal_frames.get(),
        min_move_px: config.control.min_move_px.map(crate::config::MinMove::get),
        runaway_frames: config.control.runaway.frames.get(),
        runaway_min_error_px: config.control.runaway.min_error_px.get(),
        min_pulse: config.mount.min_pulse.get(),
        max_pulse: config.mount.max_pulse.get(),
    }
}

/// `no_stars`.
pub(super) fn no_stars() -> ApiError {
    ApiError::new(
        ErrorCode::NoStars,
        "the frame has fewer guide stars than stars.min",
    )
}

/// `not_guiding`.
fn not_guiding() -> ApiError {
    ApiError::new(
        ErrorCode::NotGuiding,
        "the guider is not in a run that guides",
    )
}

impl Cycle {
    /// Serve `command`, received during a frame or a full pause.
    pub(super) fn handle(&mut self, command: super::Command) {
        match command {
            super::Command::Reselect(reply) => {
                if self.status.lock().exposing() {
                    self.reselect = Some(reply);
                } else {
                    let _ = reply.send(Err(ApiError::new(
                        ErrorCode::NotExposing,
                        "the guider takes no frames",
                    )));
                }
            }
            super::Command::Calibrate(run) => {
                if matches!(self.mode, super::Mode::Looping) {
                    self.calibrate(*run);
                } else {
                    let _ = run.reply.send(Err(ApiError::new(
                        ErrorCode::NotIdle,
                        "the guider is in a run",
                    )));
                }
            }
            super::Command::Start(run) => self.start(*run),
            super::Command::Pause { full, reply } => {
                let _ = reply.send(self.pause(full));
            }
            super::Command::Resume(reply) => {
                let _ = reply.send(self.resume());
            }
            super::Command::Dither(run) => self.dither(*run),
            super::Command::AbortDither(reply) => {
                let _ = reply.send(self.abort_dither());
            }
        }
    }

    /// Take up a `guiding/start`.
    pub(super) fn start(&mut self, run: GuidingRun) {
        if run.recalibrate {
            if let super::Mode::Guiding(_) = &self.mode {
                return self.recalibrate(run);
            }
        }
        match &mut self.mode {
            super::Mode::Guiding(guiding) => {
                let GuidingRun {
                    settle,
                    reply,
                    turn,
                    ..
                } = run;
                self.status.lock().start_accepted();
                if let Sub::Paused { .. } = guiding.sub {
                    guiding.controller.resume();
                    guiding.sub = Sub::Guiding;
                    self.status.lock().set_state(State::Guiding);
                }
                if guiding.sub == Sub::StarLost {
                    self.status.lock().interruption();
                    guiding.select = Some(SelectWait {
                        settle,
                        reply,
                        turn,
                    });
                    self.select_next = true;
                    return;
                }
                let tolerance = settle.tolerance.pixels(guiding.pixel_scale());
                guiding.settle = Some(SettleWait {
                    settle: Settle::new(settle_params(tolerance, settle), self.epoch.elapsed()),
                    reply,
                    dither: false,
                    _turn: turn,
                });
                self.status.lock().set_settling(true);
            }
            super::Mode::Looping => self.begin_start(run, Found::Looping),
            super::Mode::Calibrating(_) | super::Mode::Starting(_) => {
                let _ = run.reply.send(Err(ApiError::new(
                    ErrorCode::NotIdle,
                    "the guider is starting a run",
                )));
            }
        }
    }

    /// Take up a `guiding/start` with `recalibrate` in a run: calibrate, on
    /// a fresh selection in `star_lost`, then guide with a new lock.
    fn recalibrate(&mut self, run: GuidingRun) {
        let super::Mode::Guiding(guiding) = std::mem::replace(&mut self.mode, super::Mode::Looping)
        else {
            return;
        };
        let GuidingRun {
            settle,
            reply,
            turn,
            ..
        } = run;
        {
            let mut status = self.status.lock();
            status.start_accepted();
            status.interruption();
        }
        let telescope = guiding.mount.telescope();
        let start = guiding.start;
        let reselect = guiding.sub == Sub::StarLost;
        let after = StartAfter {
            settle,
            reply,
            found: Found::Run(guiding),
        };
        self.calibrate_with(telescope, start, turn, Then::Guide(after), reselect);
    }

    /// Take up a start found in `idle` or `looping`: select on a fresh
    /// frame, then guide with the stored calibration that serves the run, or
    /// calibrate first.
    pub(super) fn begin_start(&mut self, run: GuidingRun, found: Found) {
        let GuidingRun {
            mount,
            recalibrate,
            settle,
            reply,
            turn,
        } = run;
        let Some((telescope, start)) = mount else {
            let _ = reply.send(Err(ApiError::new(
                ErrorCode::Internal,
                "a start from idle or looping came without the mount",
            )));
            return;
        };
        self.status.lock().start_accepted();
        let setup = self.setup(&start);
        let dec = self.config.control.dec.mode != crate::config::DecMode::Off;
        let stored = (!recalibrate)
            .then(|| self.store.get(start.side))
            .flatten()
            .map(|record| record.calibration)
            .filter(|calibration| calibration.serves(&setup, dec));
        let after = StartAfter {
            settle,
            reply,
            found,
        };
        match stored {
            Some(calibration) => {
                info!("guiding starts with the stored calibration");
                self.status.lock().set_state(State::Guiding);
                self.mode = super::Mode::Starting(Box::new(Starting {
                    mount: RunMount::new(
                        Arc::clone(&telescope),
                        start.side,
                        self.config.mount.pulse_timeout.get(),
                        self.config.mount.max_failures.get(),
                    ),
                    start,
                    calibration,
                    after,
                    turn,
                }));
                self.select_next = true;
            }
            None => self.calibrate_with(telescope, start, turn, Then::Guide(after), true),
        }
    }

    /// What follows a frame of a start waiting for its fresh selection.
    pub(super) fn starting_frame(&mut self, took: &Took) -> Option<End> {
        let super::Mode::Starting(_) = &self.mode else {
            return None;
        };
        let Took::Measured {
            selection: Some(selection),
            offset,
        } = took
        else {
            return None;
        };
        let super::Mode::Starting(starting) =
            std::mem::replace(&mut self.mode, super::Mode::Looping)
        else {
            return None;
        };
        let Starting {
            mount,
            start,
            calibration,
            after,
            turn,
        } = *starting;
        if selection.is_err() {
            return self.start_refused(after, turn, no_stars());
        }
        self.begin_guiding(
            mount,
            start,
            calibration,
            offset.unwrap_or_default(),
            after,
            turn,
        );
        None
    }

    /// A start that does not guide: answered with `error`, back to the
    /// state it found; a run it calibrated again stops, unless its fresh
    /// frame had too few stars.
    pub(super) fn start_refused(
        &mut self,
        after: StartAfter,
        turn: Turn,
        error: ApiError,
    ) -> Option<End> {
        match after.found {
            Found::Run(guiding) if error.code == ErrorCode::NoStars => {
                match guiding.sub {
                    Sub::Guiding => self.status.lock().set_state(State::Guiding),
                    Sub::StarLost => self.status.lock().set_state(State::StarLost),
                    Sub::Paused { full } => self.status.lock().paused(full),
                }
                self.mode = super::Mode::Guiding(guiding);
                drop(turn);
                let _ = after.reply.send(Err(error));
                None
            }
            Found::Idle => {
                self.ending_turn = Some(turn);
                Some(End::Ended(super::Answer::Refused(after.reply, error)))
            }
            Found::Looping => {
                self.mode = super::Mode::Looping;
                self.status.lock().set_state(State::Looping);
                drop(turn);
                let _ = after.reply.send(Err(error));
                None
            }
            Found::Run(_) => {
                let stop = Stop::new(StopReason::CalibrationFailed, error.message.clone());
                self.ending_turn = Some(turn);
                let _ = after.reply.send(Err(error));
                Some(End::Stop(stop))
            }
        }
    }

    /// Guide with `calibration`, the lock at `home`, and wait for the stars
    /// to settle as `after` asks.
    pub(super) fn begin_guiding(
        &mut self,
        mount: RunMount,
        start: MountStart,
        calibration: Calibration,
        home: Offset,
        after: StartAfter,
        turn: Turn,
    ) {
        let controller = Controller::new(
            &calibration,
            start.declination_deg,
            control_params(&self.config),
            home,
        );
        let tolerance = after
            .settle
            .tolerance
            .pixels(calibration.setup.pixel_scale_arcsec());
        let settle = SettleWait {
            settle: Settle::new(settle_params(tolerance, after.settle), self.epoch.elapsed()),
            reply: after.reply,
            dither: false,
            _turn: turn,
        };
        info!(?home, "guiding started");
        {
            let mut status = self.status.lock();
            status.set_state(State::Guiding);
            status.set_settling(true);
        }
        self.mode = super::Mode::Guiding(Box::new(Guiding {
            controller,
            mount,
            start,
            calibration,
            sub: Sub::Guiding,
            settle: Some(settle),
            select: None,
            last_offset: Some(home),
        }));
    }

    /// `guiding/pause`.
    fn pause(&mut self, full: bool) -> Result<State, ApiError> {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return Err(not_guiding());
        };
        if !matches!(guiding.sub, Sub::Paused { .. }) {
            self.status.lock().interruption();
        }
        guiding.sub = Sub::Paused { full };
        self.status.lock().paused(full);
        info!(full, "guiding paused");
        Ok(State::Paused)
    }

    /// `guiding/resume`.
    fn resume(&mut self) -> Result<State, ApiError> {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return Err(not_guiding());
        };
        match guiding.sub {
            Sub::Paused { .. } => {
                guiding.controller.resume();
                guiding.sub = Sub::Guiding;
                self.status.lock().set_state(State::Guiding);
                info!("guiding resumed");
                Ok(State::Guiding)
            }
            Sub::Guiding => Ok(State::Guiding),
            Sub::StarLost => Ok(State::StarLost),
        }
    }

    /// `dither`.
    fn dither(&mut self, run: DitherRun) {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            let _ = run.reply.send(Err(not_guiding()));
            return;
        };
        if guiding.sub != Sub::Guiding {
            let _ = run.reply.send(Err(not_guiding()));
            return;
        }
        let scale = guiding.pixel_scale();
        let amount_px = run.amount.pixels(scale);
        let lock = guiding
            .controller
            .dither(amount_px, run.ra_only, &mut self.rng);
        info!(amount_px, ?lock, "dither");
        let tolerance = run.settle.tolerance.pixels(scale);
        guiding.settle = Some(SettleWait {
            settle: Settle::new(settle_params(tolerance, run.settle), self.epoch.elapsed()),
            reply: run.reply,
            dither: true,
            _turn: run.turn,
        });
        self.status.lock().set_settling(true);
    }

    /// `dither/abort`.
    fn abort_dither(&mut self) -> AbortAnswer {
        let state = self.status.lock().state();
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return AbortAnswer {
                state,
                aborted: false,
            };
        };
        if !guiding.settle.as_ref().is_some_and(|wait| wait.dither) {
            return AbortAnswer {
                state,
                aborted: false,
            };
        }
        guiding
            .controller
            .lock_at(guiding.last_offset.unwrap_or_default());
        if let Some(wait) = guiding.settle.take() {
            let _ = wait.reply.send(Err(ApiError::new(
                ErrorCode::DitherAborted,
                "a dither/abort arrived while the dither waited to settle",
            )));
        }
        self.status.lock().set_settling(false);
        info!("dither aborted");
        AbortAnswer {
            state,
            aborted: true,
        }
    }

    /// What follows a frame of a guiding run.
    pub(super) async fn guiding_frame(
        &mut self,
        took: Took,
        cancel: &CancellationToken,
    ) -> Option<End> {
        let now = self.epoch.elapsed();
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        let (offset, failed) = match &took {
            Took::Measured { offset, selection } => {
                if let Some(selection) = selection {
                    if let Some(wait) = guiding.select.take() {
                        match selection {
                            Ok(_) => {
                                guiding.controller.rehome(Offset::default());
                                guiding.sub = Sub::Guiding;
                                let tolerance = wait.settle.tolerance.pixels(guiding.pixel_scale());
                                guiding.settle = Some(SettleWait {
                                    settle: Settle::new(settle_params(tolerance, wait.settle), now),
                                    reply: wait.reply,
                                    dither: false,
                                    _turn: wait.turn,
                                });
                                let mut status = self.status.lock();
                                status.set_state(State::Guiding);
                                status.set_settling(true);
                            }
                            Err(()) => {
                                let _ = wait.reply.send(Err(no_stars()));
                            }
                        }
                    } else if selection.is_ok() {
                        // A star/reselect: the new stars are the run's home.
                        guiding.controller.rehome(Offset::default());
                        self.status.lock().interruption();
                    }
                }
                (*offset, false)
            }
            Took::Failed(_) => (None, true),
        };
        let mut pulses = [None, None];
        match (guiding.sub, offset) {
            (Sub::Paused { .. }, offset) => {
                if offset.is_none() && !failed {
                    self.status.lock().star_lost_frame();
                }
            }
            (Sub::Guiding | Sub::StarLost, None) => {
                if !failed {
                    if guiding.sub == Sub::Guiding {
                        guiding.sub = Sub::StarLost;
                        self.status.lock().set_state(State::StarLost);
                        debug!("star lost");
                    }
                    guiding.controller.star_lost();
                    self.status.lock().star_lost_frame();
                }
                if let Some(end) = self.settle_frame(None, now) {
                    return Some(end);
                }
            }
            (Sub::Guiding | Sub::StarLost, Some(offset)) => {
                if guiding.sub == Sub::StarLost {
                    guiding.sub = Sub::Guiding;
                    self.status.lock().set_state(State::Guiding);
                    debug!("stars back");
                }
                guiding.last_offset = Some(offset);
                let distance = guiding.controller.distance_from_lock(offset);
                let step = match guiding.controller.step(offset) {
                    Ok(step) => step,
                    Err(ControlError::Runaway { axis }) => {
                        return Some(End::Stop(Stop::new(
                            StopReason::Runaway,
                            format!(
                                "the {axis} corrections ran away: the error did not follow them"
                            ),
                        )));
                    }
                };
                let dec = guiding.controller.corrects_dec().then_some(step.error.dec);
                self.status.lock().guide_step(step.error.ra, dec);
                pulses = [step.ra, step.dec];
                if let Some(end) = self.settle_frame(Some(distance), now) {
                    return Some(end);
                }
            }
        }
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        match send_pulses(&guiding.mount, pulses, cancel).await {
            Err(stop) => Some(End::Stop(stop)),
            Ok(failed) => guiding.mount.cycle(failed.as_deref()).map(End::Stop),
        }
    }

    /// Judge a settle that waits, with a frame's distance from the lock:
    /// answer it once settled or timed out.
    fn settle_frame(&mut self, distance: Option<f64>, now: std::time::Duration) -> Option<End> {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        let settling = guiding.settle.as_mut()?.settle.frame(distance, now);
        if settling == Settling::Settling {
            return None;
        }
        let wait = guiding.settle.take()?;
        if wait.dither {
            guiding.controller.dither_ended();
        }
        let scale = Some(guiding.pixel_scale());
        let mut status = self.status.lock();
        status.set_settling(false);
        let answer = match settling {
            Settling::Settled => {
                debug!("settled");
                Ok(status.settled(scale))
            }
            Settling::TimedOut | Settling::Settling => {
                debug!("settle timed out");
                Err(ApiError::new(
                    ErrorCode::SettleTimeout,
                    "guiding did not settle within the settle timeout; guiding goes on",
                ))
            }
        };
        drop(status);
        let _ = wait.reply.send(answer);
        None
    }

    /// A cycle of a full pause: no frame, the mount checked once an
    /// exposure's time, requests heard.
    pub(super) async fn full_pause(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut tokio::sync::mpsc::UnboundedReceiver<super::Command>,
    ) -> Option<End> {
        let until = tokio::time::Instant::now()
            .checked_add(self.exposer.exposure)
            .unwrap_or_else(tokio::time::Instant::now);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Some(End::Stop(Stop::requested())),
                Some(command) = commands.recv() => {
                    self.handle(command);
                    if !self.full_paused() {
                        return None;
                    }
                }
                () = tokio::time::sleep_until(until) => break,
            }
        }
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        match send_pulses(&guiding.mount, [None, None], cancel).await {
            Err(stop) => Some(End::Stop(stop)),
            Ok(failed) => guiding.mount.cycle(failed.as_deref()).map(End::Stop),
        }
    }

    /// Whether the run is in a full pause.
    pub(super) fn full_paused(&self) -> bool {
        matches!(
            &self.mode,
            super::Mode::Guiding(guiding) if guiding.sub == Sub::Paused { full: true }
        )
    }
}

impl Guiding {
    /// The run's pixel scale, its calibration's.
    fn pixel_scale(&self) -> f64 {
        self.calibration.setup.pixel_scale_arcsec()
    }
}

/// A settle's terms in pixels.
const fn settle_params(tolerance_px: f64, spec: SettleSpec) -> SettleParams {
    SettleParams {
        tolerance_px,
        time: spec.time,
        timeout: spec.timeout,
    }
}

/// Send `pulses`, RA then Dec, each after the mount check raced against
/// `cancel`; with none to send, check the mount once. The message of the
/// first failed mount call, after which no further pulse goes out; the stop
/// when the run stops.
pub(super) async fn send_pulses(
    mount: &RunMount,
    pulses: [Option<rp_guiding::Pulse>; 2],
    cancel: &CancellationToken,
) -> Result<Option<String>, Stop> {
    let mut sent = false;
    for pulse in pulses.into_iter().flatten() {
        sent = true;
        let checked = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::requested()),
            checked = mount.check() => checked,
        };
        let fault = match checked {
            Err(fault) => Some(fault),
            Ok(()) if cancel.is_cancelled() => return Err(Stop::requested()),
            Ok(()) => mount.pulse(pulse).await.err(),
        };
        match fault {
            None => {}
            Some(Fault::Stop(stop)) => return Err(stop),
            Some(Fault::Failed(message) | Fault::Unconfirmed(message)) => return Ok(Some(message)),
        }
    }
    if sent {
        return Ok(None);
    }
    let checked = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Stop::requested()),
        checked = mount.check() => checked,
    };
    match checked {
        Ok(()) => Ok(None),
        Err(Fault::Stop(stop)) => Err(stop),
        Err(Fault::Failed(message) | Fault::Unconfirmed(message)) => Ok(Some(message)),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, SystemTime};

    use ascom_alpaca::api::telescope::GuideDirection;
    use rp_guiding::{Axis, DecAxis, Direction, PierSide, Pulse};

    use super::super::tests::looping_cycle;
    use super::super::{Mode, Return};
    use super::*;
    use crate::calibration_store::Record;
    use crate::test_mount::FakeMount;

    type Answer = oneshot::Receiver<Result<SettledAnswer, ApiError>>;
    type Turns = Arc<tokio::sync::Mutex<()>>;

    /// What the mount read at the start: East of the pier at declination 0°,
    /// guide rates of 0.004 °/s.
    const fn mount_start() -> MountStart {
        MountStart {
            side: PierSide::East,
            declination_deg: 0.0,
            guide_rates: Some(rp_guiding::GuideRates {
                ra_deg_per_s: 0.004,
                dec_deg_per_s: 0.004,
            }),
        }
    }

    /// Settled within 1 px for 1 s, in at most 10 s.
    const fn settle() -> SettleSpec {
        SettleSpec {
            tolerance: Size::Pixels(1.0),
            time: Duration::from_secs(1),
            timeout: Duration::from_secs(10),
        }
    }

    /// A calibration of the loop's rig: West moves the stars along +x and
    /// North along +y, both at 2 px a second.
    fn calibration(cycle: &Cycle) -> Calibration {
        let axis = |angle_deg| Axis {
            angle_deg,
            rate_px_per_s: 2.0,
            steps: 4,
            step: Duration::from_secs(1),
        };
        Calibration {
            setup: cycle.setup(&mount_start()),
            declination_deg: 0.0,
            ra: axis(0.0),
            dec: Some(DecAxis {
                axis: axis(90.0),
                backlash: Duration::ZERO,
            }),
            warnings: Vec::new(),
        }
    }

    fn run_mount(mount: Arc<FakeMount>) -> RunMount {
        RunMount::new(mount, PierSide::East, Duration::from_secs(5), 3)
    }

    async fn turn() -> (Turn, Turns) {
        let turns = Arc::new(tokio::sync::Mutex::new(()));
        (Arc::clone(&turns).lock_owned().await, turns)
    }

    /// A start's answer channel, with what it found.
    fn after(found: Found) -> (StartAfter, Answer) {
        let (reply, answer) = oneshot::channel();
        (
            StartAfter {
                settle: settle(),
                reply,
                found,
            },
            answer,
        )
    }

    /// A loop guiding on `mount` from a start that waits to settle, the
    /// lock at home.
    async fn guiding_on(mount: Arc<FakeMount>) -> (Cycle, Answer, Turns) {
        guiding_from(mount, Offset::default()).await
    }

    /// A loop guiding on `mount` from a start that waits to settle, home at
    /// `home`.
    async fn guiding_from(mount: Arc<FakeMount>, home: Offset) -> (Cycle, Answer, Turns) {
        let mut cycle = looping_cycle();
        let calibration = calibration(&cycle);
        let (after, answer) = after(Found::Looping);
        let (turn, turns) = turn().await;
        cycle.begin_guiding(
            run_mount(mount),
            mount_start(),
            calibration,
            home,
            after,
            turn,
        );
        (cycle, answer, turns)
    }

    /// A loop guiding on a scripted mount whose start has settled.
    async fn settled_guiding(mount: Arc<FakeMount>) -> Cycle {
        settled_guiding_from(mount, Offset::default()).await
    }

    /// A loop guiding on a scripted mount from `home`, its start settled.
    async fn settled_guiding_from(mount: Arc<FakeMount>, home: Offset) -> Cycle {
        let (mut cycle, mut answer, _turns) = guiding_from(mount, home).await;
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(home.x, home.y)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        answer.try_recv().unwrap().unwrap();
        cycle
    }

    const fn at(x: f64, y: f64) -> Took {
        Took::Measured {
            offset: Some(Offset { x, y }),
            selection: None,
        }
    }

    const fn lost() -> Took {
        Took::Measured {
            offset: None,
            selection: None,
        }
    }

    const fn selection(stars: Result<usize, ()>) -> Took {
        Took::Measured {
            offset: Some(Offset { x: 0.0, y: 0.0 }),
            selection: Some(stars),
        }
    }

    async fn guide(cycle: &mut Cycle, took: Took) -> Option<End> {
        cycle.guiding_frame(took, &CancellationToken::new()).await
    }

    fn guiding(cycle: &Cycle) -> &Guiding {
        let Mode::Guiding(guiding) = &cycle.mode else {
            panic!("the loop is not guiding");
        };
        guiding
    }

    fn state(cycle: &Cycle) -> State {
        cycle.status.lock().state()
    }

    /// A start in a run, with its answer and the turns its turn came from.
    async fn start_in_a_run(recalibrate: bool) -> (GuidingRun, Answer, Turns) {
        let (reply, answer) = oneshot::channel();
        let (turn, turns) = turn().await;
        let run = GuidingRun {
            mount: None,
            recalibrate,
            settle: settle(),
            reply,
            turn,
        };
        (run, answer, turns)
    }

    /// A start from `idle` or `looping` on `mount`.
    async fn start_from_outside(
        mount: Arc<FakeMount>,
        recalibrate: bool,
    ) -> (GuidingRun, Answer, Turns) {
        let (mut run, answer, turns) = start_in_a_run(recalibrate).await;
        run.mount = Some((mount, mount_start()));
        (run, answer, turns)
    }

    fn dither_run(amount_px: f64) -> (DitherRun, Answer, oneshot::Receiver<()>) {
        let (reply, answer) = oneshot::channel();
        let turns = Arc::new(tokio::sync::Mutex::new(()));
        let turn = Arc::clone(&turns).try_lock_owned().expect("an unheld turn");
        // The receiver ends once the turn is given up.
        let (held, given_up) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _turn = turns.lock().await;
            let _ = held.send(());
        });
        (
            DitherRun {
                amount: Size::Pixels(amount_px),
                ra_only: false,
                settle: settle(),
                reply,
                turn,
            },
            answer,
            given_up,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_answers_once_the_stars_have_stayed_within_the_tolerance_for_the_time() {
        let (mut cycle, mut answer, turns) = guiding_on(Arc::new(FakeMount::default())).await;
        assert_eq!(state(&cycle), State::Guiding);
        assert!(cycle.status.lock().settling());
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        tokio::time::advance(Duration::from_millis(600)).await;
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        assert!(answer.try_recv().is_err(), "settled before the time");
        tokio::time::advance(Duration::from_millis(600)).await;
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        let settled = answer.try_recv().unwrap().unwrap();
        assert_eq!(settled.state, State::Guiding);
        assert_eq!(settled.sample_count, 3);
        assert!(!cycle.status.lock().settling());
        assert!(turns.try_lock().is_ok(), "the turn is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn a_settle_that_does_not_come_answers_settle_timeout_and_the_run_goes_on() {
        let (mut cycle, mut answer, turns) = guiding_on(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        assert!(answer.try_recv().is_err(), "timed out at the timeout");
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        assert_eq!(
            answer.try_recv().unwrap().unwrap_err().code,
            ErrorCode::SettleTimeout
        );
        assert_eq!(state(&cycle), State::StarLost);
        assert!(turns.try_lock().is_ok(), "the turn is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_without_stars_puts_the_run_in_star_lost_and_their_return_takes_it_back() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        assert_eq!(state(&cycle), State::StarLost);
        assert_eq!(cycle.status.lock().totals().star_lost, 1);
        assert!(guide(&mut cycle, at(0.0, 0.0)).await.is_none());
        assert_eq!(state(&cycle), State::Guiding);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guide_step_counts_each_axis_and_pulses_against_the_error() {
        let mount = Arc::new(FakeMount::default());
        let mut cycle = settled_guiding(Arc::clone(&mount)).await;
        let before = cycle.status.lock().totals();
        assert!(guide(&mut cycle, at(3.0, 3.0)).await.is_none());
        let totals = cycle.status.lock().totals();
        assert_eq!(totals.guide_steps, before.guide_steps + 1);
        assert_eq!(totals.ra.count(), before.ra.count() + 1);
        assert_eq!(totals.dec.count(), before.dec.count() + 1);
        // The stars stand West of the lock along +x and North along +y: the
        // corrections are East, then South.
        assert_eq!(
            *mount.directions.lock(),
            [GuideDirection::East, GuideDirection::South]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn corrections_the_error_does_not_follow_stop_the_run_with_runaway() {
        let mut cycle = looping_cycle();
        cycle.config = Arc::new(
            serde_json::from_value(serde_json::json!({
                "optics": { "focal_length_mm": 200.0 },
                "control": { "runaway": { "frames": 3, "min_error_px": 1.0 } }
            }))
            .unwrap(),
        );
        let calibration = calibration(&cycle);
        let (after, _answer) = after(Found::Looping);
        let (turn, _turns) = turn().await;
        cycle.begin_guiding(
            run_mount(Arc::new(FakeMount::default())),
            mount_start(),
            calibration,
            Offset::default(),
            after,
            turn,
        );
        let mut end = None;
        for x in 2..10 {
            end = guide(&mut cycle, at(f64::from(x), 0.0)).await;
            if end.is_some() {
                break;
            }
        }
        let Some(End::Stop(stop)) = end else {
            panic!("the run went on");
        };
        assert_eq!(stop.reason, StopReason::Runaway);
    }

    #[tokio::test(start_paused = true)]
    async fn a_star_lost_frame_ends_the_frames_in_a_row_the_runaway_stop_counts() {
        let mut cycle = looping_cycle();
        cycle.config = Arc::new(
            serde_json::from_value(serde_json::json!({
                "optics": { "focal_length_mm": 200.0 },
                "control": { "runaway": { "frames": 3, "min_error_px": 1.0 } }
            }))
            .unwrap(),
        );
        let calibration = calibration(&cycle);
        let (after, _answer) = after(Found::Looping);
        let (turn, _turns) = turn().await;
        cycle.begin_guiding(
            run_mount(Arc::new(FakeMount::default())),
            mount_start(),
            calibration,
            Offset::default(),
            after,
            turn,
        );
        for took in [
            at(2.0, 0.0),
            at(3.0, 0.0),
            lost(),
            at(4.0, 0.0),
            at(5.0, 0.0),
        ] {
            assert!(guide(&mut cycle, took).await.is_none(), "the run stopped");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_mount_that_fails_its_check_stops_the_run() {
        let mount = Arc::new(FakeMount::default());
        let mut cycle = settled_guiding(Arc::clone(&mount)).await;
        let Mode::Guiding(guiding) = &mut cycle.mode else {
            panic!("the loop is not guiding");
        };
        guiding.mount = run_mount(Arc::new(FakeMount {
            slewing: true,
            ..FakeMount::default()
        }));
        let Some(End::Stop(stop)) = guide(&mut cycle, at(0.0, 0.0)).await else {
            panic!("the run went on");
        };
        assert_eq!(stop.reason, StopReason::MountSlewing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_holds_the_corrections_and_counts_one_interruption() {
        let mount = Arc::new(FakeMount::default());
        let mut cycle = settled_guiding(Arc::clone(&mount)).await;
        assert_eq!(cycle.pause(false).unwrap(), State::Paused);
        assert_eq!(cycle.pause(false).unwrap(), State::Paused);
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
        let steps = cycle.status.lock().totals().guide_steps;
        assert!(guide(&mut cycle, at(3.0, 3.0)).await.is_none());
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 0);
        assert_eq!(cycle.status.lock().totals().guide_steps, steps);
        assert!(cycle.status.lock().exposing());
        assert_eq!(cycle.resume().unwrap(), State::Guiding);
        assert_eq!(state(&cycle), State::Guiding);
    }

    #[tokio::test(start_paused = true)]
    async fn pause_and_resume_outside_a_run_that_guides_answer_not_guiding() {
        let mut cycle = looping_cycle();
        assert_eq!(cycle.pause(false).unwrap_err().code, ErrorCode::NotGuiding);
        assert_eq!(cycle.resume().unwrap_err().code, ErrorCode::NotGuiding);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_in_star_lost_changes_nothing() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        assert_eq!(cycle.resume().unwrap(), State::StarLost);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_pause_takes_no_frame_and_checks_the_mount_once_an_exposure() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(true).unwrap();
        assert!(cycle.full_paused());
        assert!(!cycle.status.lock().exposing());
        let Mode::Guiding(guiding) = &mut cycle.mode else {
            panic!("the loop is not guiding");
        };
        guiding.mount = run_mount(Arc::new(FakeMount {
            at_park: true,
            ..FakeMount::default()
        }));
        let (_commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();
        let Some(End::Stop(stop)) = cycle
            .full_pause(&CancellationToken::new(), &mut received)
            .await
        else {
            panic!("the full pause went on");
        };
        assert_eq!(stop.reason, StopReason::MountParked);
        assert_eq!(started.elapsed(), cycle.exposer.exposure);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_ends_a_full_pause_at_once() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(true).unwrap();
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (reply, resumed) = oneshot::channel();
        commands.send(super::super::Command::Resume(reply)).unwrap();
        let started = tokio::time::Instant::now();
        assert!(cycle
            .full_pause(&CancellationToken::new(), &mut received)
            .await
            .is_none());
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(resumed.await.unwrap().unwrap(), State::Guiding);
        assert!(!cycle.full_paused());
        assert!(cycle.status.lock().exposing());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reselect_in_a_full_pause_answers_not_exposing() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(true).unwrap();
        let (reply, answer) = oneshot::channel();
        cycle.handle(super::super::Command::Reselect(reply));
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::NotExposing
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dither_moves_the_lock_and_answers_once_settled_there() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, mut answer, _given_up) = dither_run(4.0);
        cycle.dither(run);
        let lock = guiding(&cycle).controller.lock();
        assert!(
            lock.ra.hypot(lock.dec) >= 2.0,
            "the lock moved less than half the amount: {lock:?}"
        );
        assert!(cycle.status.lock().settling());
        let (x, y) = (lock.ra, lock.dec);
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(x, y)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        assert_eq!(answer.try_recv().unwrap().unwrap().state, State::Guiding);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dither_in_star_lost_answers_not_guiding() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, answer, _given_up) = dither_run(4.0);
        cycle.dither(run);
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::NotGuiding
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_sets_the_lock_to_the_stars_and_answers_the_dither_with_dither_aborted() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, answer, given_up) = dither_run(4.0);
        cycle.dither(run);
        assert!(guide(&mut cycle, at(0.25, -0.5)).await.is_none());
        let aborted = cycle.abort_dither();
        assert_eq!(
            aborted,
            AbortAnswer {
                state: State::Guiding,
                aborted: true
            }
        );
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::DitherAborted
        );
        given_up.await.unwrap();
        let lock = guiding(&cycle).controller.lock();
        assert!(
            (lock.ra - 0.25).abs() < 1e-9 && (lock.dec + 0.5).abs() < 1e-9,
            "{lock:?}"
        );
        assert!(!cycle.status.lock().settling());
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_leaves_a_start_that_waits_to_settle_waiting() {
        let (mut cycle, mut answer, _turns) = guiding_on(Arc::new(FakeMount::default())).await;
        assert!(!cycle.abort_dither().aborted);
        assert!(answer.try_recv().is_err(), "the start was answered");
        assert!(cycle.status.lock().settling());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dither_that_has_settled_corrects_dec_in_its_modes_direction_again() {
        let mount = Arc::new(FakeMount::default());
        let mut cycle = looping_cycle();
        cycle.config = Arc::new(
            serde_json::from_value(serde_json::json!({
                "optics": { "focal_length_mm": 200.0 },
                "control": { "dec": { "mode": "north" } }
            }))
            .unwrap(),
        );
        let calibration = calibration(&cycle);
        let (after, mut started) = after(Found::Looping);
        let (turn, _turns) = turn().await;
        cycle.begin_guiding(
            run_mount(Arc::clone(&mount)),
            mount_start(),
            calibration,
            Offset::default(),
            after,
            turn,
        );
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(0.0, 0.0)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        started.try_recv().unwrap().unwrap();
        let (run, mut dithered, _given_up) = dither_run(4.0);
        cycle.dither(run);
        let lock = guiding(&cycle).controller.lock();
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(lock.ra, lock.dec)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        dithered.try_recv().unwrap().unwrap();
        // The stars stand North of the lock: in the north mode no South
        // correction follows, however many frames show it.
        for _ in 0..5 {
            assert!(guide(&mut cycle, at(lock.ra, lock.dec + 3.0))
                .await
                .is_none());
        }
        assert!(
            !mount.directions.lock().contains(&GuideDirection::South),
            "{:?}",
            mount.directions.lock()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_mount_check_sends_no_pulse() {
        let cancel = CancellationToken::new();
        let mount = Arc::new(FakeMount {
            cancels: Some(cancel.clone()),
            ..FakeMount::default()
        });
        let stop = send_pulses(
            &run_mount(Arc::clone(&mount)),
            [Some(pulse(Direction::West)), None],
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(stop.reason, StopReason::Requested);
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_with_no_dither_waiting_changes_nothing() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(!cycle.abort_dither().aborted);
        assert!(!looping_cycle().abort_dither().aborted);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_while_guiding_keeps_the_lock_and_settles_again() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, mut answer, _turns) = start_in_a_run(false).await;
        cycle.start(run);
        assert!(cycle.status.lock().settling());
        assert_eq!(cycle.status.lock().metrics_len(), 0);
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(0.0, 0.0)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        assert_eq!(answer.try_recv().unwrap().unwrap().sample_count, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_in_paused_resumes_first() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(true).unwrap();
        let (run, _answer, _turns) = start_in_a_run(false).await;
        cycle.start(run);
        assert_eq!(state(&cycle), State::Guiding);
        assert!(!cycle.full_paused());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_in_star_lost_selects_anew_on_the_next_frame_and_takes_a_new_lock() {
        let home = Offset { x: 2.0, y: 1.0 };
        let mut cycle = settled_guiding_from(Arc::new(FakeMount::default()), home).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(false).await;
        cycle.start(run);
        assert!(cycle.select_next);
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
        assert!(guide(&mut cycle, selection(Ok(3))).await.is_none());
        assert_eq!(state(&cycle), State::Guiding);
        assert!(cycle.status.lock().settling());
        assert!(
            guiding(&cycle)
                .controller
                .distance_from_lock(Offset::default())
                < 1e-9
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reselect_in_a_run_takes_the_new_stars_as_home_and_counts_an_interruption() {
        let home = Offset { x: 2.0, y: 1.0 };
        let mut cycle = settled_guiding_from(Arc::new(FakeMount::default()), home).await;
        assert!(guide(&mut cycle, selection(Ok(3))).await.is_none());
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
        assert!(
            guiding(&cycle)
                .controller
                .distance_from_lock(Offset::default())
                < 1e-9
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_star_lost_frame_in_a_pause_counts() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(false).unwrap();
        assert!(guide(&mut cycle, lost()).await.is_none());
        assert_eq!(cycle.status.lock().totals().star_lost, 1);
        assert_eq!(state(&cycle), State::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_in_star_lost_whose_fresh_frame_has_too_few_stars_answers_no_stars() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, answer, turns) = start_in_a_run(false).await;
        cycle.start(run);
        let refused = Took::Measured {
            offset: None,
            selection: Some(Err(())),
        };
        assert!(guide(&mut cycle, refused).await.is_none());
        assert_eq!(answer.await.unwrap().unwrap_err().code, ErrorCode::NoStars);
        assert_eq!(state(&cycle), State::StarLost);
        assert!(turns.try_lock().is_ok(), "the turn is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_a_run_calibrates_then_guides_with_the_lock_where_the_stars_stand() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        assert_eq!(state(&cycle), State::Calibrating);
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
        let record = Record {
            calibrated_at: SystemTime::now(),
            calibration: calibration(&cycle),
        };
        let here = Offset { x: 1.5, y: -2.0 };
        assert!(cycle.calibration_ended(Ok(record), Some(here)).is_none());
        assert_eq!(state(&cycle), State::Guiding);
        assert!(guiding(&cycle).controller.distance_from_lock(here) < 1e-9);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_star_lost_whose_fresh_frame_has_too_few_stars_returns_to_the_run() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, answer, turns) = start_in_a_run(true).await;
        cycle.start(run);
        assert!(cycle.select_next);
        let end = cycle
            .after_frame(
                Took::Measured {
                    offset: None,
                    selection: Some(Err(())),
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(end.is_none());
        assert_eq!(answer.await.unwrap().unwrap_err().code, ErrorCode::NoStars);
        assert_eq!(state(&cycle), State::StarLost);
        assert_eq!(guiding(&cycle).sub, Sub::StarLost);
        assert!(turns.try_lock().is_ok(), "the turn is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_whose_calibration_fails_stops_the_run_with_calibration_failed() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        let failed = ApiError::new(ErrorCode::CalibrationFailed, "the RA leg did not move");
        let Some(End::Stop(stop)) = cycle.calibration_ended(Err(failed), None) else {
            panic!("the run went on");
        };
        assert_eq!(stop.reason, StopReason::CalibrationFailed);
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::CalibrationFailed
        );
        assert!(cycle.ending_turn.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_from_looping_with_a_stored_calibration_guides_after_a_fresh_selection() {
        let mut cycle = looping_cycle();
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: calibration(&cycle),
            })
            .unwrap();
        let (run, _answer, _turns) =
            start_from_outside(Arc::new(FakeMount::default()), false).await;
        cycle.begin_start(run, Found::Looping);
        assert!(matches!(cycle.mode, Mode::Starting(_)));
        assert!(cycle.select_next);
        assert!(cycle.starting_frame(&selection(Ok(3))).is_none());
        assert_eq!(state(&cycle), State::Guiding);
        assert!(cycle.status.lock().settling());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_with_no_stored_calibration_calibrates_first_on_a_fresh_selection() {
        let mut cycle = looping_cycle();
        let (run, _answer, _turns) =
            start_from_outside(Arc::new(FakeMount::default()), false).await;
        cycle.begin_start(run, Found::Looping);
        let Mode::Calibrating(calibrating) = &cycle.mode else {
            panic!("the loop is not calibrating");
        };
        assert!(calibrating.selecting);
        assert!(matches!(calibrating.then, Then::Guide(_)));
        assert!(cycle.select_next);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stored_calibration_of_another_setup_is_not_used() {
        let mut cycle = looping_cycle();
        let mut other = calibration(&cycle);
        other.setup.camera.binning = 2;
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: other,
            })
            .unwrap();
        let (run, _answer, _turns) =
            start_from_outside(Arc::new(FakeMount::default()), false).await;
        cycle.begin_start(run, Found::Looping);
        assert!(matches!(cycle.mode, Mode::Calibrating(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_with_recalibrate_calibrates_whatever_the_store_holds() {
        let mut cycle = looping_cycle();
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: calibration(&cycle),
            })
            .unwrap();
        let (run, _answer, _turns) = start_from_outside(Arc::new(FakeMount::default()), true).await;
        cycle.begin_start(run, Found::Looping);
        assert!(matches!(cycle.mode, Mode::Calibrating(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_from_looping_whose_fresh_frame_has_too_few_stars_goes_back_to_looping() {
        let mut cycle = looping_cycle();
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: calibration(&cycle),
            })
            .unwrap();
        let (run, answer, turns) = start_from_outside(Arc::new(FakeMount::default()), false).await;
        cycle.begin_start(run, Found::Looping);
        assert!(cycle.starting_frame(&selection(Err(()))).is_none());
        assert_eq!(state(&cycle), State::Looping);
        assert!(matches!(cycle.mode, Mode::Looping));
        assert_eq!(answer.await.unwrap().unwrap_err().code, ErrorCode::NoStars);
        assert!(turns.try_lock().is_ok(), "the turn is still held");
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_from_idle_that_does_not_guide_ends_the_loop_holding_its_turn() {
        let mut cycle = looping_cycle();
        let (after, _answer) = after(Found::Idle);
        let (turn, turns) = turn().await;
        let end = cycle.start_refused(after, turn, no_stars());
        assert!(matches!(
            end,
            Some(End::Ended(super::super::Answer::Refused(..)))
        ));
        assert!(turns.try_lock().is_err(), "the turn was given up");
        assert!(cycle.ending_turn.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_from_looping_whose_calibration_fails_goes_back_to_looping() {
        let mut cycle = looping_cycle();
        let (run, answer, _turns) = start_from_outside(Arc::new(FakeMount::default()), false).await;
        cycle.begin_start(run, Found::Looping);
        let failed = ApiError::new(ErrorCode::CalibrationFailed, "the RA leg did not move");
        assert!(cycle.calibration_ended(Err(failed), None).is_none());
        assert_eq!(state(&cycle), State::Looping);
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::CalibrationFailed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibrate_in_a_run_answers_not_idle() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (reply, answer) = oneshot::channel();
        let (turn, _turns) = turn().await;
        cycle.handle(super::super::Command::Calibrate(Box::new(
            super::super::CalibrationRun {
                mount: Arc::new(FakeMount::default()),
                start: mount_start(),
                reply,
                return_to: Return::Looping,
                turn,
            },
        )));
        assert_eq!(answer.await.unwrap().unwrap_err().code, ErrorCode::NotIdle);
        assert_eq!(state(&cycle), State::Guiding);
    }

    const fn pulse(direction: Direction) -> Pulse {
        Pulse {
            direction,
            duration: Duration::from_millis(100),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_pulses_go_out_ra_first() {
        let mount = Arc::new(FakeMount::default());
        let sent = send_pulses(
            &run_mount(Arc::clone(&mount)),
            [Some(pulse(Direction::West)), Some(pulse(Direction::North))],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(sent.unwrap(), None);
        assert_eq!(
            *mount.directions.lock(),
            [GuideDirection::West, GuideDirection::North]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_pulse_goes_out_after_one_that_failed() {
        let mount = Arc::new(FakeMount {
            pulse_refused: true,
            ..FakeMount::default()
        });
        let sent = send_pulses(
            &run_mount(Arc::clone(&mount)),
            [Some(pulse(Direction::West)), Some(pulse(Direction::North))],
            &CancellationToken::new(),
        )
        .await;
        assert!(sent.unwrap().is_some(), "the failure was not reported");
        assert_eq!(*mount.directions.lock(), [GuideDirection::West]);
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_pulse_to_send_the_mount_is_still_checked() {
        let mount = Arc::new(FakeMount {
            at_park: true,
            ..FakeMount::default()
        });
        let stop = send_pulses(&run_mount(mount), [None, None], &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(stop.reason, StopReason::MountParked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_before_the_check_sends_no_pulse() {
        let mount = Arc::new(FakeMount::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let stop = send_pulses(
            &run_mount(Arc::clone(&mount)),
            [Some(pulse(Direction::West)), None],
            &cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(stop.reason, StopReason::Requested);
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_control_settings_come_from_the_configuration() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "control": {
                "ra": { "aggression": 0.5 },
                "dec": { "mode": "north" },
                "min_move_px": 0.3
            }
        }))
        .unwrap();
        let params = control_params(&config);
        assert_eq!(params.ra_aggression, 0.5);
        assert_eq!(params.dec_mode, rp_guiding::DecMode::North);
        assert_eq!(params.min_move_px, Some(0.3));
    }
}

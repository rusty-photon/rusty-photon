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
use parking_lot::Mutex;
use rp_guiding::{
    Calibration, ControlError, ControlParams, Controller, Offset, Position, Settle, SettleParams,
    Settling,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{Cycle, End, Then, Took};
use crate::api::{ApiError, ErrorCode, SettleSpec, Size, Stop, StopReason};
use crate::config::Config;
use crate::guide_log::{self, GuideLog};
use crate::guider::{AbortAnswer, SettledAnswer, State, Status, Turn};
use crate::mount::{Fault, MountStart, RunMount};

/// The guide log's note of a settle that begins.
const SETTLING_STARTED: &str = "SETTLING STATE CHANGE, Settling started";

/// The guide log's note of a new lock at `position` on the sensor.
fn lock_note(position: Position) -> String {
    format!(
        "SET LOCK POSITION, new lock pos = {:.3}, {:.3}",
        position.x, position.y
    )
}

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

impl SettleWait {
    /// A wait from `now` on `spec`, its tolerance taken at `pixel_scale`.
    fn new(
        spec: SettleSpec,
        pixel_scale: f64,
        now: std::time::Duration,
        reply: SettledReply,
        dither: bool,
        turn: Turn,
    ) -> Self {
        let params = SettleParams {
            tolerance_px: spec.tolerance.pixels(pixel_scale),
            time: spec.time,
            timeout: spec.timeout,
        };
        Self {
            settle: Settle::new(params, now),
            reply,
            dither,
            _turn: turn,
        }
    }
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
    /// The stars' offset where the lock is at zero, for the guide log.
    home: Offset,
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

impl Cycle {
    /// Serve `command`, received during a frame or a full pause.
    pub(super) fn handle(&mut self, command: super::Command) {
        match command {
            super::Command::Reselect(reply) => {
                if self.status.lock().exposing() {
                    self.reselect = Some(reply);
                } else {
                    let _ = reply.send(Err(ApiError::not_exposing()));
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
                    self.guide_log.lock().note("Server received guiding/start");
                }
                if guiding.sub == Sub::StarLost {
                    guiding.select = Some(SelectWait {
                        settle,
                        reply,
                        turn,
                    });
                    self.select_next = true;
                    return;
                }
                guiding.settle = Some(SettleWait::new(
                    settle,
                    guiding.pixel_scale(),
                    self.epoch.elapsed(),
                    reply,
                    false,
                    turn,
                ));
                self.status.lock().set_settling(true);
                self.guide_log.lock().note(SETTLING_STARTED);
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
        let telescope = guiding.mount.telescope();
        let start = guiding.start;
        // In `star_lost` the run calibrates again only once its fresh frame
        // has selected guide stars, and counts the interruption then: a
        // fresh frame with too few stars leaves the run as it was. Its
        // section of the guide log ends where the calibration's begins.
        let reselect = guiding.sub == Sub::StarLost;
        let mut status = self.status.lock();
        status.start_accepted();
        if !reselect {
            status.interruption();
        }
        drop(status);
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
                // The state stays the one the start found, `looping` from
                // `idle`, until guiding begins on the fresh selection.
                info!("guiding starts with the stored calibration");
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

    /// What follows a frame of a start waiting for its fresh selection: the
    /// mount check, as in every cycle of a run that sends no pulse, then the
    /// selection when the frame made one.
    pub(super) async fn starting_frame(
        &mut self,
        took: &Took,
        cancel: &CancellationToken,
    ) -> Option<End> {
        let super::Mode::Starting(starting) = &mut self.mode else {
            return None;
        };
        let failed = match send_pulses(&starting.mount, &mut [None, None], cancel).await {
            Err(stop) => return Some(End::Stop(stop)),
            Ok(failed) => failed,
        };
        if let Some(stop) = starting.mount.cycle(failed.as_deref()) {
            return Some(End::Stop(stop));
        }
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
            return self.start_refused(after, turn, ApiError::no_stars());
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
        let settle = SettleWait::new(
            after.settle,
            calibration.setup.pixel_scale_arcsec(),
            self.epoch.elapsed(),
            after.reply,
            false,
            turn,
        );
        info!(?home, "guiding started");
        {
            let mut status = self.status.lock();
            status.set_state(State::Guiding);
            status.set_settling(true);
        }
        let control = guide_log::Control {
            ra_hysteresis: self.config.control.ra.hysteresis.get(),
            ra_aggression: self.config.control.ra.aggression.get(),
            dec_aggression: self.config.control.dec.aggression.get(),
            dec_reversal_frames: self.config.control.dec.reversal_frames.get(),
            dec_mode: match self.config.control.dec.mode {
                crate::config::DecMode::Auto => Some("Auto"),
                crate::config::DecMode::North => Some("North"),
                crate::config::DecMode::South => Some("South"),
                crate::config::DecMode::Off => None,
            },
            min_move_px: controller.min_move_px(),
            max_pulse: self.config.mount.max_pulse.get(),
        };
        let common = self.log_common(&start, home, home);
        {
            let mut log = self.guide_log.lock();
            log.begin_guiding(&common, &calibration, &control, self.epoch.elapsed());
            log.note(SETTLING_STARTED);
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
            home,
        }));
    }

    /// `guiding/pause`.
    fn pause(&mut self, full: bool) -> Result<State, ApiError> {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return Err(ApiError::not_guiding());
        };
        if !matches!(guiding.sub, Sub::Paused { .. }) {
            self.status.lock().interruption();
        }
        guiding.sub = Sub::Paused { full };
        self.status.lock().paused(full);
        self.guide_log
            .lock()
            .note(&format!("Server received guiding/pause, full = {full}"));
        info!(full, "guiding paused");
        Ok(State::Paused)
    }

    /// `guiding/resume`.
    fn resume(&mut self) -> Result<State, ApiError> {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return Err(ApiError::not_guiding());
        };
        match guiding.sub {
            Sub::Paused { .. } => {
                guiding.controller.resume();
                guiding.sub = Sub::Guiding;
                self.status.lock().set_state(State::Guiding);
                self.guide_log.lock().note("Server received guiding/resume");
                info!("guiding resumed");
                Ok(State::Guiding)
            }
            Sub::Guiding => Ok(State::Guiding),
            Sub::StarLost => Ok(State::StarLost),
        }
    }

    /// `dither`.
    fn dither(&mut self, run: DitherRun) {
        let reference = self.star_at(Offset::default());
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            let _ = run.reply.send(Err(ApiError::not_guiding()));
            return;
        };
        if guiding.sub != Sub::Guiding {
            let _ = run.reply.send(Err(ApiError::not_guiding()));
            return;
        }
        let scale = guiding.pixel_scale();
        let amount_px = run.amount.pixels(scale);
        let was = guiding.controller.lock();
        let lock = guiding
            .controller
            .dither(amount_px, run.ra_only, &mut self.rng);
        info!(amount_px, ?lock, "dither");
        let now = guide_log::to_sensor(&guiding.calibration, lock);
        // The lock's move on the axes, signed as PHD2 signs its raw
        // distances: the next frame's are the move turned around.
        self.guide_log.lock().note(&format!(
            "DITHER by {:.3}, {:.3}, new lock pos = {:.3}, {:.3}",
            was.ra - lock.ra,
            lock.dec - was.dec,
            reference.x + guiding.home.x + now.x,
            reference.y + guiding.home.y + now.y
        ));
        guiding.settle = Some(SettleWait::new(
            run.settle,
            scale,
            self.epoch.elapsed(),
            run.reply,
            true,
            run.turn,
        ));
        self.status.lock().set_settling(true);
        self.guide_log.lock().note(SETTLING_STARTED);
    }

    /// `dither/abort`.
    fn abort_dither(&mut self) -> AbortAnswer {
        let reference = self.star_at(Offset::default());
        abort(
            &mut self.mode,
            &self.status,
            &mut self.inbox,
            &self.guide_log,
            reference,
        )
    }

    /// What follows a frame of a guiding run.
    pub(super) async fn guiding_frame(
        &mut self,
        took: Took,
        cancel: &CancellationToken,
    ) -> Option<End> {
        let now = self.epoch.elapsed();
        // The first guide star's reference after this frame's selection,
        // if it made one.
        let reference = self.star_at(Offset::default());
        let summary = self.last_summary;
        let flux = summary.and_then(|summary| summary.flux);
        let snr = summary.and_then(|summary| summary.snr);
        if let Took::Measured {
            selection: Some(selection),
            ..
        } = took
        {
            self.selected(selection, reference, now);
        }
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        let (offset, failed) = match &took {
            Took::Measured { offset, .. } => (*offset, false),
            Took::Failed(_) => (None, true),
        };
        // Every measured frame with an offset, paused or rehomed, is where
        // an aborted dither sets the lock.
        if offset.is_some() {
            guiding.last_offset = offset;
        }
        let mut pulses = [None, None];
        // A guide step's row and its distance from the lock, written and
        // judged once its pulses have gone out.
        let mut stepped_frame = None;
        match (guiding.sub, offset) {
            (Sub::Paused { .. }, offset) => {
                if offset.is_none() && !failed {
                    self.status.lock().star_lost_frame();
                }
            }
            // A failed frame is no star-lost frame: it leaves the state,
            // the counts and the settle as they were.
            (Sub::Guiding | Sub::StarLost, None) if failed => {}
            (Sub::Guiding | Sub::StarLost, None) => {
                if guiding.sub == Sub::Guiding {
                    guiding.sub = Sub::StarLost;
                    self.status.lock().set_state(State::StarLost);
                    debug!("star lost");
                }
                guiding.controller.star_lost();
                self.status.lock().star_lost_frame();
                self.guide_log.lock().star_lost(flux, snr, now);
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
                let distance = guiding.controller.distance_from_lock(offset);
                // A guide step whether or not a pulse follows it, the step
                // that runs away included.
                let error = guiding.controller.error(offset);
                let dec = guiding.controller.corrects_dec().then_some(error.dec);
                self.status.lock().guide_step(error.ra, dec);
                let lock = guide_log::to_sensor(&guiding.calibration, guiding.controller.lock());
                let stepped = guiding.controller.step(offset);
                let row = guide_row(guiding, offset, lock, error, (flux, snr));
                let step = match stepped {
                    Ok(step) => step,
                    Err(ControlError::Runaway { axis }) => {
                        self.guide_log.lock().guide_step(&row, now);
                        return Some(End::Stop(Stop::new(
                            StopReason::Runaway,
                            format!(
                                "the {axis} corrections ran away: the error did not follow them"
                            ),
                        )));
                    }
                };
                pulses = [step.ra, step.dec];
                stepped_frame = Some((row, distance));
            }
        }
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        let sent = send_pulses(&guiding.mount, &mut pulses, cancel).await;
        if let Some((mut row, distance)) = stepped_frame {
            // The row gives the pulses that went out, as PHD2's does.
            [row.ra, row.dec] = pulses;
            self.guide_log.lock().guide_step(&row, now);
            if let Some(end) = self.settle_frame(Some(distance), now) {
                return Some(end);
            }
        }
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return None;
        };
        match sent {
            Err(stop) => Some(End::Stop(stop)),
            Ok(failed) => guiding.mount.cycle(failed.as_deref()).map(End::Stop),
        }
    }

    /// A frame of a guiding run that selected guide stars, the first guide
    /// star's reference after it at `reference`: a start in `star_lost`
    /// waiting for it takes a new lock and settles, or answers `no_stars`;
    /// otherwise it is a `star/reselect`, whose stars are the run's home.
    fn selected(
        &mut self,
        selection: Result<usize, ()>,
        reference: Position,
        now: std::time::Duration,
    ) {
        let super::Mode::Guiding(guiding) = &mut self.mode else {
            return;
        };
        let waiting = guiding.select.take();
        match (waiting, selection) {
            (Some(wait), Ok(_)) => {
                guiding.controller.rehome(Offset::default());
                guiding.home = Offset::default();
                guiding.sub = Sub::Guiding;
                guiding.settle = Some(SettleWait::new(
                    wait.settle,
                    guiding.pixel_scale(),
                    now,
                    wait.reply,
                    false,
                    wait.turn,
                ));
                {
                    let mut log = self.guide_log.lock();
                    log.note(&lock_note(reference));
                    log.note(SETTLING_STARTED);
                }
                let mut status = self.status.lock();
                status.interruption();
                status.set_state(State::Guiding);
                status.set_settling(true);
            }
            (Some(wait), Err(())) => {
                let _ = wait.reply.send(Err(ApiError::no_stars()));
            }
            (None, Ok(_)) => {
                guiding.controller.rehome(Offset::default());
                guiding.home = Offset::default();
                self.guide_log.lock().note(&lock_note(reference));
                self.status.lock().interruption();
            }
            (None, Err(())) => {}
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
        self.guide_log
            .lock()
            .note(if settling == Settling::Settled {
                "SETTLING STATE CHANGE, Settling complete"
            } else {
                "SETTLING STATE CHANGE, Settling failed"
            });
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
        match send_pulses(&guiding.mount, &mut [None, None], cancel).await {
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

/// A guide step's row for the guide log: the stars at `offset` with the
/// lock at `lock` on the sensor, the controller's `error` and the frame's
/// flux and signal-to-noise ratio, with no pulse until the step's pulses
/// have gone out.
fn guide_row(
    guiding: &Guiding,
    offset: Offset,
    lock: Offset,
    error: rp_guiding::AxisOffset,
    (flux, snr): (Option<f64>, Option<f64>),
) -> guide_log::GuideRow {
    guide_log::GuideRow {
        from_lock: Offset {
            x: offset.x - guiding.home.x - lock.x,
            y: offset.y - guiding.home.y - lock.y,
        },
        error,
        ra: None,
        dec: None,
        ra_rate_px_per_s: guiding
            .calibration
            .ra_rate_at(guiding.start.declination_deg),
        dec_rate_px_per_s: guiding
            .calibration
            .dec
            .as_ref()
            .map_or(0.0, |dec| dec.axis.rate_px_per_s),
        flux,
        snr,
    }
}

impl Guiding {
    /// The run's pixel scale, its calibration's.
    fn pixel_scale(&self) -> f64 {
        self.calibration.setup.pixel_scale_arcsec()
    }
}

/// `dither/abort` on the loop's `mode`, `status` and the requests in its
/// `inbox`: a dither still in the inbox is answered `dither_aborted` and
/// never moves the lock; one that waits to settle has the lock set to where
/// the stars are in the latest frame, and is answered `dither_aborted`.
pub(super) fn abort(
    mode: &mut super::Mode,
    status: &Mutex<Status>,
    inbox: &mut Vec<super::Command>,
    guide_log: &Mutex<GuideLog>,
    reference: Position,
) -> AbortAnswer {
    let state = status.lock().state();
    let unaborted = AbortAnswer {
        state,
        aborted: false,
    };
    let queued = inbox
        .iter()
        .position(|command| matches!(command, super::Command::Dither(_)));
    if let Some(at) = queued {
        if let super::Command::Dither(run) = inbox.remove(at) {
            let _ = run.reply.send(Err(ApiError::dither_aborted()));
        }
        info!("dither aborted before it moved the lock");
        return AbortAnswer {
            state,
            aborted: true,
        };
    }
    let super::Mode::Guiding(guiding) = mode else {
        return unaborted;
    };
    if !guiding.settle.as_ref().is_some_and(|wait| wait.dither) {
        return unaborted;
    }
    guiding
        .controller
        .lock_at(guiding.last_offset.unwrap_or_default());
    // The lock the controller holds, which the guide log's rows measure
    // from: with Dec off, the stars' place on the RA axis.
    let lock = guide_log::to_sensor(&guiding.calibration, guiding.controller.lock());
    guide_log.lock().note(&lock_note(Position {
        x: reference.x + guiding.home.x + lock.x,
        y: reference.y + guiding.home.y + lock.y,
    }));
    if let Some(wait) = guiding.settle.take() {
        let _ = wait.reply.send(Err(ApiError::dither_aborted()));
    }
    status.lock().set_settling(false);
    info!("dither aborted");
    AbortAnswer {
        state,
        aborted: true,
    }
}

/// Send `pulses`, RA then Dec, each after the mount check raced against
/// `cancel`; with none to send, check the mount once. The message of the
/// first failed mount call, after which no further pulse goes out; the stop
/// when the run stops. `pulses` is left holding the pulses that went out:
/// each `PulseGuide` asked for and not refused.
pub(super) async fn send_pulses(
    mount: &RunMount,
    pulses: &mut [Option<rp_guiding::Pulse>; 2],
    cancel: &CancellationToken,
) -> Result<Option<String>, Stop> {
    let mut asked = false;
    let mut ended = None;
    for slot in pulses.iter_mut() {
        let Some(pulse) = *slot else {
            continue;
        };
        asked = true;
        if ended.is_some() {
            *slot = None;
            continue;
        }
        let (went_out, end) = send_pulse(mount, pulse, cancel).await;
        if !went_out {
            *slot = None;
        }
        ended = end;
    }
    if let Some(end) = ended {
        return end;
    }
    if asked {
        return Ok(None);
    }
    let checked = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Stop::requested()),
        checked = mount.check() => checked,
    };
    checked.map_or_else(fault_end, |()| Ok(None))
}

/// Send `pulse` after the mount check raced against `cancel`: whether it
/// went out, and how the cycle ends, if it ends with this pulse.
async fn send_pulse(
    mount: &RunMount,
    pulse: rp_guiding::Pulse,
    cancel: &CancellationToken,
) -> (bool, Option<Result<Option<String>, Stop>>) {
    let checked = tokio::select! {
        biased;
        () = cancel.cancelled() => return (false, Some(Err(Stop::requested()))),
        checked = mount.check() => checked,
    };
    let fault = match checked {
        Err(fault) => return (false, Some(fault_end(fault))),
        Ok(()) if cancel.is_cancelled() => return (false, Some(Err(Stop::requested()))),
        Ok(()) => mount.pulse(pulse).await.err(),
    };
    match fault {
        None => (true, None),
        Some(Fault::Failed(message)) => (false, Some(Ok(Some(message)))),
        // `PulseGuide` took the pulse; only its end was not seen.
        Some(Fault::Unconfirmed(message)) => (true, Some(Ok(Some(message)))),
        Some(Fault::Stop(stop)) => (true, Some(Err(stop))),
    }
}

/// How a cycle ends on a mount `fault`: the stop, or the failed call's
/// message.
fn fault_end(fault: Fault) -> Result<Option<String>, Stop> {
    match fault {
        Fault::Stop(stop) => Err(stop),
        Fault::Failed(message) | Fault::Unconfirmed(message) => Ok(Some(message)),
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
            pointing: None,
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
    async fn a_failed_frame_leaves_the_settle_time_running() {
        let (mut cycle, mut answer, _turns) = guiding_on(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        tokio::time::advance(Duration::from_millis(600)).await;
        let failed = Took::Failed("no frame".to_owned());
        assert!(guide(&mut cycle, failed).await.is_none());
        assert_eq!(state(&cycle), State::Guiding);
        assert_eq!(cycle.status.lock().totals().star_lost, 0);
        tokio::time::advance(Duration::from_millis(600)).await;
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        assert_eq!(answer.try_recv().unwrap().unwrap().state, State::Guiding);
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
        let mut frames = 0;
        for x in 2..10 {
            frames += 1;
            end = guide(&mut cycle, at(f64::from(x), 0.0)).await;
            if end.is_some() {
                break;
            }
        }
        let Some(End::Stop(stop)) = end else {
            panic!("the run went on");
        };
        // The frame whose step ran away is a guide step too.
        assert_eq!(cycle.status.lock().totals().guide_steps, frames);
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
        assert!(
            guide_log_text(&cycle)
                .contains("INFO: SET LOCK POSITION, new lock pos = 0.250, -0.500"),
            "{}",
            guide_log_text(&cycle)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_dec_off_an_abort_notes_the_lock_on_the_ra_axis_the_rows_measure_from() {
        let mut cycle = looping_cycle();
        cycle.config = Arc::new(
            serde_json::from_value(serde_json::json!({
                "optics": { "focal_length_mm": 200.0 },
                "control": { "dec": { "mode": "off" } }
            }))
            .unwrap(),
        );
        let calibration = calibration(&cycle);
        let (after, mut started) = after(Found::Looping);
        let (turn, _turns) = turn().await;
        cycle.begin_guiding(
            run_mount(Arc::new(FakeMount::default())),
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
        let (run, _answer, _given_up) = dither_run(4.0);
        cycle.dither(run);
        assert!(guide(&mut cycle, at(0.25, -0.5)).await.is_none());
        assert!(cycle.abort_dither().aborted);
        // West moves the stars along +x: the lock is where they stand along
        // that axis.
        let text = guide_log_text(&cycle);
        assert!(
            text.contains("INFO: SET LOCK POSITION, new lock pos = 0.250, 0.000"),
            "{text}"
        );
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
        let mut pulses = [Some(pulse(Direction::West)), None];
        let stop = send_pulses(&run_mount(Arc::clone(&mount)), &mut pulses, &cancel)
            .await
            .unwrap_err();
        assert_eq!(stop.reason, StopReason::Requested);
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 0);
        assert_eq!(pulses, [None, None]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_answers_a_dither_the_loop_has_not_taken_up_and_leaves_the_lock() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, answer, given_up) = dither_run(4.0);
        cycle
            .inbox
            .push(super::super::Command::Dither(Box::new(run)));
        let aborted = cycle.abort_dither();
        assert!(aborted.aborted);
        assert_eq!(
            answer.await.unwrap().unwrap_err().code,
            ErrorCode::DitherAborted
        );
        given_up.await.unwrap();
        assert!(cycle.inbox.is_empty());
        assert_eq!(
            guiding(&cycle).controller.lock(),
            rp_guiding::AxisOffset::default()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_is_served_while_a_frame_is_still_exposing() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, dithered, _given_up) = dither_run(4.0);
        cycle.dither(run);
        cycle.exposer.camera = Arc::new(crate::test_camera::FakeCamera {
            start_hangs: true,
            ..crate::test_camera::FakeCamera::default()
        });
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (reply, abort) = oneshot::channel();
        commands
            .send(super::super::Command::AbortDither(reply))
            .unwrap();
        let cancel = CancellationToken::new();
        let aborted = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = cycle.frame(&cancel, &mut received) => panic!("the frame ended"),
                aborted = abort => aborted.unwrap(),
            }
        })
        .await
        .expect("the abort waited for the frame");
        assert!(aborted.aborted);
        assert_eq!(
            dithered.await.unwrap().unwrap_err().code,
            ErrorCode::DitherAborted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_waiting_for_its_fresh_frame_checks_the_mount() {
        let mut cycle = looping_cycle();
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: calibration(&cycle),
            })
            .unwrap();
        let mount = Arc::new(FakeMount {
            slewing: true,
            ..FakeMount::default()
        });
        let (run, _answer, _turns) = start_from_outside(mount, false).await;
        cycle.begin_start(run, Found::Looping);
        let failed = Took::Failed("no frame".to_owned());
        let Some(End::Stop(stop)) = cycle
            .starting_frame(&failed, &CancellationToken::new())
            .await
        else {
            panic!("the start went on");
        };
        assert_eq!(stop.reason, StopReason::MountSlewing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_waiting_for_its_fresh_frame_counts_the_mount_check_failures() {
        let mut cycle = looping_cycle();
        cycle
            .store
            .put(Record {
                calibrated_at: SystemTime::now(),
                calibration: calibration(&cycle),
            })
            .unwrap();
        let mount = Arc::new(FakeMount {
            tracking_fails: true,
            ..FakeMount::default()
        });
        let (run, _answer, _turns) = start_from_outside(mount, false).await;
        cycle.begin_start(run, Found::Looping);
        let max = cycle.config.mount.max_failures.get();
        let cancel = CancellationToken::new();
        for _ in 1..max {
            let failed = Took::Failed("no frame".to_owned());
            assert!(cycle.starting_frame(&failed, &cancel).await.is_none());
        }
        let failed = Took::Failed("no frame".to_owned());
        let Some(End::Stop(stop)) = cycle.starting_frame(&failed, &cancel).await else {
            panic!("the start went on");
        };
        assert_eq!(stop.reason, StopReason::MountUnreachable);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_sets_the_lock_where_a_paused_frame_saw_the_stars() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        cycle.pause(false).unwrap();
        assert!(guide(&mut cycle, at(3.0, 1.0)).await.is_none());
        cycle.resume().unwrap();
        let (run, _answer, _given_up) = dither_run(4.0);
        cycle.dither(run);
        assert!(cycle.abort_dither().aborted);
        let lock = guiding(&cycle).controller.lock();
        assert!(
            (lock.ra - 3.0).abs() < 1e-9 && (lock.dec - 1.0).abs() < 1e-9,
            "{lock:?}"
        );
    }

    /// The guide log the loop wrote so far.
    fn guide_log_text(cycle: &Cycle) -> String {
        let path = cycle
            .guide_log
            .lock()
            .path()
            .expect("a guide log is open")
            .to_owned();
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_a_run_ends_the_runs_section_where_the_calibrations_begins() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        assert!(!guide_log_text(&cycle).contains("Guiding Ends"));
        // The test's loop holds no guide stars, so the calibration selects
        // on its first frame.
        assert!(cycle
            .after_frame(selection(Ok(3)), &CancellationToken::new())
            .await
            .is_none());
        let text = guide_log_text(&cycle);
        let ends = text
            .find("Guiding Ends at")
            .expect("the run's section ended");
        let calibration = text
            .find("Calibration Begins at")
            .expect("the calibration's section");
        assert!(ends < calibration, "{text}");
    }

    /// The guide log of a run that calibrates again and stops with `stop`
    /// before the calibration's first frame.
    async fn stopped_before_the_calibration_began(stop: &Stop) -> String {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        cycle.log_stop(stop);
        guide_log_text(&cycle)
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_stops_before_its_calibration_begins_ends_its_section_with_the_stop() {
        for (stop, note) in [
            (Stop::requested(), "INFO: Server received guiding/stop"),
            (
                Stop::new(StopReason::MountSlewing, "the mount is slewing"),
                "INFO: GUIDING STOPPED, mount_slewing: the mount is slewing",
            ),
        ] {
            let text = stopped_before_the_calibration_began(&stop).await;
            let noted = text.find(note).expect("the stop's note");
            let ends = text
                .find("Guiding Ends at")
                .expect("the run's section ended");
            assert!(noted < ends, "{text}");
            assert!(!text.contains("Calibration Begins"), "{text}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_star_lost_refused_for_too_few_stars_goes_on_logging_the_run() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        let refused = Took::Measured {
            offset: None,
            selection: Some(Err(())),
        };
        assert!(cycle
            .after_frame(refused, &CancellationToken::new())
            .await
            .is_none());
        assert!(guide(&mut cycle, at(0.25, 0.0)).await.is_none());
        let text = guide_log_text(&cycle);
        assert!(!text.contains("Guiding Ends"), "{text}");
        assert!(
            text.lines().last().unwrap().contains(",\"Mount\","),
            "{text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_star_lost_ends_the_runs_section_once_its_fresh_frame_selects() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        assert!(!guide_log_text(&cycle).contains("Guiding Ends"));
        assert!(cycle
            .after_frame(selection(Ok(3)), &CancellationToken::new())
            .await
            .is_none());
        let text = guide_log_text(&cycle);
        let ends = text.find("Guiding Ends").expect("the run's section ended");
        let calibration = text
            .find("Calibration Begins")
            .expect("the calibration's section");
        assert!(ends < calibration, "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dither_note_gives_the_locks_move_on_the_axes_as_the_next_rows_raw_distances_turned_around(
    ) {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, _answer, _given_up) = dither_run(4.0);
        cycle.dither(run);
        assert!(guide(&mut cycle, at(0.0, 0.0)).await.is_none());
        let text = guide_log_text(&cycle);
        let note = text
            .lines()
            .find_map(|line| line.strip_prefix("INFO: DITHER by "))
            .expect("a dither note");
        let by: Vec<f64> = note
            .split(", ")
            .take(2)
            .map(|amount| amount.parse().unwrap())
            .collect();
        let row = text.lines().last().unwrap();
        let fields: Vec<&str> = row.split(',').collect();
        let raw: Vec<f64> = fields[5..7]
            .iter()
            .map(|field| field.parse().unwrap())
            .collect();
        assert!(by[0].abs() + by[1].abs() > 1.0, "{note}");
        assert!(
            (by[0] + raw[0]).abs() < 0.002 && (by[1] + raw[1]).abs() < 0.002,
            "{note} / {row}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_settles_note_follows_the_row_of_the_frame_that_settled() {
        let (mut cycle, mut answer, _turns) = guiding_on(Arc::new(FakeMount::default())).await;
        for _ in 0..3 {
            assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
            tokio::time::advance(Duration::from_millis(600)).await;
        }
        answer.try_recv().unwrap().unwrap();
        let text = guide_log_text(&cycle);
        let lines: Vec<&str> = text.lines().collect();
        let [.., row, note] = lines[..] else {
            panic!("{text}");
        };
        assert_eq!(
            note, "INFO: SETTLING STATE CHANGE, Settling complete",
            "{text}"
        );
        assert!(row.contains(",\"Mount\","), "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_guide_steps_row_gives_no_pulse_the_mount_refused() {
        let mount = Arc::new(FakeMount {
            pulse_refused: true,
            ..FakeMount::default()
        });
        let mut cycle = settled_guiding(mount).await;
        assert!(guide(&mut cycle, at(0.5, 0.0)).await.is_none());
        let text = guide_log_text(&cycle);
        let row = text.lines().last().unwrap();
        let fields: Vec<&str> = row.split(',').collect();
        // RAGuideDistance, RADuration and RADirection.
        assert_eq!(
            (fields[7], fields[9], fields[10]),
            ("0.000", "0", ""),
            "{row}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_of_its_own_in_a_recalibration_still_in_the_runs_section_is_a_runs_note() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        cycle.log_stop(&Stop::new(StopReason::MountSlewing, "the mount is slewing"));
        let text = guide_log_text(&cycle);
        assert!(
            text.contains("INFO: GUIDING STOPPED, mount_slewing: the mount is slewing"),
            "{text}"
        );
        assert!(text.contains("Guiding Ends at"), "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_of_its_own_in_a_calibration_is_a_calibrations_note() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        // The test's loop holds no guide stars, so the calibration selects
        // on its first frame.
        assert!(cycle
            .after_frame(selection(Ok(3)), &CancellationToken::new())
            .await
            .is_none());
        cycle.log_stop(&Stop::new(StopReason::MountSlewing, "the mount is slewing"));
        let text = guide_log_text(&cycle);
        assert!(
            text.contains("INFO: CALIBRATION STOPPED, mount_slewing: the mount is slewing"),
            "{text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_reselect_the_guide_logs_rows_measure_from_the_new_stars() {
        let home = Offset { x: 2.0, y: 1.0 };
        let mut cycle = settled_guiding_from(Arc::new(FakeMount::default()), home).await;
        assert!(guide(&mut cycle, selection(Ok(3))).await.is_none());
        assert!(guide(&mut cycle, at(0.25, 0.0)).await.is_none());
        let text = guide_log_text(&cycle);
        let row = text.lines().last().unwrap();
        let fields: Vec<&str> = row.split(',').collect();
        assert_eq!((fields[3], fields[4]), ("0.250", "0.000"), "{row}");
        assert!(
            text.contains("INFO: SET LOCK POSITION, new lock pos = "),
            "{text}"
        );
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
        let text = guide_log_text(&cycle);
        let [.., paused, started, settling] = text.lines().collect::<Vec<_>>()[..] else {
            panic!("{text}");
        };
        assert_eq!(
            (paused, started, settling),
            (
                "INFO: Server received guiding/pause, full = true",
                "INFO: Server received guiding/start",
                "INFO: SETTLING STATE CHANGE, Settling started"
            ),
            "{text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_in_star_lost_selects_anew_on_the_next_frame_and_takes_a_new_lock() {
        let home = Offset { x: 2.0, y: 1.0 };
        let mut cycle = settled_guiding_from(Arc::new(FakeMount::default()), home).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(false).await;
        cycle.start(run);
        assert!(cycle.select_next);
        assert_eq!(cycle.status.lock().totals().interruptions, 0);
        assert!(guide(&mut cycle, selection(Ok(3))).await.is_none());
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
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
        // The run kept its stars: it was not interrupted.
        assert_eq!(cycle.status.lock().totals().interruptions, 0);
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
        assert_eq!(cycle.status.lock().totals().interruptions, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recalibrate_in_star_lost_counts_its_interruption_once_its_fresh_frame_selects() {
        let mut cycle = settled_guiding(Arc::new(FakeMount::default())).await;
        assert!(guide(&mut cycle, lost()).await.is_none());
        let (run, _answer, _turns) = start_in_a_run(true).await;
        cycle.start(run);
        assert_eq!(cycle.status.lock().totals().interruptions, 0);
        let end = cycle
            .after_frame(selection(Ok(3)), &CancellationToken::new())
            .await;
        assert!(end.is_none());
        assert_eq!(cycle.status.lock().totals().interruptions, 1);
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
        cycle.status.lock().set_state(State::Looping);
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
        assert_eq!(state(&cycle), State::Looping);
        assert!(cycle
            .starting_frame(&selection(Ok(3)), &CancellationToken::new())
            .await
            .is_none());
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
        assert!(cycle
            .starting_frame(&selection(Err(())), &CancellationToken::new())
            .await
            .is_none());
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
        let end = cycle.start_refused(after, turn, ApiError::no_stars());
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
        let both = [Some(pulse(Direction::West)), Some(pulse(Direction::North))];
        let mut pulses = both;
        let sent = send_pulses(
            &run_mount(Arc::clone(&mount)),
            &mut pulses,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(sent.unwrap(), None);
        assert_eq!(
            *mount.directions.lock(),
            [GuideDirection::West, GuideDirection::North]
        );
        assert_eq!(pulses, both);
    }

    #[tokio::test(start_paused = true)]
    async fn no_pulse_goes_out_after_one_that_failed() {
        let mount = Arc::new(FakeMount {
            pulse_refused: true,
            ..FakeMount::default()
        });
        let mut pulses = [Some(pulse(Direction::West)), Some(pulse(Direction::North))];
        let sent = send_pulses(
            &run_mount(Arc::clone(&mount)),
            &mut pulses,
            &CancellationToken::new(),
        )
        .await;
        assert!(sent.unwrap().is_some(), "the failure was not reported");
        assert_eq!(*mount.directions.lock(), [GuideDirection::West]);
        assert_eq!(pulses, [None, None], "a refused pulse went out");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_the_mount_check_held_back_did_not_go_out() {
        let mount = Arc::new(FakeMount {
            at_park: true,
            ..FakeMount::default()
        });
        let mut pulses = [Some(pulse(Direction::West)), None];
        let stop = send_pulses(&run_mount(mount), &mut pulses, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(stop.reason, StopReason::MountParked);
        assert_eq!(pulses, [None, None]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_whose_end_could_not_be_read_went_out_and_the_next_did_not() {
        let mount = Arc::new(FakeMount {
            pulse_guiding_fails: true,
            ..FakeMount::default()
        });
        let mut pulses = [Some(pulse(Direction::West)), Some(pulse(Direction::North))];
        let sent = send_pulses(
            &run_mount(Arc::clone(&mount)),
            &mut pulses,
            &CancellationToken::new(),
        )
        .await;
        assert!(sent.unwrap().is_some(), "the failure was not reported");
        assert_eq!(pulses, [Some(pulse(Direction::West)), None]);
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_pulse_to_send_the_mount_is_still_checked() {
        let mount = Arc::new(FakeMount {
            at_park: true,
            ..FakeMount::default()
        });
        let stop = send_pulses(
            &run_mount(mount),
            &mut [None, None],
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(stop.reason, StopReason::MountParked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_before_the_check_sends_no_pulse() {
        let mount = Arc::new(FakeMount::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut pulses = [Some(pulse(Direction::West)), None];
        let stop = send_pulses(&run_mount(Arc::clone(&mount)), &mut pulses, &cancel)
            .await
            .unwrap_err();
        assert_eq!(stop.reason, StopReason::Requested);
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 0);
        assert_eq!(pulses, [None, None]);
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

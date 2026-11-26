//! The camera loop.
//!
//! While the guider is not idle it takes one frame after another, measures
//! the guide stars in each and keeps what they tell for `stats`, `metrics`
//! and `frame/latest` (docs/services/guider.md § The camera loop, § Sensor
//! handling, § Star selection and measurement).
//!
//! A cycle starts the exposure, sleeps through it, polls `ImageReady`,
//! fetches the frame and measures it. A frame fails when a camera call
//! fails, when it has not arrived `camera.frame_timeout` after its exposure
//! should have ended, or when its dimensions are not the ones asked for.
//! After every failure the loop reads the camera's static properties again
//! and writes the frame settings again; `camera.max_frame_failures`
//! failures in a row stop it with `camera_error`, and so does a camera that
//! comes back with another sensor. The loop ends its own exposure when it
//! stops: an abort when the camera can abort, a wait for its end
//! otherwise.
//!
//! A calibration runs in the loop (§ Calibration): after each frame the
//! calibrator says what comes next, and the loop checks the mount and sends
//! the pulse before the next exposure starts, so that no pulse overlaps an
//! exposure. A step whose pulse was not sent is asked for again. Guiding
//! runs in the loop the same way (`guiding`).
//!
//! The requests the loop hears during a frame are served after it, from
//! the next frame on, so that a start selects on a frame exposed after it
//! was taken up. A `dither/abort` is served as it arrives during a frame,
//! and before the next frame otherwise.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::ImageArray;
use ascom_alpaca::api::{Camera, Telescope};
use ndarray::Axis;
use parking_lot::Mutex;
use rp_guiding::{
    CalibrationParams, Calibrator, CameraGeometry, Frame, GuideStars, Offset, Progress, Setup,
    StarParams,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::api::{ApiError, Stop, StopReason};
use crate::calibration_store::{CalibrationStore, Record};
use crate::config::Config;
use crate::devices::{
    call, camera_sensor, CallError, CameraKind, CameraSensor, DeviceClient, DevicesState, Found,
};
use crate::guide_log::{self, GuideLog};
use crate::guider::{AbortAnswer, FrameSummary, State, Status, Turn};
use crate::mount::{Fault, MountStart, RunMount};
use crate::sensor::{changed, effective_binning, FrameSettings, Reading};
pub use guiding::{DitherRun, GuidingRun, SettledReply};
use guiding::{Guiding, StartAfter, Starting};

mod guiding;

/// How often `ImageReady` is read once the exposure should have ended.
const READY_POLL: Duration = Duration::from_millis(50);

/// What a loop starts with: the camera as the request that started it
/// found it, and where it reports.
pub struct LoopSetup {
    pub config: Arc<Config>,
    pub client: Arc<DeviceClient>,
    pub camera: Arc<dyn Camera>,
    pub sensor: CameraSensor,
    pub reading: Reading,
    pub status: Arc<Mutex<Status>>,
    pub devices: watch::Receiver<DevicesState>,
    pub store: Arc<CalibrationStore>,
    pub guide_log: Arc<Mutex<GuideLog>>,
    /// Answered with the guide stars of the first measured frame, for a
    /// `loop`.
    pub first: Option<oneshot::Sender<usize>>,
    /// The calibration the loop starts with, for a `calibrate` from `idle`.
    pub calibration: Option<CalibrationRun>,
    /// The start the loop begins with, for a `guiding/start` from `idle`.
    pub start: Option<GuidingRun>,
}

/// The state a calibration returns to: the one its request found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Return {
    /// The loop ends with the calibration.
    Idle,
    /// The loop goes on.
    Looping,
}

/// A calibration that passed, and the state the guider is in after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibrated {
    pub state: State,
    pub record: Record,
}

/// A calibration's answer.
pub type CalibrationReply = oneshot::Sender<Result<Calibrated, ApiError>>;

/// What a `calibrate` hands the loop.
pub struct CalibrationRun {
    pub mount: Arc<dyn Telescope>,
    /// What the request read of the mount.
    pub start: MountStart,
    pub reply: CalibrationReply,
    pub return_to: Return,
    /// The request's turn, held until the calibration ends.
    pub turn: Turn,
}

/// A request a running loop serves.
pub enum Command {
    /// Select the guide stars anew on the next frame, answering with how
    /// many were selected, or `no_stars`.
    Reselect(ReselectReply),
    /// Calibrate, from `looping`.
    Calibrate(Box<CalibrationRun>),
    /// Start guiding, from `looping` or in a run.
    Start(Box<GuidingRun>),
    /// Pause the corrections, and with `full` the exposures.
    Pause {
        full: bool,
        reply: oneshot::Sender<Result<State, ApiError>>,
    },
    /// Resume the corrections.
    Resume(oneshot::Sender<Result<State, ApiError>>),
    /// Move the lock.
    Dither(Box<DitherRun>),
    /// End a dither that waits to settle.
    AbortDither(oneshot::Sender<AbortAnswer>),
}

/// The star settings of the configuration, as the core takes them.
fn star_params(config: &Config) -> StarParams {
    StarParams {
        max: usize::try_from(config.stars.max).unwrap_or(usize::MAX),
        min: usize::try_from(config.stars.min.get()).unwrap_or(usize::MAX),
        search_region_px: config.stars.search_region_px.get(),
        min_snr: config.stars.min_snr.get(),
        mass_change: config.stars.mass_change.get(),
        calibration_distance_px: config.calibration.distance_px.get(),
    }
}

/// The calibration settings of the configuration, as the core takes them.
fn calibration_params(config: &Config) -> CalibrationParams {
    CalibrationParams {
        distance_px: config.calibration.distance_px.get(),
        steps: config.calibration.steps,
        max_steps: config.calibration.max_steps,
        backlash_clear_px: config.calibration.backlash_clear_px.get(),
        fallback_step: config.calibration.fallback_step.get(),
        timeout: config.calibration.timeout.get(),
        min_pulse: config.mount.min_pulse.get(),
        max_pulse: config.mount.max_pulse.get(),
        dec: config.control.dec.mode != crate::config::DecMode::Off,
    }
}

/// A reselect's answer.
type ReselectReply = oneshot::Sender<Result<usize, ApiError>>;

/// How the loop ends.
enum End {
    /// It stopped: the stop is recorded.
    Stop(Stop),
    /// A run a request found in `idle` ended without a stop: the request is
    /// answered once the guider is idle again.
    Ended(Answer),
}

/// The answer of a request whose run ended the loop without a stop.
enum Answer {
    /// A calibration's.
    Calibration(CalibrationReply, Result<Record, ApiError>),
    /// A start's that did not guide.
    Refused(SettledReply, ApiError),
}

/// What the loop does with its frames.
enum Mode {
    /// It measures them.
    Looping,
    /// It calibrates on them.
    Calibrating(Box<Calibrating>),
    /// A start waits for its fresh selection.
    Starting(Box<Starting>),
    /// It guides on them.
    Guiding(Box<Guiding>),
}

/// What a calibration leads to.
enum Then {
    /// A `calibrate`'s answer, and the state its request found.
    Answer {
        reply: CalibrationReply,
        return_to: Return,
    },
    /// A `guiding/start`'s guiding.
    Guide(StartAfter),
}

/// A calibration in the loop.
struct Calibrating {
    /// The calibration's steps, taken out while a frame is judged.
    calibrator: Option<Calibrator>,
    mount: RunMount,
    /// What the request read of the mount.
    start: MountStart,
    /// Whether the guide stars are still to be selected: on the first frame
    /// from `idle`, on the next from a loop that holds none, and on a fresh
    /// frame for a start.
    selecting: bool,
    /// Whether the guide log's section has begun, which it does on the
    /// first frame with an offset after the selection.
    logged: bool,
    /// The request's turn, held until the guider is back in the state the
    /// request found, or has settled.
    turn: Turn,
    then: Then,
}

/// What a calibration's selection makes of a frame.
enum Selection {
    /// The frame goes on to the calibrator.
    Steps,
    /// The calibration ended on it, the loop going on or ending.
    Ended(Option<Box<End>>),
}

/// A calibration's frame as the guide log takes it: whether it was
/// measured, its offset, and what the calibrator made of it.
struct Logged<'a> {
    measured: bool,
    offset: Option<Offset>,
    progress: Option<&'a Progress>,
}

/// What a cycle's frame gave.
enum Took {
    /// A measured frame: its offset, and when it was a selection, how many
    /// guide stars it selected, or that it had too few.
    Measured {
        offset: Option<Offset>,
        selection: Option<Result<usize, ()>>,
    },
    /// A frame that failed.
    Failed(String),
}

/// Run the loop until `cancel` is cancelled or it stops by itself, then
/// end its exposure and report how it ended.
pub async fn run(
    setup: LoopSetup,
    cancel: CancellationToken,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let LoopSetup {
        config,
        client,
        camera,
        sensor,
        reading,
        status,
        devices,
        store,
        guide_log,
        first,
        calibration,
        start,
    } = setup;
    let mut cycle = Cycle {
        params: star_params(&config),
        max_failures: config.camera.max_frame_failures.get(),
        exposer: Exposer {
            camera,
            exposure: config.camera.exposure.get(),
            frame_timeout: config.camera.frame_timeout.get(),
            settings: FrameSettings::new(&sensor, reading, &config),
            in_flight: None,
        },
        client,
        sensor,
        reading,
        devices,
        status,
        store,
        guide_log,
        last_summary: None,
        config: Arc::clone(&config),
        epoch: Instant::now(),
        mode: Mode::Looping,
        ending_turn: None,
        first,
        reselect: None,
        inbox: Vec::new(),
        select_next: false,
        rng: rand::make_rng(),
        stars: None,
        failures: 0,
        settings_due: Some(Settings::Start),
        attempt_started: Instant::now(),
        retry_at: None,
    };
    info!(
        exposure = %humantime::format_duration(cycle.exposer.exposure),
        bin = cycle.exposer.settings.bin,
        "the camera loop started"
    );
    if let Some(run) = calibration {
        cycle.calibrate(run);
    }
    if let Some(run) = start {
        cycle.begin_start(run, guiding::Found::Idle);
    }
    let end = loop {
        if let Some(end) = cycle.turn(&cancel, &mut commands).await {
            break end;
        }
    };
    if let End::Stop(stop) = &end {
        cycle.log_stop(stop);
    }
    cycle
        .exposer
        .end_exposure(cycle.sensor.can_abort_exposure)
        .await;
    match end {
        End::Stop(stop) => {
            info!(reason = stop.reason.as_str(), message = %stop.message, "the camera loop stopped");
            cycle.status.lock().stopped(stop);
        }
        End::Ended(answer) => {
            info!("the camera loop ended with its run");
            cycle.status.lock().idle();
            drop(cycle.ending_turn.take());
            match answer {
                Answer::Calibration(reply, outcome) => {
                    let _ = reply.send(outcome.map(|record| Calibrated {
                        state: State::Idle,
                        record,
                    }));
                }
                Answer::Refused(reply, error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
    }
    // A run a stop ended gives up its turn here, once the guider is idle,
    // and a request that waited on it hears the stop.
    drop(cycle);
}

/// The loop between its cycles.
struct Cycle {
    params: StarParams,
    max_failures: u32,
    exposer: Exposer,
    client: Arc<DeviceClient>,
    /// The camera's static properties: the sensor the loop started on, with
    /// what else was read of it since.
    sensor: CameraSensor,
    reading: Reading,
    devices: watch::Receiver<DevicesState>,
    status: Arc<Mutex<Status>>,
    store: Arc<CalibrationStore>,
    guide_log: Arc<Mutex<GuideLog>>,
    /// What the latest measured frame gave, for the guide log's rows.
    last_summary: Option<FrameSummary>,
    config: Arc<Config>,
    /// The start of the loop's clock, which a calibration's timeout runs
    /// on.
    epoch: Instant,
    mode: Mode,
    /// The turn of a run that ends the loop, given up once the guider is
    /// idle again.
    ending_turn: Option<Turn>,
    first: Option<oneshot::Sender<usize>>,
    reselect: Option<ReselectReply>,
    /// The requests heard during a frame, served after it. Those of a loop
    /// that stops first drop with the loop once its stop is recorded.
    inbox: Vec<Command>,
    /// Whether the next measured frame selects the guide stars anew, for a
    /// start.
    select_next: bool,
    /// The draws of dithers.
    rng: rand::rngs::StdRng,
    stars: Option<GuideStars>,
    /// Frames failed in a row.
    failures: u32,
    settings_due: Option<Settings>,
    /// When the current attempt at a frame began, and when the next may
    /// begin after a failed one.
    attempt_started: Instant,
    retry_at: Option<Instant>,
}

impl Cycle {
    /// One cycle: the frame settings when they are due, a frame and its
    /// measurement, and in a calibration what comes after it. How the loop
    /// ends, when it does.
    async fn turn(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Option<End> {
        if let Some(at) = self.retry_at.take() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Some(End::Stop(Stop::requested())),
                () = tokio::time::sleep_until(at) => {}
            }
        }
        if self.full_paused() {
            return self.full_pause(cancel, commands).await;
        }
        self.attempt_started = Instant::now();
        let took = match self.take(cancel, commands).await {
            Ok(took) => took,
            Err(stop) => return Some(End::Stop(stop)),
        };
        if let Took::Failed(message) = &took {
            if let Some(stop) = self.failed(message) {
                return Some(End::Stop(stop));
            }
        }
        let end = self.after_frame(took, cancel).await;
        if end.is_none() {
            self.serve_inbox();
        }
        end
    }

    /// Serve the requests heard during the frame, from the next frame on:
    /// a start's fresh frame is one exposed after the loop took it up.
    fn serve_inbox(&mut self) {
        for command in std::mem::take(&mut self.inbox) {
            self.handle(command);
        }
    }

    /// The frame settings when they are due, then a frame and its
    /// measurement; the stop when the loop stops before they are done.
    async fn take(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<Took, Stop> {
        if let Some(due) = self.settings_due {
            let written = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Stop::requested()),
                written = write_settings(due, &self.client, &mut self.sensor, &mut self.exposer) => written,
            };
            match written {
                Ok(()) => self.settings_due = None,
                Err(Rewrite::Changed(message)) => return Err(Stop::camera_error(message)),
                Err(Rewrite::Failed(message)) => return Ok(Took::Failed(message)),
            }
        }
        match self.frame(cancel, commands).await? {
            Err(message) => Ok(Took::Failed(message)),
            Ok(image) => self.measure(image, cancel).await,
        }
    }

    /// A frame, while requests and the device poll are heard; the stop when
    /// the loop ends before it arrives.
    async fn frame(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<Result<ImageArray, String>, Stop> {
        let taken = {
            let reference = self.star_at(Offset::default());
            let Self {
                exposer,
                devices,
                sensor,
                inbox,
                mode,
                status,
                guide_log,
                ..
            } = self;
            let frame = exposer.frame();
            tokio::pin!(frame);
            let taken = loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(Stop::requested()),
                    Some(command) = commands.recv() => match command {
                        // An abort never queues: it acts as it arrives.
                        Command::AbortDither(reply) => {
                            let _ = reply.send(guiding::abort(
                                mode, status, inbox, guide_log, reference,
                            ));
                        }
                        command => inbox.push(command),
                    },
                    Ok(()) = devices.changed() => {
                        if let Some(message) = came_back_changed(devices, sensor) {
                            return Err(Stop::camera_error(message));
                        }
                    }
                    result = &mut frame => break result,
                }
            };
            taken
        };
        Ok(taken)
    }

    /// Measure `image` off the async workers, answer the requests that
    /// waited on it and record it.
    async fn measure(
        &mut self,
        image: ImageArray,
        cancel: &CancellationToken,
    ) -> Result<Took, Stop> {
        let reselecting = self.reselect.is_some();
        let job = Job {
            stars: self.stars.take(),
            params: self.params,
            reading: self.reading.sensor,
            max_adu: self.sensor.max_adu,
            reselect: reselecting || self.select_next,
        };
        let measured = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Stop::requested()),
            measured = tokio::task::spawn_blocking(move || job.run(&image)) => measured,
        };
        let Measured {
            stars,
            summary,
            offset,
            selection,
            frame,
        } = match measured {
            Ok(Ok(measured)) => measured,
            Ok(Err(message)) => return Ok(Took::Failed(message)),
            // A panic of the core's ends the loop's task without a stop, as
            // the failure domains of guider.md say.
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => return Err(Stop::requested()),
        };
        self.failures = 0;
        if selection.is_some() {
            self.select_next = false;
        }
        let centres = stars
            .as_ref()
            .map_or_else(Vec::new, GuideStars::search_centres);
        self.stars = stars;
        // Recorded before the answers go out, so that a client that reads
        // stats or the latest frame on its answer finds the frame there.
        self.last_summary = Some(summary);
        self.status.lock().record(summary, frame, centres);
        // A request that is gone has nothing to hear.
        if let Some(first) = self.first.take() {
            let _ = first.send(selection.unwrap_or(Ok(0)).unwrap_or(0));
        }
        if reselecting {
            if let (Some(reply), Some(selection)) = (self.reselect.take(), selection) {
                let _ = reply.send(selection.map_err(|()| ApiError::no_stars()));
            }
        }
        Ok(Took::Measured { offset, selection })
    }

    /// Take up `run`: the loop calibrates from its next frame on.
    fn calibrate(&mut self, run: CalibrationRun) {
        let CalibrationRun {
            mount,
            start,
            reply,
            return_to,
            turn,
        } = run;
        self.calibrate_with(mount, start, turn, Then::Answer { reply, return_to }, false);
    }

    /// What a run on `start` is made on: the pier side and the guide rates
    /// the request read, the loop's camera and the configured focal length.
    fn setup(&self, start: &MountStart) -> Setup {
        let binning =
            effective_binning(self.sensor.sensor_type, self.reading.bin).map_or(1, u32::from);
        Setup {
            pier_side: start.side,
            camera: CameraGeometry {
                pixel_size_um: self.sensor.pixel_size_um,
                width: self.sensor.width,
                height: self.sensor.height,
                binning,
            },
            focal_length_mm: self
                .config
                .optics
                .focal_length_mm
                .map_or(f64::NAN, crate::config::FocalLength::get),
            guide_rates: start.guide_rates,
        }
    }

    /// Calibrate from the next frame on, then `then`; on a fresh selection
    /// when `reselect`, or when the loop holds no guide stars.
    fn calibrate_with(
        &mut self,
        mount: Arc<dyn Telescope>,
        start: MountStart,
        turn: Turn,
        then: Then,
        reselect: bool,
    ) {
        let calibrator = Calibrator::new(
            self.setup(&start),
            start.declination_deg,
            calibration_params(&self.config),
            self.now(),
        );
        info!(pier_side = ?start.side, declination_deg = start.declination_deg, "calibration started");
        self.status.lock().set_state(State::Calibrating);
        self.select_next = reselect;
        self.mode = Mode::Calibrating(Box::new(Calibrating {
            calibrator: Some(calibrator),
            mount: RunMount::new(
                mount,
                start.side,
                self.config.mount.pulse_timeout.get(),
                self.config.mount.max_failures.get(),
            ),
            start,
            selecting: reselect || self.stars.is_none(),
            logged: false,
            turn,
            then,
        }));
    }

    /// The guide log's name of the camera.
    fn camera_name(&self) -> String {
        self.config.devices.camera.as_ref().map_or_else(
            || "Alpaca Camera".to_owned(),
            |camera| format!("Alpaca Camera {}", camera.device_number),
        )
    }

    /// The guide log's name of the mount.
    fn mount_name(&self) -> String {
        self.config.devices.mount.as_ref().map_or_else(
            || "Alpaca Telescope".to_owned(),
            |mount| format!("Alpaca Telescope {}", mount.device_number),
        )
    }

    /// Where `offset` puts the first guide star on the sensor.
    fn star_at(&self, offset: Offset) -> rp_guiding::Position {
        let reference = self
            .stars
            .as_ref()
            .and_then(|stars| stars.references().first().copied())
            .unwrap_or_default();
        rp_guiding::Position {
            x: reference.x + offset.x,
            y: reference.y + offset.y,
        }
    }

    /// What both kinds of the guide log's header give, for a run on
    /// `start`, with the lock and the stars at `lock` and `star`.
    fn log_common(&self, start: &MountStart, lock: Offset, star: Offset) -> guide_log::Common {
        guide_log::Common {
            pixel_scale: self.setup(start).pixel_scale_arcsec(),
            binning: effective_binning(self.sensor.sensor_type, self.reading.bin)
                .map_or(1, u32::from),
            focal_length_mm: self
                .config
                .optics
                .focal_length_mm
                .map_or(0.0, crate::config::FocalLength::get),
            search_region_px: self.config.stars.search_region_px.get(),
            mass_tolerance: self.config.stars.mass_change.get(),
            stars: self
                .stars
                .as_ref()
                .map_or(0, |stars| stars.references().len()),
            camera: self.camera_name(),
            gain: self.config.camera.gain,
            width: self.sensor.width,
            height: self.sensor.height,
            pixel_size_um: self.sensor.pixel_size_um,
            exposure: self.exposer.exposure,
            mount: self.mount_name(),
            guide_rates: start.guide_rates,
            pointing: start.pointing,
            declination_deg: start.declination_deg,
            pier_side: start.side,
            lock: self.star_at(lock),
            star: self.star_at(star),
            hfd: self.last_summary.and_then(|summary| summary.hfd),
        }
    }

    /// Write a run's stop to the guide log: what the server received, or the
    /// guider's own reason.
    fn log_stop(&self, stop: &Stop) {
        let note = if stop.reason == StopReason::Requested {
            "Server received guiding/stop".to_owned()
        } else {
            format!(
                "GUIDING STOPPED, {}: {}",
                stop.reason.as_str(),
                stop.message
            )
        };
        self.guide_log.lock().stopped(&note, self.now());
    }

    /// Write a calibration's frame to the guide log, beginning its section
    /// on the first frame with an offset, and the result of a leg the frame
    /// ended; whether the section has begun.
    fn log_calibration(
        &self,
        start: &MountStart,
        logged: bool,
        before: &Calibrator,
        frame: &Logged<'_>,
    ) -> bool {
        let logged =
            self.log_calibration_frame(start, logged, before, frame.offset, frame.measured);
        if let Some(progress) = frame.progress {
            self.log_leg_change(before, progress, start.declination_deg);
        }
        logged
    }

    /// Write a calibration's frame to the guide log, beginning its section
    /// on the first frame with an offset; whether the section has begun.
    fn log_calibration_frame(
        &self,
        start: &MountStart,
        logged: bool,
        before: &Calibrator,
        offset: Option<Offset>,
        measured: bool,
    ) -> bool {
        if !measured {
            return logged;
        }
        let mut log = self.guide_log.lock();
        let Some(offset) = offset else {
            if logged {
                let summary = self.last_summary;
                log.calibration_star_lost(
                    summary.and_then(|summary| summary.flux),
                    summary.and_then(|summary| summary.snr),
                );
            }
            return logged;
        };
        let leg_start = before.leg_start().unwrap_or(offset);
        if !logged {
            log.begin_calibration(
                &self.log_common(start, leg_start, offset),
                before.ra_step(),
                self.config.calibration.distance_px.get(),
            );
        }
        log.calibration_row(&guide_log::CalibrationRow {
            leg: before.leg(),
            step: before.steps(),
            start: leg_start,
            now: offset,
            star: self.star_at(offset),
        });
        true
    }

    /// Write the result of the West or the North leg to the guide log, as
    /// the calibration leaves it.
    fn log_leg_change(&self, before: &Calibrator, progress: &Progress, declination_deg: f64) {
        let (Progress::Pulse(next, _) | Progress::Wait(next)) = progress else {
            return;
        };
        if next.leg() == before.leg() {
            return;
        }
        let measured = match before.leg() {
            rp_guiding::Leg::West => next.ra_axis().map(|ra| {
                // The RA rate at the declination, the rate the leg measured.
                (
                    ra.angle_deg,
                    ra.rate_px_per_s * declination_deg.to_radians().cos(),
                )
            }),
            rp_guiding::Leg::North => next
                .dec_axis()
                .map(|dec| (dec.angle_deg, dec.rate_px_per_s)),
            _ => None,
        };
        if let Some((angle_deg, rate)) = measured {
            self.guide_log
                .lock()
                .leg_complete(before.leg(), angle_deg, rate);
        }
    }

    /// The time on the loop's clock.
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    /// What follows a frame: in a calibration the calibrator's next step,
    /// for a start its selection, and in a guiding run the guide step.
    async fn after_frame(&mut self, took: Took, cancel: &CancellationToken) -> Option<End> {
        match &self.mode {
            Mode::Looping => None,
            Mode::Starting(_) => self.starting_frame(&took, cancel).await,
            Mode::Guiding(_) => self.guiding_frame(took, cancel).await,
            Mode::Calibrating(_) => self.calibration_frame(took, cancel).await,
        }
    }

    /// What follows a frame of a calibration: the calibrator's next step.
    async fn calibration_frame(&mut self, took: Took, cancel: &CancellationToken) -> Option<End> {
        let now = self.now();
        if let Selection::Ended(end) = self.calibration_selection(&took) {
            return end.map(|end| *end);
        }
        let Mode::Calibrating(calibrating) = &mut self.mode else {
            return None;
        };
        let offset = match took {
            Took::Measured { offset, .. } if !calibrating.selecting => offset,
            Took::Measured { .. } | Took::Failed(_) => None,
        };
        // A measured frame of the run with no offset is a star-lost frame.
        if offset.is_none() && !calibrating.selecting && matches!(took, Took::Measured { .. }) {
            self.status.lock().star_lost_frame();
        }
        let frame_offset = offset;
        let calibrator = calibrating.calibrator.take()?;
        let before = calibrator.clone();
        let (start, logged) = (calibrating.start, calibrating.logged);
        let progress = calibrator.frame(offset, now);
        let frame = Logged {
            measured: matches!(took, Took::Measured { .. }),
            offset,
            progress: progress.as_ref().ok(),
        };
        let logged = self.log_calibration(&start, logged, &before, &frame);
        let Mode::Calibrating(calibrating) = &mut self.mode else {
            return None;
        };
        calibrating.logged = logged;
        let progress = match progress {
            Ok(progress) => progress,
            Err(error) => {
                info!(%error, "calibration failed");
                return self.calibration_ended(Err(ApiError::calibration_failed(&error)), None);
            }
        };
        let (next, pulse) = match progress {
            Progress::Done(calibration) => {
                let record = Record {
                    calibrated_at: SystemTime::now(),
                    calibration,
                };
                if let Err(error) = self.store.put(record.clone()) {
                    warn!(%error, "the calibration could not be stored; it serves its own run");
                }
                info!("calibration passed");
                return self.calibration_ended(Ok(record), frame_offset);
            }
            Progress::Wait(next) => (next, None),
            Progress::Pulse(next, pulse) => (next, Some(pulse)),
        };
        // A stop ends the cycle before any pulse goes out: the check is
        // raced against it, and looked at again before the pulse.
        let checked = tokio::select! {
            biased;
            () = cancel.cancelled() => return Some(End::Stop(Stop::requested())),
            checked = calibrating.mount.check() => checked,
        };
        let fault = match (checked, pulse) {
            (Err(fault), _) => Some(fault),
            (Ok(()), None) => None,
            (Ok(()), Some(_)) if cancel.is_cancelled() => {
                return Some(End::Stop(Stop::requested()));
            }
            (Ok(()), Some(pulse)) => calibrating.mount.pulse(pulse).await.err(),
        };
        let failed = match fault {
            Some(Fault::Stop(stop)) => return Some(End::Stop(stop)),
            Some(Fault::Failed(message)) => {
                // The step was not sent: the next frame is judged as if it
                // had not been asked for.
                calibrating.calibrator = Some(before);
                Some(message)
            }
            Some(Fault::Unconfirmed(message)) => {
                // The step went out, and counts.
                calibrating.calibrator = Some(next);
                Some(message)
            }
            None => {
                calibrating.calibrator = Some(next);
                None
            }
        };
        calibrating.mount.cycle(failed.as_deref()).map(End::Stop)
    }

    /// The selection a calibration waits for, on `took`: with too few stars
    /// the calibration ends with `no_stars`, and the frame is done with how
    /// the loop goes on; with enough the calibration takes its steps from
    /// this frame on, and a run calibrated again on it counts the
    /// interruption.
    fn calibration_selection(&mut self, took: &Took) -> Selection {
        let Mode::Calibrating(calibrating) = &mut self.mode else {
            return Selection::Ended(None);
        };
        if !calibrating.selecting {
            return Selection::Steps;
        }
        match took {
            Took::Measured {
                selection: Some(Err(())),
                ..
            } => Selection::Ended(
                self.calibration_ended(Err(ApiError::no_stars()), None)
                    .map(Box::new),
            ),
            Took::Measured {
                selection: Some(Ok(_)),
                ..
            } => {
                calibrating.selecting = false;
                if let Then::Guide(guiding::StartAfter {
                    found: guiding::Found::Run(_),
                    ..
                }) = calibrating.then
                {
                    self.status.lock().interruption();
                }
                Selection::Steps
            }
            Took::Measured { .. } | Took::Failed(_) => Selection::Steps,
        }
    }

    /// A calibration ended with `outcome`, on a frame with `offset`: a
    /// `calibrate` goes back to the state its request found, the loop going
    /// on or ending with it; a start guides, or does not.
    fn calibration_ended(
        &mut self,
        outcome: Result<Record, ApiError>,
        offset: Option<Offset>,
    ) -> Option<End> {
        {
            let mut log = self.guide_log.lock();
            match &outcome {
                Ok(_) => log.calibration_complete(&self.mount_name()),
                Err(error) => log.calibration_failed(&error.message),
            }
        }
        let Mode::Calibrating(calibrating) = std::mem::replace(&mut self.mode, Mode::Looping)
        else {
            return None;
        };
        let Calibrating {
            mount,
            start,
            turn,
            then,
            ..
        } = *calibrating;
        match then {
            Then::Answer { reply, return_to } => match return_to {
                Return::Idle => {
                    self.ending_turn = Some(turn);
                    Some(End::Ended(Answer::Calibration(reply, outcome)))
                }
                Return::Looping => {
                    self.status.lock().set_state(State::Looping);
                    drop(turn);
                    let _ = reply.send(outcome.map(|record| Calibrated {
                        state: State::Looping,
                        record,
                    }));
                    None
                }
            },
            Then::Guide(after) => match outcome {
                Ok(record) => {
                    let home = offset.unwrap_or_default();
                    self.begin_guiding(mount, start, record.calibration, home, after, turn);
                    None
                }
                Err(error) => self.start_refused(after, turn, error),
            },
        }
    }

    /// Count a failed frame, write the frame settings again before the next
    /// and start it no sooner than an exposure after the failed one began;
    /// the stop once `camera.max_frame_failures` have failed in a row.
    fn failed(&mut self, message: &str) -> Option<Stop> {
        self.settings_due = Some(Settings::AfterFailure);
        self.retry_at = self.attempt_started.checked_add(self.exposer.exposure);
        count_failure(&mut self.failures, self.max_failures, message)
    }
}

/// Count a failed frame in `failures`; the stop once `max` have failed in a
/// row.
fn count_failure(failures: &mut u32, max: u32, message: &str) -> Option<Stop> {
    *failures = failures.saturating_add(1);
    debug!(failures = *failures, max, %message, "a frame failed");
    (*failures >= max).then(|| {
        Stop::camera_error(format!(
            "{max} frames failed in a row, the last because {message}"
        ))
    })
}

/// The camera's static properties as the device poll last read them after
/// the camera came back, when they show another sensor than `sensor`. The
/// poll's other properties replace `sensor`'s when they do not.
fn came_back_changed(
    devices: &mut watch::Receiver<DevicesState>,
    sensor: &mut CameraSensor,
) -> Option<String> {
    let polled = devices
        .borrow_and_update()
        .camera
        .as_ref()
        .filter(|camera| camera.statics_current)
        .and_then(|camera| camera.statics)?;
    if let Some(message) = changed(sensor, &polled) {
        return Some(message);
    }
    *sensor = polled;
    None
}

/// When the frame settings are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settings {
    /// As the loop starts, with the static properties its request read.
    Start,
    /// After a frame failure, once the static properties are read again.
    AfterFailure,
}

/// Why the frame settings were not written.
#[derive(Debug)]
enum Rewrite {
    /// The camera came back with another sensor.
    Changed(String),
    /// A camera call failed, which fails the next frame.
    Failed(String),
}

/// Write the frame settings; after a failure, end an exposure of the
/// guider's own that may still run, read the camera's static properties,
/// and take up the camera as its server lists it now.
async fn write_settings(
    due: Settings,
    client: &DeviceClient,
    sensor: &mut CameraSensor,
    exposer: &mut Exposer,
) -> Result<(), Rewrite> {
    if due == Settings::AfterFailure {
        exposer.end_exposure(sensor.can_abort_exposure).await;
        exposer.in_flight = None;
        let camera = match client.find::<CameraKind>().await {
            Found::Listed {
                device,
                connected: Ok(true),
                ..
            } => device,
            Found::Listed {
                connected: Ok(false),
                ..
            } => return Err(Rewrite::Failed("the camera is not connected".to_owned())),
            Found::NoServer(error)
            | Found::Listed {
                connected: Err(error),
                ..
            } => {
                return Err(Rewrite::Failed(format!(
                    "the camera does not answer: {error}"
                )))
            }
            Found::NotListed => {
                return Err(Rewrite::Failed(format!(
                    "the camera's server lists no camera at device number {}",
                    client.device_number()
                )))
            }
        };
        let now = camera_sensor(Arc::clone(&camera)).await.map_err(|error| {
            Rewrite::Failed(format!(
                "the camera's static properties could not be read: {error}"
            ))
        })?;
        if let Some(message) = changed(sensor, &now) {
            return Err(Rewrite::Changed(message));
        }
        *sensor = now;
        exposer.camera = camera;
    }
    exposer.write_settings().await.map_err(Rewrite::Failed)
}

/// An exposure that has started: when it ends, and when its frame must
/// have arrived.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    end: Instant,
    deadline: Instant,
}

/// The camera's side of a cycle.
struct Exposer {
    camera: Arc<dyn Camera>,
    exposure: Duration,
    frame_timeout: Duration,
    settings: FrameSettings,
    /// The guider's own exposure, from its start until its frame arrives.
    in_flight: Option<InFlight>,
}

impl Exposer {
    /// Write the binning, the region and, when they are set, the gain and
    /// the offset.
    async fn write_settings(&self) -> Result<(), String> {
        let FrameSettings {
            bin,
            width,
            height,
            gain,
            offset,
        } = self.settings;
        let camera = &self.camera;
        let written = |member: &str, error| format!("writing {member} failed: {error}");
        call(camera.set_bin_x(bin))
            .await
            .map_err(|e| written("BinX", e))?;
        call(camera.set_bin_y(bin))
            .await
            .map_err(|e| written("BinY", e))?;
        call(camera.set_start_x(0))
            .await
            .map_err(|e| written("StartX", e))?;
        call(camera.set_start_y(0))
            .await
            .map_err(|e| written("StartY", e))?;
        call(camera.set_num_x(width))
            .await
            .map_err(|e| written("NumX", e))?;
        call(camera.set_num_y(height))
            .await
            .map_err(|e| written("NumY", e))?;
        if let Some(gain) = gain {
            call(camera.set_gain(gain))
                .await
                .map_err(|e| written("Gain", e))?;
        }
        if let Some(offset) = offset {
            call(camera.set_offset(offset))
                .await
                .map_err(|e| written("Offset", e))?;
        }
        debug!(settings = ?self.settings, "frame settings written");
        Ok(())
    }

    /// One frame: the exposure, the wait for it, and its fetch.
    async fn frame(&mut self) -> Result<ImageArray, String> {
        // In flight from the moment it is asked for, and until its frame
        // arrives: a start whose call is out, or got no answer, may have
        // started it, and one whose frame never came may still run.
        self.in_flight = Some(self.in_flight_from(Instant::now()));
        match call(self.camera.start_exposure(self.exposure, true)).await {
            Ok(()) => {}
            Err(error @ CallError::Failed(_)) => {
                self.in_flight = None;
                return Err(format!("StartExposure failed: {error}"));
            }
            Err(error @ CallError::Deadline) => {
                return Err(format!("StartExposure failed: {error}"));
            }
        }
        let in_flight = self.in_flight_from(Instant::now());
        self.in_flight = Some(in_flight);
        let image = self.fetch(in_flight.end, in_flight.deadline).await?;
        self.in_flight = None;
        let asked = (
            usize::try_from(self.settings.width).unwrap_or(usize::MAX),
            usize::try_from(self.settings.height).unwrap_or(usize::MAX),
            1,
        );
        let (width, height, planes) = image.dim();
        if (width, height, planes) != asked {
            return Err(format!(
                "the frame is {width} × {height} in {planes} planes, not the {} × {} asked for",
                self.settings.width, self.settings.height
            ));
        }
        Ok(image)
    }

    /// An exposure that started at `started`.
    fn in_flight_from(&self, started: Instant) -> InFlight {
        let end = started.checked_add(self.exposure).unwrap_or(started);
        InFlight {
            end,
            deadline: end.checked_add(self.frame_timeout).unwrap_or(end),
        }
    }

    /// Wait for the frame of an exposure ending at `end`, and fetch it, by
    /// `deadline`.
    async fn fetch(&self, end: Instant, deadline: Instant) -> Result<ImageArray, String> {
        let not_arrived = || {
            format!(
                "the frame had not arrived {} after its exposure should have ended",
                humantime::format_duration(self.frame_timeout)
            )
        };
        tokio::time::sleep_until(end).await;
        loop {
            match tokio::time::timeout_at(deadline, call(self.camera.image_ready())).await {
                Err(_) => return Err(not_arrived()),
                Ok(Err(error)) => return Err(format!("reading ImageReady failed: {error}")),
                Ok(Ok(true)) => break,
                // A timeout polls its call before it looks at the clock, so
                // a camera that answers at once is held to the deadline here.
                Ok(Ok(false)) if Instant::now() >= deadline => return Err(not_arrived()),
                Ok(Ok(false)) => tokio::time::sleep(READY_POLL).await,
            }
        }
        match tokio::time::timeout_at(deadline, self.camera.image_array()).await {
            Err(_) => Err(not_arrived()),
            Ok(Err(error)) => Err(format!("fetching ImageArray failed: {error}")),
            Ok(Ok(image)) => Ok(image),
        }
    }

    /// End the guider's own exposure, if one is in flight: abort it when the
    /// camera can abort, and wait for its frame otherwise, as long as the
    /// frame may take to arrive.
    async fn end_exposure(&self, can_abort: bool) {
        let Some(InFlight { end, deadline }) = self.in_flight else {
            return;
        };
        if can_abort {
            match call(self.camera.abort_exposure()).await {
                Ok(()) => debug!("the guider's exposure aborted"),
                Err(error) => debug!(%error, "aborting the guider's exposure failed"),
            }
            return;
        }
        tokio::time::sleep_until(end).await;
        while Instant::now() < deadline {
            if tokio::time::timeout_at(deadline, call(self.camera.image_ready())).await
                == Ok(Ok(true))
            {
                debug!("the guider's exposure ended");
                return;
            }
            tokio::time::sleep(READY_POLL).await;
        }
        debug!("the guider's exposure did not end by the time its frame was due");
    }
}

/// The measurement of one frame, off the async workers.
struct Job {
    stars: Option<GuideStars>,
    params: StarParams,
    reading: rp_guiding::SensorType,
    max_adu: u32,
    reselect: bool,
}

/// What a measured frame gave.
struct Measured {
    /// The guide stars after the frame.
    stars: Option<GuideStars>,
    summary: FrameSummary,
    /// The frame's offset; `None` for a star-lost frame.
    offset: Option<Offset>,
    /// When the frame was a selection: how many guide stars it selected,
    /// or that it had too few.
    selection: Option<Result<usize, ()>>,
    frame: Frame,
}

impl Job {
    fn run(self, image: &ImageArray) -> Result<Measured, String> {
        // Alpaca's image is indexed [x][y], the measured frame [[y, x]].
        let view = image.index_axis(Axis(2), 0).reversed_axes();
        let frame = Frame::new(&view, self.reading, Some(self.max_adu))
            .map_err(|error| error.to_string())?;
        let Self {
            stars,
            params,
            reselect,
            ..
        } = self;
        let (stars, measurement, selection) = match stars {
            Some(mut held) if !reselect => {
                let measurement = held.measure(&frame);
                (Some(held), Some(measurement), None)
            }
            held => match GuideStars::select(&frame, params) {
                Ok((selected, measurement)) => {
                    let count = selected.references().len();
                    (Some(selected), Some(measurement), Some(Ok(count)))
                }
                Err(_) => held.map_or((None, None, Some(Err(()))), |mut held| {
                    let measurement = held.measure(&frame);
                    (Some(held), Some(measurement), Some(Err(())))
                }),
            },
        };
        Ok(Measured {
            stars,
            summary: measurement
                .as_ref()
                .map_or_else(FrameSummary::none, FrameSummary::of),
            offset: measurement
                .as_ref()
                .and_then(|measurement| measurement.offset),
            selection,
            frame,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;

    use guide_simulator::{
        CameraConfig, Seeing, SimConfig, Simulator, SkyConfig, SkyOffset, Star, StarField,
    };

    use super::*;
    use crate::api::ErrorCode;
    use crate::test_camera::FakeCamera;

    /// A 320 × 240 frame at 2″ a pixel of `stars`, each at `east`, `north`
    /// arcseconds from the centre, as the camera serves it.
    fn image(stars: &[(f64, f64)], shift_east: f64) -> ImageArray {
        let mut sim = Simulator::new(SimConfig {
            seed: 3,
            camera: CameraConfig {
                width: 320,
                height: 240,
                ..CameraConfig::default()
            },
            sky: SkyConfig {
                stars: StarField::Listed(
                    stars
                        .iter()
                        .map(|&(east, north)| Star {
                            position: SkyOffset {
                                east: east + shift_east,
                                north,
                            },
                            e_per_s: 40_000.0,
                        })
                        .collect(),
                ),
                seeing: Seeing {
                    fwhm_arcsec: 3.0,
                    common_motion_arcsec: 0.0,
                    star_motion_arcsec: 0.0,
                },
                clouds: Vec::new(),
            },
            ..SimConfig::default()
        })
        .unwrap();
        let shot = sim.expose(Duration::from_millis(500));
        ImageArray::from(shot.pixels.reversed_axes())
    }

    const FIELD: [(f64, f64); 3] = [(0.0, 0.0), (-120.0, -60.0), (120.0, 60.0)];

    fn job(stars: Option<GuideStars>, reselect: bool) -> Job {
        Job {
            stars,
            params: StarParams::default(),
            reading: rp_guiding::SensorType::Monochrome,
            max_adu: 65_535,
            reselect,
        }
    }

    #[test]
    fn a_frame_with_no_guide_stars_is_a_selection() {
        let measured = job(None, false).run(&image(&FIELD, 0.0)).unwrap();
        assert_eq!(measured.selection, Some(Ok(3)));
        assert_eq!(measured.summary.star_count, 3);
        assert!(!measured.summary.star_lost);
        assert!(measured.stars.is_some());
    }

    #[test]
    fn a_selection_without_enough_stars_holds_no_guide_star_and_is_star_lost() {
        let measured = job(None, false).run(&image(&[], 0.0)).unwrap();
        assert_eq!(measured.selection, Some(Err(())));
        assert_eq!(measured.summary, FrameSummary::none());
        assert!(measured.stars.is_none());
    }

    #[test]
    fn held_guide_stars_are_measured_and_not_selected_again() {
        let selected = job(None, false).run(&image(&FIELD, 0.0)).unwrap();
        // Shifted by 30 px, beyond the search region: held stars are not
        // found there, where a selection would find them all.
        let measured = job(selected.stars, false)
            .run(&image(&FIELD, 60.0))
            .unwrap();
        assert_eq!(measured.selection, None);
        assert_eq!(measured.summary.star_count, 0);
        assert!(measured.summary.star_lost);
    }

    #[test]
    fn a_reselect_selects_on_the_frame() {
        let selected = job(None, false).run(&image(&FIELD, 0.0)).unwrap();
        let measured = job(selected.stars, true).run(&image(&FIELD, 60.0)).unwrap();
        assert_eq!(measured.selection, Some(Ok(3)));
        assert_eq!(measured.summary.star_count, 3);
    }

    #[test]
    fn a_reselect_without_enough_stars_keeps_the_guide_stars() {
        let selected = job(None, false).run(&image(&FIELD, 0.0)).unwrap();
        let failed = job(selected.stars, true).run(&image(&[], 0.0)).unwrap();
        assert_eq!(failed.selection, Some(Err(())));
        // The kept stars are found where they were selected, and not 30 px
        // away, where a new selection would have found them.
        let back = job(failed.stars.clone(), false)
            .run(&image(&FIELD, 0.0))
            .unwrap();
        assert_eq!(back.summary.star_count, 3);
        let shifted = job(failed.stars, false).run(&image(&FIELD, 60.0)).unwrap();
        assert_eq!(shifted.summary.star_count, 0);
    }

    #[test]
    fn a_frame_with_nothing_to_measure_fails() {
        let empty = ImageArray::from(ndarray::Array2::<u16>::zeros((0, 0)));
        let error = job(None, false).run(&empty).err().unwrap();
        assert!(error.contains("no pixels"), "{error}");
    }

    fn camera_of(pixel_size_um: f64) -> CameraSensor {
        camera(pixel_size_um, 65_535)
    }

    fn camera(pixel_size_um: f64, max_adu: u32) -> CameraSensor {
        CameraSensor {
            sensor_type: ascom_alpaca::api::camera::SensorType::Monochrome,
            pixel_size_um,
            width: 320,
            height: 240,
            max_adu,
            max_bin: 2,
            can_abort_exposure: true,
        }
    }

    fn polled(statics: CameraSensor, current: bool) -> watch::Receiver<DevicesState> {
        let (_, devices) = watch::channel(DevicesState {
            camera: Some(crate::devices::CameraState {
                reach: crate::devices::Reach::Connected,
                unique_id: None,
                statics: Some(statics),
                statics_current: current,
            }),
            mount: None,
        });
        devices
    }

    #[test]
    fn a_camera_the_poll_read_with_another_sensor_has_come_back_changed() {
        let mut sensor = camera(2.0, 65_535);
        let mut devices = polled(camera(3.75, 65_535), true);
        let message = came_back_changed(&mut devices, &mut sensor).unwrap();
        assert!(message.contains("pixel size 2 µm → 3.75 µm"), "{message}");
    }

    #[test]
    fn what_the_poll_read_before_the_camera_came_back_is_not_compared() {
        let mut sensor = camera(2.0, 65_535);
        let mut devices = polled(camera(3.75, 65_535), false);
        assert_eq!(came_back_changed(&mut devices, &mut sensor), None);
        assert_eq!(sensor, camera(2.0, 65_535));
    }

    #[test]
    fn a_camera_that_came_back_with_its_sensor_lends_the_loop_its_other_properties() {
        let mut sensor = camera(2.0, 65_535);
        let mut devices = polled(camera(2.0, 4_095), true);
        assert_eq!(came_back_changed(&mut devices, &mut sensor), None);
        assert_eq!(sensor.max_adu, 4_095);
    }

    fn exposer(camera: Arc<FakeCamera>) -> Exposer {
        Exposer {
            camera,
            exposure: Duration::from_secs(2),
            frame_timeout: Duration::from_secs(10),
            settings: FrameSettings {
                bin: 1,
                width: 320,
                height: 240,
                gain: None,
                offset: None,
            },
            in_flight: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_arrives_with_the_dimensions_asked_for() {
        let mut exposer = exposer(Arc::new(FakeCamera::default()));
        let image = exposer.frame().await.unwrap();
        assert_eq!(image.dim(), (320, 240, 1));
        assert!(exposer.in_flight.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_exposure_whose_start_is_still_out_is_in_flight() {
        let camera = Arc::new(FakeCamera {
            start_hangs: true,
            ..FakeCamera::default()
        });
        let mut exposer = exposer(Arc::clone(&camera));
        let left = tokio::time::timeout(Duration::from_secs(1), exposer.frame()).await;
        assert!(left.is_err(), "the start answered");
        assert!(exposer.in_flight.is_some());
        exposer.end_exposure(true).await;
        assert_eq!(camera.aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_that_gets_no_answer_stays_in_flight() {
        let camera = Arc::new(FakeCamera {
            start_hangs: true,
            ..FakeCamera::default()
        });
        let mut exposer = exposer(camera);
        let error = exposer.frame().await.unwrap_err();
        assert!(error.contains("no answer within 10s"), "{error}");
        assert!(
            exposer.in_flight.is_some(),
            "a start that got no answer may have started"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_settings_after_a_failure_end_the_exposure_that_may_still_run_first() {
        let camera = Arc::new(FakeCamera::default());
        let mut exposer = exposer(Arc::clone(&camera));
        exposer.in_flight = Some(exposer.in_flight_from(Instant::now()));
        // A server that does not listen: the read after the ended exposure
        // fails.
        let address: crate::config::DeviceAddress =
            serde_json::from_value(serde_json::json!({ "alpaca_url": "http://127.0.0.1:9" }))
                .unwrap();
        let client = DeviceClient::new(&address, None).unwrap();
        let mut sensor = camera_of(2.0);
        let written =
            write_settings(Settings::AfterFailure, &client, &mut sensor, &mut exposer).await;
        assert!(matches!(written, Err(Rewrite::Failed(_))), "{written:?}");
        assert_eq!(camera.aborts.load(Ordering::SeqCst), 1);
        assert!(exposer.in_flight.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_start_leaves_nothing_in_flight() {
        let camera = Arc::new(FakeCamera {
            start_refused: true,
            ..FakeCamera::default()
        });
        let mut exposer = exposer(camera);
        let error = exposer.frame().await.unwrap_err();
        assert!(error.starts_with("StartExposure failed"), "{error}");
        assert!(exposer.in_flight.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_of_other_dimensions_fails() {
        let camera = Arc::new(FakeCamera {
            frame: (319, 240),
            ..FakeCamera::default()
        });
        let error = exposer(camera).frame().await.unwrap_err();
        assert!(error.contains("not the 320 × 240 asked for"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_never_ready_fails_at_the_frame_timeout() {
        let camera = Arc::new(FakeCamera::default());
        camera.ready.store(false, Ordering::SeqCst);
        let mut exposer = exposer(camera);
        let started = Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(60), exposer.frame())
            .await
            .expect("the frame failed by its deadline")
            .unwrap_err();
        assert!(error.contains("had not arrived 10s after"), "{error}");
        assert!(started.elapsed() >= Duration::from_secs(12));
        assert!(
            exposer.in_flight.is_some(),
            "an exposure whose frame never came may still run"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_exposure_the_camera_cannot_abort_is_waited_for() {
        let camera = Arc::new(FakeCamera {
            start_hangs: true,
            ..FakeCamera::default()
        });
        let mut exposer = exposer(Arc::clone(&camera));
        let _ = tokio::time::timeout(Duration::from_millis(500), exposer.frame()).await;
        let started = Instant::now();
        exposer.end_exposure(false).await;
        // The exposure was asked for 500 ms before the wait began, and runs
        // 2 s.
        assert!(started.elapsed() >= Duration::from_millis(1_500));
        assert_eq!(camera.aborts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_no_exposure_calls_nothing() {
        let camera = Arc::new(FakeCamera::default());
        exposer(Arc::clone(&camera)).end_exposure(true).await;
        assert_eq!(camera.aborts.load(Ordering::SeqCst), 0);
    }

    /// A loop on the fake camera, in `looping`, with the guider's default
    /// configuration and a focal length of 200 mm.
    pub(super) fn looping_cycle() -> Cycle {
        let config: Config = serde_json::from_value(serde_json::json!({
            "optics": { "focal_length_mm": 200.0 }
        }))
        .unwrap();
        let address: crate::config::DeviceAddress =
            serde_json::from_value(serde_json::json!({ "alpaca_url": "http://127.0.0.1:9" }))
                .unwrap();
        let sensor = camera_of(2.0);
        let reading = Reading {
            sensor: rp_guiding::SensorType::Monochrome,
            bin: 1,
        };
        let (_, devices) = watch::channel(DevicesState::default());
        let dir = tempfile::tempdir().unwrap().keep();
        Cycle {
            params: star_params(&config),
            max_failures: 3,
            exposer: exposer(Arc::new(FakeCamera::default())),
            client: Arc::new(DeviceClient::new(&address, None).unwrap()),
            sensor,
            reading,
            devices,
            status: Arc::new(Mutex::new(Status::new())),
            store: Arc::new(CalibrationStore::open(&dir)),
            guide_log: Arc::new(Mutex::new(GuideLog::new(
                &dir,
                crate::config::GuideLogConfig::default(),
            ))),
            last_summary: None,
            config: Arc::new(config),
            epoch: Instant::now(),
            mode: Mode::Looping,
            ending_turn: None,
            first: None,
            reselect: None,
            inbox: Vec::new(),
            select_next: false,
            rng: rand::make_rng(),
            stars: None,
            failures: 0,
            settings_due: None,
            attempt_started: Instant::now(),
            retry_at: None,
        }
    }

    /// A calibration on `mount` that returns to `return_to`, with the
    /// receiver of its answer and the turns its turn was taken from.
    async fn run_on(
        mount: crate::test_mount::FakeMount,
        return_to: Return,
    ) -> (
        CalibrationRun,
        oneshot::Receiver<Result<Calibrated, ApiError>>,
        Arc<tokio::sync::Mutex<()>>,
    ) {
        let turns = Arc::new(tokio::sync::Mutex::new(()));
        let (reply, answer) = oneshot::channel();
        let run = CalibrationRun {
            mount: Arc::new(mount),
            start: MountStart {
                side: rp_guiding::PierSide::East,
                declination_deg: 0.0,
                guide_rates: Some(rp_guiding::GuideRates {
                    ra_deg_per_s: 0.004,
                    dec_deg_per_s: 0.004,
                }),
                pointing: None,
            },
            reply,
            return_to,
            turn: Arc::clone(&turns).lock_owned().await,
        };
        (run, answer, turns)
    }

    /// The selection's frame of a calibration, with the stars at home.
    const fn selected() -> Took {
        Took::Measured {
            offset: Some(Offset { x: 0.0, y: 0.0 }),
            selection: Some(Ok(3)),
        }
    }

    /// The steps of the leg the calibrator a calibrating cycle holds has
    /// sent, read off its debug output, whose last field they are.
    fn leg_steps(cycle: &Cycle) -> String {
        let Mode::Calibrating(calibrating) = &cycle.mode else {
            panic!("the loop is not calibrating");
        };
        let held = format!("{:?}", calibrating.calibrator);
        let (_, last) = held.rsplit_once("steps: ").expect("the calibrator's steps");
        format!("steps: {}", last.trim_end_matches(['}', ')', ' ']))
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibration_takes_the_state_calibrating_and_pulses_after_its_first_frame() {
        let mut cycle = looping_cycle();
        let (run, _answer, _turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Idle).await;
        cycle.calibrate(run);
        assert_eq!(cycle.status.lock().state(), State::Calibrating);
        assert!(cycle
            .after_frame(selected(), &CancellationToken::new())
            .await
            .is_none());
        assert_eq!(leg_steps(&cycle), "steps: 1");
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_whose_pulse_failed_is_asked_for_again() {
        let mut cycle = looping_cycle();
        let mount = crate::test_mount::FakeMount {
            pulse_refused: true,
            ..crate::test_mount::FakeMount::default()
        };
        let (run, _answer, _turns) = run_on(mount, Return::Idle).await;
        cycle.calibrate(run);
        assert!(cycle
            .after_frame(selected(), &CancellationToken::new())
            .await
            .is_none());
        assert_eq!(leg_steps(&cycle), "steps: 0");
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_whose_pulse_went_out_counts_though_its_end_could_not_be_read() {
        let mut cycle = looping_cycle();
        let mount = crate::test_mount::FakeMount {
            pulse_guiding_fails: true,
            ..crate::test_mount::FakeMount::default()
        };
        let (run, _answer, _turns) = run_on(mount, Return::Idle).await;
        cycle.calibrate(run);
        assert!(cycle
            .after_frame(selected(), &CancellationToken::new())
            .await
            .is_none());
        assert_eq!(leg_steps(&cycle), "steps: 1");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_mount_check_sends_no_pulse() {
        let mut cycle = looping_cycle();
        let mount = Arc::new(crate::test_mount::FakeMount {
            read_delay: Duration::from_secs(5),
            ..crate::test_mount::FakeMount::default()
        });
        let (mut run, _answer, _turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Idle).await;
        run.mount = Arc::clone(&mount) as Arc<dyn Telescope>;
        cycle.calibrate(run);
        let cancel = CancellationToken::new();
        let stopping = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            stopping.cancel();
        });
        let started = Instant::now();
        let Some(End::Stop(stop)) = cycle.after_frame(selected(), &cancel).await else {
            panic!("the calibration went on");
        };
        assert_eq!(stop.reason, crate::api::StopReason::Requested);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the stop waited for the check to end"
        );
        assert_eq!(mount.pulses.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_mount_that_fails_its_check_stops_the_calibration() {
        let mut cycle = looping_cycle();
        let mount = crate::test_mount::FakeMount {
            slewing: true,
            ..crate::test_mount::FakeMount::default()
        };
        let (run, _answer, _turns) = run_on(mount, Return::Idle).await;
        cycle.calibrate(run);
        let Some(End::Stop(stop)) = cycle
            .after_frame(selected(), &CancellationToken::new())
            .await
        else {
            panic!("the calibration went on");
        };
        assert_eq!(stop.reason, crate::api::StopReason::MountSlewing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cycle_that_sends_no_pulse_still_checks_the_mount() {
        let mut cycle = looping_cycle();
        let mount = crate::test_mount::FakeMount {
            at_park: true,
            ..crate::test_mount::FakeMount::default()
        };
        let (run, _answer, _turns) = run_on(mount, Return::Idle).await;
        cycle.calibrate(run);
        let Some(End::Stop(stop)) = cycle
            .after_frame(
                Took::Failed("no frame".to_owned()),
                &CancellationToken::new(),
            )
            .await
        else {
            panic!("the calibration went on");
        };
        assert_eq!(stop.reason, crate::api::StopReason::MountParked);
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibration_frame_without_stars_counts_as_star_lost() {
        let mut cycle = looping_cycle();
        let (run, _answer, _turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Idle).await;
        cycle.calibrate(run);
        let cancel = CancellationToken::new();
        assert!(cycle.after_frame(selected(), &cancel).await.is_none());
        assert_eq!(cycle.status.lock().totals().star_lost, 0);
        let lost = Took::Measured {
            offset: None,
            selection: None,
        };
        assert!(cycle.after_frame(lost, &cancel).await.is_none());
        assert_eq!(cycle.status.lock().totals().star_lost, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibration_from_looping_goes_back_to_looping_and_gives_up_its_turn() {
        let mut cycle = looping_cycle();
        let (run, answer, turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Looping).await;
        cycle.calibrate(run);
        let record_error = ApiError::new(ErrorCode::NoStars, "none");
        assert!(cycle.calibration_ended(Err(record_error), None).is_none());
        assert_eq!(cycle.status.lock().state(), State::Looping);
        assert!(turns.try_lock().is_ok(), "the turn is still held");
        assert_eq!(answer.await.unwrap().unwrap_err().code, ErrorCode::NoStars);
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibration_from_idle_ends_the_loop_holding_its_turn_until_the_guider_is_idle() {
        let mut cycle = looping_cycle();
        let (run, _answer, turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Idle).await;
        cycle.calibrate(run);
        let end = cycle.calibration_ended(Err(ApiError::new(ErrorCode::NoStars, "none")), None);
        assert!(matches!(end, Some(End::Ended(Answer::Calibration(..)))));
        assert_eq!(cycle.status.lock().state(), State::Calibrating);
        assert!(
            turns.try_lock().is_err(),
            "the turn was given up before the guider was idle"
        );
        assert!(cycle.ending_turn.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_calibration_from_idle_without_enough_stars_answers_no_stars() {
        let mut cycle = looping_cycle();
        let (run, _answer, _turns) =
            run_on(crate::test_mount::FakeMount::default(), Return::Idle).await;
        cycle.calibrate(run);
        let end = cycle
            .after_frame(
                Took::Measured {
                    offset: None,
                    selection: Some(Err(())),
                },
                &CancellationToken::new(),
            )
            .await;
        let Some(End::Ended(Answer::Calibration(_, outcome))) = end else {
            panic!("the calibration went on");
        };
        assert_eq!(outcome.unwrap_err().code, ErrorCode::NoStars);
    }

    #[test]
    fn a_failure_stops_the_loop_only_at_the_most_allowed_in_a_row() {
        let failures = |count: u32| {
            let mut failures = 0;
            (0..count)
                .map(|n| count_failure(&mut failures, 3, &format!("failure {n}")))
                .collect::<Vec<_>>()
        };
        let stops = failures(3);
        assert_eq!(stops[..2], [None, None]);
        let stop = stops[2].clone().unwrap();
        assert_eq!(stop.reason, crate::api::StopReason::CameraError);
        assert_eq!(
            stop.message,
            "3 frames failed in a row, the last because failure 2"
        );
    }
}

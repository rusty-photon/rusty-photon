//! The guider's state and its requests.
//!
//! `loop`, `calibrate`, `star/reselect`, `guiding/stop` and
//! `calibration/clear`, and the reads `stats`, `metrics`, `frame/latest`
//! and `calibration` answer from (docs/services/guider.md § States,
//! § Concurrency, § HTTP API).
//!
//! Mutating requests take turns behind one mutex, and queue; a calibration
//! keeps its request's turn until it ends. A `guiding/stop` takes no turn:
//! it cancels the loop, and every mutating request waiting or queued when
//! it arrives answers `stopped`, since each watches the count of stops from
//! its arrival. A loop is started, and handed a calibration, under the same
//! lock a stop cancels under, so nothing accepted before a stop runs after
//! it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use ascom_alpaca::api::Camera;
use parking_lot::Mutex;
use rp_guiding::{Frame, FrameMeasurement, Position};
use rusty_photon_rolling_stats::SquareSums;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::api::{
    ApiError, DitherBody, ErrorCode, PauseBody, SettleSpec, Size, StartBody, Stop, StopReason,
};
use crate::calibration_store::{CalibrationStore, RecordBody, StoreBody};
use crate::camera_loop::{self, CalibrationRun, Command, DitherRun, GuidingRun, LoopSetup, Return};
use crate::config::Config;
use crate::devices::{
    camera_sensor, CameraKind, CameraSensor, DeviceClient, DevicesState, Found, Kind, MountKind,
    Reach,
};
use crate::mount;
use crate::sensor;

/// A turn of the mutating requests, which a calibration holds until it
/// ends.
pub type Turn = tokio::sync::OwnedMutexGuard<()>;

/// The frames the metrics window holds.
const METRICS_WINDOW: usize = 50;

/// The guide steps the RMS window holds.
const RMS_WINDOW: usize = 50;

/// The guider's state (guider.md § States).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing runs.
    Idle,
    /// Frames are taken and measured.
    Looping,
    /// The guider learns what a pulse in each direction does to the stars.
    Calibrating,
    /// The guide loop is closed.
    Guiding,
    /// A client paused the corrections.
    Paused,
    /// Too few of the guide stars are usable; the guider keeps looking.
    StarLost,
}

impl State {
    /// Whether the state is a run state.
    #[must_use]
    pub const fn is_run(self) -> bool {
        matches!(
            self,
            Self::Calibrating | Self::Guiding | Self::Paused | Self::StarLost
        )
    }
}

/// What one measured frame told of its usable stars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSummary {
    pub star_count: usize,
    pub star_lost: bool,
    /// The median half-flux diameter, signal-to-noise ratio and flux of the
    /// usable stars.
    pub hfd: Option<f64>,
    pub snr: Option<f64>,
    pub flux: Option<f64>,
}

impl FrameSummary {
    /// A frame with no usable star: a star-lost frame.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            star_count: 0,
            star_lost: true,
            hfd: None,
            snr: None,
            flux: None,
        }
    }

    /// What `measurement` found.
    #[must_use]
    pub const fn of(measurement: &FrameMeasurement) -> Self {
        Self {
            star_count: measurement.usable,
            star_lost: measurement.star_lost,
            hfd: measurement.median_hfd,
            snr: measurement.median_snr,
            flux: measurement.median_flux,
        }
    }
}

/// One entry of the metrics window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FrameMetrics {
    frame: u64,
    hfd: Option<f64>,
    snr: Option<f64>,
    star_mass: Option<f64>,
    star_count: usize,
    star_lost: bool,
}

/// The latest measured frame, and where its guide stars are looked for in
/// the next.
#[derive(Debug)]
pub struct LatestFrame {
    pub frame: Frame,
    pub centres: Vec<Position>,
    metrics: FrameMetrics,
}

/// The latest stop, and when it happened.
#[derive(Debug, Clone)]
struct LastStop {
    stop: Stop,
    at: SystemTime,
}

/// The running totals (guider.md § Statistics): what the service has
/// counted since it started, never reset.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Totals {
    pub(crate) guide_steps: u64,
    pub(crate) ra: SquareSums,
    pub(crate) dec: SquareSums,
    pub(crate) star_lost: u64,
    pub(crate) interruptions: u64,
}

/// What the loop reports and the reads answer from.
#[derive(Debug)]
pub struct Status {
    state: State,
    /// Whether the guider takes frames: false in `idle` and in a full pause.
    exposing: bool,
    /// Whether a `guiding/start` or a `dither` waits for the stars to
    /// settle.
    settling: bool,
    /// The frames measured since the service started.
    frames: u64,
    metrics: VecDeque<FrameMetrics>,
    latest: Option<Arc<LatestFrame>>,
    last_stop: Option<LastStop>,
    /// The errors of the last guide steps: RA, and Dec when it is
    /// corrected.
    rms: VecDeque<(f64, Option<f64>)>,
    totals: Totals,
}

impl Status {
    pub(crate) const fn new() -> Self {
        Self {
            state: State::Idle,
            exposing: false,
            settling: false,
            frames: 0,
            metrics: VecDeque::new(),
            latest: None,
            last_stop: None,
            rms: VecDeque::new(),
            totals: Totals {
                guide_steps: 0,
                ra: SquareSums::new(),
                dec: SquareSums::new(),
                star_lost: 0,
                interruptions: 0,
            },
        }
    }

    /// The loop starts from `idle`, in `state`: the latest frame is cleared,
    /// and the metrics window with it when `clear_metrics`.
    fn loop_started(&mut self, state: State, clear_metrics: bool) {
        self.state = state;
        self.exposing = true;
        if clear_metrics {
            self.metrics.clear();
        }
        self.latest = None;
    }

    /// The loop goes on in `state`, exposing unless it is a full pause.
    pub const fn set_state(&mut self, state: State) {
        self.state = state;
        self.exposing = true;
    }

    /// The corrections are paused, and with `full` the exposures too.
    pub const fn paused(&mut self, full: bool) {
        self.state = State::Paused;
        self.exposing = !full;
    }

    /// The loop ended with no stop, its exposure ended: a run that a
    /// request found in `idle` returned there.
    pub const fn idle(&mut self) {
        self.state = State::Idle;
        self.exposing = false;
        self.settling = false;
    }

    /// Whether a request waits for the stars to settle.
    pub const fn set_settling(&mut self, settling: bool) {
        self.settling = settling;
    }

    /// An accepted `guiding/start` clears the RMS window and the metrics
    /// window.
    pub fn start_accepted(&mut self) {
        self.rms.clear();
        self.metrics.clear();
    }

    /// A guide step with its errors: RA, and Dec when it is corrected.
    pub fn guide_step(&mut self, ra: f64, dec: Option<f64>) {
        if self.rms.len() >= RMS_WINDOW {
            self.rms.pop_front();
        }
        self.rms.push_back((ra, dec));
        self.totals.guide_steps = self.totals.guide_steps.saturating_add(1);
        self.totals.ra.add(ra);
        if let Some(dec) = dec {
            self.totals.dec.add(dec);
        }
    }

    /// A star-lost frame of a run.
    pub const fn star_lost_frame(&mut self) {
        self.totals.star_lost = self.totals.star_lost.saturating_add(1);
    }

    /// A run was paused, calibrated again or selected stars again.
    pub const fn interruption(&mut self) {
        self.totals.interruptions = self.totals.interruptions.saturating_add(1);
    }

    /// The RMS of the RA errors, of the Dec errors, and their total, over
    /// the RMS window; the Dec and the total are `None` when Dec is not
    /// corrected, and all three with no guide step.
    fn rms(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        let mut ra = SquareSums::new();
        let mut dec = SquareSums::new();
        let mut every_dec = true;
        for (ra_error, dec_error) in &self.rms {
            ra.add(*ra_error);
            match dec_error {
                Some(dec_error) => dec.add(*dec_error),
                None => every_dec = false,
            }
        }
        let ra = ra.rms();
        let dec = dec.rms().filter(|_| every_dec);
        let total = ra.zip(dec).map(|(ra, dec)| ra.hypot(dec));
        (ra, dec, total)
    }

    /// What a `guiding/start` or a `dither` answers once settled, the
    /// arcseconds at `pixel_scale`.
    #[must_use]
    pub fn settled(&self, pixel_scale: Option<f64>) -> SettledAnswer {
        let (ra, dec, total) = self.rms();
        let arcsec = |pixels: Option<f64>| pixels.zip(pixel_scale).map(|(px, scale)| px * scale);
        SettledAnswer {
            state: State::Guiding,
            rms_ra_px: ra,
            rms_dec_px: dec,
            total_rms_px: total,
            rms_ra_arcsec: arcsec(ra),
            rms_dec_arcsec: arcsec(dec),
            total_rms_arcsec: arcsec(total),
            sample_count: self.rms.len(),
        }
    }

    /// A frame was measured.
    pub fn record(&mut self, summary: FrameSummary, frame: Frame, centres: Vec<Position>) {
        self.frames = self.frames.saturating_add(1);
        let metrics = FrameMetrics {
            frame: self.frames,
            hfd: summary.hfd,
            snr: summary.snr,
            star_mass: summary.flux,
            star_count: summary.star_count,
            star_lost: summary.star_lost,
        };
        if self.metrics.len() >= METRICS_WINDOW {
            self.metrics.pop_front();
        }
        self.metrics.push_back(metrics);
        self.latest = Some(Arc::new(LatestFrame {
            frame,
            centres,
            metrics,
        }));
    }

    /// The loop stopped, its exposure ended. A run that stops counts as
    /// an interruption.
    pub fn stopped(&mut self, stop: Stop) {
        if self.state.is_run() {
            self.interruption();
        }
        self.state = State::Idle;
        self.exposing = false;
        self.settling = false;
        self.last_stop = Some(LastStop {
            stop,
            at: SystemTime::now(),
        });
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// Whether the guider takes frames.
    #[must_use]
    pub const fn exposing(&self) -> bool {
        self.exposing
    }

    /// Whether a request waits for the stars to settle.
    #[cfg(test)]
    pub(crate) const fn settling(&self) -> bool {
        self.settling
    }

    /// The running totals.
    #[cfg(test)]
    pub(crate) const fn totals(&self) -> Totals {
        self.totals
    }

    /// The frames in the metrics window.
    #[cfg(test)]
    pub(crate) fn metrics_len(&self) -> usize {
        self.metrics.len()
    }

    /// The guide stars of the latest measured frame, 0 before the first.
    fn star_count(&self) -> usize {
        self.latest
            .as_ref()
            .map_or(0, |latest| latest.metrics.star_count)
    }
}

/// A loop that runs, or ran last.
struct Running {
    cancel: CancellationToken,
    commands: mpsc::UnboundedSender<Command>,
    /// True once the loop's task has ended.
    done: watch::Receiver<bool>,
}

/// The answer of `loop` and `star/reselect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SelectionAnswer {
    pub state: State,
    pub star_count: usize,
}

/// The answer of a request that reports only the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StateAnswer {
    pub state: State,
}

/// The answer of `calibrate`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrateAnswer {
    pub state: State,
    pub calibration: RecordBody,
}

/// The answer of `guiding/start` and `dither` once settled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SettledAnswer {
    pub state: State,
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    pub rms_ra_arcsec: Option<f64>,
    pub rms_dec_arcsec: Option<f64>,
    pub total_rms_arcsec: Option<f64>,
    pub sample_count: usize,
}

/// The answer of `dither/abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AbortAnswer {
    pub state: State,
    pub aborted: bool,
}

/// The guider.
pub struct Guider {
    config: Arc<Config>,
    camera: Option<Arc<DeviceClient>>,
    mount: Option<Arc<DeviceClient>>,
    devices: watch::Receiver<DevicesState>,
    /// The mutating requests' turns.
    turns: Arc<tokio::sync::Mutex<()>>,
    store: Arc<CalibrationStore>,
    /// The count of stops.
    stops: watch::Sender<u64>,
    running: Mutex<Option<Running>>,
    status: Arc<Mutex<Status>>,
    /// This run of the service, as the running totals and the metrics name
    /// it.
    epoch: String,
    /// Cancelled when a loop's task ends without a stop.
    ended_abnormally: CancellationToken,
}

impl Guider {
    #[must_use]
    pub fn new(
        config: Arc<Config>,
        camera: Option<Arc<DeviceClient>>,
        mount: Option<Arc<DeviceClient>>,
        devices: watch::Receiver<DevicesState>,
        store: Arc<CalibrationStore>,
    ) -> Self {
        Self {
            config,
            camera,
            mount,
            devices,
            turns: Arc::new(tokio::sync::Mutex::new(())),
            store,
            stops: watch::Sender::new(0),
            running: Mutex::new(None),
            status: Arc::new(Mutex::new(Status::new())),
            epoch: uuid::Uuid::new_v4().to_string(),
            ended_abnormally: CancellationToken::new(),
        }
    }

    /// Cancelled when a loop's task ends without a stop, which the process
    /// does not outlive.
    #[must_use]
    pub fn ended_abnormally(&self) -> CancellationToken {
        self.ended_abnormally.clone()
    }

    /// `POST /api/v1/loop`.
    ///
    /// # Errors
    ///
    /// `stopped`, `not_idle`, `not_configured`, `device_unreachable`,
    /// `device_not_connected`, `camera_unsupported` and `guide_failed`, as
    /// guider.md § `POST /api/v1/loop` and § Error envelope say.
    pub async fn start_loop(&self) -> Result<SelectionAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        self.not_in_a_run()?;
        if self.looping() {
            return Ok(SelectionAnswer {
                state: State::Looping,
                star_count: self.status.lock().star_count(),
            });
        }
        let devices = self.devices_for_a_start(&mut stops).await?;
        let (first, first_frame) = oneshot::channel();
        self.spawn(
            self.setup(devices, Some(first), None, None),
            &stops,
            State::Looping,
            true,
        )?;
        let star_count = self.answer(first_frame, &mut stops).await?;
        Ok(SelectionAnswer {
            state: State::Looping,
            star_count,
        })
    }

    /// `POST /api/v1/calibrate`.
    ///
    /// # Errors
    ///
    /// `stopped`, `not_idle`, `not_configured`, `device_unreachable`,
    /// `device_not_connected`, `camera_unsupported`, `mount_not_ready`,
    /// `no_stars`, `calibration_failed` and `guide_failed`, as guider.md
    /// § `POST /api/v1/calibrate` and § Error envelope say.
    pub async fn calibrate(&self) -> Result<CalibrateAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let turn = self.take_turn(&mut stops).await?;
        self.not_in_a_run()?;
        let looping = self.looping();
        let devices = self.devices_for_a_start(&mut stops).await?;
        let start = mount::ready(&*devices.mount).await?;
        let (reply, outcome) = oneshot::channel();
        let run = CalibrationRun {
            mount: Arc::clone(&devices.mount),
            start,
            reply,
            return_to: if looping {
                Return::Looping
            } else {
                Return::Idle
            },
            turn,
        };
        if looping {
            let running = self.running.lock();
            if stops.has_changed().unwrap_or(true) {
                return Err(ApiError::stopped());
            }
            running
                .as_ref()
                .filter(|running| self.takes_frames(running))
                .ok_or_else(ApiError::stopped)?
                .commands
                .send(Command::Calibrate(Box::new(run)))
                .map_err(|_| ApiError::stopped())?;
        } else {
            self.spawn(
                self.setup(devices, None, Some(run), None),
                &stops,
                State::Calibrating,
                false,
            )?;
        }
        let answer = self.answer(outcome, &mut stops).await??;
        Ok(CalibrateAnswer {
            state: answer.state,
            calibration: RecordBody::from(&answer.record),
        })
    }

    /// `POST /api/v1/guiding/start`.
    ///
    /// # Errors
    ///
    /// `invalid_request`, `stopped`, `not_configured`, `device_unreachable`,
    /// `device_not_connected`, `camera_unsupported`, `mount_not_ready`,
    /// `no_stars`, `calibration_failed`, `settle_timeout` and
    /// `guide_failed`, as guider.md § `POST /api/v1/guiding/start` and
    /// § Error envelope say.
    pub async fn start_guiding(&self, body: StartBody) -> Result<SettledAnswer, ApiError> {
        let settle = body.settle.resolve(self.default_settle())?;
        let mut stops = self.stops.subscribe();
        let turn = self.take_turn(&mut stops).await?;
        self.start_on_turn(turn, body.recalibrate, settle, &mut stops)
            .await
    }

    /// A `guiding/start` once it has its turn, which it hands the loop.
    async fn start_on_turn(
        &self,
        turn: Turn,
        recalibrate: bool,
        settle: SettleSpec,
        stops: &mut watch::Receiver<u64>,
    ) -> Result<SettledAnswer, ApiError> {
        let (mount, devices) = self.mount_for_a_start(stops).await?;
        let (reply, answer) = oneshot::channel();
        let run = GuidingRun {
            mount,
            recalibrate,
            settle,
            reply,
            turn,
        };
        match devices {
            None => self.send(Command::Start(Box::new(run)), stops, ApiError::stopped)?,
            Some(devices) => self.spawn(
                self.setup(devices, None, None, Some(run)),
                stops,
                State::Guiding,
                false,
            )?,
        }
        self.answer(answer, stops).await?
    }

    /// What a start reads of the mount and the devices: nothing in a run,
    /// which guides with the run's mount; the mount from `looping`; the
    /// mount and the devices the loop starts on from `idle`.
    async fn mount_for_a_start(
        &self,
        stops: &mut watch::Receiver<u64>,
    ) -> Result<(Option<StartMount>, Option<StartDevices>), ApiError> {
        if self.status.lock().state.is_run() {
            return Ok((None, None));
        }
        let looping = self.looping();
        let devices = self.devices_for_a_start(stops).await?;
        let start = mount::ready(&*devices.mount).await?;
        let mount = Some((Arc::clone(&devices.mount), start));
        Ok((mount, (!looping).then_some(devices)))
    }

    /// `POST /api/v1/guiding/pause`.
    ///
    /// # Errors
    ///
    /// `stopped` and `not_guiding`.
    pub async fn pause(&self, body: PauseBody) -> Result<StateAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        if !matches!(
            self.status.lock().state,
            State::Guiding | State::StarLost | State::Paused
        ) {
            return Err(ApiError::not_guiding());
        }
        let (reply, answer) = oneshot::channel();
        self.send(
            Command::Pause {
                full: body.full,
                reply,
            },
            &stops,
            ApiError::not_guiding,
        )?;
        let state = self.answer(answer, &mut stops).await??;
        Ok(StateAnswer { state })
    }

    /// `POST /api/v1/guiding/resume`.
    ///
    /// # Errors
    ///
    /// `stopped` and `not_guiding`.
    pub async fn resume(&self) -> Result<StateAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        let state = self.status.lock().state;
        match state {
            State::Guiding | State::StarLost => Ok(StateAnswer { state }),
            State::Paused => {
                let (reply, answer) = oneshot::channel();
                self.send(Command::Resume(reply), &stops, ApiError::not_guiding)?;
                let state = self.answer(answer, &mut stops).await??;
                Ok(StateAnswer { state })
            }
            State::Idle | State::Looping | State::Calibrating => Err(ApiError::not_guiding()),
        }
    }

    /// `POST /api/v1/dither`.
    ///
    /// # Errors
    ///
    /// `invalid_request`, `stopped`, `not_guiding`, `settle_timeout`,
    /// `dither_aborted` and `guide_failed`.
    pub async fn dither(&self, body: DitherBody) -> Result<SettledAnswer, ApiError> {
        let amount = body.amount()?;
        let settle = body.settle.resolve(self.default_settle())?;
        let mut stops = self.stops.subscribe();
        let (reply, answer) = oneshot::channel();
        let run = DitherRun {
            amount,
            ra_only: body.ra_only,
            settle,
            reply,
            turn: self.take_turn(&mut stops).await?,
        };
        if self.status.lock().state != State::Guiding {
            return Err(ApiError::not_guiding());
        }
        self.send(
            Command::Dither(Box::new(run)),
            &stops,
            ApiError::not_guiding,
        )?;
        self.answer(answer, &mut stops).await?
    }

    /// `POST /api/v1/dither/abort`: takes no turn.
    pub async fn abort_dither(&self) -> AbortAnswer {
        let state = self.status.lock().state;
        let (reply, answer) = oneshot::channel();
        let sent = {
            let running = self.running.lock();
            running
                .as_ref()
                .filter(|running| alive(running))
                .is_some_and(|running| running.commands.send(Command::AbortDither(reply)).is_ok())
        };
        let unaborted = AbortAnswer {
            state,
            aborted: false,
        };
        if !sent {
            return unaborted;
        }
        answer.await.unwrap_or(unaborted)
    }

    /// `POST /api/v1/calibration/clear`.
    ///
    /// # Errors
    ///
    /// `stopped`, `not_idle`, and `internal` when the store's file cannot be
    /// written.
    pub async fn clear_calibration(&self) -> Result<StateAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        self.not_in_a_run()?;
        self.store
            .clear()
            .map_err(|error| ApiError::new(ErrorCode::Internal, error))?;
        Ok(StateAnswer {
            state: self.status.lock().state,
        })
    }

    /// `GET /api/v1/calibration`.
    #[must_use]
    pub fn calibration(&self) -> StoreBody {
        self.store.body()
    }

    /// `POST /api/v1/star/reselect`.
    ///
    /// # Errors
    ///
    /// `stopped`, `not_exposing`, `no_stars` and `guide_failed`.
    pub async fn reselect(&self) -> Result<SelectionAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        let (reply, selection) = oneshot::channel();
        {
            let running = self.running.lock();
            if stops.has_changed().unwrap_or(true) {
                return Err(ApiError::stopped());
            }
            let not_exposing =
                || ApiError::new(ErrorCode::NotExposing, "the guider takes no frames");
            running
                .as_ref()
                .filter(|running| alive(running) && self.status.lock().exposing)
                .ok_or_else(not_exposing)?
                .commands
                .send(Command::Reselect(reply))
                .map_err(|_| not_exposing())?;
        }
        let star_count = self.answer(selection, &mut stops).await??;
        Ok(SelectionAnswer {
            state: self.status.lock().state,
            star_count,
        })
    }

    /// `POST /api/v1/guiding/stop`.
    ///
    /// # Errors
    ///
    /// `stop_timeout` when the loop has not ended within `stop_timeout`.
    pub async fn stop(&self) -> Result<StateAnswer, ApiError> {
        let done = {
            let running = self.running.lock();
            self.stops
                .send_modify(|count| *count = count.wrapping_add(1));
            running.as_ref().map(|running| {
                running.cancel.cancel();
                running.done.clone()
            })
        };
        if let Some(mut done) = done {
            let timeout = self.config.stop_timeout.get();
            if tokio::time::timeout(timeout, done.wait_for(|done| *done))
                .await
                .is_err()
            {
                return Err(ApiError::new(
                    ErrorCode::StopTimeout,
                    format!(
                        "the guider's exposure had not ended {} after the stop",
                        humantime::format_duration(timeout)
                    ),
                ));
            }
        }
        Ok(StateAnswer { state: State::Idle })
    }

    /// `GET /api/v1/guiding/stats`.
    #[must_use]
    pub fn stats(&self) -> StatsBody {
        let pixel_scale = self.pixel_scale();
        let status = self.status.lock();
        let latest = status.latest.as_ref().map(|latest| latest.metrics);
        let settled = status.settled(pixel_scale);
        let totals = status.totals;
        StatsBody {
            state: status.state,
            guiding: status.state == State::Guiding,
            settling: status.settling,
            pixel_scale_arcsec: pixel_scale,
            rms_ra_px: settled.rms_ra_px,
            rms_dec_px: settled.rms_dec_px,
            total_rms_px: settled.total_rms_px,
            rms_ra_arcsec: settled.rms_ra_arcsec,
            rms_dec_arcsec: settled.rms_dec_arcsec,
            total_rms_arcsec: settled.total_rms_arcsec,
            snr: latest.and_then(|latest| latest.snr),
            star_mass: latest.and_then(|latest| latest.star_mass),
            star_count: latest.map(|latest| latest.star_count),
            sample_count: settled.sample_count,
            last_stop: status.last_stop.as_ref().map(|last| LastStopBody {
                reason: last.stop.reason.as_str(),
                at: humantime::format_rfc3339_seconds(last.at).to_string(),
                message: last.stop.message.clone(),
            }),
            totals: TotalsBody {
                epoch: self.epoch.clone(),
                guide_steps: totals.guide_steps,
                ra_samples: totals.ra.count(),
                ra_sum_sq_px2: totals.ra.sum_of_squares(),
                dec_samples: totals.dec.count(),
                dec_sum_sq_px2: totals.dec.sum_of_squares(),
                star_lost: totals.star_lost,
                interruptions: totals.interruptions,
            },
        }
    }

    /// `GET /api/v1/guiding/metrics`.
    #[must_use]
    pub fn metrics(&self) -> MetricsBody {
        let status = self.status.lock();
        MetricsBody {
            state: status.state,
            guiding: status.state == State::Guiding,
            epoch: self.epoch.clone(),
            frames: status.metrics.iter().copied().collect(),
        }
    }

    /// The latest measured frame, for `GET /api/v1/frame/latest`.
    ///
    /// # Errors
    ///
    /// `no_frame` when no frame has been measured since the loop last
    /// started.
    pub fn latest_frame(&self) -> Result<Arc<LatestFrame>, ApiError> {
        self.status.lock().latest.clone().ok_or_else(|| {
            ApiError::new(
                ErrorCode::NoFrame,
                "no frame has been measured since the loop last started",
            )
        })
    }

    /// `not_idle` in a run state.
    fn not_in_a_run(&self) -> Result<(), ApiError> {
        let state = self.status.lock().state;
        if state.is_run() {
            return Err(ApiError::new(
                ErrorCode::NotIdle,
                format!("the guider is in a run: {}", state_name(state)),
            ));
        }
        Ok(())
    }

    /// The devices a request that starts the loop or a run reads afresh,
    /// once a loop that is ending has ended.
    async fn devices_for_a_start(
        &self,
        stops: &mut watch::Receiver<u64>,
    ) -> Result<StartDevices, ApiError> {
        let (Some(camera_client), Some(mount_client), Some(_)) = (
            self.camera.as_ref(),
            self.mount.as_ref(),
            self.config.optics.focal_length_mm,
        ) else {
            return Err(ApiError::new(
                ErrorCode::NotConfigured,
                "the configuration names no camera, no mount or no focal length",
            ));
        };
        if !self.looping() {
            self.ended(stops).await?;
        }
        let (camera, mount) = tokio::join!(
            camera_client.find::<CameraKind>(),
            mount_client.find::<MountKind>()
        );
        let camera = connected::<CameraKind>(camera, camera_client.device_number())?;
        let mount = connected::<MountKind>(mount, mount_client.device_number())?;
        let sensor = self.camera_sensor(&camera).await?;
        let reading = sensor::reading(&sensor, self.config.camera.binning.get())
            .map_err(|error| ApiError::new(ErrorCode::CameraUnsupported, error.to_string()))?;
        Ok(StartDevices {
            client: Arc::clone(camera_client),
            camera,
            sensor,
            reading,
            mount,
        })
    }

    /// What a loop starts with.
    fn setup(
        &self,
        devices: StartDevices,
        first: Option<oneshot::Sender<usize>>,
        calibration: Option<CalibrationRun>,
        start: Option<GuidingRun>,
    ) -> LoopSetup {
        LoopSetup {
            config: Arc::clone(&self.config),
            client: devices.client,
            camera: devices.camera,
            sensor: devices.sensor,
            reading: devices.reading,
            status: Arc::clone(&self.status),
            devices: self.devices.clone(),
            store: Arc::clone(&self.store),
            first,
            calibration,
            start,
        }
    }

    /// Whether a loop runs that no stop has ended or is ending.
    fn looping(&self) -> bool {
        self.running
            .lock()
            .as_ref()
            .is_some_and(|running| self.takes_frames(running))
    }

    /// Whether `running` loops: it is alive, in `looping`.
    fn takes_frames(&self, running: &Running) -> bool {
        alive(running) && self.status.lock().state == State::Looping
    }

    /// Send `command` to the loop while it is alive, unless a stop arrived
    /// since the request did; `refused` when no loop is alive.
    fn send(
        &self,
        command: Command,
        stops: &watch::Receiver<u64>,
        refused: impl FnOnce() -> ApiError,
    ) -> Result<(), ApiError> {
        let running = self.running.lock();
        if stops.has_changed().unwrap_or(true) {
            return Err(ApiError::stopped());
        }
        running
            .as_ref()
            .filter(|running| alive(running))
            .ok_or_else(refused)?
            .commands
            .send(command)
            .map_err(|_| ApiError::stopped())
    }

    /// The configuration's `settling`, as a settle's terms.
    fn default_settle(&self) -> SettleSpec {
        SettleSpec {
            tolerance: Size::Pixels(self.config.settling.pixels.get()),
            time: self.config.settling.time.get(),
            timeout: self.config.settling.timeout.get(),
        }
    }

    /// The pixel scale `stats` reports.
    fn pixel_scale(&self) -> Option<f64> {
        let sensor = self
            .devices
            .borrow()
            .camera
            .as_ref()
            .and_then(|camera| camera.statics);
        sensor::pixel_scale(&self.config, sensor.as_ref())
    }

    /// Take a turn behind the mutating requests ahead, unless a stop
    /// arrives first.
    async fn take_turn(&self, stops: &mut watch::Receiver<u64>) -> Result<Turn, ApiError> {
        // Biased: a stop that arrived by the time the turn comes wins.
        tokio::select! {
            biased;
            _ = stops.changed() => Err(ApiError::stopped()),
            turn = Arc::clone(&self.turns).lock_owned() => Ok(turn),
        }
    }

    /// Wait for a loop that is ending to end.
    async fn ended(&self, stops: &mut watch::Receiver<u64>) -> Result<(), ApiError> {
        let done = self
            .running
            .lock()
            .as_ref()
            .map(|running| running.done.clone());
        if let Some(mut done) = done {
            tokio::select! {
                biased;
                _ = stops.changed() => return Err(ApiError::stopped()),
                _ = done.wait_for(|done| *done) => {}
            }
        }
        Ok(())
    }

    /// The camera's static properties: the poll's while they are current,
    /// read afresh otherwise.
    async fn camera_sensor(&self, camera: &Arc<dyn Camera>) -> Result<CameraSensor, ApiError> {
        let polled = self
            .devices
            .borrow()
            .camera
            .as_ref()
            .filter(|state| state.statics_current && state.reach == Reach::Connected)
            .and_then(|state| state.statics);
        if let Some(sensor) = polled {
            return Ok(sensor);
        }
        camera_sensor(Arc::clone(camera)).await.map_err(|error| {
            ApiError::new(
                ErrorCode::DeviceUnreachable,
                format!("the camera's static properties could not be read: {error}"),
            )
        })
    }

    /// Start the loop in `state`, unless a stop arrived since the request
    /// did; the metrics window is cleared when `clear_metrics`.
    fn spawn(
        &self,
        setup: LoopSetup,
        stops: &watch::Receiver<u64>,
        state: State,
        clear_metrics: bool,
    ) -> Result<(), ApiError> {
        let mut running = self.running.lock();
        if stops.has_changed().unwrap_or(true) {
            return Err(ApiError::stopped());
        }
        let cancel = CancellationToken::new();
        let (commands, received) = mpsc::unbounded_channel();
        let (ended, done) = watch::channel(false);
        self.status.lock().loop_started(state, clear_metrics);
        let task = tokio::spawn(camera_loop::run(setup, cancel.clone(), received));
        let ended_abnormally = self.ended_abnormally.clone();
        tokio::spawn(async move {
            if let Err(join) = task.await {
                error!(error = %join, "the camera loop's task ended without a stop");
                ended_abnormally.cancel();
            }
            ended.send_replace(true);
        });
        *running = Some(Running {
            cancel,
            commands,
            done,
        });
        drop(running);
        debug!("the camera loop spawned");
        Ok(())
    }

    /// The loop's answer to a request, or why it has none.
    async fn answer<T>(
        &self,
        answer: oneshot::Receiver<T>,
        stops: &mut watch::Receiver<u64>,
    ) -> Result<T, ApiError> {
        tokio::select! {
            biased;
            _ = stops.changed() => Err(ApiError::stopped()),
            answer = answer => answer.map_err(|_| {
                // The loop ended without answering, its stop recorded.
                match &self.status.lock().last_stop {
                    Some(last) if last.stop.reason != StopReason::Requested => {
                        ApiError::guide_failed(&last.stop)
                    }
                    _ => ApiError::stopped(),
                }
            }),
        }
    }
}

/// The mount as a start from `idle` or `looping` read it.
type StartMount = (Arc<dyn ascom_alpaca::api::Telescope>, mount::MountStart);

/// What a request that starts the loop or a run read of the devices.
struct StartDevices {
    client: Arc<DeviceClient>,
    camera: Arc<dyn Camera>,
    sensor: CameraSensor,
    reading: sensor::Reading,
    mount: Arc<dyn ascom_alpaca::api::Telescope>,
}

/// `state` as the API names it.
const fn state_name(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Looping => "looping",
        State::Calibrating => "calibrating",
        State::Guiding => "guiding",
        State::Paused => "paused",
        State::StarLost => "star_lost",
    }
}

/// Whether `running` is alive: it has not stopped, and no stop is ending
/// it, as one that waits for its exposure to end after a `stop_timeout`
/// is.
fn alive(running: &Running) -> bool {
    !running.cancel.is_cancelled() && !*running.done.borrow()
}

/// The device a fresh read found, when it is connected.
fn connected<K: Kind>(
    found: Found<K::Device>,
    device_number: u32,
) -> Result<Arc<K::Device>, ApiError> {
    let unreachable = |message: String| ApiError::new(ErrorCode::DeviceUnreachable, message);
    match found {
        Found::NoServer(error)
        | Found::Listed {
            connected: Err(error),
            ..
        } => Err(unreachable(format!(
            "the {} does not answer: {error}",
            K::NAME
        ))),
        Found::NotListed => Err(unreachable(format!(
            "the {}'s server lists no {} at device number {device_number}",
            K::NAME,
            K::LISTED_AS
        ))),
        Found::Listed {
            connected: Ok(false),
            ..
        } => Err(ApiError::new(
            ErrorCode::DeviceNotConnected,
            format!("the {} is not connected", K::NAME),
        )),
        Found::Listed {
            device,
            connected: Ok(true),
            ..
        } => Ok(device),
    }
}

/// `GET /api/v1/guiding/stats`.
#[derive(Debug, Serialize)]
pub struct StatsBody {
    state: State,
    guiding: bool,
    settling: bool,
    pixel_scale_arcsec: Option<f64>,
    rms_ra_px: Option<f64>,
    rms_dec_px: Option<f64>,
    total_rms_px: Option<f64>,
    rms_ra_arcsec: Option<f64>,
    rms_dec_arcsec: Option<f64>,
    total_rms_arcsec: Option<f64>,
    snr: Option<f64>,
    star_mass: Option<f64>,
    star_count: Option<usize>,
    sample_count: usize,
    last_stop: Option<LastStopBody>,
    totals: TotalsBody,
}

#[derive(Debug, Serialize)]
struct LastStopBody {
    reason: &'static str,
    at: String,
    message: String,
}

/// The running totals (guider.md § Statistics).
#[derive(Debug, Serialize)]
struct TotalsBody {
    epoch: String,
    guide_steps: u64,
    ra_samples: u64,
    ra_sum_sq_px2: f64,
    dec_samples: u64,
    dec_sum_sq_px2: f64,
    star_lost: u64,
    interruptions: u64,
}

/// `GET /api/v1/guiding/metrics`.
#[derive(Debug, Serialize)]
pub struct MetricsBody {
    state: State,
    guiding: bool,
    epoch: String,
    frames: Vec<FrameMetrics>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_camera::FakeCamera;

    fn guider() -> Guider {
        let config: Config = serde_json::from_value(serde_json::json!({
            "stop_timeout": "1s"
        }))
        .unwrap();
        let (_, devices) = watch::channel(DevicesState::default());
        Guider::new(Arc::new(config), None, None, devices, store())
    }

    /// A store in a directory of its own, which the test leaves behind for
    /// the system to clean up.
    fn store() -> Arc<CalibrationStore> {
        let dir = tempfile::tempdir().unwrap().keep();
        Arc::new(CalibrationStore::open(&dir))
    }

    fn summary(star_count: usize) -> FrameSummary {
        FrameSummary {
            star_count,
            star_lost: false,
            hfd: Some(2.5),
            snr: Some(30.0),
            flux: Some(5_000.0),
        }
    }

    fn frame() -> Frame {
        let pixels = ndarray::Array2::<u16>::from_elem((4, 4), 100);
        Frame::new(&pixels.view(), rp_guiding::SensorType::Monochrome, None).unwrap()
    }

    #[test]
    fn the_metrics_window_keeps_the_last_fifty_frames_numbered_from_the_start() {
        let mut status = Status::new();
        for _ in 0..60 {
            status.record(summary(3), frame(), Vec::new());
        }
        assert_eq!(status.metrics.len(), METRICS_WINDOW);
        assert_eq!(status.metrics.front().unwrap().frame, 11);
        assert_eq!(status.metrics.back().unwrap().frame, 60);
    }

    #[test]
    fn a_loop_from_idle_clears_the_window_and_the_latest_frame_and_the_count_goes_on() {
        let mut status = Status::new();
        status.record(summary(3), frame(), Vec::new());
        status.loop_started(State::Looping, true);
        assert!(status.metrics.is_empty());
        assert!(status.latest.is_none());
        status.record(summary(2), frame(), Vec::new());
        assert_eq!(status.metrics.back().unwrap().frame, 2);
        assert_eq!(status.star_count(), 2);
    }

    #[test]
    fn a_stop_returns_to_idle_and_is_the_last_stop() {
        let mut status = Status::new();
        status.loop_started(State::Looping, true);
        status.stopped(Stop::requested());
        assert_eq!(status.state, State::Idle);
        assert_eq!(status.last_stop.unwrap().stop.reason, StopReason::Requested);
    }

    #[test]
    fn a_run_that_stops_counts_an_interruption_and_a_loop_does_not() {
        let mut status = Status::new();
        status.loop_started(State::Looping, true);
        status.stopped(Stop::requested());
        assert_eq!(status.totals.interruptions, 0);
        status.loop_started(State::Guiding, true);
        status.stopped(Stop::requested());
        assert_eq!(status.totals.interruptions, 1);
    }

    #[test]
    fn the_rms_window_holds_the_last_guide_steps() {
        let mut status = Status::new();
        status.guide_step(10.0, Some(10.0));
        for _ in 0..RMS_WINDOW {
            status.guide_step(3.0, Some(4.0));
        }
        let settled = status.settled(Some(2.0));
        assert_eq!(settled.sample_count, RMS_WINDOW);
        assert_eq!(settled.rms_ra_px, Some(3.0));
        assert_eq!(settled.total_rms_px, Some(5.0));
        assert_eq!(settled.total_rms_arcsec, Some(10.0));
        assert_eq!(status.totals.guide_steps, 51);
        assert_eq!(status.totals.dec.count(), 51);
    }

    #[test]
    fn a_window_with_a_step_that_did_not_correct_dec_has_no_dec_or_total_rms() {
        let mut status = Status::new();
        status.guide_step(1.0, Some(1.0));
        status.guide_step(1.0, None);
        let settled = status.settled(None);
        assert_eq!(settled.rms_ra_px, Some(1.0));
        assert_eq!((settled.rms_dec_px, settled.total_rms_px), (None, None));
        assert_eq!(settled.rms_ra_arcsec, None);
        assert_eq!(status.totals.dec.count(), 1);
    }

    #[test]
    fn an_accepted_start_clears_the_rms_and_metrics_windows_and_not_the_totals() {
        let mut status = Status::new();
        status.guide_step(1.0, None);
        status.start_accepted();
        assert_eq!(status.settled(None).sample_count, 0);
        assert_eq!(status.totals.guide_steps, 1);
    }

    #[tokio::test]
    async fn a_pause_a_resume_or_a_dither_in_idle_answers_not_guiding() {
        let guider = guider();
        let pause = guider.pause(PauseBody::default()).await.unwrap_err();
        assert_eq!(pause.code, ErrorCode::NotGuiding);
        assert_eq!(
            guider.resume().await.unwrap_err().code,
            ErrorCode::NotGuiding
        );
        let body: DitherBody = serde_json::from_str(r#"{"amount_px": 4.0}"#).unwrap();
        assert_eq!(
            guider.dither(body).await.unwrap_err().code,
            ErrorCode::NotGuiding
        );
    }

    #[tokio::test]
    async fn a_dither_or_a_start_it_cannot_read_is_refused_before_its_turn() {
        let guider = guider();
        let _held = guider.turns.lock().await;
        let dither: DitherBody = serde_json::from_str("{}").unwrap();
        assert_eq!(
            guider.dither(dither).await.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let start: StartBody =
            serde_json::from_str(r#"{"settle": {"pixels": 1.0, "arcsec": 2.0}}"#).unwrap();
        assert_eq!(
            guider.start_guiding(start).await.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn an_abort_with_no_loop_answers_unaborted_at_once() {
        let guider = guider();
        let _held = guider.turns.lock().await;
        assert_eq!(
            guider.abort_dither().await,
            AbortAnswer {
                state: State::Idle,
                aborted: false
            }
        );
    }

    #[tokio::test]
    async fn a_stop_in_idle_answers_idle_and_records_no_stop() {
        let guider = guider();
        assert_eq!(
            guider.stop().await.unwrap(),
            StateAnswer { state: State::Idle }
        );
        assert!(guider.status.lock().last_stop.is_none());
    }

    #[tokio::test]
    async fn a_request_queued_behind_a_turn_answers_stopped_when_a_stop_arrives() {
        let guider = Arc::new(guider());
        let held = guider.turns.lock().await;
        let queued = tokio::spawn({
            let guider = Arc::clone(&guider);
            async move { guider.start_loop().await }
        });
        // The queued request has subscribed to the stops once it waits on
        // the turn: the stop count's receivers say so.
        while guider.stops.receiver_count() < 1 {
            tokio::task::yield_now().await;
        }
        guider.stop().await.unwrap();
        drop(held);
        assert_eq!(queued.await.unwrap().unwrap_err().code, ErrorCode::Stopped);
    }

    #[tokio::test]
    async fn a_request_that_arrives_after_a_stop_is_not_stopped_by_it() {
        let guider = guider();
        guider.stop().await.unwrap();
        // With nothing configured the loop is refused for that, not for the
        // stop before it.
        assert_eq!(
            guider.start_loop().await.unwrap_err().code,
            ErrorCode::NotConfigured
        );
    }

    #[tokio::test]
    async fn requests_that_need_idle_or_looping_answer_not_idle_in_a_run() {
        let guider = guider();
        guider.status.lock().set_state(State::Calibrating);
        for error in [
            guider.clear_calibration().await.unwrap_err(),
            guider.calibrate().await.unwrap_err(),
            guider.start_loop().await.unwrap_err(),
        ] {
            assert_eq!(
                (error.code, error.message.as_str()),
                (ErrorCode::NotIdle, "the guider is in a run: calibrating")
            );
        }
    }

    #[tokio::test]
    async fn clearing_in_idle_empties_the_store_and_answers_idle() {
        let guider = guider();
        assert_eq!(
            guider.clear_calibration().await.unwrap(),
            StateAnswer { state: State::Idle }
        );
        assert_eq!(guider.calibration(), StoreBody::default());
    }

    #[tokio::test]
    async fn a_reselect_in_idle_is_refused_with_not_exposing() {
        assert_eq!(
            guider().reselect().await.unwrap_err().code,
            ErrorCode::NotExposing
        );
    }

    #[tokio::test]
    async fn an_answer_the_loop_never_gives_after_its_own_stop_is_guide_failed() {
        let guider = guider();
        guider
            .status
            .lock()
            .stopped(Stop::camera_error("3 frames failed".to_owned()));
        let (reply, answer) = oneshot::channel::<usize>();
        drop(reply);
        let mut stops = guider.stops.subscribe();
        let error = guider.answer(answer, &mut stops).await.unwrap_err();
        assert_eq!(
            (error.code, error.stop_reason()),
            (ErrorCode::GuideFailed, Some(StopReason::CameraError))
        );
    }

    #[tokio::test]
    async fn an_answer_the_loop_never_gives_after_a_requested_stop_is_stopped() {
        let guider = guider();
        guider.status.lock().stopped(Stop::requested());
        let (reply, answer) = oneshot::channel::<usize>();
        drop(reply);
        let mut stops = guider.stops.subscribe();
        let error = guider.answer(answer, &mut stops).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Stopped);
    }

    /// A loop in `running`, which the test ends with the sender, and the
    /// requests it would serve.
    fn install_loop(guider: &Guider) -> (watch::Sender<bool>, mpsc::UnboundedReceiver<Command>) {
        let (ended, done) = watch::channel(false);
        let (commands, received) = mpsc::unbounded_channel();
        guider.status.lock().loop_started(State::Looping, true);
        *guider.running.lock() = Some(Running {
            cancel: CancellationToken::new(),
            commands,
            done,
        });
        (ended, received)
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_that_a_stop_is_ending_takes_no_frames() {
        let guider = guider();
        let (_ended, mut received) = install_loop(&guider);
        assert!(guider.looping());
        guider.running.lock().as_ref().unwrap().cancel.cancel();
        assert!(!guider.looping());
        let answer = tokio::time::timeout(Duration::from_secs(1), guider.reselect()).await;
        assert_eq!(
            answer.expect("the reselect answered").unwrap_err().code,
            ErrorCode::NotExposing
        );
        assert!(received.try_recv().is_err(), "the ending loop was asked");
    }

    #[tokio::test]
    async fn an_abort_to_a_loop_a_stop_is_ending_answers_unaborted_at_once() {
        let guider = guider();
        let (_ended, mut received) = install_loop(&guider);
        guider.running.lock().as_ref().unwrap().cancel.cancel();
        let answer = tokio::time::timeout(Duration::from_secs(1), guider.abort_dither()).await;
        assert!(!answer.expect("the abort answered").aborted);
        assert!(received.try_recv().is_err(), "the ending loop was asked");
    }

    #[tokio::test]
    async fn a_loop_that_ended_takes_no_frames() {
        let guider = guider();
        let (ended, _received) = install_loop(&guider);
        ended.send_replace(true);
        assert!(!guider.looping());
    }

    #[tokio::test]
    async fn a_reselect_while_looping_asks_the_loop() {
        let guider = Arc::new(guider());
        let (_ended, mut received) = install_loop(&guider);
        let request = tokio::spawn({
            let guider = Arc::clone(&guider);
            async move { guider.reselect().await }
        });
        let Some(Command::Reselect(reply)) = received.recv().await else {
            panic!("the loop was not asked");
        };
        reply.send(Ok(4)).unwrap();
        assert_eq!(
            request.await.unwrap().unwrap(),
            SelectionAnswer {
                state: State::Looping,
                star_count: 4
            }
        );
    }

    #[tokio::test]
    async fn the_static_properties_the_poll_read_since_the_camera_came_back_are_used() {
        let camera: Arc<dyn Camera> = Arc::new(FakeCamera::default());
        let polled = CameraSensor {
            pixel_size_um: 3.75,
            ..sensor_of(2.0)
        };
        let guider = guider_polled(polled, true);
        assert_eq!(guider.camera_sensor(&camera).await.unwrap(), polled);
    }

    #[tokio::test]
    async fn static_properties_the_poll_read_before_the_camera_came_back_are_read_afresh() {
        let fake = Arc::new(FakeCamera::default());
        let camera: Arc<dyn Camera> = Arc::clone(&fake) as Arc<dyn Camera>;
        let guider = guider_polled(
            CameraSensor {
                pixel_size_um: 3.75,
                ..sensor_of(2.0)
            },
            false,
        );
        assert_eq!(guider.camera_sensor(&camera).await.unwrap(), sensor_of(2.0));
        assert!(fake.static_reads.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    /// The fake camera's static properties, at `pixel_size_um`.
    fn sensor_of(pixel_size_um: f64) -> CameraSensor {
        CameraSensor {
            sensor_type: ascom_alpaca::api::camera::SensorType::Monochrome,
            pixel_size_um,
            width: 320,
            height: 240,
            max_adu: 65_535,
            max_bin: 2,
            can_abort_exposure: true,
        }
    }

    /// A guider whose poll found the camera connected with `statics`.
    fn guider_polled(statics: CameraSensor, current: bool) -> Guider {
        let (_, devices) = watch::channel(DevicesState {
            camera: Some(crate::devices::CameraState {
                reach: Reach::Connected,
                unique_id: None,
                statics: Some(statics),
                statics_current: current,
            }),
            mount: None,
        });
        Guider::new(Arc::new(Config::default()), None, None, devices, store())
    }

    #[tokio::test]
    async fn a_loop_request_while_a_stop_ends_the_loop_does_not_answer_with_its_values() {
        let guider = Arc::new(guider());
        let (ended, _received) = install_loop(&guider);
        guider.running.lock().as_ref().unwrap().cancel.cancel();
        let request = tokio::spawn({
            let guider = Arc::clone(&guider);
            async move { guider.start_loop().await }
        });
        // With nothing configured, a request that goes on to start a new
        // loop is refused for that.
        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::NotConfigured);
        drop(ended);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_whose_loop_does_not_end_answers_stop_timeout() {
        let guider = guider();
        let (_ended, _received) = install_loop(&guider);
        let error = guider.stop().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::StopTimeout);
    }

    #[tokio::test]
    async fn the_stats_of_a_guider_that_has_measured_nothing() {
        let guider = guider();
        let stats = serde_json::to_value(guider.stats()).unwrap();
        assert_eq!(stats["state"], "idle");
        assert_eq!(stats["star_count"], serde_json::Value::Null);
        assert_eq!(stats["pixel_scale_arcsec"], serde_json::Value::Null);
        assert_eq!(stats["totals"]["epoch"], guider.epoch.as_str());
        let metrics = serde_json::to_value(guider.metrics()).unwrap();
        assert_eq!(metrics["frames"], serde_json::json!([]));
        assert_eq!(metrics["epoch"], guider.epoch.as_str());
    }

    #[test]
    fn a_device_that_does_not_answer_or_is_not_listed_is_unreachable() {
        let not_listed = connected::<MountKind>(Found::NotListed, 2).err().unwrap();
        assert_eq!(not_listed.code, ErrorCode::DeviceUnreachable);
        assert_eq!(
            not_listed.message,
            "the mount's server lists no telescope at device number 2"
        );
        let no_server =
            connected::<CameraKind>(Found::NoServer(crate::devices::CallError::Deadline), 0)
                .err()
                .unwrap();
        assert_eq!(no_server.code, ErrorCode::DeviceUnreachable);
        assert!(no_server.message.starts_with("the camera does not answer"));
    }
}

//! The guider's state and its requests (docs/services/guider.md § States,
//! § Concurrency, § HTTP API): `loop`, `star/reselect` and `guiding/stop`,
//! and the reads `stats`, `metrics` and `frame/latest` answer from.
//!
//! Mutating requests take turns behind one mutex, and queue. A
//! `guiding/stop` takes no turn: it cancels the loop, and every mutating
//! request waiting or queued when it arrives answers `stopped`, since each
//! watches the count of stops from its arrival. A loop is started under the
//! same lock a stop cancels under, so nothing accepted before a stop runs
//! after it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use ascom_alpaca::api::Camera;
use parking_lot::Mutex;
use rp_guiding::{Frame, FrameMeasurement, Position};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::api::{ApiError, ErrorCode, Stop, StopReason};
use crate::camera_loop::{self, Command, LoopSetup};
use crate::config::Config;
use crate::devices::{
    camera_sensor, CameraKind, CameraSensor, DeviceClient, DevicesState, Found, Kind, MountKind,
    Reach,
};
use crate::sensor;

/// The frames the metrics window holds.
const METRICS_WINDOW: usize = 50;

/// The guider's state (guider.md § States).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing runs.
    Idle,
    /// Frames are taken and measured.
    Looping,
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

/// What the loop reports and the reads answer from.
#[derive(Debug)]
pub struct Status {
    state: State,
    /// The frames measured since the service started.
    frames: u64,
    metrics: VecDeque<FrameMetrics>,
    latest: Option<Arc<LatestFrame>>,
    last_stop: Option<LastStop>,
}

impl Status {
    const fn new() -> Self {
        Self {
            state: State::Idle,
            frames: 0,
            metrics: VecDeque::new(),
            latest: None,
            last_stop: None,
        }
    }

    /// The loop starts from `idle`: the metrics window and the latest frame
    /// are cleared.
    fn loop_started(&mut self) {
        self.state = State::Looping;
        self.metrics.clear();
        self.latest = None;
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

    /// The loop stopped, its exposure ended.
    pub fn stopped(&mut self, stop: Stop) {
        self.state = State::Idle;
        self.last_stop = Some(LastStop {
            stop,
            at: SystemTime::now(),
        });
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

/// The guider.
pub struct Guider {
    config: Arc<Config>,
    camera: Option<Arc<DeviceClient>>,
    mount: Option<Arc<DeviceClient>>,
    devices: watch::Receiver<DevicesState>,
    /// The mutating requests' turns.
    turns: tokio::sync::Mutex<()>,
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
    ) -> Self {
        Self {
            config,
            camera,
            mount,
            devices,
            turns: tokio::sync::Mutex::new(()),
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
    /// `stopped`, `not_configured`, `device_unreachable`,
    /// `device_not_connected`, `camera_unsupported` and `guide_failed`, as
    /// guider.md § `POST /api/v1/loop` and § Error envelope say.
    pub async fn start_loop(&self) -> Result<SelectionAnswer, ApiError> {
        let mut stops = self.stops.subscribe();
        let _turn = self.take_turn(&mut stops).await?;
        if self.looping() {
            return Ok(SelectionAnswer {
                state: State::Looping,
                star_count: self.status.lock().star_count(),
            });
        }
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
        self.ended(&mut stops).await?;
        let (camera, mount) = tokio::join!(
            camera_client.find::<CameraKind>(),
            mount_client.find::<MountKind>()
        );
        let camera = connected::<CameraKind>(camera, camera_client.device_number())?;
        connected::<MountKind>(mount, mount_client.device_number())?;
        let sensor = self.camera_sensor(&camera).await?;
        let reading = sensor::reading(&sensor, self.config.camera.binning.get())
            .map_err(|error| ApiError::new(ErrorCode::CameraUnsupported, error.to_string()))?;
        let (first, first_frame) = oneshot::channel();
        self.spawn(
            LoopSetup {
                config: Arc::clone(&self.config),
                client: Arc::clone(camera_client),
                camera,
                sensor,
                reading,
                status: Arc::clone(&self.status),
                devices: self.devices.clone(),
                first,
            },
            &stops,
        )?;
        let star_count = self.answer(first_frame, &mut stops).await?;
        Ok(SelectionAnswer {
            state: State::Looping,
            star_count,
        })
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
                .filter(|running| self.takes_frames(running))
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
        let sensor = self
            .devices
            .borrow()
            .camera
            .as_ref()
            .and_then(|camera| camera.statics);
        let pixel_scale = sensor::pixel_scale(&self.config, sensor.as_ref());
        let status = self.status.lock();
        let latest = status.latest.as_ref().map(|latest| latest.metrics);
        StatsBody {
            state: status.state,
            guiding: false,
            settling: false,
            pixel_scale_arcsec: pixel_scale,
            rms_ra_px: None,
            rms_dec_px: None,
            total_rms_px: None,
            rms_ra_arcsec: None,
            rms_dec_arcsec: None,
            total_rms_arcsec: None,
            snr: latest.and_then(|latest| latest.snr),
            star_mass: latest.and_then(|latest| latest.star_mass),
            star_count: latest.map(|latest| latest.star_count),
            sample_count: 0,
            last_stop: status.last_stop.as_ref().map(|last| LastStopBody {
                reason: last.stop.reason.as_str(),
                at: humantime::format_rfc3339_seconds(last.at).to_string(),
                message: last.stop.message.clone(),
            }),
            totals: TotalsBody {
                epoch: self.epoch.clone(),
                guide_steps: 0,
                ra_samples: 0,
                ra_sum_sq_px2: 0.0,
                dec_samples: 0,
                dec_sum_sq_px2: 0.0,
                star_lost: 0,
                interruptions: 0,
            },
        }
    }

    /// `GET /api/v1/guiding/metrics`.
    #[must_use]
    pub fn metrics(&self) -> MetricsBody {
        let status = self.status.lock();
        MetricsBody {
            state: status.state,
            guiding: false,
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

    /// Whether a loop runs that no stop has ended or is ending.
    fn looping(&self) -> bool {
        self.running
            .lock()
            .as_ref()
            .is_some_and(|running| self.takes_frames(running))
    }

    /// Whether `running` takes frames: it has not stopped, and no stop is
    /// ending it, as one that waits for its exposure to end after a
    /// `stop_timeout` is.
    fn takes_frames(&self, running: &Running) -> bool {
        !running.cancel.is_cancelled()
            && !*running.done.borrow()
            && self.status.lock().state == State::Looping
    }

    /// Take a turn behind the mutating requests ahead, unless a stop
    /// arrives first.
    async fn take_turn(
        &self,
        stops: &mut watch::Receiver<u64>,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, ApiError> {
        // Biased: a stop that arrived by the time the turn comes wins.
        tokio::select! {
            biased;
            _ = stops.changed() => Err(ApiError::stopped()),
            turn = self.turns.lock() => Ok(turn),
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

    /// Start the loop, unless a stop arrived since the request did.
    fn spawn(&self, setup: LoopSetup, stops: &watch::Receiver<u64>) -> Result<(), ApiError> {
        let mut running = self.running.lock();
        if stops.has_changed().unwrap_or(true) {
            return Err(ApiError::stopped());
        }
        let cancel = CancellationToken::new();
        let (commands, received) = mpsc::unbounded_channel();
        let (ended, done) = watch::channel(false);
        self.status.lock().loop_started();
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
    sample_count: u32,
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
        Guider::new(Arc::new(config), None, None, devices)
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
        status.loop_started();
        assert!(status.metrics.is_empty());
        assert!(status.latest.is_none());
        status.record(summary(2), frame(), Vec::new());
        assert_eq!(status.metrics.back().unwrap().frame, 2);
        assert_eq!(status.star_count(), 2);
    }

    #[test]
    fn a_stop_returns_to_idle_and_is_the_last_stop() {
        let mut status = Status::new();
        status.loop_started();
        status.stopped(Stop::requested());
        assert_eq!(status.state, State::Idle);
        assert_eq!(status.last_stop.unwrap().stop.reason, StopReason::Requested);
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
            (error.code, error.stop_reason),
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
        guider.status.lock().loop_started();
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
        Guider::new(Arc::new(Config::default()), None, None, devices)
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

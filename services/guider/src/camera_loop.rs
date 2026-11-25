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

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::camera::ImageArray;
use ascom_alpaca::api::Camera;
use ndarray::Axis;
use parking_lot::Mutex;
use rp_guiding::{Frame, GuideStars, StarParams};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::api::{ApiError, ErrorCode, Stop};
use crate::config::Config;
use crate::devices::{
    call, camera_sensor, CallError, CameraKind, CameraSensor, DeviceClient, DevicesState, Found,
};
use crate::guider::{FrameSummary, Status};
use crate::sensor::{changed, FrameSettings, Reading};

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
    /// Answered with the guide stars of the first measured frame.
    pub first: oneshot::Sender<usize>,
}

/// A request a running loop serves.
#[derive(Debug)]
pub enum Command {
    /// Select the guide stars anew on the next frame, answering with how
    /// many were selected, or `no_stars`.
    Reselect(oneshot::Sender<Result<usize, ApiError>>),
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

/// A reselect's answer.
type ReselectReply = oneshot::Sender<Result<usize, ApiError>>;

/// Run the loop until `cancel` is cancelled or it stops by itself, then
/// end its exposure and report the stop.
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
        first,
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
        first: Some(first),
        reselect: None,
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
    let stop = loop {
        if let Some(stop) = cycle.turn(&cancel, &mut commands).await {
            break stop;
        }
    };
    cycle
        .exposer
        .end_exposure(cycle.sensor.can_abort_exposure)
        .await;
    info!(reason = stop.reason.as_str(), message = %stop.message, "the camera loop stopped");
    cycle.status.lock().stopped(stop);
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
    first: Option<oneshot::Sender<usize>>,
    reselect: Option<ReselectReply>,
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
    /// measurement. The stop when the loop ends.
    async fn turn(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Option<Stop> {
        if let Some(at) = self.retry_at.take() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Some(Stop::requested()),
                () = tokio::time::sleep_until(at) => {}
            }
        }
        self.attempt_started = Instant::now();
        if let Some(due) = self.settings_due {
            let written = tokio::select! {
                biased;
                () = cancel.cancelled() => return Some(Stop::requested()),
                written = write_settings(due, &self.client, &mut self.sensor, &mut self.exposer) => written,
            };
            match written {
                Ok(()) => self.settings_due = None,
                Err(Rewrite::Changed(message)) => return Some(Stop::camera_error(message)),
                Err(Rewrite::Failed(message)) => return self.failed(&message),
            }
        }
        let image = match self.frame(cancel, commands).await {
            Err(stop) => return Some(stop),
            Ok(Err(message)) => return self.failed(&message),
            Ok(Ok(image)) => image,
        };
        self.measure(image, cancel).await
    }

    /// A frame, while requests and the device poll are heard; the stop when
    /// the loop ends before it arrives.
    async fn frame(
        &mut self,
        cancel: &CancellationToken,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<Result<ImageArray, String>, Stop> {
        let Self {
            exposer,
            devices,
            sensor,
            reselect,
            ..
        } = self;
        let frame = exposer.frame();
        tokio::pin!(frame);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Stop::requested()),
                Some(command) = commands.recv() => match command {
                    Command::Reselect(reply) => *reselect = Some(reply),
                },
                Ok(()) = devices.changed() => {
                    if let Some(message) = came_back_changed(devices, sensor) {
                        return Err(Stop::camera_error(message));
                    }
                }
                result = &mut frame => return Ok(result),
            }
        }
    }

    /// Measure `image` off the async workers, answer the requests that
    /// waited on it and record it.
    async fn measure(&mut self, image: ImageArray, cancel: &CancellationToken) -> Option<Stop> {
        let reselecting = self.reselect.is_some();
        let job = Job {
            stars: self.stars.take(),
            params: self.params,
            reading: self.reading.sensor,
            max_adu: self.sensor.max_adu,
            reselect: reselecting,
        };
        let measured = tokio::select! {
            biased;
            () = cancel.cancelled() => return Some(Stop::requested()),
            measured = tokio::task::spawn_blocking(move || job.run(&image)) => measured,
        };
        let Measured {
            stars,
            summary,
            selection,
            frame,
        } = match measured {
            Ok(Ok(measured)) => measured,
            Ok(Err(message)) => return self.failed(&message),
            // A panic of the core's ends the loop's task without a stop, as
            // the failure domains of guider.md say.
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => return Some(Stop::requested()),
        };
        self.failures = 0;
        let centres = stars
            .as_ref()
            .map_or_else(Vec::new, GuideStars::search_centres);
        self.stars = stars;
        // A request that is gone has nothing to hear.
        if let Some(first) = self.first.take() {
            let _ = first.send(selection.unwrap_or(Ok(0)).unwrap_or(0));
        }
        if reselecting {
            if let (Some(reply), Some(selection)) = (self.reselect.take(), selection) {
                let _ = reply.send(selection.map_err(|()| {
                    ApiError::new(
                        ErrorCode::NoStars,
                        "the frame has fewer guide stars than stars.min",
                    )
                }));
            }
        }
        self.status.lock().record(summary, frame, centres);
        None
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

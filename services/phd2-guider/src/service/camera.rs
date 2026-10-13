//! `Phd2Camera`: the camera facade's ASCOM Alpaca Camera device.
//!
//! Contract: `docs/services/phd2-guider.md` § "Camera facade". PHD2
//! captures every frame through [`CaptureEngine`]; this module is the
//! ASCOM surface and the exposure state machine in front of it.
//!
//! All of a session's state sits behind one mutex, so every transition
//! (start, the capture's commit, abort, disconnect) is one step. A
//! capture carries the generation it started under and commits only
//! while that generation is current, so a disconnect or a later start
//! can never be overwritten by a capture that finishes after it.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use ndarray::Array2;
use rusty_photon_driver::ConfigActionCtx;
use tracing::{debug, warn};

use crate::client::Phd2Client;
use crate::config_actions::Phd2GuiderDriver;
use crate::error::Phd2Error;

use super::capture::{CaptureEngine, Frame, FrameEnd, StartError, ABORT_WAIT};

/// 0x500: ASCOM's driver-specific catch-all.
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

/// PHD2's own bounds for a single frame.
const EXPOSURE_MIN: Duration = Duration::from_millis(1);
const EXPOSURE_MAX: Duration = Duration::from_secs(600);
const EXPOSURE_RESOLUTION: Duration = Duration::from_millis(1);

/// The first PHD2 release whose `capture_single_frame` saves to a named
/// file and reports the frame with `SingleFrameComplete`.
const MIN_PHD2_VERSION: (u32, u32, u32) = (2, 6, 14);

const DEVICE_NAME: &str = "PHD2 Guide Camera";
const DEVICE_DESCRIPTION: &str = "The guide camera, its frames captured by PHD2 (phd2-guider)";

/// What a connect read from PHD2.
#[derive(Debug, Clone)]
struct Geometry {
    width: u32,
    height: u32,
    sensor_name: String,
}

/// The subframe a client set, validated only at `StartExposure`.
#[derive(Debug, Clone, Copy)]
struct Subframe {
    start_x: u32,
    start_y: u32,
    num_x: u32,
    num_y: u32,
}

/// A served image, cropped to the subframe, row-major.
#[derive(Debug)]
struct Image {
    width: usize,
    height: usize,
    pixels: Vec<i32>,
}

#[derive(Debug)]
enum Exposure {
    Idle,
    /// Accepted by this device, not yet by PHD2.
    Starting {
        generation: u64,
        aborting: bool,
    },
    /// PHD2 is exposing the frame.
    InFlight {
        generation: u64,
        path: std::path::PathBuf,
        aborting: bool,
    },
    /// PHD2 saved the frame; the facade is reading it.
    Reading {
        generation: u64,
        aborting: bool,
    },
    Ready(Arc<Image>),
    Failed(String),
}

#[derive(Debug)]
struct Session {
    connected: bool,
    geometry: Option<Geometry>,
    subframe: Subframe,
    exposure: Exposure,
    generation: u64,
    last_start: Option<SystemTime>,
    last_duration: Option<Duration>,
}

/// `PixelSizeX` and `PixelSizeY`, in micrometres, from config.
#[derive(Debug, Clone, Copy)]
pub struct PixelSize {
    pub x_um: f64,
    pub y_um: f64,
}

struct Inner {
    client: Arc<Phd2Client>,
    engine: Arc<CaptureEngine>,
    unique_id: String,
    pixel_size: PixelSize,
    session: std::sync::Mutex<Session>,
    /// Serialises `Connected` writes: a connect awaits PHD2, and a
    /// disconnect must not land inside it.
    lifecycle: tokio::sync::Mutex<()>,
}

#[derive(Clone, derive_more::Debug)]
pub struct Phd2Camera {
    #[debug(skip)]
    inner: Arc<Inner>,
    /// Skipped from `Debug`: `ConfigActionCtx` has none.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<Phd2GuiderDriver>>,
}

impl Phd2Camera {
    pub fn new(
        client: Arc<Phd2Client>,
        engine: Arc<CaptureEngine>,
        unique_id: String,
        pixel_size: PixelSize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                engine,
                unique_id,
                pixel_size,
                session: std::sync::Mutex::new(Session {
                    connected: false,
                    geometry: None,
                    subframe: Subframe {
                        start_x: 0,
                        start_y: 0,
                        num_x: 0,
                        num_y: 0,
                    },
                    exposure: Exposure::Idle,
                    generation: 0,
                    last_start: None,
                    last_duration: None,
                }),
                lifecycle: tokio::sync::Mutex::new(()),
            }),
            config_ctx: None,
        }
    }

    /// Attach the config actions (`config.get` / `config.apply` /
    /// `config.schema`).
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<Phd2GuiderDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn connected_session(&self) -> ASCOMResult<std::sync::MutexGuard<'_, Session>> {
        let session = self.session();
        if session.connected {
            Ok(session)
        } else {
            Err(not_connected("the camera is not connected"))
        }
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        self.connected_session().map(drop)
    }

    fn geometry(&self) -> ASCOMResult<Geometry> {
        self.connected_session()?
            .geometry
            .clone()
            .ok_or_else(|| not_connected("the camera is not connected"))
    }

    /// Read what a connect needs from PHD2, writing nothing to it.
    async fn read_phd2_camera(&self) -> ASCOMResult<Geometry> {
        let client = &self.inner.client;
        if !client.is_connected().await {
            return Err(not_connected(format!(
                "PHD2 is not reachable at {}",
                client.phd2_addr()
            )));
        }
        let version = phd2_version(client).await?;
        if !version_at_least(&version, MIN_PHD2_VERSION) {
            return Err(not_connected(format!(
                "PHD2 {version} is too old for the camera facade, which needs PHD2 2.6.14 or later"
            )));
        }
        let (width, height) = client.get_camera_frame_size().await.map_err(|e| {
            not_connected(format!("PHD2's camera is not available: {}", phd2_text(&e)))
        })?;
        let sensor_name = client
            .get_current_equipment()
            .await
            .map_err(|e| not_connected(format!("cannot read PHD2's equipment: {}", phd2_text(&e))))?
            .camera
            .map(|camera| camera.name)
            .unwrap_or_default();
        Ok(Geometry {
            width,
            height,
            sensor_name,
        })
    }

    /// Mark the exposure in flight as aborted and return the frame file
    /// to stop in PHD2, if PHD2 has accepted it.
    fn begin_abort(&self) -> Option<std::path::PathBuf> {
        let mut session = self.session();
        match &mut session.exposure {
            Exposure::Starting { aborting, .. } | Exposure::Reading { aborting, .. } => {
                *aborting = true;
                None
            }
            Exposure::InFlight { path, aborting, .. } => {
                *aborting = true;
                Some(path.clone())
            }
            Exposure::Idle | Exposure::Ready(_) | Exposure::Failed(_) => None,
        }
    }

    /// The capture behind one accepted `StartExposure`: wait for PHD2's
    /// report, read the frame, delete the file, and commit the outcome
    /// if the session is still the one it started in.
    async fn run_capture(
        self,
        generation: u64,
        inflight: super::capture::InFlight,
        geometry: Geometry,
        subframe: Subframe,
    ) {
        let engine = Arc::clone(&self.inner.engine);
        let path = inflight.path.clone();
        let end = engine.wait(inflight).await;
        let outcome = match end {
            FrameEnd::Saved => {
                {
                    let mut session = self.session();
                    if let Exposure::InFlight {
                        generation: current,
                        aborting,
                        ..
                    } = session.exposure
                    {
                        if current == generation {
                            session.exposure = Exposure::Reading {
                                generation,
                                aborting,
                            };
                        }
                    }
                }
                let read = engine.read(&path, geometry.width, geometry.height).await;
                engine.discard(&path);
                read.and_then(|frame| crop(&frame, subframe))
            }
            FrameEnd::Failed { message, stopped } => {
                engine.discard(&path);
                Err(if stopped {
                    format!("the exposure was aborted: PHD2's frame was stopped ({message})")
                } else {
                    format!("PHD2 failed the frame: {message}")
                })
            }
            FrameEnd::NotReported { waited } => Err(format!(
                "PHD2 did not report the frame within {}",
                humantime::format_duration(waited)
            )),
        };
        self.commit(generation, outcome);
    }

    /// Publish a capture's outcome, if the session is still the one the
    /// capture started in.
    fn commit(&self, generation: u64, outcome: Result<Image, String>) {
        let mut session = self.session();
        let (Exposure::InFlight {
            generation: current,
            aborting,
            ..
        }
        | Exposure::Reading {
            generation: current,
            aborting,
        }) = session.exposure
        else {
            return;
        };
        if current != generation {
            return;
        }
        session.exposure = if aborting {
            debug!("aborted exposure ended");
            Exposure::Idle
        } else {
            match outcome {
                Ok(image) => {
                    debug!(
                        width = image.width,
                        height = image.height,
                        "facade image ready"
                    );
                    Exposure::Ready(Arc::new(image))
                }
                Err(message) => {
                    warn!("facade exposure failed: {message}");
                    Exposure::Failed(message)
                }
            }
        };
    }

    /// Start a session on what a connect read from PHD2: the full frame
    /// as the subframe.
    fn begin_session(&self, geometry: Geometry) {
        let mut session = self.session();
        session.subframe = Subframe {
            start_x: 0,
            start_y: 0,
            num_x: geometry.width,
            num_y: geometry.height,
        };
        session.geometry = Some(geometry);
        session.connected = true;
    }

    /// End the session. A capture still running discards its outcome.
    fn end_session(&self) {
        let mut session = self.session();
        session.connected = false;
        session.generation = session.generation.wrapping_add(1);
        session.exposure = Exposure::Idle;
        session.last_start = None;
        session.last_duration = None;
    }

    /// Undo a start PHD2 refused, if nothing has replaced it since.
    fn cancel_start(&self, generation: u64) {
        let mut session = self.session();
        if matches!(session.exposure, Exposure::Starting { generation: g, .. } if g == generation) {
            session.exposure = Exposure::Idle;
        }
    }
}

#[async_trait::async_trait]
impl Device for Phd2Camera {
    fn static_name(&self) -> &str {
        DEVICE_NAME
    }

    fn unique_id(&self) -> &str {
        &self.inner.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.session().connected)
    }

    /// A connect only reads PHD2: its version, its camera's frame size
    /// and name. No exposure, no equipment connection, no stop
    /// (workspace tenet 3).
    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        let _transition = self.inner.lifecycle.lock().await;
        if connected {
            if self.session().connected {
                return Ok(());
            }
            let geometry = self.read_phd2_camera().await?;
            debug!(
                width = geometry.width,
                height = geometry.height,
                sensor = %geometry.sensor_name,
                "facade camera connected"
            );
            self.begin_session(geometry);
            return Ok(());
        }
        if let Some(path) = self.begin_abort() {
            debug!("disconnect ends the facade's frame in flight");
            self.inner.engine.abort(&path, ABORT_WAIT).await;
        }
        self.end_session();
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(DEVICE_DESCRIPTION.to_string())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon phd2-guider camera facade".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<Phd2GuiderDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait::async_trait]
impl Camera for Phd2Camera {
    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        Ok(self.geometry()?.width)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        Ok(self.geometry()?.height)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        Ok(self.inner.pixel_size.x_um)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        Ok(self.inner.pixel_size.y_um)
    }

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        Ok(EXPOSURE_MIN)
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        Ok(EXPOSURE_MAX)
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        Ok(EXPOSURE_RESOLUTION)
    }

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        Ok(65535)
    }

    async fn max_bin_x(&self) -> ASCOMResult<u8> {
        Ok(1)
    }

    async fn max_bin_y(&self) -> ASCOMResult<u8> {
        Ok(1)
    }

    async fn bin_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        Ok(1)
    }

    async fn set_bin_x(&self, bin_x: u8) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if bin_x == 1 {
            Ok(())
        } else {
            Err(ASCOMError::invalid_value(format!(
                "bin {bin_x} is not supported: PHD2's frames are served at bin 1"
            )))
        }
    }

    async fn bin_y(&self) -> ASCOMResult<u8> {
        self.bin_x().await
    }

    async fn set_bin_y(&self, bin_y: u8) -> ASCOMResult<()> {
        self.set_bin_x(bin_y).await
    }

    async fn num_x(&self) -> ASCOMResult<u32> {
        Ok(self.connected_session()?.subframe.num_x)
    }

    /// Subframe setters accept any value; `StartExposure` checks the
    /// subframe against the sensor, as ASCOM expects.
    async fn set_num_x(&self, num_x: u32) -> ASCOMResult<()> {
        self.connected_session()?.subframe.num_x = num_x;
        Ok(())
    }

    async fn num_y(&self) -> ASCOMResult<u32> {
        Ok(self.connected_session()?.subframe.num_y)
    }

    async fn set_num_y(&self, num_y: u32) -> ASCOMResult<()> {
        self.connected_session()?.subframe.num_y = num_y;
        Ok(())
    }

    async fn start_x(&self) -> ASCOMResult<u32> {
        Ok(self.connected_session()?.subframe.start_x)
    }

    async fn set_start_x(&self, start_x: u32) -> ASCOMResult<()> {
        self.connected_session()?.subframe.start_x = start_x;
        Ok(())
    }

    async fn start_y(&self) -> ASCOMResult<u32> {
        Ok(self.connected_session()?.subframe.start_y)
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.connected_session()?.subframe.start_y = start_y;
        Ok(())
    }

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        let (generation, exposure_ms, geometry, subframe) = {
            let mut session = self.connected_session()?;
            if !light {
                return Err(ASCOMError::invalid_value(
                    "dark frames are not supported: PHD2 has no dark-frame capture and the \
                     guide camera has no shutter",
                ));
            }
            let exposure_ms = exposure_ms(duration)?;
            let geometry = session
                .geometry
                .clone()
                .ok_or_else(|| not_connected("the camera is not connected"))?;
            check_subframe(session.subframe, &geometry)?;
            if matches!(
                session.exposure,
                Exposure::Starting { .. } | Exposure::InFlight { .. } | Exposure::Reading { .. }
            ) {
                return Err(ASCOMError::invalid_operation(
                    "an exposure is already in flight",
                ));
            }
            session.generation = session.generation.wrapping_add(1);
            let generation = session.generation;
            session.exposure = Exposure::Starting {
                generation,
                aborting: false,
            };
            (generation, exposure_ms, geometry, session.subframe)
        };

        let inflight = match self.inner.engine.start(exposure_ms).await {
            Ok(inflight) => inflight,
            Err(e) => {
                self.cancel_start(generation);
                debug!("facade exposure refused: {e}");
                return Err(start_error_to_ascom(&e));
            }
        };

        let abort_now = {
            let mut session = self.session();
            match session.exposure {
                Exposure::Starting {
                    generation: g,
                    aborting,
                } if g == generation => {
                    session.exposure = Exposure::InFlight {
                        generation,
                        path: inflight.path.clone(),
                        aborting,
                    };
                    session.last_start = Some(SystemTime::now());
                    session.last_duration = Some(Duration::from_millis(u64::from(exposure_ms)));
                    aborting
                }
                // A disconnect or a newer session took over while PHD2
                // accepted the frame: end it.
                _ => true,
            }
        };
        let path = inflight.path.clone();
        tokio::spawn(
            self.clone()
                .run_capture(generation, inflight, geometry, subframe),
        );
        if abort_now {
            self.inner.engine.abort(&path, ABORT_WAIT).await;
        }
        Ok(())
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    /// Ends PHD2's frame (`stop_capture`) and discards it. Idle, it does
    /// nothing and succeeds.
    async fn abort_exposure(&self) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if let Some(path) = self.begin_abort() {
            if !self.inner.engine.abort(&path, ABORT_WAIT).await {
                debug!("PHD2 has not reported the aborted frame yet");
            }
        }
        Ok(())
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        Ok(matches!(
            self.connected_session()?.exposure,
            Exposure::Ready(_)
        ))
    }

    async fn last_exposure_start_time(&self) -> ASCOMResult<SystemTime> {
        self.connected_session()?
            .last_start
            .ok_or_else(|| ASCOMError::invalid_operation("no exposure has started yet"))
    }

    async fn last_exposure_duration(&self) -> ASCOMResult<Duration> {
        self.connected_session()?
            .last_duration
            .ok_or_else(|| ASCOMError::invalid_operation("no exposure has started yet"))
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        let image = match &self.connected_session()?.exposure {
            Exposure::Ready(image) => Arc::clone(image),
            Exposure::Failed(message) => {
                return Err(ASCOMError::new(UNSPECIFIED_ERROR, message.clone()));
            }
            _ => return Err(ASCOMError::invalid_operation("no image is ready")),
        };
        // ASCOM's ImageArray is indexed [x][y]; the image is row-major,
        // so build (height, width) and swap the axes without a copy.
        let array = Array2::from_shape_vec((image.height, image.width), image.pixels.clone())
            .map_err(|e| ASCOMError::new(UNSPECIFIED_ERROR, format!("image shape: {e}")))?
            .reversed_axes();
        Ok(ImageArray::from(array))
    }

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        Ok(match self.connected_session()?.exposure {
            Exposure::Idle | Exposure::Ready(_) => CameraState::Idle,
            Exposure::Starting { .. } | Exposure::InFlight { .. } => CameraState::Exposing,
            Exposure::Reading { .. } => CameraState::Download,
            Exposure::Failed(_) => CameraState::Error,
        })
    }

    async fn percent_completed(&self) -> ASCOMResult<u8> {
        Ok(match self.connected_session()?.exposure {
            Exposure::Ready(_) => 100,
            _ => 0,
        })
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        Ok(self.geometry()?.sensor_name)
    }

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        Ok(SensorType::Monochrome)
    }

    async fn readout_modes(&self) -> ASCOMResult<Vec<String>> {
        Ok(vec!["Default".to_string()])
    }

    async fn readout_mode(&self) -> ASCOMResult<usize> {
        Ok(0)
    }

    async fn set_readout_mode(&self, readout_mode: usize) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if readout_mode == 0 {
            Ok(())
        } else {
            Err(ASCOMError::invalid_value(format!(
                "ReadoutMode {readout_mode} is not supported (only 0)"
            )))
        }
    }
}

/// How long a connect waits for PHD2's `Version` event, which PHD2
/// sends first thing on every connection.
const VERSION_WAIT: Duration = Duration::from_secs(2);

/// PHD2's version, from its `Version` event. The client counts as
/// connected once its socket is, which can be before the reader has
/// taken that event off the wire; so subscribe first, then read the
/// cache, then wait. The client caches the version before it
/// broadcasts the event, so an event that lands between the read and
/// the wait is still received.
async fn phd2_version(client: &Phd2Client) -> ASCOMResult<String> {
    let mut events = client.subscribe();
    if let Some(version) = client.get_phd2_version().await {
        return Ok(version);
    }
    let waited = tokio::time::timeout(VERSION_WAIT, async {
        loop {
            match events.recv().await {
                Ok(crate::events::Phd2Event::Version { phd_version, .. }) => {
                    return Some(phd_version);
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    match waited {
        Ok(Some(version)) => Ok(version),
        _ => Err(not_connected("PHD2 has not reported its version")),
    }
}

fn not_connected(message: impl Into<String>) -> ASCOMError {
    ASCOMError::new(ASCOMErrorCode::NOT_CONNECTED, message.into())
}

/// PHD2's own text for an RPC error, the client's message otherwise.
fn phd2_text(e: &Phd2Error) -> String {
    match e {
        Phd2Error::RpcError { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// `duration` as PHD2's whole milliseconds, or `InvalidValue` outside
/// 1 ms to 600 s.
fn exposure_ms(duration: Duration) -> ASCOMResult<u32> {
    if !(EXPOSURE_MIN..=EXPOSURE_MAX).contains(&duration) {
        return Err(ASCOMError::invalid_value(format!(
            "Duration {} s is outside PHD2's 0.001 s to 600 s",
            duration.as_secs_f64()
        )));
    }
    let rounded = duration.as_nanos().saturating_add(500_000) / 1_000_000;
    u32::try_from(rounded)
        .map(|ms| ms.clamp(1, 600_000))
        .map_err(|_| ASCOMError::invalid_value("Duration is outside PHD2's 0.001 s to 600 s"))
}

fn check_subframe(subframe: Subframe, geometry: &Geometry) -> ASCOMResult<()> {
    let Subframe {
        start_x,
        start_y,
        num_x,
        num_y,
    } = subframe;
    if num_x == 0 || num_y == 0 {
        return Err(ASCOMError::invalid_value(format!(
            "NumX and NumY must be greater than 0 (got {num_x} x {num_y})"
        )));
    }
    if start_x.saturating_add(num_x) > geometry.width
        || start_y.saturating_add(num_y) > geometry.height
    {
        return Err(ASCOMError::invalid_value(format!(
            "subframe {num_x} x {num_y} at ({start_x}, {start_y}) does not fit the {} x {} sensor",
            geometry.width, geometry.height
        )));
    }
    Ok(())
}

/// Cut `subframe` out of a full frame.
fn crop(frame: &Frame, subframe: Subframe) -> Result<Image, String> {
    let to_usize = |v: u32| usize::try_from(v).map_err(|_| "subframe out of range".to_string());
    let (sx, sy) = (to_usize(subframe.start_x)?, to_usize(subframe.start_y)?);
    let (nx, ny) = (to_usize(subframe.num_x)?, to_usize(subframe.num_y)?);
    let mut pixels = Vec::with_capacity(nx.saturating_mul(ny));
    for row in sy..sy.saturating_add(ny) {
        let begin = row.saturating_mul(frame.width).saturating_add(sx);
        let cols = frame
            .pixels
            .get(begin..begin.saturating_add(nx))
            .ok_or_else(|| "subframe does not fit PHD2's frame".to_string())?;
        pixels.extend_from_slice(cols);
    }
    Ok(Image {
        width: nx,
        height: ny,
        pixels,
    })
}

/// Whether `version` (PHD2's `PHDVersion`, e.g. `2.6.14`) is at least
/// `min`. Anything after the leading numbers is ignored; an
/// unparseable version is too old.
fn version_at_least(version: &str, min: (u32, u32, u32)) -> bool {
    let mut parts = version.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    let mut next = || parts.next().flatten();
    let (Some(major), Some(minor)) = (next(), next()) else {
        return false;
    };
    let patch = next().unwrap_or(0);
    (major, minor, patch) >= min
}

/// Map a capture that could not start to its ASCOM error.
fn start_error_to_ascom(e: &StartError) -> ASCOMError {
    match e {
        StartError::Busy(_) | StartError::NotStopped(_) | StartError::Phd2Busy(_) => {
            ASCOMError::invalid_operation(e.to_string())
        }
        StartError::Unreachable(_) => not_connected(e.to_string()),
        StartError::Phd2Refused(_) | StartError::Marker(..) => {
            ASCOMError::new(UNSPECIFIED_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_exposure_is_rounded_to_the_nearest_millisecond() {
        assert_eq!(exposure_ms(Duration::from_micros(123_600)).unwrap(), 124);
        assert_eq!(exposure_ms(Duration::from_micros(123_400)).unwrap(), 123);
        assert_eq!(exposure_ms(Duration::from_millis(1)).unwrap(), 1);
        assert_eq!(exposure_ms(Duration::from_secs(600)).unwrap(), 600_000);
    }

    #[test]
    fn an_exposure_outside_phd2s_range_is_refused() {
        let below = exposure_ms(Duration::from_micros(400)).unwrap_err();
        assert_eq!(below.code, ASCOMErrorCode::INVALID_VALUE);
        let above = exposure_ms(Duration::from_millis(600_001)).unwrap_err();
        assert_eq!(above.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[test]
    fn phd2_versions_from_2_6_14_on_are_accepted() {
        assert!(version_at_least("2.6.14", MIN_PHD2_VERSION));
        assert!(version_at_least("2.6.15", MIN_PHD2_VERSION));
        assert!(version_at_least("2.7", MIN_PHD2_VERSION));
        assert!(version_at_least("3.0.0", MIN_PHD2_VERSION));
        assert!(!version_at_least("2.6.13", MIN_PHD2_VERSION));
        assert!(!version_at_least("2.6.11-mock", MIN_PHD2_VERSION));
        assert!(!version_at_least("garbage", MIN_PHD2_VERSION));
    }

    fn ramp(width: usize, height: usize) -> Frame {
        Frame {
            width,
            height,
            pixels: (0..width * height)
                .map(|i| i32::try_from(i).unwrap())
                .collect(),
        }
    }

    #[test]
    fn a_subframe_is_cut_from_the_full_frame() {
        let image = crop(
            &ramp(8, 6),
            Subframe {
                start_x: 2,
                start_y: 1,
                num_x: 4,
                num_y: 3,
            },
        )
        .unwrap();
        assert_eq!((image.width, image.height), (4, 3));
        assert_eq!(image.pixels[0], 10); // x 2, y 1 of the 8-wide ramp
        assert_eq!(image.pixels[4], 18); // x 2, y 2
    }

    #[test]
    fn a_subframe_outside_the_sensor_is_refused() {
        let geometry = Geometry {
            width: 8,
            height: 6,
            sensor_name: String::new(),
        };
        let fits = Subframe {
            start_x: 4,
            start_y: 3,
            num_x: 4,
            num_y: 3,
        };
        check_subframe(fits, &geometry).unwrap();
        let too_wide = Subframe { start_x: 6, ..fits };
        assert_eq!(
            check_subframe(too_wide, &geometry).unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        let empty = Subframe { num_y: 0, ..fits };
        assert_eq!(
            check_subframe(empty, &geometry).unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }
}

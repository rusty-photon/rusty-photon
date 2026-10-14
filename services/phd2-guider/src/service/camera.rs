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
use rusty_photon_camera_core::Roi;
use rusty_photon_driver::ConfigActionCtx;
use tracing::{debug, warn};

use crate::client::Phd2Client;
use crate::config_actions::Phd2GuiderDriver;
use crate::error::Phd2Error;

use super::capture::{
    CaptureEngine, Frame, FrameEnd, StartError, ABORT_WAIT, LOCKED_RPC_TIMEOUT, MAX_EXPOSURE,
};
use super::lease::{self, LeaseAction, LeaseEnd, LeaseError};

/// How long an abort or a disconnect waits for its exposure to leave the
/// session: a start's two requests and the stop, each bounded by
/// [`LOCKED_RPC_TIMEOUT`], then [`ABORT_WAIT`] for PHD2's report.
const ABORT_SETTLE: Duration =
    Duration::from_secs(3 * LOCKED_RPC_TIMEOUT.as_secs() + ABORT_WAIT.as_secs());

/// 0x500: ASCOM's driver-specific catch-all.
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

/// PHD2's own bounds for a single frame.
const EXPOSURE_MIN: Duration = Duration::from_millis(1);
const EXPOSURE_MAX: Duration = MAX_EXPOSURE;
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
    pixels: Vec<u16>,
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
    Ready(ImageArray),
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
    /// Fires each time an exposure leaves the session: committed,
    /// cancelled, or ended with the session.
    exposure_ended: tokio::sync::Notify,
}

/// An exposure [`Phd2Camera::begin_abort`] marked as aborted.
#[derive(Debug, PartialEq, Eq)]
struct Aborted {
    generation: u64,
    /// PHD2's frame file, once PHD2 has accepted the frame.
    path: Option<std::path::PathBuf>,
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
                exposure_ended: tokio::sync::Notify::new(),
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

    /// Mark the exposure in flight as aborted and return it, with the
    /// frame file to stop in PHD2 if PHD2 has accepted the frame. `None`
    /// when no exposure is in flight.
    fn begin_abort(&self) -> Option<Aborted> {
        let mut session = self.session();
        match &mut session.exposure {
            Exposure::Starting {
                generation,
                aborting,
            }
            | Exposure::Reading {
                generation,
                aborting,
            } => {
                *aborting = true;
                Some(Aborted {
                    generation: *generation,
                    path: None,
                })
            }
            Exposure::InFlight {
                generation,
                path,
                aborting,
            } => {
                *aborting = true;
                Some(Aborted {
                    generation: *generation,
                    path: Some(path.clone()),
                })
            }
            Exposure::Idle | Exposure::Ready(_) | Exposure::Failed(_) => None,
        }
    }

    /// End an exposure [`Self::begin_abort`] marked: stop PHD2's frame if
    /// PHD2 has accepted it, then wait until the exposure has left the
    /// session, so a client's next call finds the camera idle. One still
    /// starting is stopped by its capture task once PHD2 answers, and
    /// this waits for that too. Bounded by [`ABORT_SETTLE`].
    async fn finish_abort(&self, aborted: Aborted) {
        if let Some(path) = &aborted.path {
            if !self.inner.engine.abort(path, ABORT_WAIT).await {
                debug!("PHD2 has not reported the aborted frame yet");
                return;
            }
        }
        if !self
            .wait_exposure_left(aborted.generation, ABORT_SETTLE)
            .await
        {
            debug!("the aborted exposure has not ended yet");
        }
    }

    /// Wait until no exposure of `generation` is in the session, or
    /// `limit` passes. Returns whether it left.
    async fn wait_exposure_left(&self, generation: u64, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now().checked_add(limit);
        loop {
            let ended = self.inner.exposure_ended.notified();
            let mut ended = std::pin::pin!(ended);
            // Register before checking, so an end between the check and
            // the wait is not missed.
            ended.as_mut().enable();
            let in_flight = match self.session().exposure {
                Exposure::Starting { generation: g, .. }
                | Exposure::InFlight { generation: g, .. }
                | Exposure::Reading { generation: g, .. } => g == generation,
                Exposure::Idle | Exposure::Ready(_) | Exposure::Failed(_) => false,
            };
            if !in_flight {
                return true;
            }
            let Some(deadline) = deadline else {
                ended.await;
                continue;
            };
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                return false;
            }
        }
    }

    /// Everything after `StartExposure`'s own checks, in a task the
    /// request cannot cancel: ask PHD2 for the frame, send `StartExposure`
    /// its answer through `started`, and run the capture.
    async fn begin_capture(
        self,
        generation: u64,
        exposure_ms: u32,
        geometry: Geometry,
        subframe: Subframe,
        started: tokio::sync::oneshot::Sender<ASCOMResult<()>>,
    ) {
        let inflight = match self.inner.engine.start(exposure_ms).await {
            Ok(inflight) => inflight,
            Err(e) => {
                self.cancel_start(generation);
                debug!("facade exposure refused: {e}");
                let _ = started.send(Err(start_error_to_ascom(&e)));
                return;
            }
        };

        // `Some(aborting)` while the session is still the one the
        // exposure started in; `None` once a disconnect replaced it.
        let accepted = {
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
                    drop(session);
                    Some(aborting)
                }
                _ => None,
            }
        };
        if accepted != Some(false) {
            // Aborted, or disconnected, while PHD2 was being asked: answer
            // first, then end the frame PHD2 accepted. The capture still
            // waits for its report, which releases the slot.
            let _ = started.send(if accepted.is_some() {
                Ok(())
            } else {
                Err(not_connected(
                    "the camera was disconnected while PHD2 started the frame",
                ))
            });
            let path = inflight.path.clone();
            tokio::spawn(
                self.clone()
                    .run_capture(generation, inflight, geometry, subframe),
            );
            self.inner.engine.abort(&path, ABORT_WAIT).await;
            return;
        }
        let _ = started.send(Ok(()));
        // After the answer, so StartExposure never waits on the sweep.
        self.inner.engine.sweep_orphans(&inflight.path).await;
        self.run_capture(generation, inflight, geometry, subframe)
            .await;
    }

    /// Whether the capture of `generation` still wants its frame: the
    /// session is the one it started in and nobody aborted it. If so, it
    /// moves to Reading.
    fn start_reading(&self, generation: u64) -> bool {
        let mut session = self.session();
        match session.exposure {
            Exposure::InFlight {
                generation: current,
                aborting: false,
                ..
            } if current == generation => {
                session.exposure = Exposure::Reading {
                    generation,
                    aborting: false,
                };
                true
            }
            _ => false,
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
                // Aborted, or its session is gone: the frame is not read.
                let read = if self.start_reading(generation) {
                    // Bounded by the grace that covers PHD2's own
                    // download and save: a read that stalls fails the
                    // exposure rather than leaving the camera downloading.
                    tokio::time::timeout(
                        engine.grace(),
                        engine.read(&path, geometry.width, geometry.height),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(format!(
                            "PHD2's frame {} could not be read within {}",
                            path.display(),
                            humantime::format_duration(engine.grace())
                        ))
                    })
                } else {
                    Err("the frame is no longer wanted".to_string())
                };
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
            match outcome.and_then(image_array_of) {
                Ok(image) => {
                    debug!(shape = ?image.shape(), "facade image ready");
                    Exposure::Ready(image)
                }
                Err(message) => {
                    warn!("facade exposure failed: {message}");
                    Exposure::Failed(message)
                }
            }
        };
        drop(session);
        self.inner.exposure_ended.notify_waiters();
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
        drop(session);
        self.inner.exposure_ended.notify_waiters();
    }

    /// Answer one of the sweep lease's Actions
    /// (`docs/services/phd2-guider.md` § "The sweep lease").
    async fn lease_action(&self, action: LeaseAction, parameters: &str) -> ASCOMResult<String> {
        let ops = self.inner.engine.ops();
        let reply = match action {
            LeaseAction::Acquire => {
                let request =
                    lease::parse_acquire(parameters).map_err(ASCOMError::invalid_value)?;
                // A disconnect waits for the acquire, so it always ends
                // the lease the acquire granted.
                let _transition = self.inner.lifecycle.lock().await;
                self.ensure_connected()?;
                let granted = ops
                    .acquire_lease(&request)
                    .await
                    .map_err(|e| lease_error_to_ascom(&e))?;
                serde_json::to_string(&granted)
            }
            LeaseAction::Renew => {
                let token = lease::parse_token(lease::RENEW, parameters)
                    .map_err(ASCOMError::invalid_value)?;
                let granted = ops
                    .renew_lease(&token)
                    .map_err(ASCOMError::invalid_operation)?;
                serde_json::to_string(&granted)
            }
            LeaseAction::Release => {
                let token = lease::parse_token(lease::RELEASE, parameters)
                    .map_err(ASCOMError::invalid_value)?;
                serde_json::to_string(&lease::Released {
                    released: ops.release_lease(&token),
                })
            }
        };
        reply.map_err(|e| ASCOMError::new(UNSPECIFIED_ERROR, format!("lease answer: {e}")))
    }

    /// Undo a start PHD2 refused, if nothing has replaced it since.
    fn cancel_start(&self, generation: u64) {
        let mut session = self.session();
        if matches!(session.exposure, Exposure::Starting { generation: g, .. } if g == generation) {
            session.exposure = Exposure::Idle;
            drop(session);
            self.inner.exposure_ended.notify_waiters();
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
        self.inner.engine.ops().end_lease(LeaseEnd::Disconnected);
        if let Some(aborted) = self.begin_abort() {
            debug!("disconnect ends the facade's exposure in flight");
            self.finish_abort(aborted).await;
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

    /// The sweep lease's three Actions, then the config Actions.
    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        let mut names: Vec<String> = lease::ACTIONS.iter().map(ToString::to_string).collect();
        names.extend(rusty_photon_driver::supported_actions(&self.config_ctx));
        Ok(names)
    }

    /// A lease Action, or else a config Action. An unknown name is
    /// `ActionNotImplemented`, from the config dispatch.
    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        match LeaseAction::from_name(&action) {
            Some(lease_action) => self.lease_action(lease_action, &parameters).await,
            None => {
                rusty_photon_driver::dispatch::<Phd2GuiderDriver>(
                    &self.config_ctx,
                    action,
                    parameters,
                )
                .await
            }
        }
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

        // The rest runs in a task of its own. Axum drops a handler's
        // future when the client's connection closes, and a drop between
        // claiming the slot and handing PHD2's frame to a waiter would
        // leave the slot held and the camera Starting for good.
        let (started, outcome) = tokio::sync::oneshot::channel();
        tokio::spawn(self.clone().begin_capture(
            generation,
            exposure_ms,
            geometry,
            subframe,
            started,
        ));
        outcome.await.unwrap_or_else(|_| {
            Err(ASCOMError::new(
                UNSPECIFIED_ERROR,
                "the capture task ended before PHD2 answered",
            ))
        })
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    /// Ends PHD2's frame (`stop_capture`) and discards it, returning once
    /// the camera is idle again (bounded, see [`Self::finish_abort`]).
    /// Idle, it does nothing and succeeds.
    async fn abort_exposure(&self) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if let Some(aborted) = self.begin_abort() {
            self.finish_abort(aborted).await;
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

    /// The image built at commit; an `ImageArray` clone shares its data.
    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        match &self.connected_session()?.exposure {
            Exposure::Ready(image) => Ok(image.clone()),
            Exposure::Failed(message) => Err(ASCOMError::new(UNSPECIFIED_ERROR, message.clone())),
            _ => Err(ASCOMError::invalid_operation("no image is ready")),
        }
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
        let session = self.connected_session()?;
        Ok(match session.exposure {
            Exposure::Ready(_) => 100,
            Exposure::InFlight { .. } => match (session.last_start, session.last_duration) {
                (Some(start), Some(total)) => rusty_photon_camera_core::progress_percent(
                    SystemTime::now().duration_since(start).unwrap_or_default(),
                    total,
                ),
                _ => 0,
            },
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

/// The subframe against the sensor, by the rules every camera driver
/// shares. The facade has one bin and no alignment rule.
fn check_subframe(subframe: Subframe, geometry: &Geometry) -> ASCOMResult<()> {
    let roi = Roi {
        start_x: subframe.start_x,
        start_y: subframe.start_y,
        width: subframe.num_x,
        height: subframe.num_y,
    };
    rusty_photon_camera_core::check(roi, geometry.width, geometry.height, 1, None)
        .map_err(ASCOMError::from)
}

/// The ASCOM `ImageArray` for a row-major image. ASCOM's array is
/// indexed `[x][y]`, so build `(height, width)` and swap the axes
/// without a copy.
fn image_array_of(image: Image) -> Result<ImageArray, String> {
    Array2::from_shape_vec((image.height, image.width), image.pixels)
        .map(|array| ImageArray::from(array.reversed_axes()))
        .map_err(|e| format!("image shape: {e}"))
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

/// Map a lease acquire that was not granted to its ASCOM error.
fn lease_error_to_ascom(e: &LeaseError) -> ASCOMError {
    match e {
        LeaseError::NotConnected(message) => not_connected(message.clone()),
        LeaseError::Refused(message) => ASCOMError::invalid_operation(message.clone()),
    }
}

/// Map a capture that could not start to its ASCOM error.
fn start_error_to_ascom(e: &StartError) -> ASCOMError {
    match e {
        StartError::Busy(_) | StartError::NotStopped(_) | StartError::Phd2Busy(_) => {
            ASCOMError::invalid_operation(e.to_string())
        }
        StartError::Unreachable(_) => not_connected(e.to_string()),
        StartError::Phd2Refused(_) | StartError::Marker(..) | StartError::Unconfirmed(_) => {
            ASCOMError::new(UNSPECIFIED_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use crate::config::{Phd2Config, SettleParams};
    use crate::service::guider::GuiderOps;

    fn test_camera(image_dir: &std::path::Path) -> Phd2Camera {
        let client = Arc::new(Phd2Client::new(Phd2Config::default()));
        let ops = Arc::new(GuiderOps::new(
            Arc::clone(&client),
            SettleParams::default(),
            Duration::from_secs(10),
        ));
        let engine = Arc::new(CaptureEngine::new(
            ops,
            Arc::clone(&client),
            image_dir.to_path_buf(),
            Duration::from_secs(10),
        ));
        Phd2Camera::new(
            client,
            engine,
            "facade-test-id".to_string(),
            PixelSize {
                x_um: 2.9,
                y_um: 3.1,
            },
        )
    }

    /// A camera with a session open on an 8 x 6 sensor, as a connect
    /// leaves it.
    fn connected_camera(image_dir: &std::path::Path) -> Phd2Camera {
        let camera = test_camera(image_dir);
        camera.begin_session(Geometry {
            width: 8,
            height: 6,
            sensor_name: "Mock Camera".to_string(),
        });
        camera
    }

    fn set_exposure(camera: &Phd2Camera, exposure: Exposure) {
        camera.session().exposure = exposure;
    }

    fn in_flight(generation: u64) -> Exposure {
        Exposure::InFlight {
            generation,
            path: std::path::PathBuf::from("/images/phd2-guider-a.fits"),
            aborting: false,
        }
    }

    #[tokio::test]
    async fn the_fixed_surface_answers_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        assert_eq!(camera.static_name(), "PHD2 Guide Camera");
        assert_eq!(camera.unique_id(), "facade-test-id");
        assert!(!camera.connected().await.unwrap());
        assert!(camera.description().await.unwrap().contains("PHD2"));
        assert!(camera.driver_info().await.unwrap().contains("phd2-guider"));
        assert_eq!(
            camera.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            camera.supported_actions().await.unwrap(),
            [
                "rustyphoton:sweep-lease-acquire",
                "rustyphoton:sweep-lease-renew",
                "rustyphoton:sweep-lease-release"
            ]
        );
        assert!((camera.pixel_size_x().await.unwrap() - 2.9).abs() < f64::EPSILON);
        assert!((camera.pixel_size_y().await.unwrap() - 3.1).abs() < f64::EPSILON);
        assert_eq!(
            camera.exposure_min().await.unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            camera.exposure_max().await.unwrap(),
            Duration::from_secs(600)
        );
        assert_eq!(
            camera.exposure_resolution().await.unwrap(),
            Duration::from_millis(1)
        );
        assert!(!camera.has_shutter().await.unwrap());
        assert_eq!(camera.max_adu().await.unwrap(), 65535);
        assert_eq!(camera.max_bin_x().await.unwrap(), 1);
        assert_eq!(camera.max_bin_y().await.unwrap(), 1);
        assert!(camera.can_abort_exposure().await.unwrap());
        assert!(!camera.can_stop_exposure().await.unwrap());
        assert_eq!(camera.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert_eq!(camera.readout_modes().await.unwrap(), ["Default"]);
        assert_eq!(camera.readout_mode().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_session_surface_refuses_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        let not_connected = |e: ASCOMError| assert_eq!(e.code, ASCOMErrorCode::NOT_CONNECTED);
        not_connected(camera.camera_x_size().await.unwrap_err());
        not_connected(camera.camera_y_size().await.unwrap_err());
        not_connected(camera.sensor_name().await.unwrap_err());
        not_connected(camera.bin_x().await.unwrap_err());
        not_connected(camera.set_bin_y(1).await.unwrap_err());
        not_connected(camera.num_x().await.unwrap_err());
        not_connected(camera.set_num_y(3).await.unwrap_err());
        not_connected(camera.start_x().await.unwrap_err());
        not_connected(camera.set_start_y(1).await.unwrap_err());
        not_connected(camera.image_ready().await.unwrap_err());
        not_connected(camera.image_array().await.unwrap_err());
        not_connected(camera.camera_state().await.unwrap_err());
        not_connected(camera.percent_completed().await.unwrap_err());
        not_connected(camera.last_exposure_duration().await.unwrap_err());
        not_connected(camera.last_exposure_start_time().await.unwrap_err());
        not_connected(camera.abort_exposure().await.unwrap_err());
        not_connected(camera.set_readout_mode(0).await.unwrap_err());
        not_connected(
            camera
                .start_exposure(Duration::from_secs(1), true)
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn a_session_starts_on_the_full_frame_at_bin_1() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        assert!(camera.connected().await.unwrap());
        assert_eq!(camera.camera_x_size().await.unwrap(), 8);
        assert_eq!(camera.camera_y_size().await.unwrap(), 6);
        assert_eq!(camera.sensor_name().await.unwrap(), "Mock Camera");
        assert_eq!(camera.start_x().await.unwrap(), 0);
        assert_eq!(camera.start_y().await.unwrap(), 0);
        assert_eq!(camera.num_x().await.unwrap(), 8);
        assert_eq!(camera.num_y().await.unwrap(), 6);
        assert_eq!(camera.bin_x().await.unwrap(), 1);
        assert_eq!(camera.bin_y().await.unwrap(), 1);
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
        assert_eq!(camera.percent_completed().await.unwrap(), 0);
        assert!(!camera.image_ready().await.unwrap());
        assert_eq!(
            camera.image_array().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert_eq!(
            camera.last_exposure_duration().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert_eq!(
            camera.last_exposure_start_time().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn only_bin_1_and_readout_mode_0_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        camera.set_bin_x(1).await.unwrap();
        camera.set_bin_y(1).await.unwrap();
        assert_eq!(
            camera.set_bin_x(2).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        camera.set_readout_mode(0).await.unwrap();
        assert_eq!(
            camera.set_readout_mode(1).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn the_subframe_setters_take_any_value_until_an_exposure() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        camera.set_start_x(7).await.unwrap();
        camera.set_start_y(5).await.unwrap();
        camera.set_num_x(4).await.unwrap();
        camera.set_num_y(3).await.unwrap();
        assert_eq!(camera.start_x().await.unwrap(), 7);
        assert_eq!(camera.start_y().await.unwrap(), 5);
        assert_eq!(camera.num_x().await.unwrap(), 4);
        assert_eq!(camera.num_y().await.unwrap(), 3);
        assert_eq!(
            camera
                .start_exposure(Duration::from_secs(1), true)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn a_dark_frame_or_a_duration_outside_the_range_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        assert_eq!(
            camera
                .start_exposure(Duration::from_secs(1), false)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            camera
                .start_exposure(Duration::from_secs(601), true)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn a_second_exposure_is_refused_while_one_is_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(&camera, in_flight(1));
        assert_eq!(
            camera
                .start_exposure(Duration::from_secs(1), true)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn an_unreachable_phd2_refuses_the_exposure_and_leaves_the_camera_idle() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        let err = camera
            .start_exposure(Duration::from_secs(1), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED, "{err}");
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
    }

    #[tokio::test]
    async fn the_exposure_state_maps_to_camera_state_and_progress() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(
            &camera,
            Exposure::Starting {
                generation: 1,
                aborting: false,
            },
        );
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Exposing);
        set_exposure(&camera, in_flight(1));
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Exposing);
        set_exposure(
            &camera,
            Exposure::Reading {
                generation: 1,
                aborting: false,
            },
        );
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Download);
        set_exposure(
            &camera,
            Exposure::Failed("PHD2 failed the frame".to_string()),
        );
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Error);
        let err = camera.image_array().await.unwrap_err();
        assert_eq!(err.code, UNSPECIFIED_ERROR);
        assert_eq!(err.message, "PHD2 failed the frame");
        assert_eq!(camera.percent_completed().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_ready_image_is_served_indexed_x_then_y() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(
            &camera,
            Exposure::Ready(
                image_array_of(Image {
                    width: 3,
                    height: 2,
                    pixels: vec![0, 1, 2, 10, 11, 12],
                })
                .unwrap(),
            ),
        );
        assert!(camera.image_ready().await.unwrap());
        assert_eq!(camera.percent_completed().await.unwrap(), 100);
        assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
        let image = camera.image_array().await.unwrap();
        assert_eq!(&image.shape()[..2], &[3, 2]);
        assert_eq!(image[[2, 0, 0]], 2);
        assert_eq!(image[[0, 1, 0]], 10);
    }

    #[tokio::test]
    async fn a_capture_commits_only_into_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        let image = || {
            Ok(Image {
                width: 1,
                height: 1,
                pixels: vec![7],
            })
        };
        set_exposure(&camera, in_flight(2));
        camera.commit(1, image());
        assert!(matches!(
            camera.session().exposure,
            Exposure::InFlight { .. }
        ));
        camera.commit(2, image());
        assert!(matches!(camera.session().exposure, Exposure::Ready(_)));
        set_exposure(&camera, in_flight(3));
        camera.commit(3, Err("PHD2 did not report the frame".to_string()));
        assert!(matches!(camera.session().exposure, Exposure::Failed(_)));
        // A commit with nothing in flight changes nothing.
        camera.commit(3, image());
        assert!(matches!(camera.session().exposure, Exposure::Failed(_)));
    }

    #[tokio::test]
    async fn an_aborted_capture_ends_idle_whatever_its_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(&camera, in_flight(4));
        assert_eq!(
            camera.begin_abort(),
            Some(Aborted {
                generation: 4,
                path: Some(std::path::PathBuf::from("/images/phd2-guider-a.fits")),
            })
        );
        camera.commit(4, Err("camera error -- see debug log".to_string()));
        assert!(matches!(camera.session().exposure, Exposure::Idle));
    }

    #[tokio::test]
    async fn an_abort_returns_once_its_exposure_has_left_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(&camera, in_flight(4));
        camera.begin_abort();
        let waiter = tokio::spawn({
            let camera = camera.clone();
            async move { camera.wait_exposure_left(4, Duration::from_secs(30)).await }
        });
        // Whether the waiter has registered yet or not, it must see the
        // commit: it registers before it checks the session.
        camera.commit(4, Err("aborted".to_string()));
        assert!(waiter.await.unwrap());
        assert!(matches!(camera.session().exposure, Exposure::Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn an_abort_waits_for_its_exposure_no_longer_than_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(&camera, in_flight(4));
        assert!(
            !camera.wait_exposure_left(4, Duration::from_secs(2)).await,
            "an exposure still in flight read as ended"
        );
        // Another exposure's generation is not the one waited for.
        assert!(camera.wait_exposure_left(3, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn the_percent_completed_follows_the_exposure() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(&camera, in_flight(4));
        {
            let mut session = camera.session();
            session.last_start = Some(SystemTime::now() - Duration::from_secs(30));
            session.last_duration = Some(Duration::from_secs(60));
        }
        let percent = camera.percent_completed().await.unwrap();
        assert!((49..=51).contains(&percent), "{percent}");
    }

    #[test]
    fn an_abort_names_a_frame_only_once_phd2_has_accepted_it() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(
            &camera,
            Exposure::Starting {
                generation: 1,
                aborting: false,
            },
        );
        assert_eq!(
            camera.begin_abort(),
            Some(Aborted {
                generation: 1,
                path: None
            })
        );
        assert!(matches!(
            camera.session().exposure,
            Exposure::Starting { aborting: true, .. }
        ));
        set_exposure(
            &camera,
            Exposure::Reading {
                generation: 1,
                aborting: false,
            },
        );
        assert_eq!(
            camera.begin_abort(),
            Some(Aborted {
                generation: 1,
                path: None
            })
        );
        assert!(matches!(
            camera.session().exposure,
            Exposure::Reading { aborting: true, .. }
        ));
        set_exposure(&camera, Exposure::Idle);
        assert_eq!(camera.begin_abort(), None);
    }

    #[test]
    fn a_refused_start_is_undone_only_if_nothing_replaced_it() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        set_exposure(
            &camera,
            Exposure::Starting {
                generation: 5,
                aborting: false,
            },
        );
        camera.cancel_start(4);
        assert!(matches!(
            camera.session().exposure,
            Exposure::Starting { .. }
        ));
        camera.cancel_start(5);
        assert!(matches!(camera.session().exposure, Exposure::Idle));
    }

    #[tokio::test]
    async fn a_disconnect_ends_the_session_and_its_exposure_record() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        {
            let mut session = camera.session();
            session.last_start = Some(SystemTime::now());
            session.last_duration = Some(Duration::from_secs(1));
            session.exposure = Exposure::Failed("old".to_string());
        }
        camera.set_connected(false).await.unwrap();
        assert!(!camera.connected().await.unwrap());
        let session = camera.session();
        assert!(matches!(session.exposure, Exposure::Idle));
        assert!(session.last_start.is_none());
        assert!(session.last_duration.is_none());
    }

    #[tokio::test]
    async fn a_connect_without_phd2_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        let err = camera.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(err.message.contains("not reachable"), "{err}");
        assert!(!camera.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connecting_again_keeps_the_running_session() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        camera.set_num_x(4).await.unwrap();
        camera.set_connected(true).await.unwrap();
        assert_eq!(camera.num_x().await.unwrap(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_version_that_never_arrives_is_not_connected() {
        let client = Phd2Client::new(Phd2Config::default());
        let err = phd2_version(&client).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(
            err.message.contains("has not reported its version"),
            "{err}"
        );
    }

    const TOKEN_A: &str = r#"{"token": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;

    #[tokio::test]
    async fn a_lease_action_with_parameters_of_another_shape_is_an_invalid_value() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        for action in [lease::ACQUIRE, lease::RENEW, lease::RELEASE] {
            let err = camera
                .action(action.to_string(), "not json".to_string())
                .await
                .unwrap_err();
            assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE, "{action}: {err}");
        }
    }

    #[tokio::test]
    async fn a_lease_on_a_camera_that_is_not_connected_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        let err = camera
            .action(lease::ACQUIRE.to_string(), TOKEN_A.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED, "{err}");
    }

    #[tokio::test]
    async fn a_lease_with_phd2_unreachable_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let camera = connected_camera(dir.path());
        let err = camera
            .action(lease::ACQUIRE.to_string(), TOKEN_A.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED, "{err}");
    }

    #[tokio::test]
    async fn a_token_that_holds_no_lease_fails_a_renewal_and_is_released_false() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        let err = camera
            .action(lease::RENEW.to_string(), TOKEN_A.to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION, "{err}");
        assert!(err.message.contains("never acquired"), "{err}");
        let reply = camera
            .action(lease::RELEASE.to_string(), TOKEN_A.to_string())
            .await
            .unwrap();
        assert_eq!(reply, r#"{"released":false}"#);
    }

    #[tokio::test]
    async fn an_action_the_facade_does_not_have_is_not_implemented() {
        let dir = tempfile::tempdir().unwrap();
        let camera = test_camera(dir.path());
        let err = camera
            .action("rustyphoton:sweep-lease-steal".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED, "{err}");
    }

    #[test]
    fn a_lease_refusal_maps_to_its_ascom_error() {
        let code = |e: LeaseError| lease_error_to_ascom(&e).code;
        assert_eq!(
            code(LeaseError::NotConnected("x".into())),
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            code(LeaseError::Refused("x".into())),
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[test]
    fn an_image_goes_out_as_unsigned_16_bit_values() {
        let image = image_array_of(Image {
            width: 2,
            height: 1,
            pixels: vec![0, 65535],
        })
        .unwrap();
        assert_eq!(image[[1, 0, 0]], 65535);
    }

    #[test]
    fn a_refused_start_maps_to_its_ascom_error() {
        let code = |e: StartError| start_error_to_ascom(&e).code;
        assert_eq!(
            code(StartError::Busy("x".into())),
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert_eq!(
            code(StartError::NotStopped(crate::events::AppState::Guiding)),
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert_eq!(
            code(StartError::Phd2Busy("x".into())),
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert_eq!(
            code(StartError::Unreachable("x".into())),
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(code(StartError::Phd2Refused("x".into())), UNSPECIFIED_ERROR);
        assert_eq!(
            code(StartError::Marker("/m".into(), "x".into())),
            UNSPECIFIED_ERROR
        );
    }

    #[test]
    fn phd2s_own_text_is_used_for_an_rpc_error() {
        let rpc = Phd2Error::RpcError {
            code: 1,
            message: "camera not connected".into(),
        };
        assert_eq!(phd2_text(&rpc), "camera not connected");
        assert!(phd2_text(&Phd2Error::NotConnected).contains("ot connected"));
    }

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
                .map(|i| u16::try_from(i).unwrap())
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

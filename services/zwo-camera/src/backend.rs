//! The SDK seam: a thin trait over the blocking `zwo-rs` `Camera` surface the
//! ASCOM device drives, plus a production wrapper and a test mock.
//!
//! Why a seam: it (1) collapses [`zwo_rs::Error`] into a typed [`BackendError`] at
//! one boundary, (2) lets the ASCOM device hold an `Arc<dyn CameraHandle>` so unit
//! tests can substitute a mock that forces paths the `zwo-rs` simulation cannot —
//! a mid-exposure SDK error (E9), a model without an ST4 port (PG2) — without
//! hardware, and (3) keeps the open/close lifecycle in one place. `zwo-rs`'s
//! `Camera` is RAII (open = [`zwo_rs::Sdk::open_camera`], close = drop) and
//! `Send + !Sync`, so the production handle keeps it behind a `parking_lot::Mutex`
//! and re-opens on connect from the cached enumeration `index`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tracing::{debug, warn};
use zwo_rs::{CameraInfo, ControlCaps, ControlType, GuideDirection, ImageType};

/// A `zwo-rs` SDK call failed: its message and its [`BackendErrorKind`].
///
/// The ASCOM device picks the `ASCOMError` per call site (the SDK error kind
/// does not map 1:1 to an ASCOM code), except for a camera that has left the
/// bus, which is a disconnect wherever it is met (C6).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    message: String,
    kind: BackendErrorKind,
}

/// What a [`BackendError`] says about the camera, beyond its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    /// The camera has left the bus (C6): the handle's presence check found it
    /// gone, or the SDK answered `ASI_ERROR_CAMERA_REMOVED`.
    Departed,
    /// Any other failure.
    Other,
}

/// Collapse a [`zwo_rs::Error`] into the typed seam error: its message, and
/// whether it is the SDK reporting the camera removed. This `From` impl lets
/// `?` convert SDK errors automatically. Any other code says nothing about
/// the camera's presence by itself; the handle asks (C6).
impl From<zwo_rs::Error> for BackendError {
    fn from(err: zwo_rs::Error) -> Self {
        let kind = if matches!(err, zwo_rs::Error::Asi(zwo_rs::AsiError::CameraRemoved)) {
            BackendErrorKind::Departed
        } else {
            BackendErrorKind::Other
        };
        Self {
            message: err.to_string(),
            kind,
        }
    }
}

impl BackendError {
    /// A failure that says nothing about the camera's presence.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: BackendErrorKind::Other,
        }
    }

    fn closed() -> Self {
        Self::new("camera not open")
    }

    /// What this failure says about the camera.
    #[must_use]
    pub const fn kind(&self) -> BackendErrorKind {
        self.kind
    }

    /// Whether the camera has left the bus (C6).
    #[must_use]
    pub fn is_departed(&self) -> bool {
        self.kind == BackendErrorKind::Departed
    }

    /// The same failure, now known to mean the camera has left the bus (C6).
    #[must_use]
    pub fn departed(self) -> Self {
        Self {
            kind: BackendErrorKind::Departed,
            ..self
        }
    }

    /// The same failure, its message prefixed with `context`. The kind is
    /// kept, so a removal reported from inside a larger step still reads as
    /// one.
    #[must_use]
    pub fn context(self, context: &str) -> Self {
        Self {
            message: format!("{context}: {}", self.message),
            kind: self.kind,
        }
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// What a [`CameraHandle`] holds, read in one step (see
/// [`CameraHandle::session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No camera is open.
    Closed,
    /// A camera is open, and the SDK has not reported it removed.
    Live,
    /// A camera is open, but the SDK has reported it removed since the open
    /// (C6). It stays held until a close.
    Lost,
}

/// The ROI + exposure parameters for a single capture, validated by the device.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// Post-binning frame width (`NumX`).
    pub width: u32,
    /// Post-binning frame height (`NumY`).
    pub height: u32,
    /// Symmetric binning factor.
    pub bin: u32,
    /// Post-binning ROI start X (`StartX`).
    pub start_x: u32,
    /// Post-binning ROI start Y (`StartY`).
    pub start_y: u32,
    /// Exposure time in microseconds (the ASI `ASI_EXPOSURE` control unit).
    pub exposure_us: i64,
    /// The download format to configure for this frame — the readout mode the
    /// device negotiated from the camera's `SupportedVideoFormat` (RM1/RM2).
    /// Sizes the download buffer and tells `camera.rs` which unpack the bytes
    /// need.
    pub image_type: ImageType,
    /// The gain to arm this frame with (GO2), pinned by `StartExposure` with
    /// the geometry. `None` when the camera does not advertise the control, or
    /// has no gain to arm yet (GO1): nothing is sent.
    pub gain: Option<i32>,
    /// The offset to arm this frame with, on [`Self::gain`]'s terms.
    pub offset: Option<i32>,
    /// Wall-clock integration time the capture honours so an in-flight exposure
    /// is observable (the `zwo-rs` simulation completes after one poll regardless).
    pub duration: Duration,
    /// Request a dark frame (no-op on shutterless ASI sensors).
    pub is_dark: bool,
    /// **This capture's** stop cell: the device sets it to abort or gracefully
    /// stop this frame, and [`capture`](CameraHandle::capture) reads it between
    /// integration steps. See [`StopSignal`] for why it is per-capture.
    pub stop: Arc<StopSignal>,
}

/// What an in-flight [`capture`](CameraHandle::capture) has been asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRequest {
    /// Nothing asked for — integrate to the end of the exposure.
    None,
    /// Abort: stop at the SDK and discard the frame (E7).
    Abort,
    /// Graceful stop: stop at the SDK but keep the partially-integrated frame
    /// (E8). ASI's data-preserving `ASIStopExposure` has no `svbony-camera`
    /// analogue, which is why this cell is tri-state where svbony's is a bare
    /// `AtomicBool`.
    Preserve,
}

/// The stop cell of exactly **one** capture: created by the `StartExposure`
/// that spawns it and moved into that capture's [`CaptureRequest`].
///
/// Per-capture rather than one cell shared by the handle, because a shared cell
/// is reset by whichever capture starts next — and a disconnect + reconnect
/// mid-exposure lets a *next* capture exist while the previous one is still
/// draining (`reset_exposure_state` releases the in-flight claim, so a new
/// `StartExposure` is accepted). Resetting a shared cell there erases
/// the disconnect's abort, and the old capture integrates on into the *new*
/// exposure's camera, where it can consume that frame or stop it at the SDK. A
/// capture can only ever signal its own cell, so that window cannot open.
#[derive(Debug, Default)]
pub struct StopSignal(AtomicU8);

/// Stop request for an in-flight capture: none, abort (discard), or stop (preserve).
const STOP_NONE: u8 = 0;
const STOP_ABORT: u8 = 1;
const STOP_PRESERVE: u8 = 2;

impl StopSignal {
    /// A fresh cell with nothing requested.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU8::new(STOP_NONE))
    }

    /// Ask this capture to stop: `preserve = false` aborts (discards the frame),
    /// `preserve = true` gracefully stops (keeps it).
    pub fn request(&self, preserve: bool) {
        self.0.store(
            if preserve { STOP_PRESERVE } else { STOP_ABORT },
            Ordering::SeqCst,
        );
    }

    /// What this capture has been asked to do.
    #[must_use]
    pub fn load(&self) -> StopRequest {
        match self.0.load(Ordering::SeqCst) {
            STOP_ABORT => StopRequest::Abort,
            STOP_PRESERVE => StopRequest::Preserve,
            _ => StopRequest::None,
        }
    }
}

/// Real-clock budget for the post-integration readout-completion poll. Only a
/// safety bound on the stop-responsive courtesy poll — `download_exposure` still
/// blocks until the frame is actually ready — but expressed as a deadline (not a
/// fixed nap count) so it cannot drift under blocking-pool oversubscription.
const READOUT_TIMEOUT: Duration = Duration::from_millis(2500);

/// Send a frame's gain, then its offset, through `write` (GO2).
///
/// The one arm sequence both handles run, so a mock capture arms exactly what
/// the production one would: a control the request carries no value for is
/// not sent, and a refusal names the control ahead of the SDK's own text.
fn arm_gain_and_offset(
    request: &CaptureRequest,
    mut write: impl FnMut(ControlType, i64) -> BackendResult<()>,
) -> BackendResult<()> {
    if let Some(gain) = request.gain {
        write(ControlType::Gain, i64::from(gain)).map_err(|e| e.context("failed to set gain"))?;
    }
    if let Some(offset) = request.offset {
        write(ControlType::Offset, i64::from(offset))
            .map_err(|e| e.context("failed to set offset"))?;
    }
    debug!(gain = ?request.gain, offset = ?request.offset, "exposure armed its gain and offset");
    Ok(())
}

/// The blocking camera operations the ASCOM `Camera` device drives.
///
/// Every method is synchronous (the SDK is blocking C FFI); the device offloads
/// the long [`capture`](CameraHandle::capture) onto `spawn_blocking`.
pub trait CameraHandle: std::fmt::Debug + Send + Sync {
    /// The stable ASCOM `UniqueID` (serial-derived; read once at enumeration).
    fn unique_id(&self) -> String;

    /// The camera's enumeration [`CameraInfo`] (cached; no open required).
    fn info(&self) -> CameraInfo;

    /// Whether the handle holds a camera, and whether the SDK has answered a
    /// call on it with `ASI_ERROR_CAMERA_REMOVED` since it was opened (C6).
    ///
    /// The two are one reading, never two: a close clears the lost mark as it
    /// lets the camera go, so "open" read before it and "not lost" read after
    /// would describe a live session that never existed.
    fn session(&self) -> SessionState;
    /// Whether the handle holds an open camera — including one that has since
    /// left the bus, which stays held until a close.
    fn is_open(&self) -> bool {
        self.session() != SessionState::Closed
    }
    /// Whether the open camera has been reported removed. Only a close lets
    /// it go; a fresh open starts unmarked.
    fn is_lost(&self) -> bool {
        self.session() == SessionState::Lost
    }
    /// Open the camera (a no-op when already open).
    ///
    /// # Errors
    ///
    /// Returns a [`BackendError`] if the SDK cannot open the camera; the
    /// handle stays closed.
    fn open(&self) -> BackendResult<()>;
    /// Close the camera (a no-op when already closed).
    ///
    /// # Errors
    ///
    /// Never fails in either shipped handle: the production close is a drop
    /// (`ASICloseCamera` has no error path here), and the mock only clears a
    /// flag.
    fn close(&self) -> BackendResult<()>;

    /// Enumerate the camera's tunable controls and their ranges (`ASIGetControlCaps`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>>;

    /// Read a control's current value (`ASIGetControlValue`); temperature is in
    /// 0.1 °C units (use [`temperature_celsius`](Self::temperature_celsius)).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error
    /// (a control the model lacks included).
    fn control_value(&self, control: ControlType) -> BackendResult<i64>;

    /// Electrons per ADU at the camera's **current gain** — a live read, since
    /// the SDK scales this field by the gain register (ST2). That register is
    /// the gain the last capture armed, not one a client has set since.
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn electrons_per_adu(&self) -> BackendResult<f32>;
    /// Set a control's value (`ASISetControlValue`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error
    /// if it refuses the write (a control the model lacks included).
    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()>;
    /// Sensor temperature in °C (decodes the 0.1 °C `ASI_TEMPERATURE` units).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn temperature_celsius(&self) -> BackendResult<f64>;

    /// Run a single-frame capture under one SDK lock: set the ROI, the gain and
    /// offset, and the exposure, start, integrate (honouring
    /// [`CaptureRequest::stop`]), poll to completion, download. Returns
    /// `Ok(Some(frame))` for a completed or gracefully-stopped exposure,
    /// `Ok(None)` for an aborted one (frame discarded), or `Err` on an SDK
    /// error.
    ///
    /// Stopping is signalled through the request's own [`StopSignal`], not a
    /// handle-wide cell, so one capture can never clear another's abort.
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed when the capture is
    /// configured (a close *during* integration is reported as `Ok(None)`
    /// instead, as is a close-and-reopen); the SDK's error if the ROI,
    /// start-position, or exposure write, the start, a status poll, the ROI
    /// read-back, or the download fails, or prefixed `failed to set gain: ` /
    /// `failed to set offset: ` if the gain or offset write does; `exposure
    /// failed` when the SDK reports the exposure as failed; or a message when
    /// the frame is too large to address on this target.
    fn capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>>;

    /// Start an ST4 pulse in `direction` (`ASIPulseGuideOn`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn pulse_guide_on(&self, direction: GuideDirection) -> BackendResult<()>;
    /// End the ST4 pulse in `direction` (`ASIPulseGuideOff`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn pulse_guide_off(&self, direction: GuideDirection) -> BackendResult<()>;
}

// --- production wrapper over zwo-rs ---------------------------------------------

/// The SDK camera IDs a service's [`ZwoCameraHandle`]s hold open.
///
/// One set is shared by every handle the service builds. An open looking for
/// its camera skips them: reading a candidate's serial opens it, and the close
/// after the read would close a sibling device's session (C6).
pub type HeldCameras = Arc<Mutex<std::collections::BTreeSet<i32>>>;

/// Production [`CameraHandle`] over a real (or `zwo-rs`-simulated) camera.
///
/// Holds the [`zwo_rs::Sdk`] and the camera's identity (its name, serial and
/// startup `index`), so it can find and re-open the RAII [`zwo_rs::Camera`] on
/// connect; the open handle lives behind a `Mutex<Option<…>>` because `Camera`
/// is `Send + !Sync`.
#[derive(Debug)]
pub struct ZwoCameraHandle {
    sdk: zwo_rs::Sdk,
    index: usize,
    info: CameraInfo,
    unique_id: String,
    /// The hardware serial read at enumeration; `None` for a camera that
    /// exposes none (`noserial-{index}`), which an open finds by name.
    serial: Option<String>,
    held: HeldCameras,
    camera: Mutex<Option<zwo_rs::Camera>>,
    /// Bumped by every open that actually opens a camera, so a capture can tell
    /// that the camera it configured was closed and reopened underneath it (a
    /// reconnect). The open camera is then the *next* exposure's, not this
    /// capture's, and this one must issue no further SDK calls against it —
    /// see [`ZwoCameraHandle::is_current`], which gates every SDK call a capture
    /// makes after it starts its exposure.
    open_epoch: AtomicU64,
    /// Set when a failure on the open camera turns out to mean it has left
    /// the bus (C6); cleared by an open or a close.
    ///
    /// Written only while holding [`Self::camera`] — the mark under the lock
    /// the failing call ran under (see [`Self::judge`]), the clears
    /// under the lock an open and a close change the camera under — so it
    /// always describes the camera in the slot, never one a reconnect has
    /// replaced. [`CameraHandle::session`] reads it under that lock too, beside
    /// the slot, so the two are never read from different sessions.
    lost: AtomicBool,
}

impl ZwoCameraHandle {
    /// Build a handle for the camera found at enumeration `index`, with its
    /// cached [`CameraInfo`], the `serial` read at enumeration (`None` when it
    /// has none) and the `unique_id` minted from it. `held` is the set every
    /// handle of the service shares.
    #[must_use]
    pub const fn new(
        sdk: zwo_rs::Sdk,
        index: usize,
        info: CameraInfo,
        unique_id: String,
        serial: Option<String>,
        held: HeldCameras,
    ) -> Self {
        Self {
            sdk,
            index,
            info,
            unique_id,
            serial,
            held,
            camera: Mutex::new(None),
            open_epoch: AtomicU64::new(0),
            lost: AtomicBool::new(false),
        }
    }

    /// Best-effort `ASIStopExposure` on the camera the capture at `epoch`
    /// started, re-acquiring the lock (the integration loop runs without it). A
    /// no-op if that camera was closed (e.g. by a disconnect) or has since been
    /// reopened — stopping the exposure of whichever camera happens to be open
    /// is exactly the cross-talk `epoch` exists to prevent.
    fn stop_at_sdk(&self, epoch: u64) {
        let guard = self.camera.lock();
        if let Some(camera) = guard.as_ref().filter(|_| self.is_current(epoch)) {
            let stopped = camera.stop_exposure().map_err(BackendError::from);
            // The stop's own outcome is best-effort; only what it says about
            // the camera's presence is kept (C6).
            let _ = self.judge(camera, stopped);
        }
        drop(guard);
    }

    /// Borrow the open camera for the closure's SDK work — a single call
    /// or a multi-call sequence that must share one lock acquisition.
    /// Returns the closed error when the handle slot is empty.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the camera reference borrows the handle guard to the closure's end; the guard scope is already minimal"
    )]
    fn with_camera<T>(
        &self,
        f: impl FnOnce(&zwo_rs::Camera) -> BackendResult<T>,
    ) -> BackendResult<T> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        // Still under the guard the call ran under (C6).
        self.judge(camera, f(camera))
    }

    /// Ask, on a failure, whether `camera` has left the bus (C6). A failure
    /// that does mean so marks the session lost, logged at `warn` the first
    /// time, and comes back relabelled [`BackendErrorKind::Departed`]; any
    /// other result comes back as it was.
    ///
    /// Call it while still holding [`Self::camera`] from the SDK call that
    /// produced `result`: an open and a close change the camera only under
    /// that lock, so the answer and the mark describe the camera that failed.
    /// Made after the guard was dropped, they could land on a camera a
    /// reconnect opened in between, ending a session nothing is wrong with.
    fn judge<T>(&self, camera: &zwo_rs::Camera, result: BackendResult<T>) -> BackendResult<T> {
        match result {
            Err(failure) if self.left_the_bus(camera, &failure) => {
                if !self.lost.swap(true, Ordering::SeqCst) {
                    warn!(
                        camera = %self.unique_id,
                        failure = %failure,
                        "the camera has left the bus; it reads disconnected until a client releases it"
                    );
                }
                Err(failure.departed())
            }
            other => other,
        }
    }

    /// Whether `failure`, met on `camera`, means the camera has left the bus.
    /// The SDK hides a departure until something rescans, and then answers
    /// every call on the camera's ID with `INVALID_ID`, so this rescans and asks
    /// ([`zwo_rs::Sdk::still_connected`]). An answer that says neither leaves
    /// the failure standing: a false "lost" ends a live session, and the
    /// reconnect after it resets the cooler (C5).
    fn left_the_bus(&self, camera: &zwo_rs::Camera, failure: &BackendError) -> bool {
        if failure.is_departed() {
            return true;
        }
        match self.sdk.still_connected(camera) {
            Ok(listed) => !listed,
            Err(e) => {
                debug!(
                    camera = %self.unique_id,
                    failure = %failure,
                    error = %e,
                    "presence check answered neither way; the failure stands"
                );
                false
            }
        }
    }

    /// Find this handle's camera on the bus as it is now and open it (C6).
    ///
    /// A rescan renumbers the SDK's camera list, so the index read at startup
    /// may name another camera, or none, once one has run. This rescans,
    /// takes the cameras listed under this camera's name that no sibling
    /// device holds, the startup index first, and opens the first whose serial
    /// matches. A serial is read without `ASIInitCamera`, as at enumeration
    /// (C0, C5). A camera without a serial is taken by its name alone.
    fn open_by_identity(&self) -> BackendResult<zwo_rs::Camera> {
        let listed = self.sdk.cameras()?;
        let held = self.held.lock().clone();
        let mut candidates: Vec<usize> = listed
            .iter()
            .enumerate()
            .filter(|(_, info)| info.name == self.info.name && !held.contains(&info.id))
            .map(|(index, _)| index)
            .collect();
        candidates.sort_by_key(|&index| index != self.index);
        for index in candidates {
            if let Some(want) = &self.serial {
                match self.sdk.open_uninitialised(index).and_then(|c| c.serial()) {
                    Ok(serial) if &serial == want => {}
                    Ok(_) => continue,
                    Err(e) => {
                        debug!(camera = %self.unique_id, index, error = %e, "candidate camera unreadable; skipped");
                        continue;
                    }
                }
            }
            let camera = self.sdk.open_camera(index)?;
            // The list can be rebuilt between the read above and this open;
            // check the camera opened is the one asked for.
            let same = self.serial.as_ref().map_or_else(
                || camera.info().name == self.info.name,
                |want| camera.serial().is_ok_and(|serial| &serial == want),
            );
            if same {
                return Ok(camera);
            }
            debug!(camera = %self.unique_id, index, "the camera list moved under the open; closed again");
        }
        debug!(camera = %self.unique_id, "camera not on the bus");
        Err(zwo_rs::Error::Asi(zwo_rs::AsiError::InvalidIndex).into())
    }

    /// Is the open camera still the instance `epoch` names, or has a reconnect
    /// replaced it? Call it while holding `self.camera` — the same lock
    /// [`CameraHandle::open`] bumps the epoch under — so the answer cannot go
    /// stale before the SDK calls it guards.
    fn is_current(&self, epoch: u64) -> bool {
        self.open_epoch.load(Ordering::SeqCst) == epoch
    }
}

impl CameraHandle for ZwoCameraHandle {
    fn unique_id(&self) -> String {
        self.unique_id.clone()
    }

    fn info(&self) -> CameraInfo {
        self.info.clone()
    }

    fn session(&self) -> SessionState {
        let guard = self.camera.lock();
        let session = if guard.is_none() {
            SessionState::Closed
        } else if self.lost.load(Ordering::SeqCst) {
            SessionState::Lost
        } else {
            SessionState::Live
        };
        drop(guard);
        session
    }

    fn open(&self) -> BackendResult<()> {
        let mut guard = self.camera.lock();
        if guard.is_none() {
            let camera = self.open_by_identity()?;
            self.held.lock().insert(camera.id());
            *guard = Some(camera);
            // Under the same lock as the open itself, so no capture can read an
            // epoch that does not match the camera it is about to configure.
            self.open_epoch.fetch_add(1, Ordering::SeqCst);
            // A camera just opened has answered nothing yet (C6).
            self.lost.store(false, Ordering::SeqCst);
        }
        drop(guard);
        Ok(())
    }

    fn close(&self) -> BackendResult<()> {
        let mut guard = self.camera.lock();
        if let Some(camera) = guard.as_ref() {
            self.held.lock().remove(&camera.id());
        }
        // Dropping the `Camera` calls `ASICloseCamera`, whatever the SDK
        // makes of a camera that has left the bus (C6).
        *guard = None;
        self.lost.store(false, Ordering::SeqCst);
        drop(guard);
        Ok(())
    }

    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
        self.with_camera(|camera| Ok(camera.control_caps()?))
    }

    fn control_value(&self, control: ControlType) -> BackendResult<i64> {
        self.with_camera(|camera| Ok(camera.control_value(control)?.value))
    }

    fn electrons_per_adu(&self) -> BackendResult<f32> {
        self.with_camera(|camera| Ok(camera.electrons_per_adu()?))
    }

    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.set_control_value(control, value, false)?))
    }

    fn temperature_celsius(&self) -> BackendResult<f64> {
        self.with_camera(|camera| Ok(camera.temperature_celsius()?))
    }

    fn capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>> {
        // Configure and start the exposure under the lock, then RELEASE it for
        // the integration: holding it for the whole exposure would block every
        // other SDK read — including `is_open()` from a concurrent request — for
        // the full duration. A second exposure is already barred by the device's
        // in-flight CAS, and ASI control/status reads are safe concurrently with
        // an integrating exposure (only ROI/format changes are not, and those
        // happen only here, at the start of a capture — as do the gain and
        // offset writes, which no ASI document says are safe beside one).
        let epoch = self.with_camera(|camera| {
            // The device negotiated this format against the camera's
            // `SupportedVideoFormat` and publishes it as the ASCOM readout mode
            // (RM1) — never assume 16-bit here.
            camera.set_roi_format(
                request.width,
                request.height,
                request.bin,
                request.image_type,
            )?;
            camera.set_start_pos(request.start_x, request.start_y)?;
            // The gain and offset this frame was accepted with, ahead of the
            // exposure time and the start, so the frame integrates at them
            // (GO2). Sent on every exposure, changed or not: there is no record
            // of the camera's own values to fall out of step with them.
            arm_gain_and_offset(&request, |control, value| {
                Ok(camera.set_control_value(control, value, false)?)
            })?;
            // `ASI_EXPOSURE` is a writable control on every ASI camera, and the
            // `zwo-rs` simulation models it too, so a failure here is a genuine
            // error: fail the capture rather than silently integrate for the
            // wrong exposure time.
            camera.set_control_value(ControlType::Exposure, request.exposure_us, false)?;
            camera.start_exposure(request.is_dark)?;
            // Read under the same lock acquisition that started the exposure, so
            // this epoch names exactly the camera instance this frame belongs to.
            Ok(self.open_epoch.load(Ordering::SeqCst))
        })?;

        // Integrate for the requested duration without holding the lock, checking
        // the stop signal every `step` so an abort/stop returns promptly.
        //
        // Track a real-clock DEADLINE, not accumulated *intended* sleep time.
        // Under blocking-pool oversubscription — ConformU fires a storm of
        // concurrent property reads, each a `spawn_blocking`, so the pool holds far
        // more threads than the runner has cores — an individual
        // `std::thread::sleep(20ms)` routinely overshoots its requested nap
        // several-fold. The earlier loop summed *intended* naps
        // (`elapsed += nap`), so it always ran the full step count regardless of
        // how long each nap actually took: a 2 s exposure ballooned to ~10 s of
        // wall-clock on a contended runner (observed on the macOS CI runner — a
        // scheduling artifact, not a slow CPU), tripping ConformU's 10 s
        // async-operation timeout. A deadline bounds the integration to the
        // requested duration plus at most one overshooting nap, whatever the
        // scheduler does.
        // The duration is validated against `ExposureMax` upstream, so this
        // cannot overflow the clock; falling back to `now` would end the wait
        // immediately and let the poll below do the bounding.
        let start = std::time::Instant::now();
        let deadline = start.checked_add(request.duration).unwrap_or(start);
        let step = Duration::from_millis(20);
        let mut preserve = false;
        loop {
            match request.stop.load() {
                StopRequest::Abort => {
                    self.stop_at_sdk(epoch);
                    return Ok(None);
                }
                StopRequest::Preserve => {
                    self.stop_at_sdk(epoch);
                    preserve = true;
                    break;
                }
                StopRequest::None => {}
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(step.min(deadline.saturating_duration_since(now)));
        }

        // Poll to completion (unless gracefully stopped) and download, under the
        // lock — but only against the camera this capture actually started. A
        // close mid-integration (a disconnect) leaves nothing to poll, and a
        // close-and-reopen (a reconnect) leaves the *next* exposure's camera
        // open, where polling or downloading would consume that exposure's
        // frame. Both are reported as an aborted capture.
        let guard = self.camera.lock();
        let Some(camera) = guard.as_ref().filter(|_| self.is_current(epoch)) else {
            return Ok(None);
        };
        // Still under the guard the readout ran under (C6).
        let frame = self.read_out(camera, &request, preserve);
        let frame = self.judge(camera, frame);
        drop(guard);
        frame
    }

    fn pulse_guide_on(&self, direction: GuideDirection) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.pulse_guide_on(direction)?))
    }

    fn pulse_guide_off(&self, direction: GuideDirection) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.pulse_guide_off(direction)?))
    }
}

impl ZwoCameraHandle {
    /// The readout half of a capture: poll the camera to readout completion
    /// (unless the frame was gracefully stopped, `preserve`), then download it.
    /// `Ok(None)` when an abort arrives mid-poll, unless its stop finds the camera
    /// gone. Run under the camera lock, by [`CameraHandle::capture`] alone.
    fn read_out(
        &self,
        camera: &zwo_rs::Camera,
        request: &CaptureRequest,
        preserve: bool,
    ) -> BackendResult<Option<Vec<u8>>> {
        if !preserve {
            // Poll the SDK to readout completion against a real-clock DEADLINE,
            // for the same reason as the capture's integration wait: a fixed
            // nap count drifts unpredictably under blocking-pool
            // oversubscription.
            let readout_start = std::time::Instant::now();
            let readout_deadline = readout_start
                .checked_add(READOUT_TIMEOUT)
                .unwrap_or(readout_start);
            let step = Duration::from_millis(10);
            loop {
                match request.stop.load() {
                    StopRequest::Abort => {
                        self.stop_in_readout(camera)?;
                        return Ok(None);
                    }
                    StopRequest::Preserve => {
                        self.stop_in_readout(camera)?;
                        break;
                    }
                    StopRequest::None => {}
                }
                match camera.exposure_status()? {
                    zwo_rs::ExposureStatus::Success | zwo_rs::ExposureStatus::Idle => break,
                    zwo_rs::ExposureStatus::Failed => {
                        return Err(BackendError::new("exposure failed"))
                    }
                    zwo_rs::ExposureStatus::Working => {
                        let now = std::time::Instant::now();
                        if now >= readout_deadline {
                            break;
                        }
                        std::thread::sleep(
                            step.min(readout_deadline.saturating_duration_since(now)),
                        );
                    }
                }
            }
        }

        // Size the buffer from the SDK's own view of the ROI, which is exactly
        // what `download_exposure` checks it against. That view now includes the
        // format the capture's arm set, so the non-Raw16 path this guarded
        // against — an 8-bit readout mode (RM1/RM2) — needs no second
        // bytes-per-pixel constant here.
        let frame_len = camera
            .roi_format()?
            .buffer_len()
            .ok_or_else(|| BackendError::new("frame is too large to address on this target"))?;
        let mut buf = vec![0u8; frame_len];
        camera.download_exposure(&mut buf)?;
        if rusty_photon_camera_core::is_blank_frame(&buf) {
            // A readout the camera left in the middle of can come back a
            // success with every pixel zero, so a blank frame counts as a
            // failure and asks before it is published (C6).
            let blank = BackendError::new("the readout returned a blank frame");
            if self.left_the_bus(camera, &blank) {
                return Err(blank.departed());
            }
            debug!(camera = %self.unique_id, "blank frame from a camera still on the bus; kept");
        }
        Ok(Some(buf))
    }

    /// The `ASIStopExposure` a stop sends from the readout poll. Best-effort, as
    /// the integration's is: a refused stop changes nothing the abort or the
    /// download has already settled. A failure that turns out to mean the camera
    /// has left the bus is the exception (C6). It goes back to the capture, which
    /// marks the session by it, rather than being dropped.
    fn stop_in_readout(&self, camera: &zwo_rs::Camera) -> BackendResult<()> {
        match camera.stop_exposure().map_err(BackendError::from) {
            Err(failure) if self.left_the_bus(camera, &failure) => Err(failure.departed()),
            _ => Ok(()),
        }
    }
}

// --- test mock -----------------------------------------------------------------

/// Exercise the *production* [`ZwoCameraHandle`] against the `zwo-rs` simulation
/// backend (the mock seam below covers the device logic; this covers the real
/// SDK wrapper that the BDD suite otherwise reaches only via the spawned binary).
#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod handle_tests {
    use super::*;
    use std::time::Duration;

    /// A handle over `sdk`'s simulated camera, minted as startup mints one:
    /// its serial read through an uninitialised open.
    fn sim_handle_on(sdk: zwo_rs::Sdk, held: HeldCameras) -> ZwoCameraHandle {
        let info = sdk.cameras().expect("enumerate")[0].clone();
        let serial = sdk
            .open_uninitialised(0)
            .and_then(|camera| camera.serial())
            .expect("simulated serial");
        let unique_id = format!("ZWO:Sim:{serial}");
        ZwoCameraHandle::new(sdk, 0, info, unique_id, Some(serial), held)
    }

    fn sim_handle() -> ZwoCameraHandle {
        sim_handle_on(
            zwo_rs::Sdk::new().expect("simulation SDK"),
            HeldCameras::default(),
        )
    }

    /// A 64x64 Raw16 request integrating for `duration`, signalled by `stop`.
    fn sim_request(duration: Duration, stop: &Arc<StopSignal>) -> CaptureRequest {
        CaptureRequest {
            width: 64,
            height: 64,
            bin: 1,
            start_x: 0,
            start_y: 0,
            exposure_us: 1_000,
            image_type: ImageType::Raw16,
            gain: None,
            offset: None,
            duration,
            is_dark: false,
            stop: Arc::clone(stop),
        }
    }

    /// Block until the capture running against `handle` has actually started its
    /// exposure at the SDK, so what follows lands mid-integration rather than
    /// racing the capture's own setup — a fixed nap would only approximate that
    /// on a loaded runner. Reading the simulated status also advances it
    /// (`Working` -> `Success`), which none of these tests depends on.
    fn wait_until_exposing(handle: &ZwoCameraHandle) {
        let start = std::time::Instant::now();
        loop {
            let exposing = handle.camera.lock().as_ref().is_some_and(|camera| {
                !matches!(camera.exposure_status(), Ok(zwo_rs::ExposureStatus::Idle))
            });
            if exposing {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "capture never started an exposure"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn production_handle_round_trips_against_the_sim_sdk() {
        let handle = sim_handle();
        assert_eq!(handle.unique_id(), "ZWO:Sim:0a1b2c3d4e5f6071");
        assert!(handle.info().is_cooler_cam);
        // Open/close lifecycle.
        assert!(!handle.is_open());
        handle.open().unwrap();
        assert!(handle.is_open());
        // Controls enumerate and round-trip; temperature decodes to °C.
        let caps = handle.control_caps().unwrap();
        assert!(caps.iter().any(|c| c.control_type == ControlType::Gain));
        handle.set_control_value(ControlType::Gain, 222).unwrap();
        assert_eq!(handle.control_value(ControlType::Gain).unwrap(), 222);
        let _ = handle.temperature_celsius().unwrap();
        // ST4 pulse guide is accepted on the (simulated) ST4-capable model.
        handle.pulse_guide_on(GuideDirection::North).unwrap();
        handle.pulse_guide_off(GuideDirection::North).unwrap();
        handle.close().unwrap();
        assert!(!handle.is_open());
    }

    /// GO2/E5, on the production handle: the gain and offset a request
    /// carries are on the camera while its frame integrates. The mock seam
    /// arms through the same sequence but into its own registers, so it cannot
    /// speak for this half; and `wait_until_exposing` takes the camera lock, so
    /// it sees the exposure only once the section that armed and started it
    /// has ended. The values are read before the abort and asserted after the
    /// join, so a failure does not leave a 30 s capture running.
    #[test]
    fn production_handle_capture_arms_gain_and_offset_before_the_frame_integrates() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let request = CaptureRequest {
            gain: Some(222),
            offset: Some(77),
            ..sim_request(Duration::from_secs(30), &stop)
        };
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);
        let integrating_at = (
            handle.control_value(ControlType::Gain).unwrap(),
            handle.control_value(ControlType::Offset).unwrap(),
        );
        stop.request(false);
        capturing.join().expect("capture thread").unwrap();
        assert_eq!(integrating_at, (222, 77));
        handle.close().unwrap();
    }

    /// GO1/GO2: a control the request carries no value for is not written, so
    /// the camera keeps whatever it holds.
    #[test]
    fn production_handle_capture_leaves_a_control_with_no_value_alone() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_control_value(ControlType::Gain, 300).unwrap();
        handle.set_control_value(ControlType::Offset, 90).unwrap();
        let stop = Arc::new(StopSignal::new());
        handle
            .capture(sim_request(Duration::from_millis(10), &stop))
            .unwrap()
            .expect("a completed frame");
        assert_eq!(handle.control_value(ControlType::Gain).unwrap(), 300);
        assert_eq!(handle.control_value(ControlType::Offset).unwrap(), 90);
        handle.close().unwrap();
    }

    #[test]
    fn production_handle_capture_produces_a_frame() {
        let handle = sim_handle();
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let frame = handle
            .capture(sim_request(Duration::from_millis(10), &stop))
            .unwrap()
            .expect("a completed frame");
        assert_eq!(frame.len(), 64 * 64 * 2);
        handle.close().unwrap();
    }

    /// E7: an abort on the capture's own stop cell discards the frame.
    #[test]
    fn production_handle_capture_honours_an_abort() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let request = sim_request(Duration::from_secs(30), &stop);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);
        stop.request(false);
        assert!(
            capturing.join().expect("capture thread").unwrap().is_none(),
            "an aborted capture must discard its frame"
        );
        handle.close().unwrap();
    }

    /// E8, the ZWO divergence: a graceful stop keeps the partial frame.
    #[test]
    fn production_handle_capture_honours_a_graceful_stop() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let request = sim_request(Duration::from_secs(30), &stop);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);
        stop.request(true);
        let frame = capturing
            .join()
            .expect("capture thread")
            .unwrap()
            .expect("a preserved frame");
        assert_eq!(frame.len(), 64 * 64 * 2);
        handle.close().unwrap();
    }

    /// A capture whose camera is closed and reopened under it — a disconnect
    /// plus reconnect mid-exposure — must not poll or download from the reopened
    /// instance: that camera belongs to whatever exposure the reconnected client
    /// starts next, and this frame is not there to be read.
    #[test]
    fn production_handle_capture_does_not_download_from_a_reopened_camera() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        // Long enough that the reconnect below always lands mid-integration, and
        // short enough that the capture drains promptly once it does.
        let request = sim_request(Duration::from_millis(500), &stop);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);
        handle.close().unwrap();
        handle.open().unwrap();
        assert!(
            capturing.join().expect("capture thread").unwrap().is_none(),
            "a capture must not read a frame off a camera reopened under it"
        );
        handle.close().unwrap();
    }

    /// A handle over a simulated camera that is off the bus while `departure`
    /// exists (C6).
    fn departing_sim_handle(departure: &std::path::Path) -> ZwoCameraHandle {
        sim_handle_on(
            zwo_rs::Sdk::new()
                .expect("simulation SDK")
                .with_departure_file(departure),
            HeldCameras::default(),
        )
    }

    /// C6: a failure on a camera that has left asks, finds it gone, and comes
    /// back as a departure, marking the session lost while the camera stays
    /// held.
    #[test]
    fn production_handle_marks_its_session_lost_when_a_failure_finds_the_camera_gone() {
        let dir = tempfile::tempdir().unwrap();
        let departure = dir.path().join("departed");
        let handle = departing_sim_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();

        let err = handle
            .set_control_value(ControlType::Gain, 100)
            .unwrap_err();

        assert!(err.is_departed(), "{err}");
        assert!(handle.is_lost());
        assert!(handle.is_open(), "lost is not closed");
    }

    /// C6: only a failure asks. A departure the SDK still hides answers reads
    /// from memory, and the session is left as it was.
    #[test]
    fn production_handle_asks_nothing_of_a_read_that_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let departure = dir.path().join("departed");
        let handle = departing_sim_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();

        handle.temperature_celsius().unwrap();
        handle.control_value(ControlType::Gain).unwrap();

        assert!(!handle.is_lost());
    }

    /// C6: a failure on a camera still on the bus asks, finds it there, and
    /// stands as the failure it is.
    #[test]
    fn production_handle_keeps_a_session_that_fails_on_a_present_camera() {
        let handle = sim_handle();
        handle.open().unwrap();

        // The simulated `Temperature` control is read-only.
        let err = handle
            .set_control_value(ControlType::Temperature, 1)
            .unwrap_err();

        assert!(!err.is_departed(), "{err}");
        assert!(!handle.is_lost());
    }

    /// C6: a close releases the lost session, an open is refused while the
    /// camera is gone, and an open once it is back finds it by its identity
    /// and starts unmarked, though a rescan has renumbered the SDK's list.
    #[test]
    fn production_handle_reopens_a_returned_camera_unmarked() {
        let dir = tempfile::tempdir().unwrap();
        let departure = dir.path().join("departed");
        let handle = departing_sim_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();
        handle
            .set_control_value(ControlType::Gain, 100)
            .unwrap_err();
        assert!(handle.is_lost());

        handle.close().unwrap();
        assert!(!handle.is_lost());
        assert!(!handle.is_open());
        handle.open().unwrap_err();
        assert!(!handle.is_open());

        std::fs::remove_file(&departure).unwrap();
        handle.open().unwrap();
        assert!(!handle.is_lost());
        handle.set_control_value(ControlType::Gain, 100).unwrap();
        handle.close().unwrap();
    }

    /// C6: an open never takes a camera another device of the service holds.
    /// Reading a candidate's serial opens it, and the close after the read
    /// would end that device's session.
    #[test]
    fn production_handle_open_skips_a_camera_a_sibling_holds() {
        let held = HeldCameras::default();
        let first = sim_handle_on(zwo_rs::Sdk::new().unwrap(), Arc::clone(&held));
        let second = sim_handle_on(zwo_rs::Sdk::new().unwrap(), Arc::clone(&held));
        first.open().unwrap();

        second.open().unwrap_err();
        assert!(!second.is_open());

        first.close().unwrap();
        second.open().unwrap();
        second.close().unwrap();
    }

    /// C6, the capture path: a camera that leaves while its frame integrates
    /// is found out by the readout poll, which finds the exposure failed and
    /// asks, under the lock that poll holds. The capture fails as a departure.
    #[test]
    fn production_handle_capture_whose_camera_leaves_mid_frame_fails_as_departed() {
        let dir = tempfile::tempdir().unwrap();
        let departure = dir.path().join("departed");
        let handle = Arc::new(departing_sim_handle(&departure));
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let request = sim_request(Duration::from_millis(500), &stop);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);

        std::fs::write(&departure, b"").unwrap();

        let err = capturing.join().expect("capture thread").unwrap_err();
        assert!(err.is_departed(), "{err}");
        assert!(handle.is_lost());
    }

    /// C6: a stop that reaches the readout poll, on a camera a rescan has
    /// already dropped, fails, asks, and fails the readout as a departure for
    /// the capture to mark its session by, rather than ending it as an
    /// ordinary abort or a graceful stop.
    #[test]
    fn production_handle_readout_stop_on_a_dropped_camera_fails_as_departed() {
        for preserve in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let departure = dir.path().join("departed");
            let handle = departing_sim_handle(&departure);
            handle.open().unwrap();
            let stop = Arc::new(StopSignal::new());
            let request = sim_request(Duration::ZERO, &stop);
            stop.request(preserve);
            std::fs::write(&departure, b"").unwrap();
            // Another device's check, say: the camera list loses it.
            zwo_rs::Sdk::new()
                .unwrap()
                .with_departure_file(&departure)
                .camera_count()
                .unwrap();

            let guard = handle.camera.lock();
            let camera = guard.as_ref().expect("an open camera");
            let err = handle.read_out(camera, &request, false).unwrap_err();
            drop(guard);

            assert!(err.is_departed(), "preserve = {preserve}: {err}");
        }
    }

    /// C6: a frame that downloads blank counts as a failure. From a camera
    /// that has gone it is discarded, and the capture fails as a departure
    /// rather than publishing an all-zero frame.
    #[test]
    fn production_handle_discards_a_blank_frame_from_a_departed_camera() {
        let dir = tempfile::tempdir().unwrap();
        let departure = dir.path().join("departed");
        let handle = Arc::new(departing_sim_handle(&departure));
        handle.open().unwrap();
        let stop = Arc::new(StopSignal::new());
        let request = sim_request(Duration::from_secs(30), &stop);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_exposing(&handle);
        std::fs::write(&departure, b"").unwrap();

        // A graceful stop skips the readout poll and goes straight to the
        // download, which the departed simulated camera answers blank.
        stop.request(true);

        let err = capturing.join().expect("capture thread").unwrap_err();
        assert!(err.is_departed(), "{err}");
        assert!(handle.is_lost());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod error_tests {
    use super::*;

    /// The SDK's `CAMERA_REMOVED` crosses the seam as a departure on its own;
    /// every other code is `Other`, and only the handle's presence check can
    /// make one a departure (C6).
    #[test]
    fn only_a_camera_removed_answer_crosses_the_seam_as_a_departure() {
        let removed = BackendError::from(zwo_rs::Error::Asi(zwo_rs::AsiError::CameraRemoved));
        assert_eq!(removed.kind(), BackendErrorKind::Departed);
        for other in [
            zwo_rs::AsiError::CameraClosed,
            zwo_rs::AsiError::InvalidId,
            zwo_rs::AsiError::Timeout,
            zwo_rs::AsiError::GeneralError,
        ] {
            let err = BackendError::from(zwo_rs::Error::Asi(other));
            assert_eq!(err.kind(), BackendErrorKind::Other, "{other:?}");
        }
    }

    /// A failure the presence check finds to mean a departure is relabelled,
    /// its message kept.
    #[test]
    fn departed_relabels_a_failure_and_keeps_its_message() {
        let err = BackendError::from(zwo_rs::Error::Asi(zwo_rs::AsiError::GeneralError));
        let message = err.to_string();

        let err = err.departed();

        assert!(err.is_departed());
        assert_eq!(err.to_string(), message);
    }

    /// The arm reports a refused gain or offset with the control's name in
    /// front (GO2); a departure reported that way must still read as one (C6).
    #[test]
    fn context_names_the_step_and_keeps_the_kind() {
        let err = BackendError::from(zwo_rs::Error::Asi(zwo_rs::AsiError::CameraRemoved))
            .context("failed to set gain");
        assert!(err.is_departed());
        assert_eq!(
            err.to_string(),
            "failed to set gain: ASI camera SDK error: camera removed"
        );
    }
}

/// A configurable in-memory [`CameraHandle`] for the crate's unit tests, so the
/// device logic — including the paths the `zwo-rs` simulation cannot force, like
/// a mid-exposure SDK error (E9) or a model without an ST4 port (PG2) — is
/// exercised without hardware.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;

    /// Build the `ASI2600MM-Pro-Simulated` control set (Gain, Exposure, Offset,
    /// Temperature, `CoolerOn`, `TargetTemp`), mirroring `zwo-rs`'s `sim_control_caps`.
    fn default_caps() -> Vec<ControlCaps> {
        let cap = |name: &str, control_type, min, max, default, is_writable| ControlCaps {
            name: name.to_string(),
            control_type,
            min,
            max,
            default,
            is_writable,
            is_auto_supported: false,
        };
        vec![
            cap("Gain", ControlType::Gain, 0, 500, 100, true),
            cap(
                "Exposure",
                ControlType::Exposure,
                32,
                2_000_000_000,
                10_000,
                true,
            ),
            cap("Offset", ControlType::Offset, 0, 1000, 50, true),
            cap(
                "Temperature",
                ControlType::Temperature,
                -500,
                1000,
                0,
                false,
            ),
            cap("CoolerOn", ControlType::CoolerOn, 0, 1, 0, true),
            cap("TargetTemp", ControlType::TargetTemp, -40, 30, 0, true),
        ]
    }

    fn default_info() -> CameraInfo {
        CameraInfo {
            id: 0,
            name: "ASI2600MM-Pro-Simulated".to_string(),
            max_width: 6248,
            max_height: 4176,
            is_color: false,
            bayer_pattern: zwo_rs::BayerPattern::Rg,
            supported_bins: vec![1, 2, 3, 4],
            supported_video_formats: vec![zwo_rs::ImageType::Raw8, zwo_rs::ImageType::Raw16],
            pixel_size_um: 3.76,
            has_mechanical_shutter: false,
            has_st4_port: true,
            is_cooler_cam: true,
            is_usb3: true,
            e_per_adu: 0.25,
            bit_depth: 16,
            is_trigger_cam: false,
        }
    }

    /// Safety bound on the capture gate (see `run_capture`): long enough that no
    /// passing test reaches it, short enough that a wedged one still reports.
    const GATE_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Debug)]
    pub struct MockCameraHandle {
        info: CameraInfo,
        caps: Vec<ControlCaps>,
        open: AtomicBool,
        gain: Mutex<i64>,
        offset: Mutex<i64>,
        target_temp: Mutex<i64>,
        cooler_on: AtomicBool,
        /// E9 injection: make the next capture fail at the SDK.
        pub fail_capture: AtomicBool,
        /// Optional artificial integration time (for in-flight tests).
        capture_delay: Mutex<Duration>,
        /// While set, every capture parks at a gate placed *before* it reads its
        /// stop signal, and stays there until the gate is lowered. That lets a
        /// test hold one capture inside the device's "exposure in flight" window
        /// while it drives a disconnect, a reconnect, and a second exposure —
        /// the interleaving a sleep can only approximate.
        capture_gate: AtomicBool,
        /// One entry per `capture` call, in call order: `None` while the call is
        /// still running, then how it ended. A test can assert that a superseded
        /// capture really saw its abort instead of running on to a frame.
        capture_outcomes: Mutex<Vec<Option<CaptureOutcome>>>,
        /// The most recent [`CaptureRequest`] passed to `capture`, so a test can
        /// assert what the device configured — e.g. the negotiated download
        /// format (RM2).
        last_capture_request: Mutex<Option<CaptureRequest>>,
        /// Every control write the camera has taken, in call order, so a test
        /// can assert what reached the camera, in which order — and that
        /// nothing did.
        control_writes: Mutex<Vec<(ControlType, i64)>>,
        /// Controls whose writes the camera refuses, so an exposure arming one
        /// fails at the arm (GO2, E9).
        refused_writes: Mutex<Vec<ControlType>>,
        /// Controls whose reads the camera refuses, so a connect seeding one
        /// (GO1), and a getter that must not read one, can be exercised.
        refused_reads: Mutex<Vec<ControlType>>,
        /// The camera is off the bus ([`Self::leave_bus`]): every call that
        /// reaches it answers `CAMERA_REMOVED`, and it cannot be opened.
        departed: AtomicBool,
        /// The open session met a `CAMERA_REMOVED` answer (C6) — the mock's
        /// copy of the production handle's mark, set by [`Self::reach`].
        lost: AtomicBool,
        /// Calls still to reach the camera before it leaves the bus
        /// ([`Self::leave_bus_after`]); `None` when no departure is scheduled.
        departs_after: Mutex<Option<usize>>,
        /// Whether a `CAMERA_REMOVED` answer marks the session lost; off only
        /// through [`Self::answer_removals_unmarked`].
        marks_removals: AtomicBool,
        /// How many opens have actually opened the camera ([`Self::opens`]).
        opens: std::sync::atomic::AtomicUsize,
    }

    /// How one mock [`capture`](CameraHandle::capture) call ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CaptureOutcome {
        /// Returned a frame (ran to completion, or gracefully stopped).
        Frame,
        /// Returned no frame: the capture saw its abort.
        Aborted,
        /// Returned the injected SDK error (E9).
        Failed,
    }

    impl Default for MockCameraHandle {
        fn default() -> Self {
            Self {
                info: default_info(),
                caps: default_caps(),
                open: AtomicBool::new(false),
                gain: Mutex::new(100),
                offset: Mutex::new(50),
                target_temp: Mutex::new(0),
                cooler_on: AtomicBool::new(false),
                fail_capture: AtomicBool::new(false),
                capture_delay: Mutex::new(Duration::ZERO),
                capture_gate: AtomicBool::new(false),
                capture_outcomes: Mutex::new(Vec::new()),
                last_capture_request: Mutex::new(None),
                control_writes: Mutex::new(Vec::new()),
                refused_writes: Mutex::new(Vec::new()),
                refused_reads: Mutex::new(Vec::new()),
                departed: AtomicBool::new(false),
                lost: AtomicBool::new(false),
                departs_after: Mutex::new(None),
                marks_removals: AtomicBool::new(true),
                opens: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl MockCameraHandle {
        /// Drop a control so it reports unavailable (e.g. remove `Gain` to test
        /// the `NOT_IMPLEMENTED` gate, GO1).
        pub fn without_control(mut self, control: ControlType) -> Self {
            self.caps.retain(|c| c.control_type != control);
            self
        }

        /// Override a control's caps range (e.g. a gain range too wide for
        /// ASCOM's `i32`, to test that the control is left unadvertised rather
        /// than clamped).
        pub fn with_control_range(mut self, control: ControlType, min: i64, max: i64) -> Self {
            for cap in &mut self.caps {
                if cap.control_type == control {
                    cap.min = min;
                    cap.max = max;
                }
            }
            self
        }

        /// Present a model advertising exactly `formats` as its
        /// `SupportedVideoFormat` — the default mirrors a camera offering both
        /// raw formats. Drives the readout-mode negotiation (RM1) and its
        /// no-usable-format connect failure (RM3), neither of which the `zwo-rs`
        /// simulation can present.
        pub fn with_video_formats(mut self, formats: Vec<ImageType>) -> Self {
            self.info.supported_video_formats = formats;
            self
        }

        /// The most recent request `capture` received, if any.
        pub fn last_capture_request(&self) -> Option<CaptureRequest> {
            self.last_capture_request.lock().clone()
        }

        /// Hold every capture at the gate (or release the held ones).
        pub fn set_capture_gate(&self, closed: bool) {
            self.capture_gate.store(closed, Ordering::SeqCst);
        }

        /// How each `capture` call so far ended, in call order; `None` for one
        /// still running (parked at the gate, say).
        pub fn capture_outcomes(&self) -> Vec<Option<CaptureOutcome>> {
            self.capture_outcomes.lock().clone()
        }

        /// Every control write the camera has taken so far, in call order.
        pub fn control_writes(&self) -> Vec<(ControlType, i64)> {
            self.control_writes.lock().clone()
        }

        /// Make the camera refuse writes to `control`, or take them again.
        pub fn refuse_writes(&self, control: ControlType, refused: bool) {
            Self::mark(&self.refused_writes, control, refused);
        }

        /// Make the camera refuse reads of `control`, or answer them again.
        pub fn refuse_reads(&self, control: ControlType, refused: bool) {
            Self::mark(&self.refused_reads, control, refused);
        }

        fn mark(list: &Mutex<Vec<ControlType>>, control: ControlType, on: bool) {
            let mut list = list.lock();
            list.retain(|c| *c != control);
            if on {
                list.push(control);
            }
        }

        /// Put the gain or offset register at `value` without a write — where
        /// another application, or an earlier session, leaves a camera.
        pub fn preset_control(&self, control: ControlType, value: i64) {
            assert!(
                matches!(control, ControlType::Gain | ControlType::Offset),
                "the mock has no {control:?} register to preset"
            );
            let register = if control == ControlType::Gain {
                &self.gain
            } else {
                &self.offset
            };
            *register.lock() = value;
        }

        /// Present a model with no ST4 port (PG2's `NOT_IMPLEMENTED` branch).
        pub fn without_st4(mut self) -> Self {
            self.info.has_st4_port = false;
            self
        }

        /// Present a non-cooled model (K1's `NOT_IMPLEMENTED` branch).
        pub fn without_cooler(mut self) -> Self {
            self.info.is_cooler_cam = false;
            self
        }

        /// Present a model with the given gain-0 electrons-per-ADU and ADC bit
        /// depth (ST2: the reported value is this figure scaled by the current
        /// gain).
        pub fn with_signal(mut self, e_per_adu_at_gain_0: f32, bit_depth: u32) -> Self {
            self.info.e_per_adu = e_per_adu_at_gain_0;
            self.info.bit_depth = bit_depth;
            self
        }

        /// Present a colour model with the given Bayer pattern (ST1: `SensorType`
        /// RGGB and the `BayerOffsetX/Y` mapping, vs the mono `NOT_IMPLEMENTED`).
        pub fn with_color(mut self, pattern: zwo_rs::BayerPattern) -> Self {
            self.info.is_color = true;
            self.info.bayer_pattern = pattern;
            self
        }

        pub fn set_capture_delay(&self, delay: Duration) {
            *self.capture_delay.lock() = delay;
        }

        /// Take the camera off the bus, as a cut cable or power does (C6).
        pub fn leave_bus(&self) {
            self.departed.store(true, Ordering::SeqCst);
        }

        /// Take the camera off the bus once `calls` more calls have reached
        /// it, so the departure lands partway through a multi-call sequence
        /// such as the connect handshake.
        pub fn leave_bus_after(&self, calls: usize) {
            *self.departs_after.lock() = Some(calls);
        }

        /// How many opens so far have actually opened the camera; an open of
        /// an already open handle is a no-op and is not counted. Each counted
        /// open stands for an `ASIInitCamera` on the production handle (C5).
        pub fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }

        /// Put the camera back on the bus.
        pub fn return_to_bus(&self) {
            self.departed.store(false, Ordering::SeqCst);
        }

        /// Answer a departed camera's calls `CAMERA_REMOVED` without marking
        /// the session lost. What a caller finds when a reconnect clears the
        /// mark between the SDK's answer and the caller's look at the session.
        pub fn answer_removals_unmarked(&self) {
            self.marks_removals.store(false, Ordering::SeqCst);
        }

        /// What every call that reaches the camera meets first. A departed
        /// camera answers `CAMERA_REMOVED`, and the answer marks the open
        /// session lost — the production handle's rule (C6), reproduced on
        /// the mock's own flags.
        fn reach(&self) -> BackendResult<()> {
            let mut departs_after = self.departs_after.lock();
            match *departs_after {
                Some(0) => {
                    self.departed.store(true, Ordering::SeqCst);
                    *departs_after = None;
                }
                Some(calls) => *departs_after = Some(calls - 1),
                None => {}
            }
            drop(departs_after);
            if !self.departed.load(Ordering::SeqCst) {
                return Ok(());
            }
            if self.open.load(Ordering::SeqCst) && self.marks_removals.load(Ordering::SeqCst) {
                self.lost.store(true, Ordering::SeqCst);
            }
            Err(zwo_rs::Error::Asi(zwo_rs::AsiError::CameraRemoved).into())
        }

        /// The capture proper; [`CameraHandle::capture`] wraps it to record how
        /// it ended.
        fn run_capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>> {
            *self.last_capture_request.lock() = Some(request.clone());
            // The gate is read BEFORE the stop signal, so a capture held here has
            // not yet had the chance to observe an abort — exactly the state a
            // reconnect + second exposure has to race against.
            //
            // Bounded, because a test that panics between raising the gate and
            // lowering it would otherwise leave this thread parked forever —
            // and dropping the test's Tokio runtime waits on the blocking pool,
            // so the whole test binary would hang instead of reporting the
            // failure. The bound matches the tests' own 30 s deadline waits: a
            // gate still closed by then means the test has already failed.
            let gate_start = std::time::Instant::now();
            while self.capture_gate.load(Ordering::SeqCst) && gate_start.elapsed() < GATE_TIMEOUT {
                std::thread::sleep(Duration::from_millis(1));
            }
            // The arm's first SDK call, as on the production handle.
            self.reach()?;
            // Arm the frame's gain and offset where the production handle does
            // — before the integration — and through the same sequence, so the
            // registers (and the ElectronsPerADU derived from the gain) hold
            // what a real arm would have left there.
            arm_gain_and_offset(&request, |control, value| {
                self.set_control_value(control, value)
            })?;
            let delay = *self.capture_delay.lock();
            // Mirror the production handle: sleep against a real-clock DEADLINE,
            // not accumulated *intended* nap time, so the simulated capture can't
            // drift under a contended runtime.
            let deadline = std::time::Instant::now() + delay;
            let step = Duration::from_millis(10);
            loop {
                match request.stop.load() {
                    StopRequest::Abort => return Ok(None),
                    StopRequest::Preserve => break,
                    StopRequest::None => {}
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::sleep(step.min(deadline.saturating_duration_since(now)));
            }
            // The readout poll: the first SDK call after the integration, and
            // so where a camera that left mid-frame is found out.
            self.reach()?;
            if self.fail_capture.load(Ordering::SeqCst) {
                return Err(BackendError::new("simulated capture failure"));
            }
            Ok(Some(vec![
                0u8;
                request.width as usize
                    * request.height as usize
                    * request.image_type.bytes_per_pixel()
            ]))
        }
    }

    impl CameraHandle for MockCameraHandle {
        fn unique_id(&self) -> String {
            "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071".to_string()
        }

        fn info(&self) -> CameraInfo {
            self.info.clone()
        }

        fn session(&self) -> SessionState {
            if !self.open.load(Ordering::SeqCst) {
                SessionState::Closed
            } else if self.lost.load(Ordering::SeqCst) {
                SessionState::Lost
            } else {
                SessionState::Live
            }
        }

        fn open(&self) -> BackendResult<()> {
            if self.departed.load(Ordering::SeqCst) {
                // What the SDK answers for an index past the connected count.
                return Err(zwo_rs::Error::Asi(zwo_rs::AsiError::InvalidIndex).into());
            }
            if !self.open.swap(true, Ordering::SeqCst) {
                self.lost.store(false, Ordering::SeqCst);
                self.opens.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn close(&self) -> BackendResult<()> {
            self.open.store(false, Ordering::SeqCst);
            self.lost.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
            self.reach()?;
            Ok(self.caps.clone())
        }

        fn electrons_per_adu(&self) -> BackendResult<f32> {
            self.reach()?;
            // Mirrors a modern body's SDK response: the gain-0 figure scaled by
            // the gain register in 0.1 dB units. Real models differ (the legacy
            // ASI120MC-S uses another law), hence the live read in the driver.
            let gain = *self.gain.lock();
            let scale = 10f64.powf(gain as f64 / 200.0);
            Ok((f64::from(self.info.e_per_adu) / scale) as f32)
        }

        fn control_value(&self, control: ControlType) -> BackendResult<i64> {
            self.reach()?;
            if self.refused_reads.lock().contains(&control) {
                return Err(BackendError::new("simulated read refusal"));
            }
            let value = match control {
                ControlType::Gain => *self.gain.lock(),
                ControlType::Offset => *self.offset.lock(),
                ControlType::TargetTemp => *self.target_temp.lock(),
                ControlType::CoolerOn => i64::from(self.cooler_on.load(Ordering::SeqCst)),
                ControlType::CoolerPowerPerc => {
                    if self.cooler_on.load(Ordering::SeqCst) {
                        60
                    } else {
                        0
                    }
                }
                ControlType::Temperature => {
                    let celsius = if self.cooler_on.load(Ordering::SeqCst) {
                        *self.target_temp.lock()
                    } else {
                        20
                    };
                    celsius * 10
                }
                _ => return Err(BackendError::new("invalid control type")),
            };
            Ok(value)
        }

        fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
            self.reach()?;
            if self.refused_writes.lock().contains(&control) {
                return Err(BackendError::new("simulated write refusal"));
            }
            match control {
                ControlType::Gain => *self.gain.lock() = value,
                ControlType::Offset => *self.offset.lock() = value,
                ControlType::TargetTemp => *self.target_temp.lock() = value,
                ControlType::CoolerOn => self.cooler_on.store(value != 0, Ordering::SeqCst),
                ControlType::Exposure => {}
                _ => return Err(BackendError::new("invalid control type")),
            }
            self.control_writes.lock().push((control, value));
            Ok(())
        }

        fn temperature_celsius(&self) -> BackendResult<f64> {
            Ok(self.control_value(ControlType::Temperature)? as f64 / 10.0)
        }

        fn capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>> {
            let call = {
                let mut outcomes = self.capture_outcomes.lock();
                outcomes.push(None);
                outcomes.len().saturating_sub(1)
            };
            let result = self.run_capture(request);
            let outcome = match &result {
                Ok(Some(_)) => CaptureOutcome::Frame,
                Ok(None) => CaptureOutcome::Aborted,
                Err(_) => CaptureOutcome::Failed,
            };
            if let Some(slot) = self.capture_outcomes.lock().get_mut(call) {
                *slot = Some(outcome);
            }
            result
        }

        fn pulse_guide_on(&self, _direction: GuideDirection) -> BackendResult<()> {
            self.reach()
        }

        fn pulse_guide_off(&self, _direction: GuideDirection) -> BackendResult<()> {
            self.reach()
        }
    }
}

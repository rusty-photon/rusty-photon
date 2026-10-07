//! The SDK seam: a thin trait over the blocking `svbony-rs` `Camera` surface
//! the ASCOM device drives, plus a production wrapper and a test mock.
//!
//! Mirrors `zwo-camera`'s `backend.rs` seam pattern: it (1) collapses
//! [`svbony_rs::Error`] into a typed [`BackendError`] at one boundary, (2)
//! lets the ASCOM device hold an `Arc<dyn CameraHandle>` so unit tests can
//! substitute a mock that forces paths the `svbony-rs` simulation cannot —
//! a mid-exposure SDK error or an exceeded `SVBGetVideoData` deadline (E9),
//! a model without an ST4 port (PG2) — without hardware, and (3) keeps the
//! open/close lifecycle in one place. `svbony-rs`'s `Camera` is RAII (open =
//! [`svbony_rs::Sdk::open_camera`], close = drop) and `Send + !Sync`, so the
//! production handle keeps it behind a `parking_lot::Mutex` and re-opens on
//! connect from the cached enumeration `index`.
//!
//! **Phase E scope (this file).** The seam now covers every blocking SDK
//! operation the `Camera` trait needs: property/property-ex fetch (cached on
//! the open `svbony_rs::Camera`, so these are cheap once open), control
//! get/set (gain, exposure, black level, cooler enable/target/current-temp/
//! power), camera-mode select + video-capture start (called once at connect,
//! trigger cameras only — by `camera.rs`'s open handshake — see
//! `docs/services/svbony-camera.md` "Behavioral contracts → Exposure"
//! step 1), the soft-trigger [`CameraHandle::capture`] composite (ROI +
//! output format + exposure control + the armed gain and offset + trigger +
//! the `SVBGetVideoData` read deadline (see [`exposure_timeout_ms`]),
//! state-machine step 2), and pulse-guide.
//!
//! **The download format is the caller's choice, not this seam's.**
//! `capture` applies whatever [`CaptureRequest::image_type`] carries and
//! sizes its buffer from that format's `bytes_per_pixel`. `camera.rs`
//! negotiates the format once at connect from the camera's advertised
//! `SupportedVideoFormat` and publishes it as the ASCOM readout mode
//! (RM1/RM2) — this file never assumes 16-bit.
//!
//! **How `capture` aborts (hardware-verified, SV605CC).** `SVBony` has no
//! data-preserving or interruptible stop at the SDK level: real-hardware
//! probing confirmed that a concurrent `SVBStopVideoCapture` is *tolerated*
//! (no crash, and the handle survives a restart + fresh capture) but does
//! **not** unblock an in-flight `SVBGetVideoData`, which runs on to its full
//! deadline and then times out with the frame discarded. So no SDK call can
//! short-circuit a wait — but none is needed: `capture` already polls
//! `SVBGetVideoData` in short slices (see `VIDEO_DATA_POLL_MS`), so it
//! checks [`CaptureRequest::cancel`] between slices and bails out within
//! one slice of an abort/disconnect. The bail-out stops video capture
//! (discarding the in-flight frame — the SDK cannot preserve it anyway,
//! and a frame left in the SDK's buffer would surface as a stale frame on
//! the *next* exposure) and, for a trigger camera, re-arms it so the
//! connect-time "armed once" invariant holds for the next exposure.
//! `camera.rs`'s abort path additionally bumps the exposure generation
//! counter so a capture that completes *naturally* in the same instant is
//! still discarded — the same "single owner, generation-counter guard"
//! discipline `zwo-camera`'s `run_exposure`/`result_lock` uses.
//!
//! **One camera instance per capture.** A disconnect + reconnect during an
//! exposure opens a *new* `svbony_rs::Camera` for the next exposure while the
//! superseded capture may still be draining, so every SDK call a capture makes
//! after configuring its frame — the soft trigger (or the non-trigger restart),
//! the `SVBGetVideoData` polls, and the abort bail-out's stop/re-arm — goes
//! through [`SvbonyCameraHandle::with_camera_at`] against the `open_epoch` the
//! capture started on. A capture whose camera was reopened under it therefore
//! stops touching the SDK altogether and reports itself closed, instead of
//! triggering, consuming, or discarding the *new* exposure's frame.
//!
//! **Staying responsive during an in-flight exposure.** The production
//! handle's SDK mutex is released between `capture`'s ROI/control setup and
//! its trigger + `SVBGetVideoData` call, mirroring `zwo-camera`'s release-
//! during-integration pattern, so the simulation-only artificial wait (see
//! [`CaptureRequest::duration`]) never starves concurrent property/control
//! reads. For the `SVBGetVideoData` wait itself — the one genuinely
//! long-blocking real-hardware call, up to the read deadline (see
//! [`exposure_timeout_ms`]) — `capture` polls it in short slices (see
//! `VIDEO_DATA_POLL_MS`) instead of one single blocking call for the whole
//! deadline, **releasing the mutex between
//! polls**: a `SvbError::Timeout` from a short slice just means "no frame
//! yet," not a real failure, so the poll loop retries until either a frame
//! arrives or the overall deadline elapses. This bounds how long any other
//! `Camera` trait method that reaches the SDK (`Disconnect`, `CoolerOn`,
//! `CCDTemperature`, …) can be blocked waiting for the mutex to one poll
//! slice, not the whole exposure — `is_open` goes further still and is
//! backed by its own atomic (`SvbonyCameraHandle`'s `open` field) so
//! connection-state reads never contend the capture lock at all — every
//! `Camera` trait method calls `ensure_connected` first and must stay
//! responsive during an in-flight exposure. `Gain` and `Offset`, and their
//! setters, never take the mutex either: the device answers them from its
//! cache, and an exposure's `capture` is what sends them.
//!
//! **A camera that leaves the bus (C6).** The SDK's handle does not say so.
//! Measured on SDK 1.13.4, every call on a departed camera's handle goes on
//! answering from what the SDK cached, and `SVBGetVideoData` answers `Timeout`,
//! as it does for a frame not yet ready. What does change is the SDK's camera
//! table, and only on a rescan. So a call on the camera that fails asks
//! whether it is still there: the handle rescans the bus, looks for the
//! camera's serial, and marks itself lost when the rescan does not list it. A
//! frame read that runs out its deadline is such a failure, and so is a frame
//! that reads back blank. Nothing else asks, no timer and no read of the
//! connection state, so an idle departure is found by the next call that
//! fails. The question is asked under the camera lock the failed call held,
//! which `open` and `close` take too, so its verdict lands only on the session
//! that failed. [`BackendError`] keeps the SDK's status code beside its
//! message, so an `SVB_ERROR_CAMERA_REMOVED` (the SDK's own word for the case,
//! though never seen on hardware) marks the handle lost with no rescan, and it
//! carries whether its failure found the camera gone
//! ([`BackendError::left_the_bus`]). Like `open`, the mark is an atomic beside
//! the lock, so `is_lost` never waits on a capture, and a capture waiting for
//! its frame stops once another call's failure has set it. A lost camera is
//! still open, since the driver closes nothing on its own, and
//! [`CameraHandle::open`] refuses it until a close has released it. `open`
//! rescans too and finds the camera by serial, so one that came back opens
//! again without a reload.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};
use svbony_rs::{
    CameraInfo, CameraMode, CameraProperty, CameraPropertyEx, ControlCaps, ControlType,
    GuideDirection, ImageType, SvbError,
};

/// A `svbony-rs` SDK call failed, or this seam refused one.
///
/// Carries the message and, when the SDK answered with one, its status code,
/// so a status the driver acts on rather than reports — `CAMERA_REMOVED` (C6)
/// — survives the seam, and whether the rescan this failure asked for found
/// the camera gone (C6). The ASCOM device decides the `ASCOMError` per call
/// site.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    message: String,
    status: Option<SvbError>,
    gone: bool,
}

/// Convert a [`svbony_rs::Error`] into the seam error, keeping its status code.
impl From<svbony_rs::Error> for BackendError {
    fn from(err: svbony_rs::Error) -> Self {
        let status = match &err {
            svbony_rs::Error::Svb(status) => Some(*status),
            _ => None,
        };
        Self {
            message: err.to_string(),
            status,
            gone: false,
        }
    }
}

/// What a capture reports when it stops with no frame to hand back. `camera.rs`
/// discards a superseded capture's result either way — this text is what a log
/// line and the mock's outcome classification see.
const ABORTED_MESSAGE: &str = "exposure aborted";

impl BackendError {
    /// A failure of this seam's own, with no SDK status behind it.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            gone: false,
        }
    }

    fn closed() -> Self {
        Self::new("camera not open")
    }

    fn aborted() -> Self {
        Self::new(ABORTED_MESSAGE)
    }

    fn departed() -> Self {
        Self {
            gone: true,
            ..Self::new("the camera has left the bus")
        }
    }

    /// The message a log line, or an exposure's `last_error`, carries.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether this failure found the camera gone from the bus (C6): the SDK
    /// answered `SVB_ERROR_CAMERA_REMOVED`, or the rescan the failure asked
    /// for no longer listed the camera. Decided when the call failed, so it
    /// still says so after a reconnect has released the lost session.
    #[must_use]
    pub const fn left_the_bus(&self) -> bool {
        self.gone || self.camera_removed()
    }

    /// Whether the SDK answered `SVB_ERROR_CAMERA_REMOVED`, its own word for
    /// a camera that has left the bus (C6). SDK 1.13.4 was never seen
    /// answering it, since a departed camera's calls keep succeeding, but it
    /// needs no rescan to believe.
    const fn camera_removed(&self) -> bool {
        matches!(self.status, Some(SvbError::CameraRemoved))
    }

    /// This error with `context` ahead of its message and the rest kept, so
    /// a step that names itself does not hide what the SDK said.
    fn context(self, context: &str) -> Self {
        Self {
            message: format!("{context}: {}", self.message),
            ..self
        }
    }

    /// This error, now known to have found the camera gone from the bus (C6).
    fn found_gone(self) -> Self {
        Self {
            gone: true,
            ..self.context("the camera has left the bus")
        }
    }
}

/// Whether a frame read back blank, every byte zero. Stops at the first
/// non-zero byte, so a real frame costs next to nothing to check.
fn is_blank(frame: &[u8]) -> bool {
    frame.iter().all(|&byte| byte == 0)
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// Where `held` — a camera as it was enumerated — is in a fresh rescan of the
/// bus, or `None` when the rescan does not list it (C6). Found by serial, the
/// identity that survives an unplug and a replug. A camera that reports no
/// serial (`mint_identity`'s `noserial` case) is matched on everything else
/// the SDK reports for it, its SDK camera id included, and never on position
/// alone: with another camera in the roster, the one at its old index may be
/// a different camera, which would keep a departed session connected and
/// could be opened in its place. A changed roster therefore reads as the
/// camera gone.
fn locate(cameras: &[CameraInfo], held: &CameraInfo) -> Option<usize> {
    if held.serial.is_empty() {
        cameras.iter().position(|c| c == held)
    } else {
        cameras.iter().position(|c| c.serial == held.serial)
    }
}

/// The ROI + exposure parameters for a single soft-trigger capture, computed
/// and validated by the device (R1-R3, E3).
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// Post-binning ROI start X (`StartX`).
    pub start_x: u32,
    /// Post-binning ROI start Y (`StartY`).
    pub start_y: u32,
    /// Post-binning frame width (`NumX`).
    pub width: u32,
    /// Post-binning frame height (`NumY`).
    pub height: u32,
    /// Symmetric binning factor.
    pub bin: u32,
    /// Exposure time in microseconds (`SVB_EXPOSURE`'s hardware-confirmed
    /// unit — see `svbony_rs::ControlType::Exposure`'s doc comment).
    pub exposure_us: i64,
    /// Whether this camera is trigger-capable (`IsTriggerCam`): selects the
    /// soft-trigger path vs the non-trigger free-running restart fallback
    /// (state-machine step 5).
    pub is_trigger_cam: bool,
    /// The download format to configure for this frame — the readout mode
    /// the device negotiated at connect against the camera's
    /// `SupportedVideoFormat` (RM1/RM2). Sizes the `SVBGetVideoData` buffer
    /// and tells `camera.rs` which unpack the bytes need.
    pub image_type: ImageType,
    /// The gain (`SVB_GAIN`) to arm this frame with: written after the
    /// exposure, never before it (GO5), on every exposure whether or not a
    /// client changed it. `None` when the camera advertises no gain or has no
    /// value to arm (GO1), and then nothing is sent.
    pub gain: Option<i64>,
    /// The offset (`SVB_BLACK_LEVEL`) to arm this frame with, after the gain,
    /// on the same terms as [`Self::gain`].
    pub offset: Option<i64>,
    /// Wall-clock integration time the capture honours **under the
    /// `simulation` feature only** — `svbony-rs`'s simulated
    /// `get_video_data` never literally waits (see its doc comment), unlike
    /// the real `SVBGetVideoData`, which genuinely blocks for close to the
    /// exposure duration. Consulting this field on the real path would
    /// double-count the wait, so it is `#[cfg(feature = "simulation")]`-only
    /// in the production handle.
    pub duration: Duration,
    /// Set by the device's abort/disconnect path and checked between
    /// `SVBGetVideoData` poll slices, so an aborted capture drains within one
    /// slice instead of the rest of the read deadline — see the module docs
    /// ("How `capture` aborts").
    pub cancel: Arc<AtomicBool>,
}

impl CaptureRequest {
    /// Byte length of one frame at this request's geometry and download
    /// format, or `None` when that product exceeds what this target can
    /// address.
    ///
    /// The ROI arrives fixed-width because it is ASCOM device state; the
    /// buffer it describes is a length, so the conversion belongs here.
    /// Fallible rather than saturating because the caller *allocates* this
    /// many bytes — `usize::MAX` would abort the process instead of
    /// reporting anything.
    fn frame_len(&self) -> Option<usize> {
        let width = usize::try_from(self.width).ok()?;
        let height = usize::try_from(self.height).ok()?;
        width
            .checked_mul(height)?
            .checked_mul(self.image_type.bytes_per_pixel())
    }
}

/// Arm `request`'s exposure, then its gain, then its offset, through
/// `set_control_value` — the production handle's SDK call, or the mock's model
/// of it, so both run this one sequence.
///
/// The gain after the exposure write, never before it: the SDK refuses
/// `SVB_GAIN` while its auto-exposure state is on, and a manual exposure write
/// is the only thing that clears it (GO5). Both are sent on every exposure,
/// changed or not, so the camera is at the values this frame was accepted
/// with, whatever the SDK did to them since the last one. A refusal stops the
/// arm there; a refused gain or offset says which it was.
fn arm_controls(
    request: &CaptureRequest,
    mut set_control_value: impl FnMut(ControlType, i64) -> BackendResult<()>,
) -> BackendResult<()> {
    set_control_value(ControlType::Exposure, request.exposure_us)?;
    if let Some(gain) = request.gain {
        set_control_value(ControlType::Gain, gain).map_err(|e| e.context("failed to set gain"))?;
    }
    if let Some(offset) = request.offset {
        set_control_value(ControlType::BlackLevel, offset)
            .map_err(|e| e.context("failed to set offset"))?;
    }
    tracing::debug!(
        gain = ?request.gain,
        offset = ?request.offset,
        "gain and offset armed for this exposure"
    );
    Ok(())
}

/// The SDK's own `SVBGetVideoData` timeout recommendation, before
/// [`MIN_READ_DEADLINE_MS`] applies: twice the exposure plus 500 ms.
///
/// Takes microseconds and returns milliseconds — hence the `/ 1_000` below,
/// which is a unit conversion, not a scale factor. This is the deadline a
/// frame that costs only its own exposure needs; a read outliving it has paid
/// for something the exposure did not buy, which is what makes it worth a log
/// line.
///
/// The recommendation is recorded in `docs/plans/archive/svbony-camera.md`
/// "Verified SDK facts". Negative/zero exposures clamp to a `0` base so the
/// deadline never underflows.
fn recommended_read_deadline_ms(exposure_us: i64) -> i64 {
    let us = exposure_us.max(0);
    // 1_000 is a literal divisor, so the division is total; the base is a floor
    // the deadline must never drop below, so it saturates rather than wraps.
    (us.saturating_mul(2) / 1_000).saturating_add(500)
}

/// Floor under the read deadline, in milliseconds.
///
/// A session's first `SVBGetVideoData` pays a one-off ~2.6 s of SDK setup —
/// buffer allocation and USB — which the recommendation above cannot account
/// for: it scales with the exposure, while the setup is fixed, so short
/// exposures alone are charged for a cost they do not cause. The setup runs
/// alongside the integration rather than after it, so a session's first frame
/// arrives after `max(exposure, setup) + readout`, and the worst case this
/// floor answers for is the 2.25 s exposure where the recommendation takes
/// over: ~2.6 s of setup plus ~0.7 s of full-frame readout, which 5 s clears
/// with room for a loaded host. Above that exposure the recommendation is the
/// larger of the two and this floor never binds. Measured on an SV605CC — see
/// `docs/services/svbony-camera.md` "Behavioral contracts → Exposure" step 2d.
const MIN_READ_DEADLINE_MS: i64 = 5_000;

/// How long `capture` polls `SVBGetVideoData` for one frame before reporting
/// that the read timed out.
///
/// The SDK's [`recommended_read_deadline_ms`] under a [`MIN_READ_DEADLINE_MS`]
/// floor, as a pure, unit-testable function.
#[must_use]
pub fn exposure_timeout_ms(exposure_us: i64) -> i32 {
    let ms = recommended_read_deadline_ms(exposure_us).max(MIN_READ_DEADLINE_MS);
    i32::try_from(ms).unwrap_or(i32::MAX)
}

/// How long each `SVBGetVideoData` poll slice waits before `capture` checks
/// back in and, if no frame arrived, releases the SDK mutex and retries —
/// see the module docs ("Staying responsive during an in-flight exposure")
/// for why polling in slices instead of one blocking call for the whole
/// deadline matters.
/// Held as `u32` rather than the SDK's `i32`: a poll slice is a duration, so
/// the sign was never meaningful, and the type makes the widening to
/// `Duration`'s `u64` total. Only the SDK call narrows.
const VIDEO_DATA_POLL_MS: u32 = 250;

/// The blocking camera operations the ASCOM `Camera` device drives. Every
/// method is synchronous (the SDK is blocking C FFI); callers offload SDK
/// calls onto `spawn_blocking`.
pub trait CameraHandle: std::fmt::Debug + Send + Sync {
    /// The stable ASCOM `UniqueID` (serial-derived; read once at enumeration).
    fn unique_id(&self) -> String;

    /// The camera's enumeration [`CameraInfo`] (cached; no open required).
    fn info(&self) -> CameraInfo;

    /// Whether this handle holds an open camera: a session no close has ended,
    /// whether or not its camera is still on the bus (see
    /// [`is_lost`](Self::is_lost)). Never waits on a capture.
    fn is_open(&self) -> bool;
    /// Whether the open camera has left the bus: a call on it failed and the
    /// rescan that failure asked for did not find it, or a call answered
    /// `SVB_ERROR_CAMERA_REMOVED` (C6). Only a failure asks, so a departed
    /// camera nothing has failed on is not lost yet. A lost camera is still
    /// open — nothing closes it but [`close`](Self::close), which also clears
    /// the mark — and [`open`](Self::open) refuses it. Never waits on a
    /// capture.
    ///
    /// A caller asking both reads this before [`is_open`](Self::is_open): a
    /// close clears the open flag first and the mark second, so a mark found
    /// cleared by a close is followed by an open flag found cleared too.
    fn is_lost(&self) -> bool;
    /// Open the camera if it is closed. Returns `true` when THIS call
    /// performed the open — that caller owns the post-open handshake —
    /// and `false` when the handle was already open (a prior connect, or
    /// a concurrently racing one that won). The check and the open are
    /// one critical section under the handle's own lock, so exactly one
    /// racing caller ever observes `true` (the same shape as qhy-camera's
    /// `SharedCameraConnection`): the connect handshake's video-capture
    /// arm is not idempotent, so it must never run twice.
    ///
    /// # Errors
    ///
    /// Returns a [`BackendError`] if the camera is not on the bus or the SDK
    /// cannot open it, the handle staying closed; or if the handle still holds
    /// a camera that has left the bus, which only a close releases — opening
    /// it "again" would hand the lost session back as though it were fresh
    /// (C6).
    fn open(&self) -> BackendResult<bool>;
    /// Close the camera (a no-op when already closed), releasing one that has
    /// left the bus as well (C6).
    ///
    /// # Errors
    ///
    /// Never fails in either shipped handle: the production close is a drop
    /// (`SVBCloseCamera` has no error path here), and the mock only clears a
    /// flag.
    fn close(&self) -> BackendResult<()>;

    /// Restore the SDK's device-default parameter block
    /// (`SVBRestoreDefaultParam`) — the connect handshake's first post-open
    /// step (C1a), so a session never starts from parameters a previous one
    /// left behind. The SDK also re-persists the block to
    /// `<model>_Cfg_A.bin` in the process's working directory and reports
    /// `GeneralError` when that write fails even though the restore took
    /// effect, so callers treat a failure as advisory.
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error —
    /// the advisory `GeneralError` above included.
    fn restore_default_param(&self) -> BackendResult<()>;
    /// Enable/disable the SDK's parameter auto-save (`SVBSetAutoSaveParam`)
    /// — the connect handshake turns it off (C1a) so the SDK stops carrying
    /// session state through `<model>_Cfg_SAVE.bin` in the working
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn set_auto_save_param(&self, enable: bool) -> BackendResult<()>;

    /// The camera's [`CameraProperty`] (cached on the open `svbony_rs::Camera`
    /// at open time — a cheap accessor, no extra SDK call).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed; the production read
    /// is a cached copy with no failure of its own.
    fn property(&self) -> BackendResult<CameraProperty>;
    /// The camera's [`CameraPropertyEx`] (same caching as [`property`](Self::property)).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed; the production read
    /// is a cached copy with no failure of its own.
    fn property_ex(&self) -> BackendResult<CameraPropertyEx>;
    /// Sensor pixel size in microns (`SVBGetSensorPixelSize`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn pixel_size_microns(&self) -> BackendResult<f32>;

    /// Enumerate the camera's tunable controls and their ranges
    /// (`SVBGetControlCaps`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>>;
    /// Read a control's current value (`SVBGetControlValue`); temperature
    /// controls are in 0.1 °C units.
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error
    /// (a control the model lacks included).
    fn control_value(&self, control: ControlType) -> BackendResult<i64>;
    /// Set a control's value (`SVBSetControlValue`).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error
    /// if it refuses the write — a control the model lacks included.
    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()>;

    /// Select the camera acquisition mode (`SVBSetCameraMode`) — called once
    /// at connect for a trigger-capable camera, never per-exposure
    /// (state-machine step 1).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn set_camera_mode(&self, mode: CameraMode) -> BackendResult<()>;
    /// Start video capture (`SVBStartVideoCapture`) — called once at connect
    /// for a trigger-capable camera (state-machine step 1; tenet 3 forbids
    /// this at connect for a non-trigger camera, since its only mode is
    /// free-running), and per-exposure only on the non-trigger-camera
    /// fallback path (step 5).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error —
    /// its refusal to arm capture that is already running included.
    fn start_video_capture(&self) -> BackendResult<()>;
    /// Stop video capture (`SVBStopVideoCapture`) — used only by the
    /// non-trigger-camera per-exposure restart (step 5); never called
    /// concurrently with an in-flight [`capture`](Self::capture) on another
    /// thread (see the module docs).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn stop_video_capture(&self) -> BackendResult<()>;

    /// Run one exposure under a single SDK lock: set ROI + output format +
    /// `SVB_EXPOSURE`, then arm [`CaptureRequest::gain`] and
    /// [`CaptureRequest::offset`] in that order (the gain strictly after the
    /// exposure write, GO5), trigger a frame (soft trigger, or a free-running
    /// restart for a non-trigger camera), then `SVBGetVideoData` under the
    /// read deadline [`exposure_timeout_ms`] computes. Returns the raw frame
    /// bytes in [`CaptureRequest::image_type`]'s layout.
    ///
    /// # Errors
    ///
    /// Returns `exposure aborted` once [`CaptureRequest::cancel`] is seen —
    /// which is where a mid-capture `set_connected(false)` normally lands,
    /// since `disconnect` sets that flag *before* closing the handle;
    /// `camera not open` when the handle is closed at a step this capture
    /// reaches before its next cancel check, and when a reconnect has replaced
    /// the camera it started on; the SDK's error if a setup write, the trigger
    /// or restart, or a `SVBGetVideoData` read fails — a refused gain or
    /// offset prefixed `failed to set gain: ` or `failed to set offset: `, and
    /// the read's timeout once the deadline passes with no frame included; or
    /// a message when the frame is too large to address on this target. Any of
    /// those SDK failures, and a frame that reads back blank, asks whether the
    /// camera is still on the bus, and one that found it gone says so
    /// ([`BackendError::left_the_bus`]); so does the error a capture stops with
    /// once another call's failure has found it gone (C6).
    fn capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>>;

    /// Issue an ST4 guide pulse (`SVBPulseGuide`) — blocks at the SDK level
    /// for `duration_ms` (see `camera.rs::pulse_guide`'s doc comment for why
    /// this seam keeps that a literal blocking call in v0).
    ///
    /// # Errors
    ///
    /// Returns `camera not open` if the handle is closed, or the SDK's error.
    fn pulse_guide(&self, direction: GuideDirection, duration_ms: i32) -> BackendResult<()>;
}

// --- production wrapper over svbony-rs ------------------------------------

/// Production [`CameraHandle`] over a real (or `svbony-rs`-simulated) camera.
///
/// Holds the [`svbony_rs::Sdk`] (a ZST outside the simulation) and the
/// camera's [`CameraInfo`] as it was enumerated, by which every open finds the
/// camera again (see [`locate`]); the open RAII [`svbony_rs::Camera`] lives
/// behind a `Mutex<Option<…>>` because `Camera` is `Send + !Sync`.
#[derive(Debug)]
pub struct SvbonyCameraHandle {
    sdk: svbony_rs::Sdk,
    info: CameraInfo,
    unique_id: String,
    camera: Mutex<Option<svbony_rs::Camera>>,
    /// Mirrors `camera.is_some()` but readable without contending the
    /// `camera` mutex — [`capture`](Self::capture) legitimately holds that
    /// mutex for a long time (up to the exposure's `SVBGetVideoData`
    /// deadline), and `is_open` backs `Device::connected`/`ensure_connected`,
    /// which ASCOM clients poll and which every other `Camera` method calls
    /// first — those must stay responsive during an in-flight exposure, not
    /// block for its whole duration.
    open: AtomicBool,
    /// Set when the open camera is found to have left the bus (C6): a failed
    /// call whose rescan does not find it, or an SDK call on it that answers
    /// `CAMERA_REMOVED` (see [`Self::note_failure`]). Set only under the
    /// `camera` lock the failed call held, so on the session that failed, and
    /// cleared by the
    /// [`close`](CameraHandle::close) that releases that camera, so it is
    /// never set while no camera is held. Readable without the lock, like
    /// `open`.
    lost: AtomicBool,
    /// Bumped by every [`open`](CameraHandle::open) that actually opens a
    /// camera, so a capture can tell that the camera it configured was closed
    /// and reopened underneath it (a reconnect). The open camera is then the
    /// *next* exposure's, not this capture's, and this one must issue no
    /// further SDK calls against it — see [`Self::with_camera_at`], which
    /// gates every SDK call a capture makes after configuring its frame.
    open_epoch: AtomicU64,
}

impl SvbonyCameraHandle {
    /// Build a handle for the camera enumerated as `info`, with the
    /// serial-derived `unique_id` read at enumeration.
    #[must_use]
    pub const fn new(sdk: svbony_rs::Sdk, info: CameraInfo, unique_id: String) -> Self {
        Self {
            sdk,
            info,
            unique_id,
            camera: Mutex::new(None),
            open: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            open_epoch: AtomicU64::new(0),
        }
    }

    /// Borrow the open camera for the closure's SDK work — a single call
    /// or a multi-call sequence that must share one lock acquisition.
    /// Returns the closed error when the handle slot is empty.
    ///
    /// This is for work that belongs to whichever camera is open now: the
    /// device's own property and control calls. Work belonging to one capture
    /// must go through [`Self::with_camera_at`] instead.
    fn with_camera<T>(
        &self,
        f: impl FnOnce(&svbony_rs::Camera) -> BackendResult<T>,
    ) -> BackendResult<T> {
        self.with_camera_epoch(None, f)
    }

    /// Borrow the camera the capture at `epoch` started on. Returns the closed
    /// error both when the slot is empty and when a reconnect has replaced the
    /// camera since — from that capture's point of view the two are the same
    /// thing: the camera it was working is gone, and the one open now belongs
    /// to the next exposure.
    fn with_camera_at<T>(
        &self,
        epoch: u64,
        f: impl FnOnce(&svbony_rs::Camera) -> BackendResult<T>,
    ) -> BackendResult<T> {
        self.with_camera_epoch(Some(epoch), f)
    }

    /// The shared body of [`Self::with_camera`] and [`Self::with_camera_at`]:
    /// one lock acquisition covering the epoch check, the SDK work it guards
    /// and the question that work's failure asks (C6), so none of the three
    /// can go stale between the others.
    fn with_camera_epoch<T>(
        &self,
        epoch: Option<u64>,
        f: impl FnOnce(&svbony_rs::Camera) -> BackendResult<T>,
    ) -> BackendResult<T> {
        let guard = self.camera.lock();
        let camera = guard
            .as_ref()
            .filter(|_| epoch.is_none_or(|epoch| self.is_current(epoch)))
            .ok_or_else(BackendError::closed)?;
        let outcome = f(camera).map_err(|e| self.note_failure(&guard, e));
        drop(guard);
        outcome
    }

    /// After a call on the camera in `held`'s slot failed with `error`, ask
    /// whether that camera is still on the bus (C6), and return the error the
    /// call should report: `error` itself, or `error` marked as having found
    /// the camera gone.
    ///
    /// A `CAMERA_REMOVED` needs no asking. Otherwise the bus is rescanned and
    /// the camera looked for, and only a rescan that succeeds and does not list
    /// it counts as gone: a rescan that fails gives no verdict, since a camera
    /// wrongly taken for gone costs a reconnect, which sets it up afresh. The
    /// guard is proof the camera lock is held: the failed call ran on the
    /// camera in that slot, and while the lock is held no close or reopen can
    /// come between the two, so the mark can never land on a camera a
    /// reconnect opened since.
    fn note_failure(
        &self,
        held: &MutexGuard<'_, Option<svbony_rs::Camera>>,
        error: BackendError,
    ) -> BackendError {
        if error.camera_removed() {
            self.mark_lost(held, &error);
            return error;
        }
        match self.find_on_bus() {
            Ok(Some(_)) => error,
            Ok(None) => {
                self.mark_lost(
                    held,
                    &format_args!("{error}, and a rescan of the bus no longer finds it"),
                );
                error.found_gone()
            }
            Err(rescan) => {
                tracing::debug!(
                    camera = %self.unique_id,
                    error = %error,
                    rescan = %rescan,
                    "a call failed and the rescan it asked for failed too; no verdict"
                );
                error
            }
        }
    }

    /// Ask whether the camera the capture at `epoch` read a blank frame from
    /// is still on the bus (C6). A blank frame counts as a failed read, since
    /// an SDK can hand back a stalled readout as a success, but it is not one:
    /// a camera still there keeps its frame, which may be blank for real.
    ///
    /// # Errors
    ///
    /// Returns the camera has left the bus when the rescan no longer finds
    /// it, and the closed error when a reconnect has replaced that camera.
    fn ask_after_a_blank_frame(&self, epoch: u64) -> BackendResult<()> {
        let guard = self.camera.lock();
        if guard.is_none() || !self.is_current(epoch) {
            return Err(BackendError::closed());
        }
        let asked = self.note_failure(&guard, BackendError::new("the frame read back blank"));
        drop(guard);
        if asked.left_the_bus() {
            Err(asked)
        } else {
            Ok(())
        }
    }

    /// Mark the camera in `held`'s slot lost (C6), logging the departure once.
    /// The guard is proof the camera lock is held, so the camera the verdict is
    /// about is the one marked.
    fn mark_lost(
        &self,
        held: &MutexGuard<'_, Option<svbony_rs::Camera>>,
        why: &dyn std::fmt::Display,
    ) {
        if held.is_some() && !self.lost.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                camera = %self.unique_id,
                why = %why,
                "the camera has left the bus; it reads disconnected until a client releases it"
            );
        }
    }

    /// This camera's index in a fresh rescan of the bus, or `None` when it is
    /// not there (see [`locate`]).
    fn find_on_bus(&self) -> BackendResult<Option<usize>> {
        Ok(locate(&self.sdk.cameras()?, &self.info))
    }

    /// Is the open camera still the instance `epoch` names, or has a reconnect
    /// replaced it? Gating an SDK call on this means holding `self.camera`
    /// across both — which [`Self::with_camera_epoch`] does — so the answer
    /// cannot go stale before the call it guards.
    fn is_current(&self, epoch: u64) -> bool {
        self.open_epoch.load(Ordering::SeqCst) == epoch
    }

    /// Configure the camera for `request`'s frame in one lock acquisition —
    /// the ROI, the download format, then [`arm_controls`]'s exposure, gain
    /// and offset — and return the epoch of the camera instance it
    /// configured.
    fn configure(&self, request: &CaptureRequest) -> BackendResult<u64> {
        self.with_camera(|camera| {
            camera.set_roi_format(
                request.start_x,
                request.start_y,
                request.width,
                request.height,
                request.bin,
            )?;
            // The device negotiated this format against the camera's
            // `SupportedVideoFormat` at connect and publishes it as the
            // ASCOM readout mode (RM1). Re-applied per exposure rather
            // than once at connect so a mode change between exposures
            // needs no separate SDK call.
            camera.set_output_image_type(request.image_type)?;
            arm_controls(request, |control, value| {
                Ok(camera.set_control_value(control, value, false)?)
            })?;
            // Read under the same lock acquisition that configured the frame,
            // so this epoch names exactly the camera instance the frame
            // belongs to.
            Ok(self.open_epoch.load(Ordering::SeqCst))
        })
    }

    /// Drain an aborted capture: stop video capture (discarding the
    /// in-flight frame — the SDK has no data-preserving stop, and a frame
    /// left in its buffer would surface as a stale frame on the next
    /// exposure) and re-arm it for a trigger camera so the connect-time
    /// "armed once" invariant holds for the next exposure. A non-trigger
    /// camera is left unarmed — its per-exposure restart (state-machine
    /// step 5) arms it again. Failures here are logged, not propagated:
    /// the capture's result is already being discarded.
    ///
    /// Both SDK calls are aimed at the camera the capture at `epoch` started
    /// on. If a reconnect has replaced it there is nothing of this capture's
    /// left to discard, and stopping whichever camera *is* open would throw
    /// away the next exposure's frame instead.
    ///
    /// A drain whose stop or re-arm fails asks whether the camera is still on
    /// the bus, as any other failed SDK call does (C6): for a capture
    /// cancelled in its wait, these are the only SDK calls it makes.
    fn abort_capture(&self, request: &CaptureRequest, epoch: u64) -> BackendResult<Vec<u8>> {
        let guard = self.camera.lock();
        if let Some(camera) = guard.as_ref().filter(|_| self.is_current(epoch)) {
            if let Err(e) = camera.stop_video_capture() {
                let e = self.note_failure(&guard, e.into());
                tracing::warn!(error = %e, "stopping video capture after an abort failed");
            }
            if request.is_trigger_cam {
                if let Err(e) = camera.start_video_capture() {
                    let e = self.note_failure(&guard, e.into());
                    tracing::warn!(error = %e, "re-arming video capture after an abort failed");
                }
            }
        }
        drop(guard);
        Err(BackendError::aborted())
    }
}

impl CameraHandle for SvbonyCameraHandle {
    fn unique_id(&self) -> String {
        self.unique_id.clone()
    }

    fn info(&self) -> CameraInfo {
        self.info.clone()
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn open(&self) -> BackendResult<bool> {
        let mut guard = self.camera.lock();
        if guard.is_some() {
            if self.lost.load(Ordering::Acquire) {
                return Err(BackendError::new(
                    "the camera has left the bus; it must be released before it can be opened",
                ));
            }
            return Ok(false);
        }
        // Rescanned first, and found by serial: the SDK's camera table changes
        // only on a rescan, so without one a camera that left and came back
        // is never opened again (measured on SDK 1.13.4).
        let index = self
            .find_on_bus()?
            .ok_or_else(|| BackendError::new("the camera is not on the bus"))?;
        *guard = Some(self.sdk.open_camera(index)?);
        // Under the same lock as the open itself, so no capture can read an
        // epoch that does not match the camera it is about to configure.
        self.open_epoch.fetch_add(1, Ordering::SeqCst);
        self.open.store(true, Ordering::Release);
        drop(guard);
        Ok(true)
    }

    fn close(&self) -> BackendResult<()> {
        let mut guard = self.camera.lock();
        // Dropping the `Camera` calls `SVBCloseCamera` — on a camera that has
        // left the bus too, which is how a lost session is released (C6).
        *guard = None;
        // `open` before `lost`, and a reader loads them the other way round
        // (see `CameraHandle::is_lost`): one that sees the mark cleared here
        // then sees the camera closed too, so "open and not lost" never reads
        // true for a camera on its way out.
        self.open.store(false, Ordering::Release);
        self.lost.store(false, Ordering::Release);
        drop(guard);
        Ok(())
    }

    fn restore_default_param(&self) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.restore_default_param()?))
    }

    fn set_auto_save_param(&self, enable: bool) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.set_auto_save_param(enable)?))
    }

    fn property(&self) -> BackendResult<CameraProperty> {
        self.with_camera(|camera| Ok(camera.property().clone()))
    }

    fn property_ex(&self) -> BackendResult<CameraPropertyEx> {
        self.with_camera(|camera| Ok(*camera.property_ex()))
    }

    fn pixel_size_microns(&self) -> BackendResult<f32> {
        self.with_camera(|camera| Ok(camera.pixel_size_microns()?))
    }

    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
        self.with_camera(|camera| Ok(camera.control_caps()?))
    }

    fn control_value(&self, control: ControlType) -> BackendResult<i64> {
        self.with_camera(|camera| Ok(camera.control_value(control)?.value))
    }

    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.set_control_value(control, value, false)?))
    }

    fn set_camera_mode(&self, mode: CameraMode) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.set_camera_mode(mode)?))
    }

    fn start_video_capture(&self) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.start_video_capture()?))
    }

    fn stop_video_capture(&self) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.stop_video_capture()?))
    }

    fn capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>> {
        // Configure under the lock, then RELEASE it for the artificial
        // simulation-only wait below — holding it there would block every
        // other SDK-backed call (property/control reads) for the whole
        // exposure, exactly the hazard `zwo-camera`'s `capture` avoids by
        // releasing its lock for the integration wait. The lock is
        // re-acquired below for the trigger + `SVBGetVideoData` call, which
        // — on real hardware — is unavoidably the long-held SDK operation
        // (see the module docs on why `capture` has no interrupt path).
        let epoch = self.configure(&request)?;

        // See `CaptureRequest::duration`'s doc comment: only the simulation
        // needs an artificial wait, since its `get_video_data` never really
        // blocks; the real SDK's `SVBGetVideoData` call below already blocks
        // for close to the exposure duration on real hardware. Sliced so an
        // abort during the simulated integration drains promptly too — and on
        // the production path the deadline is already past, so what is left is
        // exactly the one pre-trigger cancel check.
        // Validated against `ExposureMax` upstream, so this cannot overflow
        // the clock; `now` would simply end the wait at once.
        let wait_start = Instant::now();
        #[cfg(feature = "simulation")]
        let wait_deadline = wait_start
            .checked_add(request.duration)
            .unwrap_or(wait_start);
        #[cfg(not(feature = "simulation"))]
        let wait_deadline = {
            let _ = request.duration;
            wait_start
        };
        loop {
            if request.cancel.load(Ordering::Acquire) {
                return self.abort_capture(&request, epoch);
            }
            if self.lost.load(Ordering::Acquire) {
                return Err(BackendError::departed());
            }
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(u64::from(VIDEO_DATA_POLL_MS))));
        }

        self.with_camera_at(epoch, |camera| {
            if request.is_trigger_cam {
                camera.send_soft_trigger()?;
            } else {
                // Non-trigger cameras have no soft trigger: restart
                // free-running capture per exposure (state-machine step 5).
                // Untested by the simulation, which always reports
                // `IsTriggerCam = true`.
                camera.stop_video_capture()?;
                camera.start_video_capture()?;
            }
            Ok(())
        })?;

        let frame_len = request
            .frame_len()
            .ok_or_else(|| BackendError::new("frame is too large to address on this target"))?;
        let mut buf = vec![0u8; frame_len];

        // Poll `SVBGetVideoData` in short slices instead of one blocking call
        // for the whole read deadline, releasing the SDK mutex between polls —
        // see the module docs. A `SvbError::Timeout` from a short slice just
        // means "no frame yet"; retry until either a frame arrives or the
        // overall deadline elapses (at which point the final `Timeout` is the
        // real, reported error).
        // The timeout is derived from a validated exposure, so this cannot
        // overflow the clock; `now` would end the poll loop on its first pass.
        let poll_start = Instant::now();
        let deadline_ms = exposure_timeout_ms(request.exposure_us);
        let deadline = poll_start
            .checked_add(Duration::from_millis(
                u64::try_from(deadline_ms).unwrap_or(0),
            ))
            .unwrap_or(poll_start);
        let recommended = Duration::from_millis(
            u64::try_from(recommended_read_deadline_ms(request.exposure_us)).unwrap_or(0),
        );
        loop {
            // Abort/disconnect check between slices — the only interrupt
            // path this SDK admits (see the module docs, "How `capture`
            // aborts": a concurrent `SVBStopVideoCapture` is tolerated but
            // does NOT unblock a pending `SVBGetVideoData`, so short slices
            // + this check are what keep the drain bounded).
            if request.cancel.load(Ordering::Acquire) {
                return self.abort_capture(&request, epoch);
            }
            // A camera another call's failure has found gone (C6) delivers no
            // frame, every slice answering `Timeout`, so the capture ends here
            // rather than at its deadline.
            if self.lost.load(Ordering::Acquire) {
                return Err(BackendError::departed());
            }
            let remaining_ms = i32::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(i32::MAX);
            // The SDK takes `i32` milliseconds; the constant is far inside that
            // range, so the saturation below is a spelling, not a clamp.
            let poll_ms = i32::try_from(VIDEO_DATA_POLL_MS)
                .unwrap_or(i32::MAX)
                .min(remaining_ms)
                .max(1);
            // Inside the closure, so a read that fails for real fails the
            // closure and asks whether the camera is still there (C6). A
            // departed camera's read is one of those: it times out at the
            // deadline. A short slice's `Timeout` is only "no frame yet" while
            // the deadline has time left.
            let frame = self.with_camera_at(epoch, |camera| {
                match camera.get_video_data(&mut buf, poll_ms) {
                    Ok(()) => Ok(true),
                    Err(svbony_rs::Error::Svb(SvbError::Timeout)) if remaining_ms > 0 => Ok(false),
                    Err(e) => Err(e.into()),
                }
            })?;
            if frame {
                if is_blank(&buf) {
                    self.ask_after_a_blank_frame(epoch)?;
                }
                // A frame that outlived the SDK's recommendation is worth
                // a line: on a short exposure it is the floor that saved
                // it, and a setup cost growing towards the floor shows up
                // here before it starts failing exposures. Which of the two
                // applies is left to the figures — the deadline exceeding
                // the recommendation is the floor being what carried this
                // read — since past the crossover the two are equal and a
                // frame can still land just after the deadline.
                let elapsed = poll_start.elapsed();
                if elapsed > recommended {
                    // Both figures are computed here rather than inside the
                    // macro: a field expression runs only when the callsite
                    // is enabled, so a value written inline is dead code
                    // whenever nothing is listening.
                    let elapsed_ms = elapsed.as_millis();
                    let recommended_ms = recommended.as_millis();
                    tracing::debug!(
                        elapsed_ms,
                        recommended_ms,
                        deadline_ms,
                        exposure_us = request.exposure_us,
                        "frame arrived after the SDK's recommended read deadline"
                    );
                }
                return Ok(buf);
            }
        }
    }

    fn pulse_guide(&self, direction: GuideDirection, duration_ms: i32) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.pulse_guide(direction, duration_ms)?))
    }
}

#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod handle_tests {
    use super::*;

    fn sim_handle() -> SvbonyCameraHandle {
        let sdk = svbony_rs::Sdk::new().expect("simulation SDK");
        let info = sdk.cameras().expect("enumerate")[0].clone();
        SvbonyCameraHandle::new(sdk, info, "SVBONY:Sim:0a1b2c3d4e5f6071".to_string())
    }

    #[test]
    fn production_handle_round_trips_against_the_sim_sdk() {
        let handle = sim_handle();
        assert_eq!(handle.unique_id(), "SVBONY:Sim:0a1b2c3d4e5f6071");
        assert_ne!(handle.info().friendly_name, "");
        assert!(!handle.is_open());
        handle.open().unwrap();
        assert!(handle.is_open());

        let property = handle.property().unwrap();
        assert_eq!(property.max_width, 3008);
        assert!(property.is_trigger_cam);
        assert!(handle.property_ex().unwrap().supports_control_temp);
        assert!(handle.pixel_size_microns().unwrap() > 0.0);

        let caps = handle.control_caps().unwrap();
        assert!(caps.iter().any(|c| c.control_type == ControlType::Gain));
        // The connect handshake's order (C1a): restore defaults, auto-save
        // off, then a manual exposure write — the simulated SDK, like the
        // real one, refuses a gain write until that exposure write clears
        // its auto-exposure state.
        handle.restore_default_param().unwrap();
        handle.set_auto_save_param(false).unwrap();
        let refused = handle
            .set_control_value(ControlType::Gain, 222)
            .unwrap_err();
        assert!(
            refused.message().contains("general error"),
            "unexpected error: {}",
            refused.message()
        );
        handle
            .set_control_value(ControlType::Exposure, 1_000_000)
            .unwrap();
        handle.set_control_value(ControlType::Gain, 222).unwrap();
        assert_eq!(handle.control_value(ControlType::Gain).unwrap(), 222);

        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();

        handle.close().unwrap();
        assert!(!handle.is_open());
    }

    /// A request describing more bytes than this target can address has no
    /// frame length. `capture` allocates from it, so a saturated answer
    /// would be an allocation abort rather than a reported error.
    #[test]
    fn frame_len_declines_a_request_too_large_to_address() {
        let unaddressable = CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: u32::MAX,
            height: u32::MAX,
            bin: 1,
            exposure_us: 1_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw16,
            gain: None,
            offset: None,
            duration: Duration::from_millis(1),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(unaddressable.frame_len(), None);

        // One that does fit still answers, so the check is not vacuous.
        let ordinary = CaptureRequest {
            width: 800,
            height: 600,
            ..unaddressable
        };
        assert_eq!(ordinary.frame_len(), Some(800 * 600 * 2));
    }

    #[test]
    fn production_handle_capture_produces_a_frame() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let request = CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: 64,
            height: 64,
            bin: 1,
            exposure_us: 1_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw16,
            gain: None,
            offset: None,
            duration: Duration::from_millis(1),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let frame = handle.capture(request).unwrap();
        assert_eq!(frame.len(), 64 * 64 * 2);
        handle.close().unwrap();
    }

    /// A `Raw8` request configures the SDK for 8-bit output and downloads
    /// one byte per pixel — the fallback path a camera without `Raw16`
    /// takes, and the one an operator selects via the readout mode (RM2).
    #[test]
    fn production_handle_capture_downloads_the_requested_8_bit_format() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let request = CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: 64,
            height: 64,
            bin: 1,
            exposure_us: 1_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw8,
            gain: None,
            offset: None,
            duration: Duration::from_millis(1),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let frame = handle.capture(request).unwrap();
        assert_eq!(frame.len(), 64 * 64);
        handle.close().unwrap();
    }

    /// GO5 at the arm: an exposure that meets the SDK's auto-exposure state
    /// still on — a connect whose clearing write was refused, or a parameter
    /// restore nobody recorded — arms its gain all the same, because its own
    /// exposure write goes first and is what clears that state. Armed the
    /// other way round, the SDK refuses the gain and the frame fails.
    #[test]
    fn production_handle_capture_arms_gain_after_its_exposure_write() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        // The state a restore leaves, as an open does: auto-exposure on. The
        // precondition is checked, not assumed — with the state already off
        // this test would pass whatever order the arm wrote in.
        handle.restore_default_param().unwrap();
        handle
            .set_control_value(ControlType::Gain, 1)
            .expect_err("the SDK took a gain with auto-exposure on, so nothing here is tested");

        let cancel = Arc::new(AtomicBool::new(false));
        let request = CaptureRequest {
            gain: Some(222),
            offset: Some(30),
            ..sim_request(Duration::ZERO, &cancel)
        };
        let frame = handle.capture(request).unwrap();
        assert_eq!(frame.len(), 64 * 64 * 2);
        assert_eq!(handle.control_value(ControlType::Gain).unwrap(), 222);
        assert_eq!(handle.control_value(ControlType::BlackLevel).unwrap(), 30);
        handle.close().unwrap();
    }

    /// The region before the controls: an exposure whose region the SDK
    /// refuses fails there, before it arms a gain or an offset, so the camera
    /// keeps the values the last frame was armed with.
    #[test]
    fn production_handle_capture_refused_at_its_region_arms_no_gain_or_offset() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        handle
            .set_control_value(ControlType::Exposure, 1_000_000)
            .unwrap();
        handle.set_control_value(ControlType::Gain, 100).unwrap();
        handle
            .set_control_value(ControlType::BlackLevel, 5)
            .unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let request = CaptureRequest {
            width: 0,
            gain: Some(222),
            offset: Some(30),
            ..sim_request(Duration::ZERO, &cancel)
        };
        handle.capture(request).unwrap_err();
        assert_eq!(handle.control_value(ControlType::Gain).unwrap(), 100);
        assert_eq!(handle.control_value(ControlType::BlackLevel).unwrap(), 5);
        handle.close().unwrap();
    }

    /// A pre-cancelled capture drains immediately with the aborted error and
    /// leaves the trigger camera re-armed (stop + start), never waiting out
    /// the simulated integration.
    #[test]
    fn production_handle_capture_honours_a_cancelled_request() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let request = CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: 64,
            height: 64,
            bin: 1,
            exposure_us: 30_000_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw16,
            gain: None,
            offset: None,
            duration: Duration::from_secs(30),
            cancel: Arc::new(AtomicBool::new(true)),
        };
        let started = Instant::now();
        let err = handle.capture(request).unwrap_err();
        assert!(
            err.message().contains("aborted"),
            "unexpected error: {}",
            err.message()
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancelled capture should drain promptly, took {:?}",
            started.elapsed()
        );
        handle.close().unwrap();
    }

    #[test]
    fn production_handle_stop_video_capture_and_pulse_guide_round_trip() {
        let handle = sim_handle();
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::Normal).unwrap();
        handle.start_video_capture().unwrap();
        handle.stop_video_capture().unwrap();

        // `svbony-rs`'s simulated `SVBPulseGuide` is a no-op that never
        // fails regardless of `supports_pulse_guide` — the ST4-availability
        // gate lives in `camera.rs`'s ASCOM layer (`sensor.supports_pulse_guide`,
        // PG1), not in this seam. This exercises the delegation itself.
        handle.pulse_guide(GuideDirection::North, 5).unwrap();
        handle.close().unwrap();
    }

    /// A 64x64 Raw16 soft-trigger request integrating for `duration`,
    /// interruptible through `cancel`.
    fn sim_request(duration: Duration, cancel: &Arc<AtomicBool>) -> CaptureRequest {
        CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: 64,
            height: 64,
            bin: 1,
            exposure_us: 1_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw16,
            gain: None,
            offset: None,
            duration,
            cancel: Arc::clone(cancel),
        }
    }

    /// Block until the capture running against `handle` has pushed its own ROI
    /// to the SDK, so what follows lands after the capture has committed to the
    /// camera it started on rather than racing its setup — a fixed nap would
    /// only approximate that on a loaded runner.
    fn wait_until_configured(handle: &SvbonyCameraHandle, width: u32) {
        let start = Instant::now();
        loop {
            let configured = handle
                .camera
                .lock()
                .as_ref()
                .is_some_and(|camera| camera.roi_format().is_ok_and(|roi| roi.width == width));
            if configured {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "capture never configured its ROI"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Block until video capture has been started `count` times on the open
    /// camera. `capturing` alone cannot report a stop-then-start restart —
    /// both edges land inside the capture's own lock acquisition — so the
    /// simulation counts the starts instead.
    fn wait_until_video_capture_starts(handle: &SvbonyCameraHandle, count: u64) {
        let start = Instant::now();
        loop {
            let started = handle
                .camera
                .lock()
                .as_ref()
                .is_some_and(|camera| camera.video_capture_starts() >= count);
            if started {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "video capture was never started {count} times"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Stand in for the exposure a reconnected client starts next: give the
    /// freshly opened camera its own ROI and soft-trigger a frame onto it.
    fn arm_next_exposure(handle: &SvbonyCameraHandle) {
        let guard = handle.camera.lock();
        let camera = guard.as_ref().expect("camera open");
        camera.set_roi_format(0, 0, 64, 64, 1).expect("roi");
        camera.send_soft_trigger().expect("soft trigger");
    }

    /// Block until the capture running against `handle` has polled
    /// `SVBGetVideoData` at least `count` times, so what follows lands after
    /// the poll loop's own clock has started rather than after the last event
    /// that precedes it.
    fn wait_until_video_data_polled(handle: &SvbonyCameraHandle, count: u64) {
        let start = Instant::now();
        loop {
            let polled = handle
                .camera
                .lock()
                .as_ref()
                .is_some_and(|camera| camera.get_video_data_calls() >= count);
            if polled {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "the capture never polled for video data {count} times"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Reconnect: close, reopen, and re-run the connect handshake's
    /// mode-select plus video-capture arm (state-machine step 1).
    fn reconnect(handle: &SvbonyCameraHandle) {
        handle.close().expect("close");
        handle.open().expect("reopen");
        handle
            .set_camera_mode(CameraMode::TrigSoft)
            .expect("mode select");
        handle.start_video_capture().expect("re-arm capture");
    }

    /// The abort path that matters on hardware: a capture already polling
    /// `SVBGetVideoData` bails within one slice instead of sitting out the rest
    /// of its read deadline (the existing pre-cancelled test never gets that
    /// far — it returns from the integration wait). Driven with a request the
    /// simulation never produces a frame for, so the poll loop is genuinely
    /// where the cancel lands.
    #[test]
    fn production_handle_capture_cancelled_mid_poll_drains_promptly() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        // A capture taking the non-trigger restart path on a camera left in
        // soft-trigger mode: the restart arms no frame, so every
        // `SVBGetVideoData` slice times out and the loop runs on to its
        // deadline — 5 s here, ample room for the cancel to interrupt it.
        let request = CaptureRequest {
            exposure_us: 2_000_000,
            is_trigger_cam: false,
            ..sim_request(Duration::ZERO, &cancel)
        };
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        // The capture's own restart is the second start on this camera, and it
        // happens strictly *after* the pre-trigger cancel check — so once it
        // has landed, the next cancel check this capture makes can only be the
        // poll loop's. That is what makes this test about the poll loop rather
        // than about how long a nap happened to be.
        wait_until_video_capture_starts(&handle, 2);
        let cancelled_at = Instant::now();
        cancel.store(true, Ordering::SeqCst);

        let error = capturing.join().expect("capture thread").unwrap_err();
        assert_eq!(error.message(), ABORTED_MESSAGE);
        // One slice is the contract; the generous multiple is only to keep a
        // loaded runner from failing a test that is about the drain, not about
        // scheduling latency.
        let bound = Duration::from_millis(u64::from(VIDEO_DATA_POLL_MS) * 8);
        assert!(
            cancelled_at.elapsed() < bound,
            "a cancelled poll should drain within a slice ({VIDEO_DATA_POLL_MS}ms), took {:?}",
            cancelled_at.elapsed()
        );
        handle.close().unwrap();
    }

    /// A read that outlives the SDK's recommended deadline still hands back
    /// its frame: the floor is what carries it. Driven through the non-trigger
    /// restart path, which arms no frame of its own, so the frame appears only
    /// when this test arms it — after the SDK's recommendation for this
    /// exposure has already elapsed. The `debug!` the poll loop emits on that
    /// path is an unasserted side effect, per testing.md 6.8 (asserting on
    /// captured tracing events is unsound in a parallel test binary).
    #[test]
    fn production_handle_capture_returns_a_frame_the_floor_carried() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        // The helper's 1 ms exposure — comfortably inside the range the device
        // validates against, so this is an exposure `capture` can really be
        // handed — buys barely more than the 500 ms base of the SDK's
        // recommendation, against the 5 s floor. So the frame can be armed well
        // past the recommendation without approaching the deadline.
        let request = CaptureRequest {
            is_trigger_cam: false,
            ..sim_request(Duration::ZERO, &cancel)
        };
        // The delay is the mechanism, so it is checked rather than assumed: a
        // frame armed inside the recommendation is not a late read at all, and
        // this test would then pass while exercising nothing.
        let recommended = Duration::from_millis(
            u64::try_from(recommended_read_deadline_ms(request.exposure_us)).unwrap(),
        );
        let arm_after = Duration::from_millis(750);
        assert!(
            arm_after > recommended,
            "arming after {arm_after:?} is inside this exposure's own {recommended:?} \
             recommendation, so the read under test would not be a late one"
        );
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        // Waiting for the first poll — not for the restart that precedes it —
        // is what makes the delay below a lower bound on the loop's *own*
        // elapsed time. Keyed on the restart instead, a capture preempted
        // between the two would start its clock late and could still be inside
        // the recommendation when the frame is armed, so this test would pass
        // against the unfloored deadline it exists to fail against.
        wait_until_video_data_polled(&handle, 1);
        std::thread::sleep(arm_after);
        arm_next_exposure(&handle);

        let frame = capturing.join().expect("capture thread").unwrap();
        assert_eq!(frame.len(), 64 * 64 * 2);
        handle.close().unwrap();
    }

    /// A capture whose camera is closed and reopened under it — a disconnect
    /// plus reconnect mid-exposure — must not trigger or download from the
    /// reopened instance: that camera belongs to whatever exposure the
    /// reconnected client starts next, and this frame is not there to be read.
    #[test]
    fn production_handle_capture_does_not_read_from_a_reopened_camera() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        // Long enough that the reconnect below always lands inside the
        // (simulation-only) integration wait, however loaded the runner.
        let request = sim_request(Duration::from_secs(2), &cancel);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_configured(&handle, 64);
        reconnect(&handle);

        let error = capturing.join().expect("capture thread").unwrap_err();
        assert_eq!(
            error.message(),
            "camera not open",
            "a capture must not read a frame off a camera reopened under it"
        );
        handle.close().unwrap();
    }

    /// The same capture's abort drain must not stop video capture on the
    /// reopened camera either: the SDK has no data-preserving stop, so that
    /// would discard the frame the *next* exposure has already triggered.
    #[test]
    fn production_handle_abort_does_not_discard_a_reopened_cameras_frame() {
        let handle = Arc::new(sim_handle());
        handle.open().unwrap();
        handle.set_camera_mode(CameraMode::TrigSoft).unwrap();
        handle.start_video_capture().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let request = sim_request(Duration::from_secs(2), &cancel);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_configured(&handle, 64);
        reconnect(&handle);
        arm_next_exposure(&handle);
        // Only now does the superseded capture learn it was aborted, so its
        // drain runs entirely against the reopened camera.
        cancel.store(true, Ordering::SeqCst);

        let error = capturing.join().expect("capture thread").unwrap_err();
        assert_eq!(error.message(), ABORTED_MESSAGE);
        let mut frame = vec![0u8; 64 * 64 * 2];
        handle
            .camera
            .lock()
            .as_ref()
            .expect("camera open")
            .get_video_data(&mut frame, 0)
            .expect("the next exposure's frame must survive the superseded capture's abort");
        handle.close().unwrap();
    }

    // --- C6: a camera that has left the bus ------------------------------------

    /// A simulated camera that leaves the bus while its departure file exists,
    /// and whose readouts stall into blank frames while its blank-frame file
    /// does, with the scratch directory holding both — under Bazel's
    /// per-action `TEST_TMPDIR` when there is one, and removed when the guard
    /// drops.
    fn departing_sim_handle() -> (SvbonyCameraHandle, tempfile::TempDir) {
        let root = std::env::var_os("TEST_TMPDIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let dir = tempfile::Builder::new()
            .prefix("svbony-departure-")
            .tempdir_in(root)
            .expect("scratch dir");
        let sdk = svbony_rs::Sdk::new()
            .expect("simulation SDK")
            .with_departure_file(dir.path().join("departed"))
            .with_blank_frame_file(dir.path().join("blank"));
        let info = sdk.cameras().expect("enumerate")[0].clone();
        let handle = SvbonyCameraHandle::new(sdk, info, "SVBONY:Sim:0a1b2c3d4e5f6071".to_string());
        (handle, dir)
    }

    fn leave_bus(dir: &tempfile::TempDir) {
        std::fs::write(dir.path().join("departed"), b"").expect("write the departure file");
    }

    fn come_back(dir: &tempfile::TempDir) {
        std::fs::remove_file(dir.path().join("departed")).expect("remove the departure file");
    }

    fn stall_readouts(dir: &tempfile::TempDir) {
        std::fs::write(dir.path().join("blank"), b"").expect("write the blank-frame file");
    }

    /// A call the SDK refuses on any camera: the simulated model has no gamma.
    fn fail_a_call(handle: &SvbonyCameraHandle) -> BackendError {
        handle
            .control_value(ControlType::Gamma)
            .expect_err("the simulated model has no gamma")
    }

    /// Open the camera and arm it for soft-triggered frames, as the connect
    /// handshake does.
    fn open_armed(handle: &SvbonyCameraHandle) {
        handle.open().expect("open");
        handle
            .set_camera_mode(CameraMode::TrigSoft)
            .expect("soft-trigger mode");
        handle.start_video_capture().expect("arm video capture");
    }

    /// A departed camera's calls keep answering, so nothing marks it until a
    /// call fails. The failure asks, the rescan does not find the camera, and
    /// the handle is marked lost without anything being closed; the failure
    /// itself says it found the camera gone (C6).
    #[test]
    fn production_handle_marks_a_departed_camera_lost_once_a_call_on_it_fails() {
        let (handle, dir) = departing_sim_handle();
        handle.open().unwrap();
        leave_bus(&dir);
        handle.control_value(ControlType::Gain).unwrap();
        assert!(!handle.is_lost(), "only a failure asks");

        let error = fail_a_call(&handle);

        assert!(error.left_the_bus(), "{error}");
        assert!(
            error.message().starts_with("the camera has left the bus: "),
            "{error}"
        );
        assert!(handle.is_lost());
        assert!(handle.is_open(), "lost is not closed");
    }

    /// A failure on a camera the rescan still finds is that failure and
    /// nothing more (C6).
    #[test]
    fn production_handle_failure_on_a_camera_still_on_the_bus_is_only_that_failure() {
        let (handle, _dir) = departing_sim_handle();
        handle.open().unwrap();

        let error = fail_a_call(&handle);

        assert!(!error.left_the_bus(), "{error}");
        assert!(!handle.is_lost());
    }

    /// A lost camera is refused by `open` until a close releases it, and that
    /// close clears the mark — so a reconnect is a fresh open, never the lost
    /// session handed back (C6).
    #[test]
    fn production_handle_reopens_a_lost_camera_only_after_a_close() {
        let (handle, dir) = departing_sim_handle();
        handle.open().unwrap();
        leave_bus(&dir);
        fail_a_call(&handle);
        come_back(&dir);

        handle.open().unwrap_err();

        handle.close().unwrap();
        assert!(
            !handle.is_lost(),
            "the close that releases it clears the mark"
        );
        assert!(handle.open().unwrap(), "a fresh open after the release");
        handle.control_value(ControlType::Gain).unwrap();
    }

    /// `open` rescans the bus: a camera that is not there is refused with the
    /// handle left closed, and the same handle opens it once it is back, with
    /// no reload in between (C2, C6).
    #[test]
    fn production_handle_opens_a_camera_only_while_it_is_on_the_bus() {
        let (handle, dir) = departing_sim_handle();
        leave_bus(&dir);

        handle.open().unwrap_err();
        assert!(!handle.is_open());

        come_back(&dir);
        assert!(handle.open().unwrap());
    }

    /// A departed camera's frame never comes, and the read's timeout at its
    /// deadline is the failure that asks: the rescan does not find the camera,
    /// so the capture fails saying it found the camera gone, and the handle is
    /// marked lost (C6). Waits out the read deadline's floor.
    #[test]
    fn production_handle_capture_on_a_departed_camera_finds_it_gone_at_its_read_deadline() {
        let (handle, dir) = departing_sim_handle();
        open_armed(&handle);
        leave_bus(&dir);
        let cancel = Arc::new(AtomicBool::new(false));

        let error = handle
            .capture(sim_request(Duration::ZERO, &cancel))
            .unwrap_err();

        assert!(error.left_the_bus(), "{error}");
        assert!(handle.is_lost());
    }

    /// A capture whose camera another call's failure finds gone during its
    /// integration wait stops there, rather than triggering a frame that will
    /// never come and polling out its read deadline (C6).
    #[test]
    fn production_handle_capture_stops_once_another_calls_failure_finds_its_camera_gone() {
        let (handle, dir) = departing_sim_handle();
        let handle = Arc::new(handle);
        open_armed(&handle);
        let cancel = Arc::new(AtomicBool::new(false));
        let request = sim_request(Duration::from_secs(30), &cancel);
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_configured(&handle, 64);

        leave_bus(&dir);
        fail_a_call(&handle);

        let error = capturing.join().expect("capture thread").unwrap_err();
        assert_eq!(error.message(), "the camera has left the bus");
        assert!(error.left_the_bus());
    }

    /// A capture already polling `SVBGetVideoData` when another call's failure
    /// finds its camera gone stops at its next slice: a departed camera's
    /// slices answer only `Timeout`, so without the mark it would poll out its
    /// deadline (C6).
    #[test]
    fn production_handle_capture_polling_stops_once_another_calls_failure_finds_its_camera_gone() {
        let (handle, dir) = departing_sim_handle();
        let handle = Arc::new(handle);
        open_armed(&handle);
        let cancel = Arc::new(AtomicBool::new(false));
        // The non-trigger restart on a camera left in soft-trigger mode arms no
        // frame, so the capture polls on, slice after slice, until something
        // other than a timeout ends it (see the mid-poll abort test above).
        let request = CaptureRequest {
            exposure_us: 2_000_000,
            is_trigger_cam: false,
            ..sim_request(Duration::ZERO, &cancel)
        };
        let capturing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.capture(request))
        };
        wait_until_video_data_polled(&handle, 1);

        leave_bus(&dir);
        fail_a_call(&handle);

        let error = capturing.join().expect("capture thread").unwrap_err();
        assert_eq!(error.message(), "the camera has left the bus");
    }

    /// A blank frame counts as a failed read: from a camera the rescan no
    /// longer finds it is discarded, the capture fails saying the camera has
    /// gone, and the handle is marked lost (C6).
    #[test]
    fn production_handle_capture_fails_on_a_blank_frame_from_a_departed_camera() {
        let (handle, dir) = departing_sim_handle();
        open_armed(&handle);
        leave_bus(&dir);
        stall_readouts(&dir);
        let cancel = Arc::new(AtomicBool::new(false));

        let error = handle
            .capture(sim_request(Duration::ZERO, &cancel))
            .unwrap_err();

        assert!(error.left_the_bus(), "{error}");
        assert!(handle.is_lost());
    }

    /// A blank frame from a camera still on the bus is that camera's frame,
    /// handed back as it is (C6).
    #[test]
    fn production_handle_capture_keeps_a_blank_frame_from_a_camera_still_on_the_bus() {
        let (handle, dir) = departing_sim_handle();
        open_armed(&handle);
        stall_readouts(&dir);
        let cancel = Arc::new(AtomicBool::new(false));

        let frame = handle
            .capture(sim_request(Duration::ZERO, &cancel))
            .unwrap();

        assert!(is_blank(&frame));
        assert!(!handle.is_lost());
    }
}

/// A configurable in-memory [`CameraHandle`] for the crate's unit tests, so
/// the device logic — including the paths the `svbony-rs` simulation cannot
/// force, like a mid-exposure SDK error or an exceeded `SVBGetVideoData`
/// deadline (E9), or a model without an ST4 port (PG2) — is exercised
/// without hardware.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    fn default_info() -> CameraInfo {
        CameraInfo {
            id: 0,
            friendly_name: "SV605CC-Simulated".to_string(),
            serial: "SVB0123456789AB".to_string(),
            port_type: "USB3".to_string(),
            device_id: 0,
        }
    }

    fn default_property() -> CameraProperty {
        CameraProperty {
            max_width: 3008,
            max_height: 3008,
            is_color: true,
            bayer_pattern: svbony_rs::BayerPattern::Rg,
            supported_bins: vec![1, 2, 3, 4],
            supported_video_formats: vec![ImageType::Raw8, ImageType::Raw16],
            max_bit_depth: 14,
            is_trigger_cam: true,
        }
    }

    fn default_property_ex() -> CameraPropertyEx {
        CameraPropertyEx {
            supports_pulse_guide: false,
            supports_control_temp: true,
        }
    }

    fn default_caps() -> Vec<ControlCaps> {
        let cap = |name: &str, control_type, min, max, default, is_writable| ControlCaps {
            name: name.to_string(),
            description: String::new(),
            control_type,
            min,
            max,
            default,
            is_writable,
            is_auto_supported: false,
        };
        vec![
            cap("Gain", ControlType::Gain, 0, 400, 100, true),
            cap(
                "Exposure",
                ControlType::Exposure,
                32,
                2_000_000_000,
                10_000,
                true,
            ),
            cap("BlackLevel", ControlType::BlackLevel, 0, 255, 0, true),
            cap("CoolerEnable", ControlType::CoolerEnable, 0, 1, 0, true),
            cap(
                "TargetTemperature",
                ControlType::TargetTemperature,
                -500,
                500,
                0,
                true,
            ),
            cap(
                "CurrentTemperature",
                ControlType::CurrentTemperature,
                -500,
                1000,
                200,
                false,
            ),
            cap("CoolerPower", ControlType::CoolerPower, 0, 100, 0, false),
        ]
    }

    /// Safety bound on the capture gate (see `run_capture`): long enough that
    /// no passing test reaches it, short enough that a wedged one still
    /// reports.
    const GATE_TIMEOUT: Duration = Duration::from_secs(30);

    /// How one mock [`capture`](CameraHandle::capture) call ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CaptureOutcome {
        /// Returned the frame it was asked for.
        Frame,
        /// Returned the aborted error: the capture saw its cancel flag.
        Aborted,
        /// Returned an injected SDK error or exceeded deadline (E9).
        Failed,
    }

    #[derive(Debug)]
    pub struct MockCameraHandle {
        unique_id: String,
        info: CameraInfo,
        property: Mutex<CameraProperty>,
        property_ex: Mutex<CameraPropertyEx>,
        caps: Mutex<Vec<ControlCaps>>,
        open: AtomicBool,
        /// Force the next `open()` call to fail (C2's open-failure branch).
        pub fail_open: AtomicBool,
        /// Force `property()` to fail — the connect-handshake failure branch
        /// (C2's handshake half; exercises `camera.rs::handshake_err`).
        pub fail_property: AtomicBool,
        /// Force control reads/writes and pulse-guide to fail, so the
        /// device's SDK-error mappings (which must carry the SDK detail)
        /// are exercisable.
        pub fail_controls: AtomicBool,
        /// Force `restore_default_param` to fail — the SDK does this on a
        /// read-only working directory even though the restore took effect,
        /// and the connect must survive it (C1a).
        pub fail_restore_default_param: AtomicBool,
        /// Force `set_auto_save_param` to fail (C1a: warn, do not fail the
        /// connect).
        pub fail_set_auto_save_param: AtomicBool,
        /// Refuse the next `Exposure` write, leaving the auto-exposure state
        /// as it was — the connect handshake's clearing write (C1a) refused,
        /// so the first exposure's arm meets the SDK's gain gate (GO5).
        pub fail_next_exposure_write: AtomicBool,
        /// Refuse every read and write of this one control, every other
        /// control working — a gain or offset the SDK refuses at arm time
        /// (E9), or a connect's seed read failing (GO1). Set through
        /// [`refuse_control`](Self::refuse_control).
        refused_control: Mutex<Option<ControlType>>,
        /// The camera is off the bus (C6), as SDK 1.13.4 was measured
        /// behaving: every call goes on answering, a capture's frame never
        /// comes, the rescan a failed call asks does not find it, and `open()`
        /// fails, as `svbony-rs`'s simulated departure does. Set through
        /// [`leave_bus`](Self::leave_bus).
        departed: AtomicBool,
        /// The next write of this control answers `CAMERA_REMOVED` while the
        /// camera is open, which marks it lost as the production handle does
        /// — a departure the SDK names, landing at one exact step of a
        /// sequence such as the connect handshake. One-shot. Set through
        /// [`answer_camera_removed_at_write`](Self::answer_camera_removed_at_write).
        removed_at_write: Mutex<Option<ControlType>>,
        /// The production handle's lost mark, made by the same rules on the
        /// mock's own flags (see [`note_failure`](Self::note_failure)): a
        /// failed call whose presence check does not find the camera, or a
        /// call answering `CAMERA_REMOVED`, marks an open camera lost;
        /// `open()` refuses it, and `close()` clears it.
        lost: AtomicBool,
        /// How many presence checks failed calls have asked for.
        presence_checks: AtomicU32,
        /// Make every presence check fail, as a rescan the SDK refuses: no
        /// verdict either way.
        pub fail_presence_check: AtomicBool,
        /// The next call that reaches the SDK fails with the SDK's catch-all
        /// error. Set through [`fail_next_call`](Self::fail_next_call).
        fail_next: AtomicBool,
        /// Once the next failed call has been noted, another client's release
        /// and reconnect land straight after it, the camera back: the device
        /// reads connected again — a fresh session — by the time that call
        /// returns. Set through
        /// [`fail_next_call_as_it_leaves_past_a_reconnect`](Self::fail_next_call_as_it_leaves_past_a_reconnect).
        reconnect_after_failure: AtomicBool,
        /// Close the camera straight after the next `is_open` or `is_lost`
        /// read, which still answers what it read: another client's release
        /// landing between a caller's two reads of the connection state. Set
        /// through
        /// [`close_after_next_state_read`](Self::close_after_next_state_read).
        close_after_state_read: AtomicBool,

        /// The SDK's auto-exposure state, mirrored from `svbony-rs`'s
        /// simulation: on after `open()` and after `restore_default_param`,
        /// cleared by an `Exposure` write, and refusing `Gain` writes while
        /// on — so a test can pin that the connect handshake clears it (C1a)
        /// and that an exposure arms its gain only after its own exposure
        /// write (GO5).
        auto_exposure: AtomicBool,
        /// Ordered log of the SDK calls whose order is contract, as they
        /// reach the seam, refused or not: the C1a handshake steps
        /// (`"restore_default_param"`, `"set_auto_save_param(false)"`) and
        /// every `Exposure`, `Gain` and `BlackLevel` write
        /// (`"set_control_value(Gain, <value>)"`), the connect's and each
        /// exposure's arm alike — so a test can assert the sequence, not just
        /// the counts.
        sdk_call_log: Mutex<Vec<String>>,

        gain: Mutex<i64>,
        black_level: Mutex<i64>,
        /// The gain and black level `restore_default_param` puts the camera
        /// at: the device's own default block, which a test can set apart from
        /// the caps' `default` to tell a connect that reads the camera from one
        /// that reads the caps (GO1).
        device_defaults: Mutex<(i64, i64)>,
        cooler_enable: AtomicBool,
        target_temp_tenths: Mutex<i64>,
        current_temp_tenths: Mutex<i64>,

        /// Serializes `open()`'s check-and-open (mirroring the real
        /// handle's critical section) so exactly one racing caller
        /// observes `true`.
        open_section: Mutex<()>,
        /// Optional artificial delay inside `open()`'s critical section
        /// (modeling the SDK open's latency), so a test can hold one
        /// connect transition in-flight while a second races it.
        open_delay: Mutex<Duration>,
        /// Optional artificial delay before `capture` returns (for in-flight /
        /// abort-race tests).
        capture_delay: Mutex<Duration>,
        /// While set, every capture parks at a gate placed *before* it reads
        /// its cancel flag, and stays there until the gate is lowered. That
        /// lets a test hold one capture inside the device's "exposure in
        /// flight" window while it drives a disconnect, a reconnect, and a
        /// second exposure — the interleaving a sleep can only approximate.
        capture_gate: AtomicBool,
        /// While set, `set_camera_mode` — on a trigger camera the connect
        /// handshake's mode select, its last step but the video arm — parks
        /// before reaching the SDK until the gate is lowered, so a test can
        /// hold a handshake in flight while it drives a second client's
        /// request. Bounded by [`GATE_TIMEOUT`] like the capture gate.
        mode_select_gate: AtomicBool,
        /// One entry per `capture` call, in call order: `None` while the call
        /// is still running, then how it ended. A test can assert that a
        /// superseded capture really saw its abort instead of running on to a
        /// frame.
        capture_outcomes: Mutex<Vec<Option<CaptureOutcome>>>,
        /// E9 injection: the next `capture` fails as a mid-exposure SDK error.
        pub fail_capture: AtomicBool,
        /// E9 injection: the next `capture` fails as an exceeded
        /// `SVBGetVideoData` deadline.
        pub exceed_deadline: AtomicBool,
        /// The most recent [`CaptureRequest`] passed to `capture`, so a test
        /// can assert what `camera.rs` computed (e.g. `is_trigger_cam` on
        /// the non-trigger-camera fallback path, state-machine step 5).
        last_capture_request: Mutex<Option<CaptureRequest>>,
        /// How many times `start_video_capture` has been called — lets a
        /// test pin tenet 3 (connect must not arm free-running capture for a
        /// non-trigger camera; a trigger camera arms exactly once, at
        /// connect).
        start_video_capture_calls: AtomicU32,
        /// How many times `stop_video_capture` has been called.
        stop_video_capture_calls: AtomicU32,
    }

    impl Default for MockCameraHandle {
        fn default() -> Self {
            Self {
                unique_id: "SVBONY:SV605CC-Simulated:SVB0123456789AB".to_string(),
                info: default_info(),
                property: Mutex::new(default_property()),
                property_ex: Mutex::new(default_property_ex()),
                caps: Mutex::new(default_caps()),
                open: AtomicBool::new(false),
                fail_open: AtomicBool::new(false),
                fail_property: AtomicBool::new(false),
                fail_controls: AtomicBool::new(false),
                fail_restore_default_param: AtomicBool::new(false),
                fail_set_auto_save_param: AtomicBool::new(false),
                fail_next_exposure_write: AtomicBool::new(false),
                refused_control: Mutex::new(None),
                departed: AtomicBool::new(false),
                removed_at_write: Mutex::new(None),
                lost: AtomicBool::new(false),
                presence_checks: AtomicU32::new(0),
                fail_presence_check: AtomicBool::new(false),
                fail_next: AtomicBool::new(false),
                reconnect_after_failure: AtomicBool::new(false),
                close_after_state_read: AtomicBool::new(false),
                auto_exposure: AtomicBool::new(true),
                sdk_call_log: Mutex::new(Vec::new()),
                gain: Mutex::new(100),
                black_level: Mutex::new(0),
                device_defaults: Mutex::new((100, 0)),
                cooler_enable: AtomicBool::new(false),
                target_temp_tenths: Mutex::new(0),
                current_temp_tenths: Mutex::new(200),
                open_section: Mutex::new(()),
                open_delay: Mutex::new(Duration::ZERO),
                capture_delay: Mutex::new(Duration::ZERO),
                capture_gate: AtomicBool::new(false),
                mode_select_gate: AtomicBool::new(false),
                capture_outcomes: Mutex::new(Vec::new()),
                fail_capture: AtomicBool::new(false),
                exceed_deadline: AtomicBool::new(false),
                last_capture_request: Mutex::new(None),
                start_video_capture_calls: AtomicU32::new(0),
                stop_video_capture_calls: AtomicU32::new(0),
            }
        }
    }

    impl MockCameraHandle {
        /// Drop a control so it reports unavailable (e.g. remove `Gain` to
        /// test the `NOT_IMPLEMENTED` gate, GO1).
        pub fn without_control(self, control: ControlType) -> Self {
            self.caps.lock().retain(|c| c.control_type != control);
            self
        }

        /// Override a control's caps range (e.g. a gain range too wide for
        /// ASCOM's `i32`, to test that the control is left unadvertised rather
        /// than clamped).
        pub fn with_control_range(self, control: ControlType, min: i64, max: i64) -> Self {
            for cap in self.caps.lock().iter_mut() {
                if cap.control_type == control {
                    cap.min = min;
                    cap.max = max;
                }
            }
            self
        }

        /// Present a monochrome model (ST1's `Monochrome`/bayer-offset
        /// `NOT_IMPLEMENTED` branch) — the default mirrors the colour
        /// SV605CC-Simulated.
        pub fn monochrome(self) -> Self {
            self.property.lock().is_color = false;
            self
        }

        /// Present a model with no temperature control (K1's
        /// `NOT_IMPLEMENTED` branch).
        pub fn without_temp_control(self) -> Self {
            self.property_ex.lock().supports_control_temp = false;
            self
        }

        /// Present an ST4-capable model (PG1/PG2's non-`NOT_IMPLEMENTED`
        /// branch) — the default mirrors the SV605CC's no-ST4-port posture.
        pub fn with_pulse_guide(self) -> Self {
            self.property_ex.lock().supports_pulse_guide = true;
            self
        }

        /// Present a non-trigger-capable model (state-machine step 5's
        /// fallback path).
        pub fn without_trigger_cam(self) -> Self {
            self.property.lock().is_trigger_cam = false;
            self
        }

        /// Present a model advertising exactly `formats` as its
        /// `SupportedVideoFormat` — the default mirrors the SV605CC's
        /// `[Raw8, Raw16]`. Drives the readout-mode negotiation (RM1) and
        /// its no-usable-format connect failure (RM3), neither of which
        /// the `svbony-rs` simulation can present.
        pub fn with_video_formats(self, formats: Vec<ImageType>) -> Self {
            self.property.lock().supported_video_formats = formats;
            self
        }

        pub fn set_capture_delay(&self, delay: Duration) {
            *self.capture_delay.lock() = delay;
        }

        /// Hold every capture at the gate (or release the held ones).
        pub fn set_capture_gate(&self, closed: bool) {
            self.capture_gate.store(closed, Ordering::SeqCst);
        }

        /// Hold every `set_camera_mode` at the gate (or release the held ones).
        pub fn set_mode_select_gate(&self, closed: bool) {
            self.mode_select_gate.store(closed, Ordering::SeqCst);
        }

        /// How each `capture` call so far ended, in call order; `None` for one
        /// still running (parked at the gate, say).
        pub fn capture_outcomes(&self) -> Vec<Option<CaptureOutcome>> {
            self.capture_outcomes.lock().clone()
        }

        /// Make the next `open()` calls linger before reporting open (runs
        /// on the `spawn_blocking` thread, so the sleep never stalls the
        /// async executor).
        pub fn set_open_delay(&self, delay: Duration) {
            *self.open_delay.lock() = delay;
        }

        /// The most recent request `capture` received, if any.
        pub fn last_capture_request(&self) -> Option<CaptureRequest> {
            self.last_capture_request.lock().clone()
        }

        /// How many times `start_video_capture` has been called so far.
        pub fn start_video_capture_call_count(&self) -> u32 {
            self.start_video_capture_calls.load(Ordering::SeqCst)
        }

        /// How many times `stop_video_capture` has been called so far.
        pub fn stop_video_capture_call_count(&self) -> u32 {
            self.stop_video_capture_calls.load(Ordering::SeqCst)
        }

        /// The ordered C1a handshake steps seen so far (see `sdk_call_log`).
        pub fn sdk_call_log(&self) -> Vec<String> {
            self.sdk_call_log.lock().clone()
        }

        /// Whether the mirrored SDK auto-exposure state is currently on.
        pub fn auto_exposure(&self) -> bool {
            self.auto_exposure.load(Ordering::SeqCst)
        }

        /// Refuse every read and write of `control` from now on, or of none.
        pub fn refuse_control(&self, control: Option<ControlType>) {
            *self.refused_control.lock() = control;
        }

        /// The gain and black level the next `restore_default_param` — so the
        /// next connect — puts the camera at.
        pub fn set_device_defaults(&self, gain: i64, black_level: i64) {
            *self.device_defaults.lock() = (gain, black_level);
        }

        /// Take the camera off the bus (C6): from now on its calls go on
        /// answering but its frames never come, the presence check a failed
        /// call asks does not find it, and `open()` fails.
        pub fn leave_bus(&self) {
            self.departed.store(true, Ordering::SeqCst);
        }

        /// Answer the next write of `control` with `CAMERA_REMOVED`, so that
        /// write is the call that finds the camera gone.
        pub fn answer_camera_removed_at_write(&self, control: ControlType) {
            *self.removed_at_write.lock() = Some(control);
        }

        /// How many presence checks failed calls have asked for so far.
        pub fn presence_check_count(&self) -> u32 {
            self.presence_checks.load(Ordering::SeqCst)
        }

        /// Put the camera back on the bus. A lost mark stays until a close
        /// clears it, as on the production handle.
        pub fn return_to_bus(&self) {
            self.departed.store(false, Ordering::SeqCst);
        }

        /// Fail the next call that reaches the SDK, which then asks whether the
        /// camera is still on the bus.
        pub fn fail_next_call(&self) {
            self.fail_next.store(true, Ordering::SeqCst);
        }

        /// Take the camera off the bus and fail the next call that reaches the
        /// SDK, so that call finds it gone; then have the camera back and
        /// another client's release and reconnect land straight after it.
        pub fn fail_next_call_as_it_leaves_past_a_reconnect(&self) {
            self.leave_bus();
            self.reconnect_after_failure.store(true, Ordering::SeqCst);
            self.fail_next_call();
        }

        /// The production handle's `note_failure`, on the mock's own flags: a
        /// failed call on an open camera asks whether it is still on the bus
        /// (C6). A `CAMERA_REMOVED` marks it lost with no asking. Otherwise
        /// the presence check is counted, gives no verdict while
        /// `fail_presence_check` is set, and finds a departed camera gone.
        fn note_failure(&self, error: BackendError) -> BackendError {
            if !self.open.load(Ordering::SeqCst) {
                return error;
            }
            if error.camera_removed() {
                self.lost.store(true, Ordering::SeqCst);
                return error;
            }
            self.presence_checks.fetch_add(1, Ordering::SeqCst);
            if self.fail_presence_check.load(Ordering::SeqCst)
                || !self.departed.load(Ordering::SeqCst)
            {
                return error;
            }
            self.lost.store(true, Ordering::SeqCst);
            error.found_gone()
        }

        /// Run one call that reaches the SDK, noting its failure as the
        /// production handle's `with_camera` does, and then land the release
        /// and reconnect
        /// [`fail_next_call_as_it_leaves_past_a_reconnect`](Self::fail_next_call_as_it_leaves_past_a_reconnect)
        /// asked for after it.
        fn noted<T>(&self, call: impl FnOnce() -> BackendResult<T>) -> BackendResult<T> {
            let outcome = call().map_err(|e| self.note_failure(e));
            if outcome.is_err() && self.reconnect_after_failure.swap(false, Ordering::SeqCst) {
                self.departed.store(false, Ordering::SeqCst);
                self.lost.store(false, Ordering::SeqCst);
            }
            outcome
        }

        /// Close the camera straight after the next read of `is_open` or
        /// `is_lost`, so a release lands between that read and the caller's
        /// next one.
        pub fn close_after_next_state_read(&self) {
            self.close_after_state_read.store(true, Ordering::SeqCst);
        }

        /// The close [`close_after_next_state_read`](Self::close_after_next_state_read)
        /// asked for, made once.
        fn close_if_asked(&self) {
            if self.close_after_state_read.swap(false, Ordering::SeqCst) {
                self.open.store(false, Ordering::SeqCst);
                self.lost.store(false, Ordering::SeqCst);
            }
        }

        /// What every call that would reach the SDK answers first: the failure
        /// [`fail_next_call`](Self::fail_next_call) asked for. A departed
        /// camera answers like any other (see `departed`).
        fn reach_sdk(&self) -> BackendResult<()> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(svbony_rs::Error::Svb(SvbError::GeneralError).into());
            }
            Ok(())
        }

        /// The read's timeout at its deadline, as the production capture
        /// reports it.
        fn read_timed_out(request: &CaptureRequest) -> BackendError {
            BackendError::new(format!(
                "SVBGetVideoData deadline exceeded ({}ms)",
                exposure_timeout_ms(request.exposure_us)
            ))
        }

        /// Mirror the production abort drain: stop, then re-arm for a trigger
        /// camera, a failure of either left behind rather than propagated (see
        /// `SvbonyCameraHandle::abort_capture`).
        fn drain(&self, request: &CaptureRequest) -> BackendResult<Vec<u8>> {
            let _ = self.stop_video_capture();
            if request.is_trigger_cam {
                let _ = self.start_video_capture();
            }
            Err(BackendError::aborted())
        }

        /// The capture proper; [`CameraHandle::capture`] wraps it to record
        /// how it ended.
        fn run_capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>> {
            *self.last_capture_request.lock() = Some(request.clone());
            // The production handle's own arm, run through this mock's model
            // of the SDK — its auto-exposure gate, its refusals and its call
            // log — so a test of the arm tests the sequence production runs,
            // not a copy of it.
            super::arm_controls(&request, |control, value| {
                self.set_control_value(control, value)
            })?;
            // The gate is read BEFORE the cancel flag, so a capture held here
            // has not yet had the chance to observe an abort — exactly the
            // state a reconnect plus a second exposure has to race against.
            //
            // Bounded, because a test that panics between raising the gate and
            // lowering it would otherwise leave this thread parked forever —
            // and dropping the test's Tokio runtime waits on the blocking
            // pool, so the whole test binary would hang instead of reporting
            // the failure. The bound matches the tests' own 30 s deadline
            // waits: a gate still closed by then means the test has already
            // failed.
            let gate_start = Instant::now();
            while self.capture_gate.load(Ordering::SeqCst) && gate_start.elapsed() < GATE_TIMEOUT {
                std::thread::sleep(Duration::from_millis(1));
            }
            // Mirror the production handle's sliced wait: an abort/disconnect
            // (request.cancel) drains the capture promptly instead of
            // sleeping out the whole delay.
            let deadline = Instant::now() + *self.capture_delay.lock();
            loop {
                if request.cancel.load(Ordering::SeqCst) {
                    return self.drain(&request);
                }
                if self.lost.load(Ordering::SeqCst) {
                    return Err(BackendError::departed());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            // The trigger, the first SDK call a capture makes after its wait.
            self.noted(|| self.reach_sdk())?;
            // A departed camera's frame never comes: the capture polls, as the
            // production handle's does, until another call's failure marks the
            // camera lost, an abort drains it, or its read deadline passes —
            // which `exceed_deadline` brings forward — and that timeout asks
            // whether the camera is still there (C6).
            let read_deadline = Instant::now()
                + Duration::from_millis(
                    u64::try_from(exposure_timeout_ms(request.exposure_us)).unwrap_or(0),
                );
            while self.departed.load(Ordering::SeqCst) {
                if request.cancel.load(Ordering::SeqCst) {
                    return self.drain(&request);
                }
                if self.lost.load(Ordering::SeqCst) {
                    return Err(BackendError::departed());
                }
                if self.exceed_deadline.load(Ordering::SeqCst) || Instant::now() >= read_deadline {
                    return self.noted(|| Err(Self::read_timed_out(&request)));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.fail_capture.load(Ordering::SeqCst) {
                return self.noted(|| Err(BackendError::new("simulated mid-exposure SDK failure")));
            }
            if self.exceed_deadline.load(Ordering::SeqCst) {
                return self.noted(|| Err(Self::read_timed_out(&request)));
            }
            Ok(vec![
                0u8;
                request.width as usize
                    * request.height as usize
                    * request.image_type.bytes_per_pixel()
            ])
        }
    }

    impl CameraHandle for MockCameraHandle {
        fn unique_id(&self) -> String {
            self.unique_id.clone()
        }

        fn info(&self) -> CameraInfo {
            self.info.clone()
        }

        fn is_open(&self) -> bool {
            let open = self.open.load(Ordering::SeqCst);
            self.close_if_asked();
            open
        }

        fn is_lost(&self) -> bool {
            let lost = self.lost.load(Ordering::SeqCst);
            self.close_if_asked();
            lost
        }

        fn open(&self) -> BackendResult<bool> {
            let _section = self.open_section.lock();
            if self.fail_open.load(Ordering::SeqCst) {
                return Err(BackendError::new("simulated open failure"));
            }
            if self.open.load(Ordering::SeqCst) {
                if self.lost.load(Ordering::SeqCst) {
                    return Err(BackendError::new(
                        "the camera has left the bus; it must be released before it can be opened",
                    ));
                }
                return Ok(false);
            }
            if self.departed.load(Ordering::SeqCst) {
                return Err(svbony_rs::Error::Svb(SvbError::InvalidIndex).into());
            }
            let delay = *self.open_delay.lock();
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            self.open.store(true, Ordering::SeqCst);
            // A freshly opened camera has the SDK's auto-exposure on.
            self.auto_exposure.store(true, Ordering::SeqCst);
            Ok(true)
        }

        fn restore_default_param(&self) -> BackendResult<()> {
            self.noted(|| {
                self.sdk_call_log
                    .lock()
                    .push("restore_default_param".to_string());
                self.reach_sdk()?;
                // The restore takes effect first — the device defaults leave
                // auto-exposure on — and only then can the SDK's follow-up
                // cfg-file write fail, which is the failure shape the injection
                // models: an error reported for a restore that did happen.
                let (gain, black_level) = *self.device_defaults.lock();
                *self.gain.lock() = gain;
                *self.black_level.lock() = black_level;
                self.auto_exposure.store(true, Ordering::SeqCst);
                if self.fail_restore_default_param.load(Ordering::SeqCst) {
                    return Err(BackendError::new(
                        "SVBony camera SDK error: general error (e.g. value out of valid range)",
                    ));
                }
                Ok(())
            })
        }

        fn set_auto_save_param(&self, enable: bool) -> BackendResult<()> {
            self.noted(|| {
                self.sdk_call_log
                    .lock()
                    .push(format!("set_auto_save_param({enable})"));
                self.reach_sdk()?;
                if self.fail_set_auto_save_param.load(Ordering::SeqCst) {
                    return Err(BackendError::new("injected SDK failure"));
                }
                Ok(())
            })
        }

        fn close(&self) -> BackendResult<()> {
            self.open.store(false, Ordering::SeqCst);
            self.lost.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn property(&self) -> BackendResult<CameraProperty> {
            if self.fail_property.load(Ordering::SeqCst) {
                return Err(BackendError::new("injected SDK failure"));
            }
            Ok(self.property.lock().clone())
        }

        fn property_ex(&self) -> BackendResult<CameraPropertyEx> {
            Ok(*self.property_ex.lock())
        }

        fn pixel_size_microns(&self) -> BackendResult<f32> {
            self.noted(|| {
                self.reach_sdk()?;
                Ok(3.76)
            })
        }

        fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
            self.noted(|| {
                self.reach_sdk()?;
                Ok(self.caps.lock().clone())
            })
        }

        fn control_value(&self, control: ControlType) -> BackendResult<i64> {
            self.noted(|| {
                self.reach_sdk()?;
                if self.fail_controls.load(Ordering::SeqCst)
                    || *self.refused_control.lock() == Some(control)
                {
                    return Err(BackendError::new("injected SDK failure"));
                }
                let value = match control {
                    ControlType::Gain => *self.gain.lock(),
                    ControlType::BlackLevel => *self.black_level.lock(),
                    ControlType::CoolerEnable => {
                        i64::from(self.cooler_enable.load(Ordering::SeqCst))
                    }
                    ControlType::TargetTemperature => *self.target_temp_tenths.lock(),
                    ControlType::CurrentTemperature => *self.current_temp_tenths.lock(),
                    ControlType::CoolerPower => {
                        if self.cooler_enable.load(Ordering::SeqCst) {
                            60
                        } else {
                            0
                        }
                    }
                    _ => return Err(BackendError::new("invalid control type")),
                };
                Ok(value)
            })
        }

        fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
            self.noted(|| {
                if matches!(
                    control,
                    ControlType::Exposure | ControlType::Gain | ControlType::BlackLevel
                ) {
                    self.sdk_call_log
                        .lock()
                        .push(format!("set_control_value({control:?}, {value})"));
                }
                {
                    let mut removed_at = self.removed_at_write.lock();
                    if *removed_at == Some(control) {
                        *removed_at = None;
                        return Err(svbony_rs::Error::Svb(SvbError::CameraRemoved).into());
                    }
                }
                self.reach_sdk()?;
                if self.fail_controls.load(Ordering::SeqCst)
                    || *self.refused_control.lock() == Some(control)
                {
                    return Err(BackendError::new("injected SDK failure"));
                }
                match control {
                    // The SDK's gate (GO5): gain is refused while auto-exposure
                    // is on, with the SDK's catch-all error text.
                    ControlType::Gain if self.auto_exposure.load(Ordering::SeqCst) => {
                        return Err(BackendError::new(
                            "SVBony camera SDK error: general error (e.g. value out of valid range)",
                        ));
                    }
                    ControlType::Gain => *self.gain.lock() = value,
                    ControlType::BlackLevel => *self.black_level.lock() = value,
                    ControlType::CoolerEnable => {
                        self.cooler_enable.store(value != 0, Ordering::SeqCst);
                    }
                    ControlType::TargetTemperature => *self.target_temp_tenths.lock() = value,
                    ControlType::Exposure => {
                        if self.fail_next_exposure_write.swap(false, Ordering::SeqCst) {
                            return Err(BackendError::new("injected SDK failure"));
                        }
                        // This seam only ever writes manual (`bAuto = false`)
                        // values, which is the SDK's one auto-exposure-off path.
                        self.auto_exposure.store(false, Ordering::SeqCst);
                    }
                    _ => return Err(BackendError::new("invalid control type")),
                }
                Ok(())
            })
        }

        fn set_camera_mode(&self, _mode: CameraMode) -> BackendResult<()> {
            let gate_start = Instant::now();
            while self.mode_select_gate.load(Ordering::SeqCst)
                && gate_start.elapsed() < GATE_TIMEOUT
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.noted(|| self.reach_sdk())
        }

        fn start_video_capture(&self) -> BackendResult<()> {
            self.start_video_capture_calls
                .fetch_add(1, Ordering::SeqCst);
            self.noted(|| self.reach_sdk())
        }

        fn stop_video_capture(&self) -> BackendResult<()> {
            self.stop_video_capture_calls.fetch_add(1, Ordering::SeqCst);
            self.noted(|| self.reach_sdk())
        }

        fn capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>> {
            let call = {
                let mut outcomes = self.capture_outcomes.lock();
                outcomes.push(None);
                outcomes.len().saturating_sub(1)
            };
            let result = self.run_capture(request);
            let outcome = match &result {
                Ok(_) => CaptureOutcome::Frame,
                Err(e) if e.message() == ABORTED_MESSAGE => CaptureOutcome::Aborted,
                Err(_) => CaptureOutcome::Failed,
            };
            if let Some(slot) = self.capture_outcomes.lock().get_mut(call) {
                *slot = Some(outcome);
            }
            result
        }

        fn pulse_guide(&self, _direction: GuideDirection, _duration_ms: i32) -> BackendResult<()> {
            self.noted(|| {
                self.reach_sdk()?;
                if self.fail_controls.load(Ordering::SeqCst) {
                    return Err(BackendError::new("injected SDK failure"));
                }
                Ok(())
            })
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod pure_fn_tests {
    use super::*;

    fn camera(id: i32, serial: &str) -> CameraInfo {
        CameraInfo {
            id,
            friendly_name: "SVBONY SV605CC".to_string(),
            serial: serial.to_string(),
            port_type: "USB3.0".to_string(),
            device_id: 4865,
        }
    }

    /// A camera with a serial is found by it, wherever the rescan lists it
    /// and whatever id the SDK gave it this time (C6).
    #[test]
    fn a_camera_is_located_by_its_serial_wherever_the_rescan_lists_it() {
        let held = camera(1, "0123481353808C03EE2512150035");
        let rescan = [
            camera(2, "OTHER"),
            camera(7, "0123481353808C03EE2512150035"),
        ];

        assert_eq!(locate(&rescan, &held), Some(1));
        assert_eq!(locate(&rescan[..1], &held), None);
    }

    /// A camera with no serial is found only where everything else it reported
    /// at enumeration still matches — never by position alone, so the camera
    /// now at its old index is not taken for it (C6).
    #[test]
    fn a_serial_less_camera_is_located_only_by_everything_else_it_reports() {
        let held = camera(1, "");

        assert_eq!(locate(&[camera(2, "OTHER"), camera(1, "")], &held), Some(1));
        assert_eq!(
            locate(&[camera(2, "")], &held),
            None,
            "a different serial-less camera at the old index"
        );
        assert_eq!(locate(&[camera(1, "OTHER")], &held), None);
    }

    #[test]
    fn the_sdk_recommendation_is_double_the_exposure_plus_500ms() {
        assert_eq!(recommended_read_deadline_ms(10_000), 20 + 500);
        assert_eq!(recommended_read_deadline_ms(1_000_000), 2_000 + 500);
    }

    #[test]
    fn the_floor_carries_every_exposure_the_sdk_recommendation_leaves_short() {
        // 10 ms: the recommendation alone would allow 520 ms. 2.25 s: the
        // exposure at which it reaches the floor exactly.
        assert_eq!(exposure_timeout_ms(10_000), 5_000);
        assert_eq!(exposure_timeout_ms(2_250_000), 5_000);
    }

    #[test]
    fn the_sdk_recommendation_takes_over_above_the_floor() {
        assert_eq!(exposure_timeout_ms(3_000_000), 6_000 + 500);
        assert_eq!(exposure_timeout_ms(10_000_000), 20_000 + 500);
    }

    #[test]
    fn a_short_exposures_deadline_covers_the_first_reads_sdk_setup_cost() {
        // What the floor exists for: a session's first frame arrives after
        // `max(exposure, ~2.6 s of one-off SDK setup) + ~0.7 s of full-frame
        // readout`, so every exposure up to the 2.25 s crossover needs 3.3 s
        // of deadline — well past what its own duration would buy.
        for exposure_us in [1_000_i64, 500_000, 2_250_000] {
            let deadline_ms = i64::from(exposure_timeout_ms(exposure_us));
            assert!(
                deadline_ms >= 3_300,
                "a {exposure_us} us exposure gets {deadline_ms} ms, \
                 too little for a session's first read"
            );
        }
    }

    #[test]
    fn a_negative_exposure_gets_the_floor() {
        assert_eq!(exposure_timeout_ms(-1), 5_000);
    }

    #[test]
    fn exposure_timeout_saturates_instead_of_overflowing() {
        assert_eq!(exposure_timeout_ms(i64::MAX), i32::MAX);
    }

    /// A failure the arm names keeps the SDK's status behind the name, so a
    /// departure that an exposure's gain write is the first to meet still
    /// reads as one (C6).
    #[test]
    fn a_named_arm_failure_keeps_the_sdk_status() {
        let request = CaptureRequest {
            start_x: 0,
            start_y: 0,
            width: 64,
            height: 64,
            bin: 1,
            exposure_us: 1_000,
            is_trigger_cam: true,
            image_type: ImageType::Raw16,
            gain: Some(1),
            offset: None,
            duration: Duration::ZERO,
            cancel: Arc::new(AtomicBool::new(false)),
        };

        let error = arm_controls(&request, |control, _| {
            if control == ControlType::Gain {
                Err(svbony_rs::Error::Svb(SvbError::CameraRemoved).into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert!(
            error.message().starts_with("failed to set gain: "),
            "{error}"
        );
        assert!(error.camera_removed(), "{error}");
    }

    /// A failure says it found the camera gone when the SDK answered
    /// `CAMERA_REMOVED`, or once the rescan it asked for found the camera
    /// gone, and not otherwise (C6).
    #[test]
    fn a_failure_says_it_found_the_camera_gone_only_when_it_did() {
        let refused = BackendError::from(svbony_rs::Error::Svb(SvbError::GeneralError));
        let removed = BackendError::from(svbony_rs::Error::Svb(SvbError::CameraRemoved));

        assert!(!refused.left_the_bus(), "{refused}");
        assert!(removed.left_the_bus(), "{removed}");
        let found = refused.found_gone();
        assert!(found.left_the_bus(), "{found}");
        assert!(
            found.message().starts_with("the camera has left the bus: "),
            "{found}"
        );
    }

    /// A frame is blank only when every byte of it is zero.
    #[test]
    fn a_frame_is_blank_only_when_every_byte_is_zero() {
        assert!(is_blank(&[0; 16]));
        assert!(!is_blank(&[0, 0, 0, 1]));
        assert!(!is_blank(&[1, 0, 0, 0]));
    }
}

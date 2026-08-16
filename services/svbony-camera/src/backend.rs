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
//! output format + exposure control + trigger + the `exposure*2+500ms`
//! `SVBGetVideoData` deadline, state-machine step 2), and pulse-guide.
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
//! **Staying responsive during an in-flight exposure.** The production
//! handle's SDK mutex is released between `capture`'s ROI/control setup and
//! its trigger + `SVBGetVideoData` call, mirroring `zwo-camera`'s release-
//! during-integration pattern, so the simulation-only artificial wait (see
//! [`CaptureRequest::duration`]) never starves concurrent property/control
//! reads. For the `SVBGetVideoData` wait itself — the one genuinely
//! long-blocking real-hardware call, up to `exposure_us*2+500ms` — `capture`
//! polls it in short slices (see `VIDEO_DATA_POLL_MS`) instead of one single
//! blocking call for the whole deadline, **releasing the mutex between
//! polls**: a `SvbError::Timeout` from a short slice just means "no frame
//! yet," not a real failure, so the poll loop retries until either a frame
//! arrives or the overall deadline elapses. This bounds how long any other
//! `Camera` trait method (`Disconnect`, `Gain`, `CoolerOn`, `CCDTemperature`,
//! …) can be blocked waiting for the mutex to one poll slice, not the whole
//! exposure — `is_open` goes further still and is backed by its own atomic
//! (`SvbonyCameraHandle`'s `open` field) so connection-state reads never
//! contend the capture lock at all — every `Camera` trait method calls
//! `ensure_connected` first and must stay responsive during an in-flight
//! exposure.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use svbony_rs::{
    CameraInfo, CameraMode, CameraProperty, CameraPropertyEx, ControlCaps, ControlType,
    GuideDirection, ImageType,
};

/// A `svbony-rs` SDK call failed. Carries the underlying message; the ASCOM
/// device decides the `ASCOMError` per call site.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

/// Collapse a [`svbony_rs::Error`] into the typed seam error.
impl From<svbony_rs::Error> for BackendError {
    fn from(err: svbony_rs::Error) -> Self {
        Self(err.to_string())
    }
}

impl BackendError {
    fn closed() -> Self {
        Self("camera not open".to_string())
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

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
    /// Wall-clock integration time the capture honours **under the
    /// `simulation` feature only** — `svbony-rs`'s simulated
    /// `get_video_data` never literally waits (see its doc comment), unlike
    /// the real `SVBGetVideoData`, which genuinely blocks for close to the
    /// exposure duration. Consulting this field on the real path would
    /// double-count the wait, so it is `#[cfg(feature = "simulation")]`-only
    /// in the production handle.
    pub duration: Duration,
    /// Set by the device's abort/disconnect path and checked between
    /// `SVBGetVideoData` poll slices, so an aborted capture drains within
    /// one slice instead of the rest of the `exposure*2+500ms` deadline —
    /// see the module docs ("How `capture` aborts").
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

/// `exposure_us * 2 + 500ms` — the SDK's own documented `SVBGetVideoData`
/// timeout recommendation (`docs/plans/archive/svbony-camera.md` "Verified SDK
/// facts"), as a pure, unit-testable function. Negative/zero exposures clamp
/// to a `0` base so the timeout never underflows.
#[must_use]
pub fn exposure_timeout_ms(exposure_us: i64) -> i32 {
    let us = exposure_us.max(0);
    // 1_000 is a literal divisor, so the division is total; the base is a floor
    // the timeout must never drop below, so it saturates rather than wraps.
    let ms = (us.saturating_mul(2) / 1_000).saturating_add(500);
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

    fn is_open(&self) -> bool;
    /// Open the camera if it is closed. Returns `true` when THIS call
    /// performed the open — that caller owns the post-open handshake —
    /// and `false` when the handle was already open (a prior connect, or
    /// a concurrently racing one that won). The check and the open are
    /// one critical section under the handle's own lock, so exactly one
    /// racing caller ever observes `true` (the same shape as qhy-camera's
    /// `SharedCameraConnection`): the connect handshake's video-capture
    /// arm is not idempotent, so it must never run twice.
    fn open(&self) -> BackendResult<bool>;
    fn close(&self) -> BackendResult<()>;

    /// The camera's [`CameraProperty`] (cached on the open `svbony_rs::Camera`
    /// at open time — a cheap accessor, no extra SDK call).
    fn property(&self) -> BackendResult<CameraProperty>;
    /// The camera's [`CameraPropertyEx`] (same caching as [`property`](Self::property)).
    fn property_ex(&self) -> BackendResult<CameraPropertyEx>;
    /// Sensor pixel size in microns (`SVBGetSensorPixelSize`).
    fn pixel_size_microns(&self) -> BackendResult<f32>;

    /// Enumerate the camera's tunable controls and their ranges
    /// (`SVBGetControlCaps`).
    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>>;
    /// Read a control's current value (`SVBGetControlValue`); temperature
    /// controls are in 0.1 °C units.
    fn control_value(&self, control: ControlType) -> BackendResult<i64>;
    /// Set a control's value (`SVBSetControlValue`).
    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()>;

    /// Select the camera acquisition mode (`SVBSetCameraMode`) — called once
    /// at connect for a trigger-capable camera, never per-exposure
    /// (state-machine step 1).
    fn set_camera_mode(&self, mode: CameraMode) -> BackendResult<()>;
    /// Start video capture (`SVBStartVideoCapture`) — called once at connect
    /// for a trigger-capable camera (state-machine step 1; tenet 3 forbids
    /// this at connect for a non-trigger camera, since its only mode is
    /// free-running), and per-exposure only on the non-trigger-camera
    /// fallback path (step 5).
    fn start_video_capture(&self) -> BackendResult<()>;
    /// Stop video capture (`SVBStopVideoCapture`) — used only by the
    /// non-trigger-camera per-exposure restart (step 5); never called
    /// concurrently with an in-flight [`capture`](Self::capture) on another
    /// thread (see the module docs).
    fn stop_video_capture(&self) -> BackendResult<()>;

    /// Run one exposure under a single SDK lock: set ROI + output format +
    /// `SVB_EXPOSURE`, trigger a frame (soft trigger, or a free-running
    /// restart for a non-trigger camera), then `SVBGetVideoData` with the
    /// `exposure*2+500ms` deadline. Returns the raw frame bytes in
    /// [`CaptureRequest::image_type`]'s layout.
    fn capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>>;

    /// Issue an ST4 guide pulse (`SVBPulseGuide`) — blocks at the SDK level
    /// for `duration_ms` (see `camera.rs::pulse_guide`'s doc comment for why
    /// this seam keeps that a literal blocking call in v0).
    fn pulse_guide(&self, direction: GuideDirection, duration_ms: i32) -> BackendResult<()>;
}

// --- production wrapper over svbony-rs ------------------------------------

/// Production [`CameraHandle`] over a real (or `svbony-rs`-simulated) camera.
///
/// Holds the [`svbony_rs::Sdk`] (a ZST) and the enumeration `index` so it can
/// re-open the RAII [`svbony_rs::Camera`] on connect; the open handle lives
/// behind a `Mutex<Option<…>>` because `Camera` is `Send + !Sync`.
#[derive(Debug)]
pub struct SvbonyCameraHandle {
    sdk: svbony_rs::Sdk,
    index: usize,
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
}

impl SvbonyCameraHandle {
    /// Build a handle for the camera at enumeration `index`, with its cached
    /// [`CameraInfo`] and the serial-derived `unique_id` read at enumeration.
    #[must_use]
    pub const fn new(
        sdk: svbony_rs::Sdk,
        index: usize,
        info: CameraInfo,
        unique_id: String,
    ) -> Self {
        Self {
            sdk,
            index,
            info,
            unique_id,
            camera: Mutex::new(None),
            open: AtomicBool::new(false),
        }
    }

    /// Drain an aborted capture: stop video capture (discarding the
    /// in-flight frame — the SDK has no data-preserving stop, and a frame
    /// left in its buffer would surface as a stale frame on the next
    /// exposure) and re-arm it for a trigger camera so the connect-time
    /// "armed once" invariant holds for the next exposure. A non-trigger
    /// camera is left unarmed — its per-exposure restart (state-machine
    /// step 5) arms it again. Failures here are logged, not propagated:
    /// the capture's result is already being discarded.
    fn abort_capture(&self, request: &CaptureRequest) -> BackendResult<Vec<u8>> {
        let guard = self.camera.lock();
        if let Some(camera) = guard.as_ref() {
            if let Err(e) = camera.stop_video_capture() {
                tracing::warn!(error = %e, "stopping video capture after an abort failed");
            }
            if request.is_trigger_cam {
                if let Err(e) = camera.start_video_capture() {
                    tracing::warn!(error = %e, "re-arming video capture after an abort failed");
                }
            }
        }
        Err(BackendError("exposure aborted".to_string()))
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

    fn open(&self) -> BackendResult<bool> {
        let mut guard = self.camera.lock();
        if guard.is_some() {
            return Ok(false);
        }
        *guard = Some(self.sdk.open_camera(self.index)?);
        self.open.store(true, Ordering::Release);
        Ok(true)
    }

    fn close(&self) -> BackendResult<()> {
        // Dropping the `Camera` calls `SVBCloseCamera`.
        *self.camera.lock() = None;
        self.open.store(false, Ordering::Release);
        Ok(())
    }

    fn property(&self) -> BackendResult<CameraProperty> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.property().clone())
    }

    fn property_ex(&self) -> BackendResult<CameraPropertyEx> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(*camera.property_ex())
    }

    fn pixel_size_microns(&self) -> BackendResult<f32> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.pixel_size_microns()?)
    }

    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.control_caps()?)
    }

    fn control_value(&self, control: ControlType) -> BackendResult<i64> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.control_value(control)?.value)
    }

    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.set_control_value(control, value, false)?)
    }

    fn set_camera_mode(&self, mode: CameraMode) -> BackendResult<()> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.set_camera_mode(mode)?)
    }

    fn start_video_capture(&self) -> BackendResult<()> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.start_video_capture()?)
    }

    fn stop_video_capture(&self) -> BackendResult<()> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.stop_video_capture()?)
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
        {
            let guard = self.camera.lock();
            let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
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
            camera.set_control_value(ControlType::Exposure, request.exposure_us, false)?;
        }

        // See `CaptureRequest::duration`'s doc comment: only the simulation
        // needs an artificial wait, since its `get_video_data` never really
        // blocks; the real SDK's `SVBGetVideoData` call below already blocks
        // for close to the exposure duration on real hardware. Sliced so an
        // abort during the simulated integration drains promptly too.
        #[cfg(feature = "simulation")]
        {
            // Validated against `ExposureMax` upstream, so this cannot overflow
            // the clock; `now` would simply end the wait at once.
            let sim_start = Instant::now();
            let sim_deadline = sim_start.checked_add(request.duration).unwrap_or(sim_start);
            loop {
                if request.cancel.load(Ordering::Acquire) {
                    return self.abort_capture(&request);
                }
                let remaining = sim_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(
                    remaining.min(Duration::from_millis(u64::from(VIDEO_DATA_POLL_MS))),
                );
            }
        }
        #[cfg(not(feature = "simulation"))]
        let _ = request.duration;

        if request.cancel.load(Ordering::Acquire) {
            return self.abort_capture(&request);
        }
        {
            let guard = self.camera.lock();
            let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
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
        }

        let frame_len = request.frame_len().ok_or_else(|| {
            BackendError("frame is too large to address on this target".to_string())
        })?;
        let mut buf = vec![0u8; frame_len];

        // Poll `SVBGetVideoData` in short slices instead of one blocking call
        // for the whole `exposure_us*2+500ms` deadline, releasing the SDK
        // mutex between polls — see the module docs. A `SvbError::Timeout`
        // from a short slice just means "no frame yet"; retry until either a
        // frame arrives or the overall deadline elapses (at which point the
        // final `Timeout` is the real, reported error).
        // The timeout is derived from a validated exposure, so this cannot
        // overflow the clock; `now` would end the poll loop on its first pass.
        let poll_start = Instant::now();
        let deadline = poll_start
            .checked_add(Duration::from_millis(
                u64::try_from(exposure_timeout_ms(request.exposure_us)).unwrap_or(0),
            ))
            .unwrap_or(poll_start);
        loop {
            // Abort/disconnect check between slices — the only interrupt
            // path this SDK admits (see the module docs, "How `capture`
            // aborts": a concurrent `SVBStopVideoCapture` is tolerated but
            // does NOT unblock a pending `SVBGetVideoData`, so short slices
            // + this check are what keep the drain bounded).
            if request.cancel.load(Ordering::Acquire) {
                return self.abort_capture(&request);
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
            let result = {
                let guard = self.camera.lock();
                let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
                camera.get_video_data(&mut buf, poll_ms)
            };
            match result {
                Ok(()) => return Ok(buf),
                Err(svbony_rs::Error::Svb(svbony_rs::SvbError::Timeout)) if remaining_ms > 0 => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn pulse_guide(&self, direction: GuideDirection, duration_ms: i32) -> BackendResult<()> {
        let guard = self.camera.lock();
        let camera = guard.as_ref().ok_or_else(BackendError::closed)?;
        Ok(camera.pulse_guide(direction, duration_ms)?)
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
        SvbonyCameraHandle::new(sdk, 0, info, "SVBONY:Sim:0a1b2c3d4e5f6071".to_string())
    }

    #[test]
    fn production_handle_round_trips_against_the_sim_sdk() {
        let handle = sim_handle();
        assert_eq!(handle.unique_id(), "SVBONY:Sim:0a1b2c3d4e5f6071");
        assert!(!handle.info().friendly_name.is_empty());
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
            duration: Duration::from_millis(1),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let frame = handle.capture(request).unwrap();
        assert_eq!(frame.len(), 64 * 64);
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
            duration: Duration::from_secs(30),
            cancel: Arc::new(AtomicBool::new(true)),
        };
        let started = Instant::now();
        let err = handle.capture(request).unwrap_err();
        assert!(err.0.contains("aborted"), "unexpected error: {}", err.0);
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

        gain: Mutex<i64>,
        black_level: Mutex<i64>,
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
                gain: Mutex::new(100),
                black_level: Mutex::new(0),
                cooler_enable: AtomicBool::new(false),
                target_temp_tenths: Mutex::new(0),
                current_temp_tenths: Mutex::new(200),
                open_section: Mutex::new(()),
                open_delay: Mutex::new(Duration::ZERO),
                capture_delay: Mutex::new(Duration::ZERO),
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
    }

    impl CameraHandle for MockCameraHandle {
        fn unique_id(&self) -> String {
            self.unique_id.clone()
        }

        fn info(&self) -> CameraInfo {
            self.info.clone()
        }

        fn is_open(&self) -> bool {
            self.open.load(Ordering::SeqCst)
        }

        fn open(&self) -> BackendResult<bool> {
            let _section = self.open_section.lock();
            if self.fail_open.load(Ordering::SeqCst) {
                return Err(BackendError("simulated open failure".to_string()));
            }
            if self.open.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let delay = *self.open_delay.lock();
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            self.open.store(true, Ordering::SeqCst);
            Ok(true)
        }

        fn close(&self) -> BackendResult<()> {
            self.open.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn property(&self) -> BackendResult<CameraProperty> {
            if self.fail_property.load(Ordering::SeqCst) {
                return Err(BackendError("injected SDK failure".to_string()));
            }
            Ok(self.property.lock().clone())
        }

        fn property_ex(&self) -> BackendResult<CameraPropertyEx> {
            Ok(*self.property_ex.lock())
        }

        fn pixel_size_microns(&self) -> BackendResult<f32> {
            Ok(3.76)
        }

        fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
            Ok(self.caps.lock().clone())
        }

        fn control_value(&self, control: ControlType) -> BackendResult<i64> {
            if self.fail_controls.load(Ordering::SeqCst) {
                return Err(BackendError("injected SDK failure".to_string()));
            }
            let value = match control {
                ControlType::Gain => *self.gain.lock(),
                ControlType::BlackLevel => *self.black_level.lock(),
                ControlType::CoolerEnable => i64::from(self.cooler_enable.load(Ordering::SeqCst)),
                ControlType::TargetTemperature => *self.target_temp_tenths.lock(),
                ControlType::CurrentTemperature => *self.current_temp_tenths.lock(),
                ControlType::CoolerPower => {
                    if self.cooler_enable.load(Ordering::SeqCst) {
                        60
                    } else {
                        0
                    }
                }
                _ => return Err(BackendError("invalid control type".to_string())),
            };
            Ok(value)
        }

        fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
            if self.fail_controls.load(Ordering::SeqCst) {
                return Err(BackendError("injected SDK failure".to_string()));
            }
            match control {
                ControlType::Gain => *self.gain.lock() = value,
                ControlType::BlackLevel => *self.black_level.lock() = value,
                ControlType::CoolerEnable => {
                    self.cooler_enable.store(value != 0, Ordering::SeqCst);
                }
                ControlType::TargetTemperature => *self.target_temp_tenths.lock() = value,
                ControlType::Exposure => {}
                _ => return Err(BackendError("invalid control type".to_string())),
            }
            Ok(())
        }

        fn set_camera_mode(&self, _mode: CameraMode) -> BackendResult<()> {
            Ok(())
        }

        fn start_video_capture(&self) -> BackendResult<()> {
            self.start_video_capture_calls
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn stop_video_capture(&self) -> BackendResult<()> {
            self.stop_video_capture_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn capture(&self, request: CaptureRequest) -> BackendResult<Vec<u8>> {
            *self.last_capture_request.lock() = Some(request.clone());
            // Mirror the production handle's sliced wait: an abort/disconnect
            // (request.cancel) drains the capture promptly instead of
            // sleeping out the whole delay.
            let deadline = Instant::now() + *self.capture_delay.lock();
            loop {
                if request.cancel.load(Ordering::SeqCst) {
                    // Mirror the production abort drain: stop, then re-arm
                    // for a trigger camera (see SvbonyCameraHandle::abort_capture).
                    self.stop_video_capture()?;
                    if request.is_trigger_cam {
                        self.start_video_capture()?;
                    }
                    return Err(BackendError("exposure aborted".to_string()));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            if self.fail_capture.load(Ordering::SeqCst) {
                return Err(BackendError(
                    "simulated mid-exposure SDK failure".to_string(),
                ));
            }
            if self.exceed_deadline.load(Ordering::SeqCst) {
                return Err(BackendError(format!(
                    "SVBGetVideoData deadline exceeded ({}ms)",
                    exposure_timeout_ms(request.exposure_us)
                )));
            }
            Ok(vec![
                0u8;
                request.width as usize
                    * request.height as usize
                    * request.image_type.bytes_per_pixel()
            ])
        }

        fn pulse_guide(&self, _direction: GuideDirection, _duration_ms: i32) -> BackendResult<()> {
            if self.fail_controls.load(Ordering::SeqCst) {
                return Err(BackendError("injected SDK failure".to_string()));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod pure_fn_tests {
    use super::*;

    #[test]
    fn exposure_timeout_is_double_plus_500ms() {
        assert_eq!(exposure_timeout_ms(10_000), 20 + 500);
        assert_eq!(exposure_timeout_ms(1_000_000), 2_000 + 500);
    }

    #[test]
    fn exposure_timeout_clamps_a_negative_exposure_to_the_500ms_floor() {
        assert_eq!(exposure_timeout_ms(-1), 500);
    }

    #[test]
    fn exposure_timeout_saturates_instead_of_overflowing() {
        assert_eq!(exposure_timeout_ms(i64::MAX), i32::MAX);
    }
}

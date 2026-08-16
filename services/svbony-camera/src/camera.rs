//! `SvbonyCamera` — the ASCOM `Device` + `Camera` implementation over the
//! [`CameraHandle`](crate::backend::CameraHandle) seam.
//!
//! Behaviour follows `docs/services/svbony-camera.md`'s "Behavioral
//! contracts", with the load-bearing divergence from the `zwo-camera`
//! template being the exposure path: `SVBony` has no snap-exposure API, so
//! every exposure rides the soft-trigger video-capture state machine (mode
//! selection + video-capture start once at connect; each `StartExposure` is
//! set-exposure → soft-trigger → `SVBGetVideoData` with a deadline) instead
//! of ZWO's `ASIStartExposure`/`ASIStopExposure` snap model. Consequences:
//! - **No data-preserving stop**: `CanStopExposure = false`, `StopExposure`
//!   is `NOT_IMPLEMENTED` unconditionally (E8) — the opposite of
//!   `zwo-camera`'s graceful stop.
//! - **`AbortExposure` never touches the SDK**: see `backend.rs`'s module
//!   docs for why — it only bumps the exposure generation counter so a
//!   late-completing capture's result is discarded (E7).
//! - **Dark frames are accepted** on every model — there is no mechanical
//!   shutter in video mode (`HasShutter = false`), so `Light = false`
//!   captures identically (E4/E7).
//! - **`ElectronsPerADU`** is a permanent `NOT_IMPLEMENTED` placeholder (ST2)
//!   — `SVB_CAMERA_PROPERTY` carries no native electrons-per-ADU field.
//! - Sensor/capability data (`SVB_CAMERA_PROPERTY`/`_EX`, pixel size) is
//!   readable only once the camera is **open**, unlike ZWO where the SDK
//!   hands back full info as part of `CameraInfo`. This device therefore
//!   caches it in [`DeviceState`] at the connect handshake, not at
//!   construction time.
//!
//! Blocking capture SDK calls run on `spawn_blocking` inside a detached
//! task; a generation counter lets abort/disconnect invalidate a
//! late-completing task — the same discipline `zwo-camera`'s
//! `run_exposure`/`result_lock` pattern uses.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, GuideDirection, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use parking_lot::Mutex;
use rusty_photon_camera_core::{self as camera_core, Alignment, PixelDepth, Roi};
use svbony_rs::{BayerPattern, CameraInfo, ControlCaps, ControlType, ImageType};
use tracing::{debug, warn};

use crate::backend::{CameraHandle, CaptureRequest};
use crate::config::DeviceOverride;
use crate::config_actions::SvbonyCameraDriver;
use rusty_photon_driver::ConfigActionCtx;

/// 0x500 — driver-specific catch-all for an asynchronous capture failure
/// surfaced lazily via `image_array` (E9).
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

/// `SVB_EXPOSURE`'s assumed unit is microseconds, so the smallest step is 1 µs
/// (see `svbony_rs::ControlType::Exposure`'s doc comment for the unit
/// caveat, to be confirmed against real hardware).
const EXPOSURE_RESOLUTION: Duration = Duration::from_micros(1);

/// The manual `SVB_EXPOSURE` the connect handshake writes (C1a) — the
/// SDK's only path that clears its auto-exposure state, which otherwise
/// refuses every gain write (GO5). One second, as `indi_svbony_ccd`'s
/// `Connect()` uses; the value itself is immaterial (every exposure sets
/// its own), so it is clamped into whatever range the camera advertises.
const CONNECT_EXPOSURE_US: i64 = 1_000_000;

/// One selectable download format: what the SDK is told to produce, the
/// name it is published under in ASCOM's `ReadoutModes`, and the full-scale
/// value `MaxADU` reports for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadoutFormat {
    image_type: ImageType,
    name: &'static str,
    max_adu: u32,
}

/// Every download format this driver can deliver, **in preference order**.
/// At connect these are intersected with the camera's advertised
/// `SVB_CAMERA_PROPERTY.SupportedVideoFormat` and the survivors become
/// `ReadoutModes`, so index 0 is always the highest precision the camera
/// actually offers (RM1).
///
/// The list is deliberately just the two raw formats — see the design doc's
/// RM4. `RGB24`/`RGB32` are SDK-debayered 8-bit-per-channel output: they
/// discard the raw sensor data and would change this device's *contract*
/// (`SensorType::Color`, no `BayerOffset`, a rank-3 `ImageArray`) rather
/// than just its buffer arithmetic, and `RGB32`'s 4-bytes-per-pixel layout
/// is an unverified assumption besides. `Y8`-`Y16` are byte-wise safe but
/// redundant: on a mono camera `Y16` is `Raw16`, and admitting them would
/// add a colour guard around a branch no known model reaches (`RAW8` is the
/// SDK's universal baseline).
const READOUT_FORMATS: [ReadoutFormat; 2] = [
    ReadoutFormat {
        image_type: ImageType::Raw16,
        name: "Raw16",
        // NOT `2^MaxBitDepth - 1`: the SDK rescales sub-16-bit ADC data to
        // the full 16-bit range — hardware-verified on the 14-bit SV605CC,
        // whose saturated Raw16 pixels read 65535 (low bits populated, so a
        // genuine rescale, not a bare left shift) — so the delivered data's
        // ceiling is the format's, not the ADC's (ST3).
        // `u16::MAX`, spelled out because `From` is not const-stable.
        max_adu: 65_535,
    },
    ReadoutFormat {
        image_type: ImageType::Raw8,
        name: "Raw8",
        // `u8::MAX`, spelled out because `From` is not const-stable.
        max_adu: 255,
    },
];

/// The `SVBony` sub-frame alignment rule (R3): a binned width that is a
/// multiple of 8 and a height that is a multiple of 2.
///
/// This is the *whole* difference between this driver's geometry and
/// `qhy-camera`'s, which passes `None` — everything else about validating a ROI
/// is shared, and lives in `rusty-photon-camera-core`.
const ALIGNMENT: Option<Alignment> = Some(Alignment::new(
    NonZeroU32::new(8).expect("8 is not zero"),
    NonZeroU32::new(2).expect("2 is not zero"),
));

/// Sensor geometry and capability data cached from `SVB_CAMERA_PROPERTY`/
/// `_EX` and `SVBGetSensorPixelSize` at the connect handshake — unlike
/// `zwo-camera`, these are **not** available at construction time because
/// `SVBony`'s SDK only returns them for an *open* camera (see the module
/// docs).
#[derive(Debug, Clone)]
struct SensorInfo {
    max_width: u32,
    max_height: u32,
    is_color: bool,
    bayer_pattern: BayerPattern,
    supported_bins: Vec<u32>,
    /// The download formats this camera can actually deliver — the
    /// [`READOUT_FORMATS`] preference order filtered by the camera's
    /// advertised `SupportedVideoFormat` (RM1). Published as
    /// `ReadoutModes`, indexed by `DeviceState::readout_mode`, and never
    /// empty: a camera offering neither raw format fails connect (RM3).
    readout_formats: Vec<ReadoutFormat>,
    pixel_size_um: f32,
    is_trigger_cam: bool,
    supports_control_temp: bool,
    supports_pulse_guide: bool,
}

/// Per-device runtime state: the connect-time property cache plus the
/// exposure state machine. Atomics for the hot/simple flags;
/// `parking_lot::Mutex` for the `Option<…>` caches and the captured image.
/// Locks are never held across an `await`.
#[derive(Debug)]
struct DeviceState {
    sensor: Mutex<Option<SensorInfo>>,

    /// Current symmetric bin (init 1).
    bin: AtomicU8,
    /// Current readout-mode index into [`SensorInfo::readout_formats`],
    /// reset to 0 (the camera's highest-precision format) on every connect.
    readout_mode: AtomicU8,
    /// Intended ROI in *binned* pixel coordinates (rescaled on bin change).
    intended_roi: Mutex<Option<Roi>>,
    /// `(min, max)` exposure microseconds from `SVBGetControlCaps(SVB_EXPOSURE)`.
    exposure_range_us: Mutex<Option<(i64, i64)>>,
    /// Gain range in ASCOM's own width, converted once at the open handshake
    /// (see [`ascom_range`]). `None` means the control is not advertised —
    /// either the model lacks it, or its range has no `i32` spelling.
    gain_min_max: Mutex<Option<(i32, i32)>>,
    /// Offset (`SVB_BLACK_LEVEL`) range, on the same terms as
    /// [`DeviceState::gain_min_max`].
    offset_min_max: Mutex<Option<(i32, i32)>>,
    target_temperature: Mutex<Option<f64>>,

    exposure_in_flight: AtomicBool,
    image_ready: AtomicBool,
    /// Set by `cancel_exposure` (abort or disconnect) and cleared by the next
    /// `start_exposure`/reconnect. `exposure_in_flight` itself deliberately
    /// stays `true` until the still-running, un-interruptible capture task
    /// drains (see `cancel_exposure`'s doc comment) — but `CameraState`/
    /// `PercentCompleted` must not keep reporting `Exposing`/a climbing
    /// percentage for that whole window just because the SDK can't be
    /// interrupted; this flag lets them report the operator's requested
    /// state (aborted → idle, not still exposing) promptly instead.
    aborted: AtomicBool,
    /// Bumped on each start / abort / disconnect so a late-completing capture
    /// task can tell it has been superseded and discard its result.
    exposure_generation: AtomicU64,
    /// The in-flight capture's cancel flag ([`CaptureRequest::cancel`]):
    /// set by `cancel_exposure` so the capture task bails out between
    /// `SVBGetVideoData` poll slices instead of draining the rest of its
    /// deadline. Replaced by each `start_exposure`.
    capture_cancel: Mutex<Option<Arc<AtomicBool>>>,
    last_exposure_start_time: Mutex<Option<SystemTime>>,
    last_exposure_duration: Mutex<Option<Duration>>,
    last_image: Mutex<Option<ImageArray>>,
    /// Set on a mid-exposure SDK failure or an exceeded `SVBGetVideoData`
    /// deadline → `CameraState::Error` (E9).
    last_error: Mutex<Option<String>>,
    /// Serializes the capture task's "check generation + commit result"
    /// against `cancel_exposure`'s "bump generation + clear `image_ready`".
    result_lock: Mutex<()>,
    /// Serializes `set_readout_mode`'s "reject if exposing, else store" against
    /// `start_exposure`'s "claim the in-flight slot, then pin the download
    /// format". Without it either order of the two unsynchronised halves can
    /// interleave into a frame captured in one format while `ReadoutMode` and
    /// `MaxADU` report the other (RM1).
    ///
    /// **Lock order:** a path that needs *both* this lock and [`Self::sensor`]
    /// takes this one first — `start_exposure` holds it across
    /// `selected_format`'s `sensor` read, and `set_readout_mode` matches. Most
    /// `sensor` reads need no lock at all and take none. Nothing ever holds
    /// `sensor` while waiting (its accessor clones and releases), so this order
    /// is discipline for future edits rather than a live hazard.
    readout_mode_lock: Mutex<()>,

    /// True only for the duration of a blocking `PulseGuide` SDK call (v0
    /// keeps `PulseGuide` synchronous — see `pulse_guide`'s doc comment).
    pulse_guiding: AtomicBool,
}

impl DeviceState {
    const fn new() -> Self {
        Self {
            sensor: Mutex::new(None),
            bin: AtomicU8::new(1),
            readout_mode: AtomicU8::new(0),
            intended_roi: Mutex::new(None),
            exposure_range_us: Mutex::new(None),
            gain_min_max: Mutex::new(None),
            offset_min_max: Mutex::new(None),
            target_temperature: Mutex::new(None),
            exposure_in_flight: AtomicBool::new(false),
            image_ready: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            exposure_generation: AtomicU64::new(0),
            capture_cancel: Mutex::new(None),
            last_exposure_start_time: Mutex::new(None),
            last_exposure_duration: Mutex::new(None),
            last_image: Mutex::new(None),
            last_error: Mutex::new(None),
            result_lock: Mutex::new(()),
            readout_mode_lock: Mutex::new(()),
            pulse_guiding: AtomicBool::new(false),
        }
    }

    /// Reset the exposure state machine to a clean idle state. Called on
    /// connect so a stale `Error` / `ImageReady` / image from a previous
    /// session does not survive a reconnect (C3).
    fn reset_exposure_state(&self) {
        let _guard = self.result_lock.lock();
        self.exposure_generation.fetch_add(1, Ordering::AcqRel);
        // A capture task somehow still draining from a previous session
        // should bail promptly rather than run out its deadline.
        if let Some(cancel) = self.capture_cancel.lock().take() {
            cancel.store(true, Ordering::Release);
        }
        self.exposure_in_flight.store(false, Ordering::Release);
        self.image_ready.store(false, Ordering::Release);
        self.aborted.store(false, Ordering::Release);
        *self.last_image.lock() = None;
        *self.last_error.lock() = None;
        *self.last_exposure_start_time.lock() = None;
        *self.last_exposure_duration.lock() = None;
        self.pulse_guiding.store(false, Ordering::Release);
    }
}

/// One ASCOM Camera device per discovered `SVBony` camera.
#[derive(Clone, derive_more::Debug)]
pub struct SvbonyCamera {
    #[debug(skip)]
    handle: Arc<dyn CameraHandle>,
    info: CameraInfo,
    unique_id: String,
    name: String,
    description: String,
    state: Arc<DeviceState>,
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<SvbonyCameraDriver>>,
}

impl SvbonyCamera {
    /// Build a device from an SDK handle and an optional per-serial config
    /// override. The ASCOM `UniqueID` is the handle's serial-derived id;
    /// `name`/`description` fall back to SDK-derived defaults.
    pub fn new(handle: Arc<dyn CameraHandle>, overrides: Option<&DeviceOverride>) -> Self {
        let info = handle.info();
        let unique_id = handle.unique_id();
        let name = overrides
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| info.friendly_name.clone());
        let description = overrides
            .and_then(|o| o.description.clone())
            .unwrap_or_else(|| format!("SVBony camera ({})", info.friendly_name));
        Self {
            handle,
            info,
            unique_id,
            name,
            description,
            state: Arc::new(DeviceState::new()),
            config_ctx: None,
        }
    }

    /// Attach config-action wiring (enables `config.get`/`apply`/`schema`).
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<SvbonyCameraDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        if self.handle.is_open() {
            Ok(())
        } else {
            Err(ASCOMError::NOT_CONNECTED)
        }
    }

    /// The connect-time sensor property cache. `ensure_connected` should
    /// already have been checked by the caller; `NOT_CONNECTED` here is a
    /// defensive fallback for the race between a connected handle and a
    /// not-yet-populated cache.
    fn sensor(&self) -> ASCOMResult<SensorInfo> {
        (*self.state.sensor.lock())
            .clone()
            .ok_or(ASCOMError::NOT_CONNECTED)
    }

    fn connect(&self) -> ASCOMResult<()> {
        // `open()` is an atomic check-and-open under the handle's own lock
        // (qhy-camera's `SharedCameraConnection` shape): of several racing
        // connects, exactly one observes `true` and owns the post-open
        // handshake below — the trigger-camera video-capture arm is not
        // idempotent, so a second handshake must never run. The losers
        // return Ok immediately, without waiting for the winner's
        // handshake; until it completes, cached properties may still be
        // unpopulated, which every cache read already treats as
        // NOT_CONNECTED (see `sensor()`'s fallback).
        let opened = self.handle.open().map_err(|e| {
            warn!(camera = %self.unique_id, error = %e, "SDK open failed");
            ASCOMError::NOT_CONNECTED
        })?;
        if !opened {
            return Ok(());
        }
        // A failed post-open handshake must leave the device disconnected
        // (C2), not opened-but-unusable, so close before propagating.
        if let Err(e) = self.open_handshake() {
            if let Err(close_err) = self.handle.close() {
                debug!(error = %close_err, "close after a failed connect handshake also failed");
            }
            return Err(e);
        }
        // A reconnect must not surface a previous session's Error /
        // ImageReady / stale frame (C3).
        self.state.reset_exposure_state();
        debug!(camera = %self.unique_id, "camera connected");
        Ok(())
    }

    /// The post-open handshake (C1a, mirroring `indi_svbony_ccd::Connect`):
    /// restore the SDK's default parameters and turn its parameter
    /// auto-save off (both advisory — a failure is logged, not fatal),
    /// read and cache the camera's properties/controls, write one manual
    /// `SVB_EXPOSURE` (the SDK's only auto-exposure-off path, without which
    /// every gain write is refused until the first exposure — GO5), then
    /// run the exposure state machine's connect-time step for trigger
    /// cameras (mode selection + video-capture start — never for a
    /// non-trigger camera, see this method's body — per
    /// `docs/services/svbony-camera.md` "Behavioral contracts → Exposure"
    /// step 1). Tenet 3 (K5): this method never
    /// touches `SVB_COOLER_ENABLE`/`SVB_TARGET_TEMPERATURE` — cooling is
    /// engaged only by an explicit operator `CoolerOn`/`SetCCDTemperature`
    /// call, never here — and none of the C1a writes actuates anything (a
    /// parameter restore, a software flag, and the exposure register on a
    /// camera whose capture is trigger-gated or not yet started).
    fn open_handshake(&self) -> ASCOMResult<()> {
        // A session starts from the SDK's device defaults, never from the
        // parameter block a previous session left behind. The SDK reports
        // a general error here when it cannot re-persist the block to
        // `<model>_Cfg_A.bin` in the working directory even though the
        // restore itself took effect, so this is advisory.
        if let Err(e) = self.handle.restore_default_param() {
            warn!(
                error = %e,
                "restoring the SDK's default parameters failed (usually its \
                 config-file write in the working directory; the restore \
                 itself normally still took effect)"
            );
        }
        // Stop the SDK carrying session state through `<model>_Cfg_SAVE.bin`
        // in the working directory (written at close, reloaded at open).
        if let Err(e) = self.handle.set_auto_save_param(false) {
            warn!(error = %e, "disabling the SDK's parameter auto-save failed");
        }

        let property = self
            .handle
            .property()
            .map_err(handshake_err("property fetch"))?;
        let property_ex = self
            .handle
            .property_ex()
            .map_err(handshake_err("property-ex fetch"))?;
        let pixel_size_um = self
            .handle
            .pixel_size_microns()
            .map_err(handshake_err("pixel-size read"))?;

        let caps = self
            .handle
            .control_caps()
            .map_err(handshake_err("control-caps enumeration"))?;
        let find = |ct: ControlType| caps.iter().find(|c| c.control_type == ct);

        let exposure = find(ControlType::Exposure).ok_or_else(|| {
            warn!("camera does not advertise an exposure control");
            ASCOMError::NOT_CONNECTED
        })?;
        *self.state.exposure_range_us.lock() = Some((exposure.min, exposure.max));
        *self.state.gain_min_max.lock() = find(ControlType::Gain).and_then(ascom_range);
        *self.state.offset_min_max.lock() = find(ControlType::BlackLevel).and_then(ascom_range);

        // One manual `SVB_EXPOSURE` write clears the SDK's auto-exposure
        // state, which is on after open (and after the restore above) and
        // refuses every gain write while on (GO5). Advisory like the two
        // steps above: a camera that keeps refusing it still exposes — its
        // gain simply stays refused until the first exposure's own write.
        let connect_exposure_us = CONNECT_EXPOSURE_US.clamp(exposure.min, exposure.max);
        if let Err(e) = self
            .handle
            .set_control_value(ControlType::Exposure, connect_exposure_us)
        {
            warn!(
                error = %e,
                "clearing the SDK's auto-exposure state at connect failed; gain \
                 will be refused until the first exposure"
            );
        }

        // Negotiate the download format against what the camera actually
        // advertises (RM1) instead of assuming Raw16: a model that does not
        // support it would otherwise fail `SVBSetOutputImageType` on every
        // exposure. Index 0 — the highest precision offered — is the
        // default, restored here on every connect.
        let readout_formats: Vec<ReadoutFormat> = READOUT_FORMATS
            .into_iter()
            .filter(|f| property.supported_video_formats.contains(&f.image_type))
            .collect();
        if readout_formats.is_empty() {
            // RM3: no raw format means nothing this driver's single-plane
            // ImageArray contract can describe. Fail loudly rather than
            // download a debayered RGB frame we would then misreport.
            warn!(
                advertised = ?property.supported_video_formats,
                "camera advertises no downloadable raw format (Raw16 or Raw8)"
            );
            return Err(ASCOMError::NOT_CONNECTED);
        }

        self.state.bin.store(1, Ordering::Release);
        self.state.readout_mode.store(0, Ordering::Release);
        // Report the sensor extent aligned down so the full frame divided by
        // every supported bin satisfies the SDK's width%8 / height%2 ROI
        // rule (SV605CC: 3008x3008 raw -> 2976x3000 reported) — clients
        // (ConformU among them) take a full frame at every bin via
        // NumX = CameraXSize / bin, which the raw extent cannot satisfy at
        // every bin (3008/3 = 1002 is not a multiple of 8).
        let supported_bins: Vec<u32> = property.supported_bins.clone();
        // Both extents from one call, taking the same `ALIGNMENT` that
        // validates ROIs — so a sensor sized for one rule can never be reported
        // while ROIs are checked against another.
        let (max_width, max_height) = camera_core::aligned_sensor(
            u32::try_from(property.max_width).unwrap_or(0),
            u32::try_from(property.max_height).unwrap_or(0),
            &supported_bins,
            ALIGNMENT,
        );
        *self.state.intended_roi.lock() = Some(Roi {
            start_x: 0,
            start_y: 0,
            width: max_width,
            height: max_height,
        });
        *self.state.target_temperature.lock() = None;

        *self.state.sensor.lock() = Some(SensorInfo {
            max_width,
            max_height,
            is_color: property.is_color,
            bayer_pattern: property.bayer_pattern,
            supported_bins: property.supported_bins.clone(),
            readout_formats,
            pixel_size_um,
            is_trigger_cam: property.is_trigger_cam,
            supports_control_temp: property_ex.supports_control_temp,
            supports_pulse_guide: property_ex.supports_pulse_guide,
        });

        // State-machine step 1, trigger cameras only (tenet 3): select
        // `SVB_MODE_TRIG_SOFT` and arm video capture once, here, never
        // repeated per-exposure. A trigger-gated capture produces no frames
        // — and therefore does not physically actuate the imaging chain —
        // until an operator's `StartExposure` sends the soft trigger, so
        // this is a read of camera-mode capability plus an armed-but-idle
        // mode-select, not actuation (see the design doc's exposure contract
        // point 1). A **non**-trigger camera has no such gate: its only mode
        // is free-running `SVB_MODE_NORMAL`, so starting video capture here
        // would begin the sensor continuously integrating and streaming
        // frames as a side effect of connecting — genuine actuation with no
        // operator action, which tenet 3 bans outright. So for non-trigger
        // cameras, video capture is left unarmed at connect; `capture`'s
        // non-trigger fallback (state-machine step 5) already
        // stops-then-starts it fresh on every operator-initiated
        // `StartExposure`, which is where a non-trigger camera's capture
        // must first arm.
        if property.is_trigger_cam {
            self.handle
                .set_camera_mode(svbony_rs::CameraMode::TrigSoft)
                .map_err(handshake_err("soft-trigger mode select"))?;
            self.handle
                .start_video_capture()
                .map_err(handshake_err("video-capture arm"))?;
        }

        Ok(())
    }

    fn disconnect(&self) -> ASCOMResult<()> {
        // An in-flight exposure is cancelled (C3) before the handle closes.
        self.cancel_exposure();
        self.handle.close().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        debug!(camera = %self.unique_id, "camera disconnected");
        Ok(())
    }

    /// Cancel any in-flight exposure (abort): bump the generation so the
    /// capture task discards its result, set the capture's cancel flag so it
    /// bails out between `SVBGetVideoData` poll slices (draining within
    /// ~one slice, not the rest of its deadline — see `backend.rs`'s module
    /// docs, "How `capture` aborts"), clear `image_ready`/`last_error`, and
    /// set `aborted` so `CameraState`/`PercentCompleted` promptly report
    /// idle (see `DeviceState::aborted`'s doc comment). Deliberately does
    /// NOT clear `exposure_in_flight` — the capture task clears that once
    /// its (now short) drain completes, so a new exposure cannot race the
    /// still-running one (the design's "one owner per device").
    fn cancel_exposure(&self) {
        if !self.state.exposure_in_flight.load(Ordering::Acquire) {
            return;
        }
        // Atomic with the capture task's commit so an abort can never be
        // overwritten by a just-completing capture.
        let _guard = self.state.result_lock.lock();
        self.state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel);
        if let Some(cancel) = self.state.capture_cancel.lock().as_ref() {
            cancel.store(true, Ordering::Release);
        }
        self.state.image_ready.store(false, Ordering::Release);
        self.state.aborted.store(true, Ordering::Release);
        *self.state.last_error.lock() = None;
    }

    /// Validate the cached ROI against the binned sensor geometry (R2/R3),
    /// returning the [`Roi`] to push to the SDK.
    fn validated_geometry(&self, sensor: &SensorInfo, bin: u32) -> ASCOMResult<Roi> {
        let roi = (*self.state.intended_roi.lock())
            .ok_or_else(|| ASCOMError::invalid_value("no ROI defined for camera"))?;
        check_geometry(roi, sensor.max_width, sensor.max_height, bin)?;
        Ok(roi)
    }

    /// The download format the current `ReadoutMode` selects (RM2). The
    /// index is validated on every write and reset at connect, so the
    /// out-of-range arm is defensive only.
    fn selected_format(&self) -> ASCOMResult<ReadoutFormat> {
        let sensor = self.sensor()?;
        let index = usize::from(self.state.readout_mode.load(Ordering::Acquire));
        sensor
            .readout_formats
            .get(index)
            .copied()
            .ok_or_else(|| ASCOMError::invalid_value("readout mode index out of range"))
    }

    fn gain_available(&self) -> bool {
        self.state.gain_min_max.lock().is_some()
    }

    fn offset_available(&self) -> bool {
        self.state.offset_min_max.lock().is_some()
    }

    /// Run a blocking SDK-seam call off the async executor. The `SVBony` FFI
    /// calls do USB I/O, so running them directly on a Tokio worker could
    /// stall other Alpaca requests; offload them like the capture, connect,
    /// and pulse-guide paths.
    async fn on_handle<T, F>(&self, f: F) -> ASCOMResult<T>
    where
        F: FnOnce(&dyn CameraHandle) -> ASCOMResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = Arc::clone(&self.handle);
        tokio::task::spawn_blocking(move || f(handle.as_ref()))
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("SDK task failed: {e}")))?
    }
}

/// Map a connect-handshake SDK failure onto `NOT_CONNECTED` (C2), logging
/// the underlying SDK error — which the mapped ASCOM code cannot carry —
/// so a real-hardware failure at any handshake step is diagnosable from
/// the service log.
fn handshake_err(step: &'static str) -> impl FnOnce(crate::backend::BackendError) -> ASCOMError {
    move |e| {
        warn!(error = %e, "connect handshake failed at {step}");
        ASCOMError::NOT_CONNECTED
    }
}

/// Geometry validation (R2/R3), as the ASCOM error a client sees.
///
/// The rules, their order, and the message text all live in
/// `rusty-photon-camera-core`, shared with `qhy-camera` and `zwo-camera`, as
/// does the ASCOM code it becomes. What this driver contributes is the
/// `SVBony` [`ALIGNMENT`] rule.
fn check_geometry(roi: Roi, sensor_w: u32, sensor_h: u32, bin: u32) -> ASCOMResult<()> {
    Ok(camera_core::check(roi, sensor_w, sensor_h, bin, ALIGNMENT)?)
}

/// A control's range as ASCOM must describe it, or `None` when the driver
/// reports bounds outside `i32`.
///
/// ASCOM's `Gain`/`GainMin`/`GainMax` (and the offset trio) are `i32` while the
/// SVBony SDK reports control caps as `long`. Converting here rather than at
/// each read asks the "does it fit?" question once, at the handshake, where
/// leaving the control unadvertised is a meaningful answer — a clamped bound
/// would advertise a maximum the camera then rejects.
fn ascom_range(caps: &ControlCaps) -> Option<(i32, i32)> {
    Some((i32::try_from(caps.min).ok()?, i32::try_from(caps.max).ok()?))
}

/// Bayer pattern → ASCOM `BayerOffsetX/Y` (ST1).
///
/// `SVB_BAYER_RG` and friends abbreviate the quad to its first row, so the
/// vendor spelling is all this maps; where the red photosite then sits is the
/// shared crate's rule.
const fn bayer_offsets(pattern: BayerPattern) -> (u8, u8) {
    match pattern {
        BayerPattern::Rg => camera_core::BayerPattern::Rggb,
        BayerPattern::Bg => camera_core::BayerPattern::Bggr,
        BayerPattern::Gr => camera_core::BayerPattern::Grbg,
        BayerPattern::Gb => camera_core::BayerPattern::Gbrg,
    }
    .offsets()
}

/// Map an ASCOM guide direction onto the `svbony-rs` one.
const fn guide_direction(direction: GuideDirection) -> svbony_rs::GuideDirection {
    match direction {
        GuideDirection::North => svbony_rs::GuideDirection::North,
        GuideDirection::South => svbony_rs::GuideDirection::South,
        GuideDirection::East => svbony_rs::GuideDirection::East,
        GuideDirection::West => svbony_rs::GuideDirection::West,
    }
}

/// Convert a single-plane frame into an ASCOM `ImageArray` with `[x][y]`
/// axis order (ASCOM stores width-major), unpacking per the format the
/// frame was downloaded in (RM2). Only the raw formats [`READOUT_FORMATS`]
/// can select are convertible; anything else is a caller error, reported
/// rather than mis-unpacked.
///
/// The unpack itself is `rusty-photon-camera-core`'s, shared with
/// `qhy-camera` and `zwo-camera`; this driver's share is the format→depth
/// decision above and the format name in the message.
fn to_image_array(
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    image_type: ImageType,
) -> Result<ImageArray, String> {
    let depth = match image_type {
        ImageType::Raw8 => PixelDepth::Eight,
        ImageType::Raw16 => PixelDepth::Sixteen,
        other => return Err(format!("unsupported download format {other:?}")),
    };
    camera_core::to_image_array(bytes, width, height, depth)
        .map_err(|error| format!("{image_type:?} {error}"))
}

/// The detached capture task: runs the blocking soft-trigger SDK chain *and*
/// the CPU-heavy frame transform on `spawn_blocking`, then stores the image
/// (or records a failure as the `Error` state, E9) — unless a newer
/// generation has superseded it (an abort or disconnect).
///
/// Both the SDK download and [`to_image_array`] run inside the one
/// `spawn_blocking` closure on purpose — see `zwo-camera`'s equivalent
/// `run_exposure` doc comment for the full rationale (a full-frame transform
/// is CPU-heavy enough in an unoptimised build to matter, and running it
/// while holding `result_lock` would contend `cancel_exposure`).
async fn run_exposure(
    handle: Arc<dyn CameraHandle>,
    state: Arc<DeviceState>,
    generation: u64,
    request: CaptureRequest,
) {
    let blocking_handle = Arc::clone(&handle);
    let (width, height, image_type) = (request.width, request.height, request.image_type);
    let result = tokio::task::spawn_blocking(move || {
        blocking_handle
            .capture(request)
            .map(|bytes| to_image_array(bytes, width, height, image_type))
    })
    .await;

    {
        // No await is held across the lock (the blocking await is above), so
        // this "check generation + record" is atomic against
        // cancel_exposure. Only the cheap commit runs here — the transform
        // already happened off-thread.
        let _guard = state.result_lock.lock();
        if state.exposure_generation.load(Ordering::Acquire) == generation {
            match result {
                Ok(Ok(Ok(array))) => {
                    *state.last_image.lock() = Some(array);
                    *state.last_error.lock() = None;
                    state.image_ready.store(true, Ordering::Release);
                }
                Ok(Ok(Err(e))) => {
                    warn!(error = %e, "failed to transform captured image");
                    *state.last_image.lock() = None;
                    *state.last_error.lock() = Some(format!("image transform failed: {e}"));
                }
                Ok(Err(e)) => {
                    warn!(error = %e.0, "mid-exposure SDK error or SVBGetVideoData deadline exceeded");
                    *state.last_error.lock() = Some(e.0);
                }
                Err(join_err) => {
                    warn!(error = %join_err, "exposure task panicked");
                    *state.last_error.lock() = Some(format!("exposure task failed: {join_err}"));
                }
            }
        }
    }
    state.exposure_in_flight.store(false, Ordering::Release);
}

#[async_trait::async_trait]
impl Device for SvbonyCamera {
    fn static_name(&self) -> &str {
        &self.name
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.handle.is_open())
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // This check is a best-effort fast path, not the connect guard: two
        // concurrent `Connected=true` requests can both pass it. The
        // authoritative check-and-transition is `connect`'s atomic
        // `handle.open()` (one critical section in the handle), which lets
        // exactly one racing connect run the non-idempotent handshake —
        // the loser no-ops without waiting for it.
        if self.handle.is_open() == connected {
            return Ok(());
        }
        // `connect`/`disconnect` do blocking SDK I/O, so offload off the
        // executor (SvbonyCamera is cheap to clone: it is `Arc`-backed).
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            if connected {
                this.connect()
            } else {
                this.disconnect()
            }
        })
        .await
        .map_err(|e| ASCOMError::invalid_operation(format!("connect task failed: {e}")))?
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.description.clone())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon svbony-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<SvbonyCameraDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait::async_trait]
impl Camera for SvbonyCamera {
    // --- geometry ---------------------------------------------------------------

    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        Ok(self.sensor()?.max_width)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        Ok(self.sensor()?.max_height)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        Ok(f64::from(self.sensor()?.pixel_size_um))
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        // SVBony exposes a single pixel size, so X == Y trivially.
        self.pixel_size_x().await
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        // ST3/RM2: the ceiling belongs to the delivered format, so it
        // tracks the selected readout mode — 65535 in Raw16, 255 in Raw8.
        self.ensure_connected()?;
        Ok(self.selected_format()?.max_adu)
    }

    async fn electrons_per_adu(&self) -> ASCOMResult<f64> {
        // ST2: permanent NOT_IMPLEMENTED placeholder — SVB_CAMERA_PROPERTY
        // carries no native electrons-per-ADU field (unlike ZWO's ElecPerADU).
        self.ensure_connected()?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        self.ensure_connected()?;
        Ok(self.info.friendly_name.clone())
    }

    // --- binning ----------------------------------------------------------------

    async fn bin_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        Ok(self.state.bin.load(Ordering::Acquire))
    }

    async fn bin_y(&self) -> ASCOMResult<u8> {
        self.bin_x().await
    }

    async fn set_bin_x(&self, bin_x: u8) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let sensor = self.sensor()?;
        if !sensor.supported_bins.contains(&u32::from(bin_x)) {
            return Err(ASCOMError::invalid_value(format!(
                "bin {bin_x} is not a supported binning mode"
            )));
        }
        let old = self.state.bin.load(Ordering::Acquire);
        if old == bin_x {
            return Ok(());
        }
        {
            let mut roi = self.state.intended_roi.lock();
            if let Some(area) = *roi {
                *roi = Some(camera_core::rescale(area, old, bin_x));
            }
        }
        self.state.bin.store(bin_x, Ordering::Release);
        Ok(())
    }

    async fn set_bin_y(&self, bin_y: u8) -> ASCOMResult<()> {
        self.set_bin_x(bin_y).await
    }

    async fn max_bin_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        self.sensor()?
            .supported_bins
            .iter()
            .copied()
            .max()
            .and_then(|m| u8::try_from(m).ok())
            .ok_or_else(|| ASCOMError::invalid_operation("no valid binning modes"))
    }

    async fn max_bin_y(&self) -> ASCOMResult<u8> {
        self.max_bin_x().await
    }

    async fn can_asymmetric_bin(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    // --- ROI (relaxed setters; validated at start_exposure) ---------------------

    async fn num_x(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.intended_roi.lock())
            .map(|r| r.width)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn num_y(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.intended_roi.lock())
            .map(|r| r.height)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn start_x(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.intended_roi.lock())
            .map(|r| r.start_x)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn start_y(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.intended_roi.lock())
            .map(|r| r.start_y)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn set_num_x(&self, num_x: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        match *roi {
            Some(area) => {
                *roi = Some(Roi {
                    width: num_x,
                    ..area
                });
                Ok(())
            }
            None => Err(ASCOMError::INVALID_VALUE),
        }
    }

    async fn set_num_y(&self, num_y: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        match *roi {
            Some(area) => {
                *roi = Some(Roi {
                    height: num_y,
                    ..area
                });
                Ok(())
            }
            None => Err(ASCOMError::INVALID_VALUE),
        }
    }

    async fn set_start_x(&self, start_x: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        match *roi {
            Some(area) => {
                *roi = Some(Roi { start_x, ..area });
                Ok(())
            }
            None => Err(ASCOMError::INVALID_VALUE),
        }
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        match *roi {
            Some(area) => {
                *roi = Some(Roi { start_y, ..area });
                Ok(())
            }
            None => Err(ASCOMError::INVALID_VALUE),
        }
    }

    // --- exposure range ---------------------------------------------------------

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        let (min, _) = (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        Ok(Duration::from_micros(min.max(0).cast_unsigned()))
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        let (_, max) = (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        Ok(Duration::from_micros(max.max(0).cast_unsigned()))
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        Ok(EXPOSURE_RESOLUTION)
    }

    // --- gain / offset ------------------------------------------------------------

    async fn gain(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        if !self.gain_available() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::Gain)
                .map_err(|e| ASCOMError::invalid_operation(format!("failed to read gain: {e}")))?;
            i32::try_from(raw)
                .map_err(|_| ASCOMError::invalid_operation(format!("camera reported gain {raw}")))
        })
        .await
    }

    async fn gain_min(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        (*self.state.gain_min_max.lock())
            .map(|(min, _)| min)
            .ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn gain_max(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        (*self.state.gain_min_max.lock())
            .map(|(_, max)| max)
            .ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_gain(&self, gain: i32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let (min, max) = (*self.state.gain_min_max.lock()).ok_or(ASCOMError::NOT_IMPLEMENTED)?;
        if gain < min || gain > max {
            return Err(ASCOMError::invalid_value(format!(
                "gain {gain} outside [{min}, {max}]"
            )));
        }
        self.on_handle(move |h| {
            h.set_control_value(ControlType::Gain, i64::from(gain))
                .map_err(|e| ASCOMError::invalid_operation(format!("failed to set gain: {e}")))
        })
        .await
    }

    async fn offset(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        if !self.offset_available() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h.control_value(ControlType::BlackLevel).map_err(|e| {
                ASCOMError::invalid_operation(format!("failed to read offset: {e}"))
            })?;
            i32::try_from(raw)
                .map_err(|_| ASCOMError::invalid_operation(format!("camera reported offset {raw}")))
        })
        .await
    }

    async fn offset_min(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        (*self.state.offset_min_max.lock())
            .map(|(min, _)| min)
            .ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn offset_max(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        (*self.state.offset_min_max.lock())
            .map(|(_, max)| max)
            .ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_offset(&self, offset: i32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let (min, max) = (*self.state.offset_min_max.lock()).ok_or(ASCOMError::NOT_IMPLEMENTED)?;
        if offset < min || offset > max {
            return Err(ASCOMError::invalid_value(format!(
                "offset {offset} outside [{min}, {max}]"
            )));
        }
        self.on_handle(move |h| {
            h.set_control_value(ControlType::BlackLevel, i64::from(offset))
                .map_err(|e| ASCOMError::invalid_operation(format!("failed to set offset: {e}")))
        })
        .await
    }

    // --- readout modes ------------------------------------------------------------

    async fn readout_mode(&self) -> ASCOMResult<usize> {
        self.ensure_connected()?;
        Ok(usize::from(self.state.readout_mode.load(Ordering::Acquire)))
    }

    async fn readout_modes(&self) -> ASCOMResult<Vec<String>> {
        // RM1: the camera's own download formats, negotiated at connect.
        self.ensure_connected()?;
        Ok(self
            .sensor()?
            .readout_formats
            .iter()
            .map(|f| f.name.to_string())
            .collect())
    }

    async fn set_readout_mode(&self, readout_mode: usize) -> ASCOMResult<()> {
        self.ensure_connected()?;
        // The mode selects the download format *and* the MaxADU describing
        // it (RM2), and the in-flight capture already carries the format it
        // was started with — so switching mid-exposure could only produce a
        // frame and a MaxADU that disagree. Validating and storing under
        // `readout_mode_lock` makes that exclusion hold against a
        // concurrently-starting exposure too, not just an already-running
        // one — see `start_exposure`'s matching critical section.
        //
        // `readout_mode_lock` is the OUTER lock wherever it and `sensor` are
        // both needed (`start_exposure` holds it across `selected_format`'s
        // `sensor` read), so it is taken before `sensor()` here even though
        // the bounds check alone would not need it. `sensor()` clones and
        // releases, so no path holds `sensor` while waiting on anything —
        // the fixed order is to keep that true as this code changes.
        let _guard = self.state.readout_mode_lock.lock();
        let available = self.sensor()?.readout_formats.len();
        if readout_mode >= available {
            return Err(ASCOMError::invalid_value(format!(
                "readout mode {readout_mode} out of range (0..{available})"
            )));
        }
        if self.state.exposure_in_flight.load(Ordering::Acquire) {
            return Err(ASCOMError::invalid_operation(
                "cannot change the readout mode while an exposure is in flight",
            ));
        }
        // Bounded by the range check above, which is itself a `usize` length, so
        // this narrowing has an answer for every index that got here.
        self.state.readout_mode.store(
            u8::try_from(readout_mode).unwrap_or(u8::MAX),
            Ordering::Release,
        );
        Ok(())
    }

    // --- sensor type / bayer -------------------------------------------------------

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        self.ensure_connected()?;
        Ok(if self.sensor()?.is_color {
            SensorType::RGGB
        } else {
            SensorType::Monochrome
        })
    }

    async fn bayer_offset_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        let sensor = self.sensor()?;
        if !sensor.is_color {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        Ok(bayer_offsets(sensor.bayer_pattern).0)
    }

    async fn bayer_offset_y(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        let sensor = self.sensor()?;
        if !sensor.is_color {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        Ok(bayer_offsets(sensor.bayer_pattern).1)
    }

    // --- cooling --------------------------------------------------------------------

    async fn can_set_ccd_temperature(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        Ok(self.sensor()?.supports_control_temp)
    }

    async fn can_get_cooler_power(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        Ok(self.sensor()?.supports_control_temp)
    }

    async fn ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        // K2: unlike zwo-camera's separately-cached temperature_available,
        // SVBony's property_ex exposes a single bSupportControlTemp flag
        // covering both the cooler and the readable sensor temperature, so
        // CCDTemperature is gated on the same flag as CanSetCCDTemperature.
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::CurrentTemperature)
                .map_err(|e| {
                    ASCOMError::new(
                        UNSPECIFIED_ERROR,
                        format!("failed to read sensor temperature: {e}"),
                    )
                })?;
            // A temperature outside `i32` is not a temperature; say so rather
            // than widen it lossily. Tenths of a degree (K3).
            i32::try_from(raw)
                .map(|t| f64::from(t) / 10.0)
                .map_err(|_| {
                    ASCOMError::new(
                        UNSPECIFIED_ERROR,
                        format!("camera reported sensor temperature {raw}"),
                    )
                })
        })
        .await
    }

    async fn set_ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if let Some(target) = *self.state.target_temperature.lock() {
            return Ok(target);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::TargetTemperature)
                .map_err(|e| {
                    ASCOMError::invalid_value(format!("failed to read target temperature: {e}"))
                })?;
            i32::try_from(raw)
                .map(|t| f64::from(t) / 10.0)
                .map_err(|_| {
                    ASCOMError::invalid_value(format!("camera reported target temperature {raw}"))
                })
        })
        .await
    }

    async fn set_set_ccd_temperature(&self, set_ccd_temperature: f64) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if !(-273.15..=80.0).contains(&set_ccd_temperature) {
            return Err(ASCOMError::invalid_value(format!(
                "target temperature {set_ccd_temperature} outside [-273.15, 80]"
            )));
        }
        // K3: encode to tenths of a degree (SVB_TARGET_TEMPERATURE's units).
        // Validated to [-273.15, 80] immediately above, so the rounded tenths are
        // in [-2732, 800] — a range `i64` holds with room to spare. No
        // `TryFrom<f64>` exists to spell that, and a fallible form would add an
        // arm the check above already makes unreachable.
        #[expect(
            clippy::as_conversions,
            reason = "bounded by the range check above; no TryFrom<f64> for i64 exists"
        )]
        let tenths = (set_ccd_temperature * 10.0).round() as i64;
        self.on_handle(move |h| {
            h.set_control_value(ControlType::TargetTemperature, tenths)
                .map_err(|e| {
                    ASCOMError::invalid_operation(format!("failed to set target temperature: {e}"))
                })
        })
        .await?;
        *self.state.target_temperature.lock() = Some(set_ccd_temperature);
        Ok(())
    }

    async fn cooler_on(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            h.control_value(ControlType::CoolerEnable)
                .map(|v| v != 0)
                .map_err(|e| ASCOMError::invalid_value(format!("failed to read cooler state: {e}")))
        })
        .await
    }

    // K5 (tenet 3): this is the ONLY code path in this file that may write
    // SVB_COOLER_ENABLE, and it is reachable solely from an explicit
    // operator ASCOM `CoolerOn` call — never from `connect`/`disconnect`/
    // `open_handshake`/`config.apply`.
    async fn set_cooler_on(&self, cooler_on: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(move |h| {
            h.set_control_value(ControlType::CoolerEnable, i64::from(cooler_on))
                .map_err(|e| {
                    ASCOMError::invalid_operation(format!("failed to set cooler state: {e}"))
                })
        })
        .await
    }

    async fn cooler_power(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if !self.sensor()?.supports_control_temp {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        // K4: SVB_COOLER_POWER is already a 0-100 percent, no normalization.
        self.on_handle(|h| {
            let raw = h.control_value(ControlType::CoolerPower).map_err(|e| {
                ASCOMError::invalid_value(format!("failed to read cooler power: {e}"))
            })?;
            i32::try_from(raw).map(f64::from).map_err(|_| {
                ASCOMError::invalid_value(format!("camera reported cooler power {raw}"))
            })
        })
        .await
    }

    // --- shutter / capability flags --------------------------------------------------

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        // No mechanical shutter in video mode (E4/E7).
        Ok(false)
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        // E8: no data-preserving stop exists at the SDK level.
        Ok(false)
    }

    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        Ok(self.sensor()?.supports_pulse_guide)
    }

    async fn is_pulse_guiding(&self) -> ASCOMResult<bool> {
        Ok(self.state.pulse_guiding.load(Ordering::Acquire))
    }

    // --- exposure state ---------------------------------------------------------------

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        if self.state.last_error.lock().is_some() {
            return Ok(CameraState::Error);
        }
        // An abort was requested: report idle promptly even though the
        // still-running, un-interruptible capture task hasn't drained yet
        // (see `DeviceState::aborted`'s doc comment) — `exposure_in_flight`
        // alone would keep reporting `Exposing` for the rest of the
        // deadline.
        if self.state.aborted.load(Ordering::Acquire) {
            return Ok(CameraState::Idle);
        }
        if self.state.exposure_in_flight.load(Ordering::Acquire) {
            return Ok(CameraState::Exposing);
        }
        Ok(CameraState::Idle)
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        Ok(self.state.image_ready.load(Ordering::Acquire)
            && !self.state.exposure_in_flight.load(Ordering::Acquire))
    }

    async fn percent_completed(&self) -> ASCOMResult<u8> {
        // Mirror `camera_state`: an abort means no image is ready, so 0 (not
        // the idle-ready branch's 100) is the honest answer while the
        // still-running capture task drains in the background.
        if self.state.aborted.load(Ordering::Acquire) {
            return Ok(0);
        }
        if !self.state.exposure_in_flight.load(Ordering::Acquire) {
            // Idle: 100 once ready, 0 in the Error state.
            return Ok(if self.state.last_error.lock().is_some() {
                0
            } else {
                100
            });
        }
        let start = *self.state.last_exposure_start_time.lock();
        let duration = *self.state.last_exposure_duration.lock();
        let (Some(start), Some(duration)) = (start, duration) else {
            return Ok(0);
        };
        // Never 100 while in flight — that answer belongs to the ready state
        // above, and the cap is shared with the sibling drivers.
        let elapsed = start.elapsed().unwrap_or(Duration::ZERO);
        Ok(camera_core::progress_percent(elapsed, duration))
    }

    async fn last_exposure_start_time(&self) -> ASCOMResult<SystemTime> {
        (*self.state.last_exposure_start_time.lock()).ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn last_exposure_duration(&self) -> ASCOMResult<Duration> {
        (*self.state.last_exposure_duration.lock()).ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        self.ensure_connected()?;
        if let Some(msg) = self.state.last_error.lock().clone() {
            return Err(ASCOMError::new(UNSPECIFIED_ERROR, msg));
        }
        // ASCOM: `ImageArray` is valid only once `ImageReady` is true. Mirror
        // the `image_ready()` condition so a client can never read a stale
        // frame.
        let ready = self.state.image_ready.load(Ordering::Acquire)
            && !self.state.exposure_in_flight.load(Ordering::Acquire);
        if !ready {
            return Err(ASCOMError::invalid_operation(
                "no image available; ImageReady is false",
            ));
        }
        self.state
            .last_image
            .lock()
            .clone()
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    // --- exposure control ---------------------------------------------------------------

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        // No mechanical shutter in video mode: dark and light frames are
        // captured identically (E4/E7) — `light` only ever informed a
        // shutter, which SVBony's video mode does not have.
        let _ = light;

        if self.state.exposure_in_flight.load(Ordering::Acquire) {
            return Err(ASCOMError::invalid_operation(
                "an exposure is already in flight",
            ));
        }

        let sensor = self.sensor()?;
        let (min_us, max_us) =
            (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        // Exact microseconds from the `Duration` itself: no float to round, and
        // a duration too long for the SDK's `i64` saturates rather than wrapping
        // into a short exposure.
        let exposure_us = i64::try_from(duration.as_micros()).unwrap_or(i64::MAX);
        if exposure_us < min_us || exposure_us > max_us {
            return Err(ASCOMError::invalid_value(format!(
                "exposure {exposure_us}us outside [{min_us}, {max_us}]"
            )));
        }

        let bin = u32::from(self.state.bin.load(Ordering::Acquire)).max(1);
        let roi = self.validated_geometry(&sensor, bin)?;

        // Claim the in-flight slot (lose the race → already exposing, E2) and
        // pin this frame's download format in ONE critical section against
        // `set_readout_mode` (RM1/RM2). Both halves must be atomic together:
        // reading the format outside the lock lets a mode change land between
        // the pin and the claim — or between the claim and the pin — leaving
        // a frame in one format while `ReadoutMode`/`MaxADU` describe the
        // other. Under the lock, a mode change either completes wholly before
        // the claim (and this exposure uses it) or observes the claim and is
        // rejected.
        let format = {
            let _guard = self.state.readout_mode_lock.lock();
            if self
                .state
                .exposure_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(ASCOMError::invalid_operation(
                    "an exposure is already in flight",
                ));
            }
            // Release the claim rather than wedging the device if the (already
            // validated, so defensive-only) format lookup fails.
            match self.selected_format() {
                Ok(format) => format,
                Err(e) => {
                    self.state
                        .exposure_in_flight
                        .store(false, Ordering::Release);
                    return Err(e);
                }
            }
        };
        let generation = self
            .state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;

        self.state.image_ready.store(false, Ordering::Release);
        self.state.aborted.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        *self.state.last_exposure_start_time.lock() = Some(SystemTime::now());
        *self.state.last_exposure_duration.lock() = Some(duration);

        let cancel = Arc::new(AtomicBool::new(false));
        *self.state.capture_cancel.lock() = Some(Arc::clone(&cancel));
        let request = CaptureRequest {
            start_x: roi.start_x,
            start_y: roi.start_y,
            width: roi.width,
            height: roi.height,
            bin,
            exposure_us,
            is_trigger_cam: sensor.is_trigger_cam,
            image_type: format.image_type,
            duration,
            cancel,
        };
        let handle = Arc::clone(&self.handle);
        let state = Arc::clone(&self.state);
        tokio::spawn(run_exposure(handle, state, generation, request));
        Ok(())
    }

    async fn abort_exposure(&self) -> ASCOMResult<()> {
        self.ensure_connected()?;
        self.cancel_exposure();
        Ok(())
    }

    async fn stop_exposure(&self) -> ASCOMResult<()> {
        // E8: no data-preserving stop exists at the SDK level, so this is
        // unconditionally NOT_IMPLEMENTED rather than pretending to
        // gracefully preserve data it cannot preserve — the opposite of
        // zwo-camera's graceful ASIStopExposure-backed stop.
        self.ensure_connected()?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn pulse_guide(&self, direction: GuideDirection, duration: Duration) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let sensor = self.sensor()?;
        if !sensor.supports_pulse_guide {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        // v0 design decision (documented in docs/services/svbony-camera.md's
        // Pulse guiding contract, PG2): unlike zwo-camera's asynchronous
        // ST4 wrapper (returns immediately, `IsPulseGuiding` tracks a
        // deadline), this call stays a literal blocking `SVBPulseGuide` —
        // `svbony_rs::Camera::pulse_guide` blocks at the SDK level for the
        // pulse duration, and no ST4-capable SVBony model has been
        // validated yet (the SV605CC has no ST4 port, so this whole branch
        // is unexercised by the simulation/BDD suite). If a future
        // ST4-capable model's guide pulses are long enough to risk
        // ConformU's ~1s response budget, revisit with the same
        // fire-and-forget-with-deadline pattern zwo-camera uses.
        let dir = guide_direction(direction);
        let duration_ms = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
        self.state.pulse_guiding.store(true, Ordering::Release);
        let result = self
            .on_handle(move |h| {
                h.pulse_guide(dir, duration_ms)
                    .map_err(|e| ASCOMError::invalid_operation(format!("pulse guide failed: {e}")))
            })
            .await;
        self.state.pulse_guiding.store(false, Ordering::Release);
        result
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::MockCameraHandle;
    use std::sync::atomic::Ordering as AtomicOrdering;

    fn roi(start_x: u32, start_y: u32, width: u32, height: u32) -> Roi {
        Roi {
            start_x,
            start_y,
            width,
            height,
        }
    }

    fn connected_device(handle: MockCameraHandle) -> SvbonyCamera {
        let device = SvbonyCamera::new(Arc::new(handle), None);
        device.connect().unwrap();
        device
    }

    async fn wait_image_ready(device: &SvbonyCamera) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !device.image_ready().await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("exposure did not complete");
    }

    async fn wait_camera_state(device: &SvbonyCamera, want: CameraState) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while device.camera_state().await.unwrap() != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("camera did not reach {want:?}"));
    }

    // --- pure helpers -------------------------------------------------------------

    /// The readout ladder is the driver's format contract (RM1/RM4): the
    /// two raw formats, highest precision first, each carrying the full
    /// scale of the data it delivers — not the ADC's `2^MaxBitDepth - 1`.
    #[test]
    fn readout_formats_are_the_two_raw_formats_highest_precision_first() {
        let listed: Vec<(&str, u32)> = READOUT_FORMATS
            .iter()
            .map(|f| (f.name, f.max_adu))
            .collect();
        assert_eq!(listed, vec![("Raw16", 65_535), ("Raw8", 255)]);
        assert_eq!(READOUT_FORMATS[0].image_type, ImageType::Raw16);
        assert_eq!(READOUT_FORMATS[1].image_type, ImageType::Raw8);
    }

    #[test]
    fn a_bin_change_rescales_a_client_set_zero_into_the_error_it_earned() {
        // The rescale arithmetic and its full case list live in
        // `rusty-photon-camera-core`; what this pins is that the two halves
        // are wired together — a 0 the client set survives the bin change and
        // `StartExposure` still answers about that 0, rather than about the %8
        // alignment rule a clamped 1 would trip instead.
        let scaled = camera_core::rescale(roi(0, 0, 0, 0), 1, 2);
        let err = check_geometry(scaled, 3008, 3008, 2).unwrap_err();
        assert!(err.message.contains("greater than 0"), "{}", err.message);
    }

    #[test]
    fn guide_direction_maps_every_ascom_direction() {
        assert_eq!(
            guide_direction(GuideDirection::North),
            svbony_rs::GuideDirection::North
        );
        assert_eq!(
            guide_direction(GuideDirection::South),
            svbony_rs::GuideDirection::South
        );
        assert_eq!(
            guide_direction(GuideDirection::East),
            svbony_rs::GuideDirection::East
        );
        assert_eq!(
            guide_direction(GuideDirection::West),
            svbony_rs::GuideDirection::West
        );
    }

    /// The offsets come from the shared crate, so these values are not a
    /// restatement of anything in this file — they pin the vendor mapping end
    /// to end. `Gr` and `Gb` are the pair worth having a test for: they differ
    /// by one letter and their offsets are transposes of each other.
    #[test]
    fn bayer_offset_mapping() {
        assert_eq!(bayer_offsets(BayerPattern::Rg), (0, 0));
        assert_eq!(bayer_offsets(BayerPattern::Bg), (1, 1));
        assert_eq!(bayer_offsets(BayerPattern::Gr), (1, 0));
        assert_eq!(bayer_offsets(BayerPattern::Gb), (0, 1));
    }

    #[test]
    fn geometry_applies_the_svbony_alignment_rule_and_reports_it_as_ascom() {
        // The rule set, its order, and the bounds arithmetic are the shared
        // crate's; this pins the two things that are this driver's — that the
        // %8/%2 rule is the one in force, and that a failure arrives as an
        // ASCOM message rather than a `GeometryError`.
        let err = check_geometry(roi(0, 0, 100, 64), 3008, 3008, 1).unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert!(err.message.contains("multiple of 8"), "{}", err.message);
        let err = check_geometry(roi(0, 0, 64, 47), 3008, 3008, 1).unwrap_err();
        assert!(err.message.contains("multiple of 2"), "{}", err.message);
        // A full frame at the reported (aligned) extent passes.
        check_geometry(roi(0, 0, 2976, 3000), 2976, 3000, 1).unwrap();
    }

    #[test]
    fn to_image_array_16bit_has_width_major_axes() {
        let bytes = vec![0u8; 64 * 48 * 2];
        let array = to_image_array(bytes, 64, 48, ImageType::Raw16).unwrap();
        // ASCOM [x][y]: first axis = width.
        assert_eq!(array.dim().0, 64);
        assert_eq!(array.dim().1, 48);
    }

    #[test]
    fn to_image_array_16bit_reads_the_wire_order() {
        // The camera puts 16-bit pixels on the wire low byte first, so `34 12`
        // is 0x1234. This pins the driver's route into the shared unpack; the
        // wire contract itself is pinned there.
        let mut bytes = vec![0u8; 64 * 48 * 2];
        bytes[0] = 0x34;
        bytes[1] = 0x12;
        let array = to_image_array(bytes, 64, 48, ImageType::Raw16).unwrap();
        assert_eq!(array[(0, 0, 0)], 0x1234_i32);
    }

    /// The Raw8 unpack reads one byte per pixel, so a 16-bit-sized buffer
    /// is not needed — the whole point of the fallback (RM2).
    #[test]
    fn to_image_array_8bit_has_width_major_axes_from_one_byte_per_pixel() {
        let bytes: Vec<u8> = (0..64 * 48).map(|i| (i % 251) as u8).collect();
        let expected = bytes[2 * 64 + 3];
        let array = to_image_array(bytes, 64, 48, ImageType::Raw8).unwrap();
        assert_eq!(array.dim().0, 64);
        assert_eq!(array.dim().1, 48);
        // Width-major: [x][y] reads back the row-major byte at (y, x).
        assert_eq!(array[(3, 2, 0)], i32::from(expected));
    }

    #[test]
    fn to_image_array_rejects_a_short_buffer_for_either_format() {
        for image_type in [ImageType::Raw16, ImageType::Raw8] {
            let err = to_image_array(vec![0u8; 10], 64, 48, image_type).unwrap_err();
            assert!(err.contains("buffer too small"), "{image_type:?}: {err}");
        }
    }

    #[test]
    fn to_image_array_names_an_unsupported_format_before_the_length() {
        // Each arm now owns its own length check, so a format the driver
        // cannot unpack is reported as such even when the buffer is also
        // short — the length it would be measured against is derived from
        // that same unusable format, so leading with it would misdirect.
        let err = to_image_array(vec![0u8; 10], 64, 48, ImageType::Rgb24).unwrap_err();
        assert!(err.contains("unsupported download format"), "{err}");
    }

    /// The device only ever selects a raw format (RM1/RM4), but the
    /// transform stays total: a format it cannot unpack is reported, not
    /// mis-read as one of the two it can.
    #[test]
    fn to_image_array_rejects_a_format_the_driver_never_selects() {
        let bytes = vec![0u8; 64 * 48 * 3];
        let err = to_image_array(bytes, 64, 48, ImageType::Rgb24).unwrap_err();
        assert!(err.contains("unsupported download format"), "{err}");
    }

    // --- connection lifecycle -------------------------------------------------------

    #[tokio::test]
    async fn starts_disconnected() {
        let cam = SvbonyCamera::new(Arc::new(MockCameraHandle::default()), None);
        assert!(!cam.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_caches_sensor_properties_from_the_handle() {
        let cam = connected_device(MockCameraHandle::default());
        // Aligned-down from the raw 3008x3008 so every binned full frame
        // (bins 1-4) satisfies the SDK's width%8 / height%2 rule.
        assert_eq!(cam.camera_x_size().await.unwrap(), 2976);
        assert_eq!(cam.camera_y_size().await.unwrap(), 3000);
        assert_eq!(cam.max_adu().await.unwrap(), 65_535);
    }

    #[tokio::test]
    async fn a_failed_open_leaves_the_device_disconnected() {
        let handle = MockCameraHandle::default();
        handle.fail_open.store(true, AtomicOrdering::SeqCst);
        let cam = SvbonyCamera::new(Arc::new(handle), None);
        cam.set_connected(true).await.unwrap_err();
        assert!(!cam.connected().await.unwrap());
    }

    #[tokio::test]
    async fn missing_exposure_control_fails_connect() {
        let handle = MockCameraHandle::default().without_control(ControlType::Exposure);
        let cam = SvbonyCamera::new(Arc::new(handle), None);
        cam.set_connected(true).await.unwrap_err();
        assert!(!cam.connected().await.unwrap());
    }

    // --- sensor properties (ST1/ST2/ST3) ---------------------------------------------

    #[tokio::test]
    async fn a_color_camera_reports_rggb() {
        let cam = connected_device(MockCameraHandle::default());
        assert_eq!(cam.sensor_type().await.unwrap(), SensorType::RGGB);
        assert_eq!(cam.bayer_offset_x().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_monochrome_camera_has_no_bayer_offset() {
        let cam = connected_device(MockCameraHandle::default().monochrome());
        assert_eq!(cam.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert_eq!(
            cam.bayer_offset_x().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    #[tokio::test]
    async fn electrons_per_adu_is_a_permanent_placeholder() {
        let cam = connected_device(MockCameraHandle::default());
        assert_eq!(
            cam.electrons_per_adu().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    // --- gain / offset (GO1/GO2/GO3) ---------------------------------------------------

    #[tokio::test]
    async fn gain_is_not_implemented_when_the_control_is_absent() {
        let cam = connected_device(MockCameraHandle::default().without_control(ControlType::Gain));
        assert_eq!(
            cam.gain().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    #[tokio::test]
    async fn set_gain_rejects_an_out_of_range_value() {
        let cam = connected_device(MockCameraHandle::default());
        let max = cam.gain_max().await.unwrap();
        assert_eq!(
            cam.set_gain(max + 1).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    #[tokio::test]
    async fn set_gain_round_trips_a_valid_value() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_gain(50).await.unwrap();
        assert_eq!(cam.gain().await.unwrap(), 50);
    }

    #[test]
    fn ascom_range_has_no_answer_for_bounds_outside_i32() {
        let cap = |min, max| ControlCaps {
            name: "Gain".to_string(),
            description: String::new(),
            control_type: ControlType::Gain,
            min,
            max,
            default: 0,
            is_writable: true,
            is_auto_supported: false,
        };
        assert_eq!(ascom_range(&cap(0, 720)), Some((0, 720)));
        assert_eq!(
            ascom_range(&cap(0, i64::from(i32::MAX) + 1)),
            None,
            "a max above i32 must not saturate into a bound the camera never offered"
        );
        assert_eq!(ascom_range(&cap(i64::from(i32::MIN) - 1, 0)), None);
    }

    #[tokio::test]
    async fn a_gain_range_outside_i32_leaves_the_control_unadvertised() {
        // Degrade rather than lie: a clamped bound would advertise a maximum the
        // camera then rejects.
        let cam = connected_device(MockCameraHandle::default().with_control_range(
            ControlType::Gain,
            0,
            i64::from(i32::MAX) + 1,
        ));
        for code in [
            cam.gain().await.unwrap_err().code,
            cam.gain_min().await.unwrap_err().code,
            cam.gain_max().await.unwrap_err().code,
            cam.set_gain(5).await.unwrap_err().code,
        ] {
            assert_eq!(code, ASCOMError::NOT_IMPLEMENTED.code);
        }
        // Offset is cached independently and is unaffected.
        cam.offset_max().await.unwrap();
    }

    // --- offset (the ASCOM Offset == SVBony BlackLevel control, GO1) ------------------

    #[tokio::test]
    async fn offset_is_not_implemented_when_the_control_is_absent() {
        let cam =
            connected_device(MockCameraHandle::default().without_control(ControlType::BlackLevel));
        assert_eq!(
            cam.offset().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
        assert_eq!(
            cam.offset_min().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
        assert_eq!(
            cam.offset_max().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
        assert_eq!(
            cam.set_offset(0).await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    #[tokio::test]
    async fn set_offset_rejects_an_out_of_range_value() {
        let cam = connected_device(MockCameraHandle::default());
        let max = cam.offset_max().await.unwrap();
        assert_eq!(
            cam.set_offset(max + 1).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    #[tokio::test]
    async fn set_offset_round_trips_a_valid_value() {
        let cam = connected_device(MockCameraHandle::default());
        assert_ne!(
            cam.offset().await.unwrap(),
            42,
            "picked a non-default value"
        );
        cam.set_offset(42).await.unwrap();
        assert_eq!(cam.offset().await.unwrap(), 42);
    }

    // --- readout mode -------------------------------------------------------------------

    /// RM1: the published list is the camera's own advertised formats,
    /// highest precision first, defaulting to index 0.
    #[tokio::test]
    async fn readout_modes_are_the_cameras_download_formats_best_first() {
        let cam = connected_device(MockCameraHandle::default());
        assert_eq!(cam.readout_modes().await.unwrap(), ["Raw16", "Raw8"]);
        assert_eq!(cam.readout_mode().await.unwrap(), 0);
        assert_eq!(cam.max_adu().await.unwrap(), 65_535);
    }

    /// The gap issue #882 filed: a camera that does not advertise Raw16
    /// must not be handed Raw16 anyway. It offers only the 8-bit mode, and
    /// `MaxADU` follows the format actually delivered (RM2).
    #[tokio::test]
    async fn a_camera_without_raw16_offers_only_the_8_bit_mode() {
        let cam =
            connected_device(MockCameraHandle::default().with_video_formats(vec![ImageType::Raw8]));
        assert_eq!(cam.readout_modes().await.unwrap(), ["Raw8"]);
        assert_eq!(cam.max_adu().await.unwrap(), 255);
        assert_eq!(
            cam.set_readout_mode(1).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    /// RM3: a camera offering neither raw format has nothing this driver's
    /// single-plane contract can describe, so connect fails rather than
    /// downloading a debayered frame.
    #[tokio::test]
    async fn connecting_fails_when_no_raw_download_format_is_advertised() {
        let cam = SvbonyCamera::new(
            Arc::new(
                MockCameraHandle::default()
                    .with_video_formats(vec![ImageType::Rgb24, ImageType::Rgb32]),
            ),
            None,
        );
        assert_eq!(
            cam.set_connected(true).await.unwrap_err().code,
            ASCOMError::NOT_CONNECTED.code
        );
        assert!(!cam.connected().await.unwrap());
    }

    #[tokio::test]
    async fn set_readout_mode_rejects_out_of_range_and_round_trips_valid() {
        let cam = connected_device(MockCameraHandle::default());
        let modes = cam.readout_modes().await.unwrap();
        assert_eq!(
            cam.set_readout_mode(modes.len()).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
        cam.set_readout_mode(modes.len() - 1).await.unwrap();
        assert_eq!(cam.readout_mode().await.unwrap(), modes.len() - 1);
    }

    /// `selected_format`'s defensive arm: every write validates the index
    /// and connect resets it, so this is unreachable through the ASCOM
    /// surface — it must still report rather than mis-describe a frame.
    #[tokio::test]
    async fn a_readout_index_past_the_list_is_reported_not_guessed() {
        let cam = connected_device(MockCameraHandle::default());
        cam.state.readout_mode.store(9, AtomicOrdering::Release);
        assert_eq!(
            cam.max_adu().await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    /// RM2: selecting the 8-bit mode is what the exposure downloads and
    /// what `MaxADU` describes — the two can never disagree.
    #[tokio::test]
    async fn selecting_the_8_bit_mode_drives_the_download_and_max_adu() {
        let handle = Arc::new(MockCameraHandle::default());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        cam.set_readout_mode(1).await.unwrap();
        assert_eq!(cam.max_adu().await.unwrap(), 255);

        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(48).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        assert_eq!(
            handle.last_capture_request().unwrap().image_type,
            ImageType::Raw8
        );
        let image = cam.image_array().await.unwrap();
        assert_eq!(image.dim().0, 64);
        assert_eq!(image.dim().1, 48);
    }

    /// A mode change mid-exposure would leave the delivered frame and the
    /// `MaxADU` describing it disagreeing, so it is refused (RM1).
    #[tokio::test]
    async fn changing_the_readout_mode_during_an_exposure_is_rejected() {
        let handle = Arc::new(MockCameraHandle::default());
        handle.set_capture_delay(Duration::from_secs(30));
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(48).await.unwrap();
        cam.start_exposure(Duration::from_secs(30), true)
            .await
            .unwrap();
        assert_eq!(
            cam.set_readout_mode(1).await.unwrap_err().code,
            ASCOMError::INVALID_OPERATION.code
        );
        cam.abort_exposure().await.unwrap();
    }

    // --- binning / ROI (B1-B3, R1-R3) --------------------------------------------------

    #[tokio::test]
    async fn set_bin_rejects_an_unsupported_value() {
        let cam = connected_device(MockCameraHandle::default());
        assert_eq!(
            cam.set_bin_x(99).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    #[tokio::test]
    async fn set_bin_rescales_the_cached_roi() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_start_x(100).await.unwrap();
        cam.set_num_x(800).await.unwrap();
        cam.set_bin_x(2).await.unwrap();
        assert_eq!(cam.start_x().await.unwrap(), 50);
        assert_eq!(cam.num_x().await.unwrap(), 400);
    }

    #[tokio::test]
    async fn roi_setters_accept_any_value_but_start_exposure_validates() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_num_x(5000).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.set_start_x(0).await.unwrap();
        cam.set_start_y(0).await.unwrap();
        assert_eq!(cam.num_x().await.unwrap(), 5000);
        let err = cam
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMError::INVALID_VALUE.code);
    }

    // --- cooling (K1-K5) ----------------------------------------------------------------

    #[tokio::test]
    async fn cooling_is_not_implemented_without_temp_control() {
        let cam = connected_device(MockCameraHandle::default().without_temp_control());
        assert!(!cam.can_set_ccd_temperature().await.unwrap());
        assert_eq!(
            cam.ccd_temperature().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
        assert_eq!(
            cam.cooler_on().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    #[tokio::test]
    async fn k5_connecting_never_enables_the_cooler() {
        // Tenet 3: connect must never actuate the cooler.
        let cam = connected_device(MockCameraHandle::default());
        assert!(!cam.cooler_on().await.unwrap());
    }

    #[tokio::test]
    async fn gain_is_settable_immediately_after_connect() {
        // GO5: the SDK refuses gain while its auto-exposure state is on,
        // which it is after every open; the connect handshake clears it, so
        // no exposure has to be taken first.
        let handle = Arc::new(MockCameraHandle::default());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert!(!handle.auto_exposure());
        cam.set_gain(50).await.unwrap();
        assert_eq!(cam.gain().await.unwrap(), 50);
    }

    #[tokio::test]
    async fn connect_restores_defaults_then_disables_auto_save_then_clears_auto_exposure() {
        // C1a, in `indi_svbony_ccd::Connect`'s order. The order matters:
        // the restore turns auto-exposure back on, so the manual exposure
        // write has to come after it.
        let handle = Arc::new(MockCameraHandle::default());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert_eq!(
            handle.sdk_call_log(),
            vec![
                "restore_default_param".to_string(),
                "set_auto_save_param(false)".to_string(),
                format!("set_control_value(Exposure, {CONNECT_EXPOSURE_US})"),
            ]
        );
    }

    #[tokio::test]
    async fn connect_clamps_the_auto_exposure_clearing_write_into_the_advertised_range() {
        // A camera whose exposure range does not reach 1 s still gets a
        // valid manual write (the value is immaterial; clearing the state
        // is the point).
        let handle = Arc::new(MockCameraHandle::default().with_control_range(
            ControlType::Exposure,
            32,
            500_000,
        ));
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert!(handle
            .sdk_call_log()
            .contains(&"set_control_value(Exposure, 500000)".to_string()));
        cam.set_gain(50).await.unwrap();
    }

    #[tokio::test]
    async fn connect_survives_a_failed_restore_default_param() {
        // The SDK reports a general error from SVBRestoreDefaultParam when
        // it cannot write `<model>_Cfg_A.bin` (read-only working
        // directory) even though the restore took effect — advisory, so
        // the connect completes and the rest of C1a still runs.
        let handle = Arc::new(MockCameraHandle::default());
        handle
            .fail_restore_default_param
            .store(true, Ordering::SeqCst);
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert!(cam.connected().await.unwrap());
        assert!(handle
            .sdk_call_log()
            .contains(&"set_auto_save_param(false)".to_string()));
        cam.set_gain(50).await.unwrap();
    }

    #[tokio::test]
    async fn connect_survives_a_failed_set_auto_save_param() {
        let handle = Arc::new(MockCameraHandle::default());
        handle
            .fail_set_auto_save_param
            .store(true, Ordering::SeqCst);
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert!(cam.connected().await.unwrap());
        cam.set_gain(50).await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_runs_the_post_open_handshake_again() {
        // A reopen puts the SDK back into its auto-exposure-on default, so
        // the handshake must clear it every time, not once per process.
        let handle = Arc::new(MockCameraHandle::default());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        cam.disconnect().unwrap();
        cam.connect().unwrap();
        assert!(!handle.auto_exposure());
        assert_eq!(
            handle
                .sdk_call_log()
                .iter()
                .filter(|c| c.as_str() == "restore_default_param")
                .count(),
            2
        );
        cam.set_gain(50).await.unwrap();
    }

    #[tokio::test]
    async fn connecting_a_trigger_camera_arms_video_capture_exactly_once() {
        // Trigger-gated capture produces no frames until an operator's soft
        // trigger, so arming it once at connect is not actuation (tenet 3).
        let handle = Arc::new(MockCameraHandle::default());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert_eq!(handle.start_video_capture_call_count(), 1);
    }

    #[tokio::test]
    async fn concurrent_connect_requests_arm_video_capture_exactly_once() {
        // Two `Connected=true` requests racing (ConformU's protocol fuzz
        // produces exactly this): `handle.open()`'s atomic check-and-open
        // lets exactly one of them own the handshake — the other no-ops
        // instead of running a second handshake whose video-capture arm
        // fails with the SDK's "video mode active". The open delay holds
        // the winner inside the open critical section so the two
        // genuinely overlap.
        let handle = Arc::new(MockCameraHandle::default());
        handle.set_open_delay(Duration::from_millis(200));
        let cam = SvbonyCamera::new(handle.clone(), None);
        let racer = cam.clone();
        let (first, second) = tokio::join!(cam.set_connected(true), async move {
            // Let the first transition park inside `open()` before the
            // duplicate arrives, so the duplicate cannot win by ordering.
            tokio::time::sleep(Duration::from_millis(50)).await;
            racer.set_connected(true).await
        });
        first.unwrap();
        second.unwrap();
        assert!(cam.connected().await.unwrap());
        assert_eq!(handle.start_video_capture_call_count(), 1);
    }

    #[tokio::test]
    async fn connecting_a_non_trigger_camera_never_starts_video_capture() {
        // Tenet 3: a non-trigger camera's only mode is free-running, so
        // starting video capture at connect would begin the sensor
        // continuously integrating/streaming with no operator action.
        // Capture must stay unarmed until the operator's first StartExposure.
        let handle = Arc::new(MockCameraHandle::default().without_trigger_cam());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        assert_eq!(handle.start_video_capture_call_count(), 0);
        assert_eq!(handle.stop_video_capture_call_count(), 0);
    }

    #[tokio::test]
    async fn set_ccd_temperature_rejects_out_of_range() {
        let cam = connected_device(MockCameraHandle::default());
        assert_eq!(
            cam.set_set_ccd_temperature(-300.0).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
        assert_eq!(
            cam.set_set_ccd_temperature(100.0).await.unwrap_err().code,
            ASCOMError::INVALID_VALUE.code
        );
    }

    #[tokio::test]
    async fn set_ccd_temperature_round_trips() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_set_ccd_temperature(-10.0).await.unwrap();
        let readback = cam.set_ccd_temperature().await.unwrap();
        assert!((readback - (-10.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn turning_the_cooler_on_is_reflected() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_cooler_on(true).await.unwrap();
        assert!(cam.cooler_on().await.unwrap());
    }

    #[tokio::test]
    async fn ccd_temperature_and_cooler_power_read_the_live_sensor() {
        let cam = connected_device(MockCameraHandle::default());
        assert!(cam.can_get_cooler_power().await.unwrap());
        // The mock's default CurrentTemperature/CoolerPower controls (K4).
        assert!((cam.ccd_temperature().await.unwrap() - 20.0).abs() < 1e-9);
        assert!((cam.cooler_power().await.unwrap() - 0.0).abs() < 1e-9);
        cam.set_cooler_on(true).await.unwrap();
        assert!((cam.cooler_power().await.unwrap() - 60.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn cooler_power_is_not_implemented_without_temp_control() {
        let cam = connected_device(MockCameraHandle::default().without_temp_control());
        assert!(!cam.can_get_cooler_power().await.unwrap());
        assert_eq!(
            cam.cooler_power().await.unwrap_err().code,
            ASCOMError::NOT_IMPLEMENTED.code
        );
    }

    // --- exposure state machine (E1-E9) --------------------------------------------------

    #[tokio::test]
    async fn start_exposure_fails_when_disconnected() {
        let cam = SvbonyCamera::new(Arc::new(MockCameraHandle::default()), None);
        let err = cam
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
    }

    #[tokio::test]
    async fn a_second_exposure_while_one_is_in_flight_is_rejected() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_millis(200));
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        let err = cam
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMError::INVALID_OPERATION.code);
    }

    #[tokio::test]
    async fn out_of_range_duration_is_rejected() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        let err = cam
            .start_exposure(Duration::from_secs(2500), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMError::INVALID_VALUE.code);
    }

    #[tokio::test]
    async fn a_successful_exposure_produces_an_image() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(48).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        let image = cam.image_array().await.unwrap();
        assert_eq!(image.dim().0, 64);
        assert_eq!(image.dim().1, 48);
        cam.last_exposure_start_time().await.unwrap();
        assert_eq!(
            cam.last_exposure_duration().await.unwrap(),
            Duration::from_millis(10)
        );
    }

    #[tokio::test]
    async fn a_dark_frame_captures_identically_to_a_light_frame() {
        let cam = connected_device(MockCameraHandle::default());
        assert!(!cam.has_shutter().await.unwrap());
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(48).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), false)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        assert!(cam.image_ready().await.unwrap());
    }

    #[tokio::test]
    async fn percent_completed_is_100_once_ready() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        wait_camera_state(&cam, CameraState::Idle).await;
        assert_eq!(cam.percent_completed().await.unwrap(), 100);
    }

    #[tokio::test]
    async fn aborting_an_in_flight_exposure_leaves_no_image_ready() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_millis(300));
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        assert!(cam.can_abort_exposure().await.unwrap());
        cam.start_exposure(Duration::from_secs(30), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        cam.abort_exposure().await.unwrap();
        assert!(!cam.image_ready().await.unwrap());
        // CameraState/PercentCompleted must reflect the abort immediately,
        // not only once the still-running, un-interruptible capture task
        // happens to drain (the whole injected 300ms capture_delay is still
        // in flight here).
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Idle);
        assert_eq!(cam.percent_completed().await.unwrap(), 0);
        // The late-completing capture task must not resurrect ImageReady or
        // an Error state once it eventually drains (the generation-counter
        // guard, E7).
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!cam.image_ready().await.unwrap());
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Idle);
    }

    /// After an abort the capture task drains within ~one poll slice (the
    /// cancel-flag bail-out), so a new exposure is accepted promptly — not
    /// only after the aborted exposure's full `exposure*2+500ms` deadline —
    /// keeping `CameraState = Idle` and "`StartExposure` accepted" consistent.
    #[tokio::test]
    async fn a_new_exposure_is_accepted_promptly_after_an_abort() {
        let handle = Arc::new(MockCameraHandle::default());
        handle.set_capture_delay(Duration::from_secs(30));
        let cam = SvbonyCamera::new(Arc::<MockCameraHandle>::clone(&handle), None);
        cam.connect().unwrap();
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_secs(30), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        cam.abort_exposure().await.unwrap();
        handle.set_capture_delay(Duration::ZERO);
        let accepted_after = std::time::Instant::now();
        loop {
            match cam.start_exposure(Duration::from_millis(10), true).await {
                Ok(()) => break,
                Err(_) if accepted_after.elapsed() < Duration::from_secs(5) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!(
                    "new exposure still rejected after {:?}: {e}",
                    accepted_after.elapsed()
                ),
            }
        }
        wait_image_ready(&cam).await;
    }

    /// Every SDK-error mapping carries the SDK's own error detail in the
    /// ASCOM error message — a bare error code is undiagnosable from a
    /// client or the service log (a real-hardware gain-write transient
    /// motivated this contract).
    #[tokio::test]
    async fn sdk_control_failures_surface_the_sdk_detail() {
        let handle = Arc::new(MockCameraHandle::default().with_pulse_guide());
        let cam = SvbonyCamera::new(Arc::<MockCameraHandle>::clone(&handle), None);
        cam.connect().unwrap();
        handle.fail_controls.store(true, AtomicOrdering::SeqCst);
        let cases: [(ASCOMError, &str); 11] = [
            (cam.gain().await.unwrap_err(), "failed to read gain"),
            (cam.set_gain(50).await.unwrap_err(), "failed to set gain"),
            (cam.offset().await.unwrap_err(), "failed to read offset"),
            (cam.set_offset(5).await.unwrap_err(), "failed to set offset"),
            (
                cam.ccd_temperature().await.unwrap_err(),
                "failed to read sensor temperature",
            ),
            (
                cam.set_ccd_temperature().await.unwrap_err(),
                "failed to read target temperature",
            ),
            (
                cam.set_set_ccd_temperature(0.0).await.unwrap_err(),
                "failed to set target temperature",
            ),
            (
                cam.cooler_on().await.unwrap_err(),
                "failed to read cooler state",
            ),
            (
                cam.set_cooler_on(true).await.unwrap_err(),
                "failed to set cooler state",
            ),
            (
                cam.cooler_power().await.unwrap_err(),
                "failed to read cooler power",
            ),
            (
                cam.pulse_guide(GuideDirection::North, Duration::from_millis(1))
                    .await
                    .unwrap_err(),
                "pulse guide failed",
            ),
        ];
        for (err, needle) in cases {
            let msg = err.to_string();
            assert!(msg.contains(needle), "{msg:?} missing {needle:?}");
            assert!(
                msg.contains("injected SDK failure"),
                "{msg:?} lost the SDK detail"
            );
        }
    }

    /// A handshake-step SDK failure leaves the device disconnected (C2's
    /// handshake half) — the camera must not be left opened-but-unusable.
    #[tokio::test]
    async fn a_failed_handshake_step_leaves_the_device_disconnected() {
        let handle = Arc::new(MockCameraHandle::default());
        handle.fail_property.store(true, AtomicOrdering::SeqCst);
        let cam = SvbonyCamera::new(Arc::<MockCameraHandle>::clone(&handle), None);
        let err = cam.connect().unwrap_err();
        assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
        assert!(!handle.is_open(), "failed handshake must close the camera");
    }

    /// Reconnecting while a previous session's capture-cancel slot is still
    /// populated cancels that capture (`reset_exposure_state`'s take-branch).
    #[tokio::test]
    async fn reconnecting_cancels_a_previous_sessions_capture_slot() {
        let cam = connected_device(MockCameraHandle::default());
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        // The completed exposure leaves its cancel flag in the slot (the
        // disconnect's cancel is a no-op with nothing in flight); the
        // reconnect must drain (take + set) it rather than leak it.
        cam.disconnect().unwrap();
        cam.connect().unwrap();
        assert!(!cam.image_ready().await.unwrap(), "reconnect resets state");
    }

    #[tokio::test]
    async fn aborting_with_no_exposure_in_flight_is_a_no_op() {
        let cam = connected_device(MockCameraHandle::default());
        cam.abort_exposure().await.unwrap();
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Idle);
        assert!(!cam.image_ready().await.unwrap());
    }

    #[tokio::test]
    async fn there_is_no_data_preserving_stop() {
        let cam = connected_device(MockCameraHandle::default());
        assert!(!cam.can_stop_exposure().await.unwrap());
        let err = cam.stop_exposure().await.unwrap_err();
        assert_eq!(err.code, ASCOMError::NOT_IMPLEMENTED.code);
    }

    /// E9: a mid-exposure SDK failure transitions to the Error state — the
    /// design doc explicitly reserves this contract for a mock-backend unit
    /// test since the `svbony-rs` simulation cannot force an SDK error.
    #[tokio::test]
    async fn e9_mid_exposure_sdk_failure_sets_the_error_state() {
        let handle = MockCameraHandle::default();
        handle.fail_capture.store(true, AtomicOrdering::SeqCst);
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_camera_state(&cam, CameraState::Error).await;
        assert!(!cam.image_ready().await.unwrap());
        assert!(cam.image_array().await.is_err());
    }

    /// E9's other branch: an exceeded `SVBGetVideoData` deadline is the same
    /// Error-state transition, distinguished only by the recorded message.
    #[tokio::test]
    async fn e9_exceeded_deadline_sets_the_error_state() {
        let handle = MockCameraHandle::default();
        handle.exceed_deadline.store(true, AtomicOrdering::SeqCst);
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_camera_state(&cam, CameraState::Error).await;
        assert!(!cam.image_ready().await.unwrap());
    }

    /// State-machine step 5: a non-trigger-capable camera still completes an
    /// exposure (via the backend's free-running restart fallback), and
    /// `camera.rs` correctly threads `sensor.is_trigger_cam = false` through
    /// to the capture request.
    #[tokio::test]
    async fn a_non_trigger_camera_still_completes_an_exposure() {
        let handle = Arc::new(MockCameraHandle::default().without_trigger_cam());
        let cam = SvbonyCamera::new(handle.clone(), None);
        cam.connect().unwrap();
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&cam).await;
        assert!(cam.image_ready().await.unwrap());
        let req = handle
            .last_capture_request()
            .expect("capture should have run");
        assert!(!req.is_trigger_cam);
    }

    // --- pulse guide (PG1/PG2) --------------------------------------------------------

    #[tokio::test]
    async fn pulse_guide_is_not_implemented_without_st4() {
        let cam = connected_device(MockCameraHandle::default());
        assert!(!cam.can_pulse_guide().await.unwrap());
        let err = cam
            .pulse_guide(GuideDirection::North, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMError::NOT_IMPLEMENTED.code);
    }

    #[tokio::test]
    async fn pulse_guide_succeeds_on_an_st4_capable_model() {
        let cam = connected_device(MockCameraHandle::default().with_pulse_guide());
        assert!(cam.can_pulse_guide().await.unwrap());
        cam.pulse_guide(GuideDirection::North, Duration::from_millis(5))
            .await
            .unwrap();
        // The blocking call has already returned by the time `pulse_guide`
        // resolves (v0 keeps it synchronous — see the doc comment).
        assert!(!cam.is_pulse_guiding().await.unwrap());
    }

    // --- disconnect cancels an in-flight exposure (C3b) --------------------------------

    #[tokio::test]
    async fn disconnecting_cancels_an_in_flight_exposure() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_millis(300));
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();
        cam.start_exposure(Duration::from_secs(30), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        cam.set_connected(false).await.unwrap();
        assert!(!cam.image_ready().await.unwrap());
        assert!(!cam.connected().await.unwrap());
    }

    // --- name / description overrides -----------------------------------------------

    #[tokio::test]
    async fn unique_id_and_name_come_from_the_handle() {
        let cam = SvbonyCamera::new(Arc::new(MockCameraHandle::default()), None);
        assert_eq!(cam.unique_id(), "SVBONY:SV605CC-Simulated:SVB0123456789AB");
        assert_eq!(cam.static_name(), "SV605CC-Simulated");
    }

    #[tokio::test]
    async fn a_name_override_wins_over_the_sdk_default() {
        let overrides = DeviceOverride {
            name: Some("Main Imaging".to_string()),
            description: None,
        };
        let cam = SvbonyCamera::new(Arc::new(MockCameraHandle::default()), Some(&overrides));
        assert_eq!(cam.static_name(), "Main Imaging");
    }
}

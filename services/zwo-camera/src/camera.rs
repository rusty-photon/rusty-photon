//! `ZwoCamera` — the ASCOM `Device` + `Camera` implementation over the
//! [`CameraHandle`](crate::backend::CameraHandle) seam.
//!
//! Behaviour follows `docs/services/zwo-camera.md`, with these deliberate
//! divergences from the `qhy-camera` precedent (all driven by the ASI SDK):
//! - **Dark frames are accepted** on every model — ASI sensors are shutterless
//!   (`HasShutter = false`), so `Light = false` captures identically (E4).
//! - **`StopExposure` works**: a graceful, data-preserving stop (`CanStopExposure
//!   = true`); abort and stop both drive `ASIStopExposure`, abort discarding the
//!   frame and stop preserving it (E7/E8).
//! - **Native asynchronous `PulseGuide`** via ST4 (`CanPulseGuide = true` when
//!   the model has an ST4 port): the call starts the pulse and returns
//!   immediately, with `IsPulseGuiding` true until the pulse's deadline (PG1/PG2).
//! - **`ElectronsPerADU`** is a real native value (`ASI_CAMERA_INFO.ElecPerADU`),
//!   read live because the SDK scales it by the gain register (ST2).
//! - **`MaxADU` is a saturation threshold, not `2^BitDepth − 1`** — the ADC
//!   scale shifted into the delivered container, one quantization step below
//!   full scale so a sensor that clips short still trips `pixel >= MaxADU`
//!   (ST3). 65535 for a 16-bit sensor, where there is no shift to step down
//!   from.
//!
//! Blocking capture SDK calls run on `spawn_blocking` inside a detached task; a
//! generation counter lets abort/disconnect invalidate a late-completing task.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, GuideDirection, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use parking_lot::Mutex;
use rusty_photon_camera_core::{self as camera_core, Alignment, PixelDepth, Roi};
use tracing::{debug, warn};
use zwo_rs::{BayerPattern, CameraInfo, ControlCaps, ControlType, ImageType};

use crate::backend::{CameraHandle, CaptureRequest, StopSignal};
use crate::config::{DeviceOverride, MaxAduReporting};
use crate::config_actions::ZwoCameraDriver;
use rusty_photon_driver::ConfigActionCtx;

/// 0x500 — driver-specific catch-all for an asynchronous capture failure
/// surfaced lazily via `image_array` (E9).
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

/// ASI exposure control is in microseconds, so the smallest step is 1 µs.
const EXPOSURE_RESOLUTION: Duration = Duration::from_micros(1);

/// One selectable download format: what `ASISetROIFormat` is told to produce
/// and the name it is published under in ASCOM's `ReadoutModes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadoutFormat {
    image_type: ImageType,
    name: &'static str,
}

/// Every download format this driver can deliver, **in preference order**.
/// These are intersected with the camera's advertised
/// `ASI_CAMERA_INFO.SupportedVideoFormat` and the survivors become
/// `ReadoutModes`, so index 0 is the highest precision the camera offers (RM1).
///
/// Only the raw formats are eligible. `RGB24` is SDK-debayered 8-bit-per-channel
/// output: selecting it would discard the raw sensor data and change this
/// device's *contract* (`SensorType::Color`, no `BayerOffset`, a rank-3
/// `ImageArray`) rather than just its buffer arithmetic. `Y8` is a mono
/// luminance format, redundant with `Raw8` on a mono sensor and wrong on a
/// Bayer one while we report `RGGB`.
const READOUT_FORMATS: [ReadoutFormat; 2] = [
    ReadoutFormat {
        image_type: ImageType::Raw16,
        name: "Raw16",
    },
    ReadoutFormat {
        image_type: ImageType::Raw8,
        name: "Raw8",
    },
];

/// The download formats this camera can actually deliver — [`READOUT_FORMATS`]
/// filtered by what it advertises. Empty means the camera offers no raw format
/// at all, which this driver's single-plane `ImageArray` contract cannot
/// describe; connect refuses rather than downloading something else (RM3).
///
/// The advertised list is the camera's claim, not proof. It was measured on a
/// physical ASI120MC-S — the model INDI singles out for unreliable 16-bit —
/// which advertises `[Raw8, Rgb24, Y8, Raw16]` *and* delivers `Raw16` correctly
/// at 320×240, full-frame 1280×960, and bin 2. INDI's blanket
/// `strstr(name, "ASI120")` guard would force that working camera to 8-bit, so
/// it is deliberately not ported: enumeration is the whole selection rule here.
fn negotiated_formats(info: &CameraInfo) -> Vec<ReadoutFormat> {
    READOUT_FORMATS
        .into_iter()
        .filter(|f| info.supported_video_formats.contains(&f.image_type))
        .collect()
}

/// The ASI sub-frame alignment rule (R3): a binned width that is a multiple of
/// 8 and a height that is a multiple of 2.
///
/// This is the *whole* difference between this driver's geometry and
/// `qhy-camera`'s, which passes `None` — everything else about validating a ROI
/// is shared, and lives in `rusty-photon-camera-core`.
const ALIGNMENT: Option<Alignment> = Some(Alignment::new(
    NonZeroU32::new(8).expect("8 is not zero"),
    NonZeroU32::new(2).expect("2 is not zero"),
));

/// Per-device runtime state: caches populated at connect plus the exposure state
/// machine. Atomics for the hot/simple flags; `parking_lot::Mutex` for the
/// `Option<…>` caches and the captured image. Locks are never held across an
/// `await`.
#[derive(Debug)]
struct DeviceState {
    /// Current symmetric bin (init 1).
    bin: AtomicU8,
    /// Current readout-mode index into [`ZwoCamera::readout_formats`], reset to
    /// 0 (the camera's highest-precision format) on every connect.
    readout_mode: AtomicU8,
    /// Intended ROI in *binned* pixel coordinates (rescaled on bin change).
    intended_roi: Mutex<Option<Roi>>,
    /// `(min, max)` exposure microseconds from `ASIGetControlCaps(ASI_EXPOSURE)`.
    exposure_range_us: Mutex<Option<(i64, i64)>>,
    /// Gain range in ASCOM's own width, converted once at the open handshake
    /// (see [`ascom_range`]). `None` means the control is not advertised —
    /// either the model lacks it, or its range has no `i32` spelling.
    gain_min_max: Mutex<Option<(i32, i32)>>,
    /// Offset range, on the same terms as [`DeviceState::gain_min_max`].
    offset_min_max: Mutex<Option<(i32, i32)>>,
    /// Whether the camera advertises an `ASI_TEMPERATURE` control (cached at the
    /// open handshake). Decoupled from cooling: most ASI cameras — cooled or not —
    /// expose a readable sensor temperature, so `CCDTemperature` is reported
    /// whenever this is set, while the cooler-setpoint members stay gated on
    /// [`CameraInfo::is_cooler_cam`].
    temperature_available: Mutex<bool>,
    target_temperature: Mutex<Option<f64>>,

    /// The in-flight capture's own stop cell ([`CaptureRequest::stop`]) and,
    /// because `Some` here *is* the in-flight claim, the single answer to
    /// "does a capture own this device?" ([`DeviceState::exposure_in_flight`]).
    ///
    /// Installed by `start_exposure`, set by `cancel_exposure`/`stop_exposure`,
    /// and taken by whichever of the capture task's drain or a reconnect's
    /// `reset_exposure_state` gets there first. Because the cell belongs to one
    /// capture, a new exposure cannot erase an older capture's abort the way a
    /// handle-wide signal let it (see [`StopSignal`]).
    ///
    /// Claim and cell are deliberately one piece of state rather than an
    /// `AtomicBool` beside an `Option`. Held apart they can disagree for as
    /// long as it takes `start_exposure` to install the cell after taking the
    /// claim, and a stop arriving in that window finds a device that reports
    /// itself exposing and nothing to signal.
    ///
    /// **Lock order:** innermost. It is taken under
    /// [`Self::readout_mode_lock`] (`start_exposure`, `set_readout_mode`) and
    /// under [`Self::result_lock`] (`cancel_exposure`, `reset_exposure_state`),
    /// never in the other direction — and no lock at all is acquired while it
    /// is held, which is what makes those two pairs the whole of the order.
    in_flight_capture: Mutex<Option<Arc<StopSignal>>>,
    image_ready: AtomicBool,
    /// Bumped on each start / abort / disconnect so a late-completing capture
    /// task can tell it has been superseded and discard its result.
    exposure_generation: AtomicU64,
    last_exposure_start_time: Mutex<Option<SystemTime>>,
    last_exposure_duration: Mutex<Option<Duration>>,
    last_image: Mutex<Option<ImageArray>>,
    /// Set on a mid-exposure SDK failure → `CameraState::Error` (E9).
    last_error: Mutex<Option<String>>,
    /// Serializes the capture task's "check generation + commit result" against
    /// `cancel_exposure`'s "bump generation + clear `image_ready`".
    ///
    /// **Lock order:** this one first, then [`Self::in_flight_capture`]
    /// (`cancel_exposure`, `reset_exposure_state`) — never the reverse.
    result_lock: Mutex<()>,
    /// Serializes `set_readout_mode`'s "reject if exposing, else store" against
    /// `start_exposure`'s "pin the download format, then claim the device", so
    /// a frame is never captured in one format while `ReadoutMode` and `MaxADU`
    /// report another (RM1).
    ///
    /// **Lock order:** this one first, then [`Self::in_flight_capture`] — never
    /// the reverse. Both critical sections consult the claim (`set_readout_mode`
    /// reads it, `start_exposure` installs it), and `in_flight_capture` is a
    /// leaf, so that pair is the only ordering this lock takes part in. Callers
    /// do acquire other locks *before* this one (`start_exposure` reads
    /// `intended_roi` via `validated_geometry`), but those are released by
    /// then.
    readout_mode_lock: Mutex<()>,
    /// Deadline of an in-flight ST4 guide pulse (asynchronous `PulseGuide`);
    /// `None` when not guiding. `IsPulseGuiding` is `now < deadline` (PG1/PG2).
    pulse_guide_until: Mutex<Option<SystemTime>>,
}

impl DeviceState {
    const fn new() -> Self {
        Self {
            bin: AtomicU8::new(1),
            readout_mode: AtomicU8::new(0),
            intended_roi: Mutex::new(None),
            exposure_range_us: Mutex::new(None),
            gain_min_max: Mutex::new(None),
            offset_min_max: Mutex::new(None),
            temperature_available: Mutex::new(false),
            target_temperature: Mutex::new(None),
            in_flight_capture: Mutex::new(None),
            image_ready: AtomicBool::new(false),
            exposure_generation: AtomicU64::new(0),
            last_exposure_start_time: Mutex::new(None),
            last_exposure_duration: Mutex::new(None),
            last_image: Mutex::new(None),
            last_error: Mutex::new(None),
            result_lock: Mutex::new(()),
            readout_mode_lock: Mutex::new(()),
            pulse_guide_until: Mutex::new(None),
        }
    }

    /// Reset the exposure state machine to a clean idle state. Called on connect
    /// so a stale `Error` / `ImageReady` / image from a previous session does not
    /// survive a reconnect (C3).
    fn reset_exposure_state(&self) {
        let _guard = self.result_lock.lock();
        self.exposure_generation.fetch_add(1, Ordering::AcqRel);
        // Taking the cell *is* releasing the in-flight claim, handing the
        // device to the next exposure while a capture from the previous session
        // may still be draining — so abort that capture on the way out: it owns
        // its own stop cell, which the next `StartExposure` therefore cannot
        // reset.
        let stop = self.in_flight_capture.lock().take();
        if let Some(stop) = stop {
            stop.request(false);
        }
        self.image_ready.store(false, Ordering::Release);
        *self.last_image.lock() = None;
        *self.last_error.lock() = None;
        *self.last_exposure_start_time.lock() = None;
        *self.last_exposure_duration.lock() = None;
        *self.pulse_guide_until.lock() = None;
    }

    /// Whether a capture currently owns the device.
    ///
    /// One lock read of [`DeviceState::in_flight_capture`], which holds the
    /// claim and the capture's stop cell as a single fact. A `parking_lot` lock
    /// rather than an atomic load: uncontended it costs tens of nanoseconds,
    /// which is nothing at `CameraState` polling rates, and it buys a claim
    /// that cannot disagree with the capture it stands for.
    fn exposure_in_flight(&self) -> bool {
        self.in_flight_capture.lock().is_some()
    }
}

/// One ASCOM Camera device per discovered ASI camera.
#[derive(Clone, derive_more::Debug)]
pub struct ZwoCamera {
    #[debug(skip)]
    handle: Arc<dyn CameraHandle>,
    info: CameraInfo,
    /// The camera's usable download formats, negotiated once at construction —
    /// unlike `svbony-camera`, ASI hands back the full `CameraInfo` (formats
    /// included) at enumeration, so this needs no open camera.
    readout_formats: Vec<ReadoutFormat>,
    unique_id: String,
    name: String,
    description: String,
    state: Arc<DeviceState>,
    /// Which `MaxADU` contract to present (ST3). Service-wide, so it arrives
    /// here rather than through the per-serial `DeviceOverride`.
    max_adu_reporting: MaxAduReporting,
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<ZwoCameraDriver>>,
}

impl ZwoCamera {
    /// Build a device from an SDK handle, an optional per-serial config
    /// override, and the service-wide `MaxADU` contract. The ASCOM `UniqueID`
    /// is the handle's serial-derived id; `name`/`description` fall back to
    /// SDK-derived defaults.
    ///
    /// `max_adu_reporting` is a parameter rather than a builder step so the
    /// compiler, not a test, guarantees the caller wires it from config.
    pub fn new(
        handle: Arc<dyn CameraHandle>,
        overrides: Option<&DeviceOverride>,
        max_adu_reporting: MaxAduReporting,
    ) -> Self {
        let info = handle.info();
        let unique_id = handle.unique_id();
        let name = overrides
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| info.name.clone());
        let description = overrides
            .and_then(|o| o.description.clone())
            .unwrap_or_else(|| format!("ZWO ASI camera ({})", info.name));
        let readout_formats = negotiated_formats(&info);
        Self {
            handle,
            info,
            readout_formats,
            unique_id,
            name,
            description,
            state: Arc::new(DeviceState::new()),
            max_adu_reporting,
            config_ctx: None,
        }
    }

    /// Attach config-action wiring (enables `config.get`/`apply`/`schema`).
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<ZwoCameraDriver>) -> Self {
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

    /// The download format the current `ReadoutMode` selects (RM2). The index is
    /// validated on every write and reset at connect, so the out-of-range arm is
    /// defensive only.
    fn selected_format(&self) -> ASCOMResult<ReadoutFormat> {
        let index = usize::from(self.state.readout_mode.load(Ordering::Acquire));
        self.readout_formats
            .get(index)
            .copied()
            .ok_or_else(|| ASCOMError::invalid_value("readout mode index out of range"))
    }

    fn connect(&self) -> ASCOMResult<()> {
        // `set_connected`'s is_open check and this transition are not one
        // atomic step, so two concurrent connects can both get here. That is
        // safe without further guarding: `handle.open()` is check-then-open
        // under the handle's own lock (a redundant open is a no-op), and
        // `open_handshake` only reads SDK state and (re)writes the same
        // locally-cached values — nothing on this path is non-idempotent,
        // unlike svbony-camera's trigger-arm handshake, which must instead
        // gate its handshake on winning the atomic open.
        self.handle.open().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // A failed post-open handshake must leave the device disconnected (C2),
        // not opened-but-unusable, so close before propagating.
        if let Err(e) = self.open_handshake() {
            if let Err(close_err) = self.handle.close() {
                debug!(error = %close_err, "close after a failed connect handshake also failed");
            }
            return Err(e);
        }
        // A reconnect must not surface a previous session's Error / ImageReady /
        // stale frame (C3).
        self.state.reset_exposure_state();
        debug!(camera = %self.unique_id, "camera connected");
        Ok(())
    }

    /// Read and cache the camera's control ranges after `open()`. The exposure
    /// control is required; gain/offset are cached only when present (GO1). Also
    /// resets the ROI to the full frame at bin 1.
    fn open_handshake(&self) -> ASCOMResult<()> {
        // RM3: a camera advertising no raw format has nothing this driver's
        // single-plane ImageArray contract can describe. Fail loudly rather
        // than download a debayered RGB24 frame we would then misreport.
        if self.readout_formats.is_empty() {
            warn!(
                advertised = ?self.info.supported_video_formats,
                "camera advertises no downloadable raw format (Raw16 or Raw8)"
            );
            return Err(ASCOMError::NOT_CONNECTED);
        }

        let caps = self
            .handle
            .control_caps()
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        let find = |ct: ControlType| caps.iter().find(|c| c.control_type == ct);

        let exposure = find(ControlType::Exposure).ok_or_else(|| {
            warn!("camera does not advertise an exposure control");
            ASCOMError::NOT_CONNECTED
        })?;
        *self.state.exposure_range_us.lock() = Some((exposure.min, exposure.max));

        *self.state.gain_min_max.lock() = find(ControlType::Gain).and_then(ascom_range);
        *self.state.offset_min_max.lock() = find(ControlType::Offset).and_then(ascom_range);
        // `CCDTemperature` is reported whenever the sensor-temperature control is
        // present — independent of cooling (an uncooled ASI still reads its sensor
        // temperature). The cooler-setpoint members remain gated on `is_cooler_cam`.
        *self.state.temperature_available.lock() = find(ControlType::Temperature).is_some();

        self.state.bin.store(1, Ordering::Release);
        self.state.readout_mode.store(0, Ordering::Release);
        let (width, height) = self.reported_sensor();
        *self.state.intended_roi.lock() = Some(Roi {
            start_x: 0,
            start_y: 0,
            width,
            height,
        });
        *self.state.target_temperature.lock() = None;
        Ok(())
    }

    fn disconnect(&self) -> ASCOMResult<()> {
        // An in-flight exposure is cancelled (C3) before the handle closes.
        self.cancel_exposure();
        self.handle.close().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        debug!(camera = %self.unique_id, "camera disconnected");
        Ok(())
    }

    /// Cancel any in-flight exposure (abort): bump the generation so the capture
    /// task discards its result, clear `image_ready`/`last_error`, and set that
    /// capture's stop cell so it stops at the SDK and drains. Deliberately does
    /// NOT release the in-flight claim — the capture task takes the cell once
    /// its blocking SDK chain drains, so a new exposure cannot race the
    /// still-running one (the design's "one owner per device").
    fn cancel_exposure(&self) {
        // Atomic with the capture task's commit so an abort can never be
        // overwritten by a just-completing capture. Held across the cell read
        // as well, so the generation bump below and the cell describe the *same*
        // capture: a capture commits under this lock before its drain releases
        // the claim, so without it the read could hand back a capture that then
        // finishes and is replaced, leaving the bump to discard the successor's
        // frame while the stop goes to a capture that is already gone.
        let _guard = self.state.result_lock.lock();
        // One read answers both "is a capture in flight?" and "what do I
        // signal?", because they are the same fact. No ordering of this against
        // `start_exposure` leaves the device claimed with nothing to stop.
        let stop = self.state.in_flight_capture.lock().clone();
        let Some(stop) = stop else {
            return;
        };
        self.state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel);
        self.state.image_ready.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        // The cell stays installed: the aborted capture still owns the device
        // until it drains, and the drain is what releases the claim.
        stop.request(false);
    }

    /// Reported `CameraXSize`/`CameraYSize` (R4): the sensor extents reduced so
    /// a full frame at every supported bin is a valid ASI ROI.
    ///
    /// Both come from one call, taking the same [`ALIGNMENT`] that validates
    /// ROIs — so a sensor sized for one rule can never be reported while ROIs
    /// are checked against another.
    fn reported_sensor(&self) -> (u32, u32) {
        camera_core::aligned_sensor(
            self.info.max_width,
            self.info.max_height,
            &self.info.supported_bins,
            ALIGNMENT,
        )
    }

    /// Validate the cached ROI against the binned sensor geometry (R2/R3),
    /// returning the [`CaptureRequest`] geometry to push to the SDK.
    fn validated_geometry(&self, bin: u32) -> ASCOMResult<Roi> {
        let roi = (*self.state.intended_roi.lock())
            .ok_or_else(|| ASCOMError::invalid_value("no ROI defined for camera"))?;
        let (sensor_w, sensor_h) = self.reported_sensor();
        check_geometry(roi, sensor_w, sensor_h, bin)?;
        Ok(roi)
    }

    fn gain_available(&self) -> bool {
        self.state.gain_min_max.lock().is_some()
    }

    fn offset_available(&self) -> bool {
        self.state.offset_min_max.lock().is_some()
    }

    /// Run a blocking SDK-seam call off the async executor. The ASI FFI calls
    /// (`control_value`, `set_control_value`, `temperature_celsius`, …) do USB
    /// I/O, so running them directly on a Tokio worker could stall other Alpaca
    /// requests; offload them like the capture, connect and pulse-guide paths.
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

/// Geometry validation (R2/R3), as the ASCOM error a client sees.
///
/// The rules, their order, and the message text all live in
/// `rusty-photon-camera-core`, shared with `qhy-camera` and `svbony-camera`,
/// as does the ASCOM code it becomes. What this driver contributes is the ASI
/// [`ALIGNMENT`] rule.
fn check_geometry(roi: Roi, sensor_w: u32, sensor_h: u32, bin: u32) -> ASCOMResult<()> {
    Ok(camera_core::check(roi, sensor_w, sensor_h, bin, ALIGNMENT)?)
}

/// A control's range as ASCOM must describe it, or `None` when the driver
/// reports bounds outside `i32`.
///
/// ASCOM's `Gain`/`GainMin`/`GainMax` (and the offset trio) are `i32` while the
/// ASI SDK reports control caps as `long`. Converting here rather than at each
/// read asks the "does it fit?" question once, at the handshake, where leaving
/// the control unadvertised is a meaningful answer — a clamped bound would
/// advertise a maximum the camera then rejects.
fn ascom_range(caps: &ControlCaps) -> Option<(i32, i32)> {
    Some((i32::try_from(caps.min).ok()?, i32::try_from(caps.max).ok()?))
}

/// `MaxADU` for a frame delivered in `image_type` from a `bit_depth`-bit ADC.
///
/// Under [`MaxAduReporting::SaturationThreshold`] (the default) this is a
/// **saturation threshold chosen to be reachable**, not an exact upper bound on
/// the pixel values. *In the shifted branch* — a sub-16-bit depth in Raw16,
/// where the margin applies — a sensor that reaches its top ADC code delivers
/// pixels one quantization step above it. Every other branch returns its own
/// container's maximum, which nothing delivered in that container can exceed:
/// `u16::MAX` for the remaining Raw16 depths, and `u8::MAX` for Raw8.
///
/// [`MaxAduReporting::ContainerFullScale`] reports that container maximum in
/// every Raw16 branch instead, matching ZWO's own ASCOM driver for clients
/// written against it — at the cost of the detection described below. The two
/// modes differ only in the shifted branch; everywhere else they already agree.
///
/// "Reachable" is a design intent, not a guarantee: a sensor clipping *two* or
/// more counts short would still never satisfy `pixel >= MaxADU`, and no
/// formula over `BitDepth` can rule that out on a model nobody has measured.
/// See the margin discussion below for why the trade is deliberate.
///
/// **Not `2^BitDepth - 1`.** ASI packs sub-16-bit ADC data into the Raw16
/// container by *left-shifting* it, so the ceiling belongs to the container,
/// not the ADC. Measured on a physical 12-bit ASI120MC-S: every
/// pixel's low 4 bits are zero and a saturated full frame tops out at exactly
/// `4095 << 4 = 65520` — sixteen times the 4095 this driver used to report, so
/// a client normalising by `MaxADU` mis-scaled everything above 1/16 of range.
/// (`svbony-camera` reached the same conclusion on its SV605CC, by rescale
/// rather than shift; ST3 there.)
///
/// **Nor is it the shifted full scale.** A sensor need not reach its top ADC
/// code: a physical 14-bit ASI178MM clips at `16382 << 2 = 65528`, one count
/// short of the `16383 << 2 = 65532` the shift alone predicts, at every gain
/// and bin. ASCOM defines this property as the maximum value the camera *can
/// produce*, and clients test saturation as `pixel >= MaxADU`, so reporting an
/// unreachable ceiling does not merely round badly — it makes saturation
/// undetectable, which is the whole point of the property for autofocus star
/// selection and flat-panel exposure targeting.
///
/// So the shifted branch reports one quantization step below full scale. The
/// shortfall is per-model — of three cameras driven to saturation, the
/// ASI178MM stops at `16382 << 2` and the ASI1600MM-Cool at `4094 << 4`, while
/// the ASI120MC-S does reach `4095 << 4`, so two identical-depth sensors
/// disagree and no formula over `BitDepth` is exact on all of them.
///
/// The margin is the measured ceiling on the two that clip. On the one that
/// does not, it calls pixels one ADC LSB early saturated: measured at 3 pixels
/// in a 1,228,800-pixel frame, against 6,095 correctly flagged. Either error
/// is *below the sensor's own resolution*, since a shifted container cannot
/// represent anything finer — but without the margin the other two report zero
/// saturated pixels on a fully saturated frame. That asymmetry is the
/// justification: understating costs a fraction of one code, overstating costs
/// the entire capability.
///
/// The margin is spent only where the shift creates it, for two distinct
/// reasons. A 16-bit ADC fills the container, so there is no shift to step
/// down from. An unknown (0) or degenerate (1) depth says nothing about the
/// packing at all, so there is no step size to step down by — and at depth 1
/// the formula would yield 0, which would make any client normalising by
/// `MaxADU` divide by zero. All of them report the container's own ceiling.
/// Raw8 delivers 8-bit data whatever the ADC is and was measured reaching
/// exactly 255 on every camera tried, so it takes no margin either.
fn max_adu_for(image_type: ImageType, bit_depth: u32, reporting: MaxAduReporting) -> u32 {
    let container_full_scale = u32::from(u16::MAX);
    match image_type {
        ImageType::Raw8 | ImageType::Y8 => u32::from(u8::MAX),
        // The compatibility contract: the container's own maximum, which is
        // what ZWO's own ASCOM driver reports. Deliberately ahead of the
        // depth arms below — it is the whole point of the mode that it
        // overrides the shift, and the arms it does not reach already agree
        // with it.
        _ if reporting == MaxAduReporting::ContainerFullScale => container_full_scale,
        // No margin, for two different reasons: a depth that already fills the
        // container has no shift and so no slack to spend, while an unknown (0)
        // or degenerate (1) depth says nothing about the packing at all — there
        // is no step size to step down by.
        _ if bit_depth <= 1 || bit_depth >= 16 => container_full_scale,
        // Reached only for `bit_depth` in 2..=15 (the arm above), where every
        // step below is exact: `1 << 15` fits, `- 2` cannot underflow from 4 or
        // more, and the final shift is by at most 14. The saturating spellings
        // are therefore identical here, not a clamp that changes the answer —
        // and this is a per-query call, not a per-pixel one.
        _ => 1u32
            .checked_shl(bit_depth)
            .unwrap_or(container_full_scale)
            .saturating_sub(2)
            .checked_shl(16u32.saturating_sub(bit_depth))
            .unwrap_or(container_full_scale),
    }
}

/// Bayer pattern → ASCOM `BayerOffsetX/Y`.
///
/// `ASI_BAYER_RG` and friends abbreviate the quad to its first row, so the
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

/// Map an ASCOM guide direction onto the `zwo-rs` one.
const fn guide_direction(direction: GuideDirection) -> zwo_rs::GuideDirection {
    match direction {
        GuideDirection::North => zwo_rs::GuideDirection::North,
        GuideDirection::South => zwo_rs::GuideDirection::South,
        GuideDirection::East => zwo_rs::GuideDirection::East,
        GuideDirection::West => zwo_rs::GuideDirection::West,
    }
}

/// Convert a single-plane frame into an ASCOM `ImageArray` with `[x][y]` axis
/// order (ASCOM stores width-major), unpacking per the format the frame was
/// downloaded in (RM2). Only the raw formats [`READOUT_FORMATS`] can select are
/// convertible; anything else is a caller error, reported rather than
/// mis-unpacked.
///
/// The unpack itself is `rusty-photon-camera-core`'s, shared with
/// `qhy-camera` and `svbony-camera`; this driver's share is the format→depth
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

/// The detached capture task: runs the blocking single-frame SDK chain *and* the
/// CPU-heavy frame transform on `spawn_blocking`, then stores the image (or
/// records a failure as the `Error` state) — unless a newer generation has
/// superseded it.
///
/// Both the SDK download and [`to_image_array`] run inside the one
/// `spawn_blocking` closure on purpose. A full-frame transform is a ~26-megapixel
/// `u16`→`i32` widen+transpose; in an unoptimised (debug/CI) build that is several
/// hundred milliseconds. Running it inline on a Tokio worker — and worse, while
/// holding `result_lock` — pins a worker thread (and contends `cancel_exposure`,
/// which also takes `result_lock`). Offloading it — exactly as the SDK seam calls
/// are (see [`ZwoCamera::on_handle`]) — keeps every Tokio worker free for HTTP,
/// and `result_lock` is then held only for the cheap commit below.
async fn run_exposure(
    handle: Arc<dyn CameraHandle>,
    state: Arc<DeviceState>,
    generation: u64,
    request: CaptureRequest,
) {
    let blocking_handle = Arc::clone(&handle);
    let (width, height, image_type) = (request.width, request.height, request.image_type);
    let stop = Arc::clone(&request.stop);
    let result = tokio::task::spawn_blocking(move || {
        blocking_handle
            .capture(request)
            .map(|frame| frame.map(|bytes| to_image_array(bytes, width, height, image_type)))
    })
    .await;

    {
        // No await is held across the lock (the blocking await is above), so this
        // "check generation + record" is atomic against cancel_exposure. Only the
        // cheap commit runs here — the transform already happened off-thread.
        let _guard = state.result_lock.lock();
        if state.exposure_generation.load(Ordering::Acquire) == generation {
            match result {
                Ok(Ok(Some(Ok(array)))) => {
                    *state.last_image.lock() = Some(array);
                    *state.last_error.lock() = None;
                    state.image_ready.store(true, Ordering::Release);
                }
                Ok(Ok(Some(Err(e)))) => {
                    warn!(error = %e, "failed to transform captured image");
                    *state.last_image.lock() = None;
                    *state.last_error.lock() = Some(format!("image transform failed: {e}"));
                }
                // Aborted: discard the frame, leave no Error state (E7).
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    warn!(error = %e.0, "mid-exposure SDK error");
                    *state.last_error.lock() = Some(e.0);
                }
                Err(join_err) => {
                    warn!(error = %join_err, "exposure task panicked");
                    *state.last_error.lock() = Some(format!("exposure task failed: {join_err}"));
                }
            }
        }
    }
    // Release the device only if this capture still owns it — taking the cell
    // *is* the release. A reconnect (`reset_exposure_state`) hands ownership on
    // while a superseded capture is still draining, so taking it unconditionally
    // here would declare a *newer*, genuinely running exposure finished —
    // letting a third one start alongside it.
    let mut slot = state.in_flight_capture.lock();
    if slot
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &stop))
    {
        *slot = None;
    }
}

#[async_trait::async_trait]
impl Device for ZwoCamera {
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
        if self.handle.is_open() == connected {
            return Ok(());
        }
        // `connect`/`disconnect` do blocking SDK I/O — `ASIOpenCamera` enumerates
        // over USB and the handshake reads `control_caps` — so offload them off
        // the executor (ZwoCamera is cheap to clone: it is `Arc`-backed).
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
        Ok("rusty-photon zwo-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<ZwoCameraDriver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait::async_trait]
impl Camera for ZwoCamera {
    // --- geometry ---------------------------------------------------------------

    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        let (width, _) = self.reported_sensor();
        Ok(width)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        let (_, height) = self.reported_sensor();
        Ok(height)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        Ok(self.info.pixel_size_um)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        // ASI exposes a single pixel size, so X == Y trivially.
        self.ensure_connected()?;
        Ok(self.info.pixel_size_um)
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        // ST3/RM2: the ceiling belongs to the delivered format, so it tracks
        // the selected readout mode as well as the ADC depth.
        self.ensure_connected()?;
        Ok(max_adu_for(
            self.selected_format()?.image_type,
            self.info.bit_depth,
            self.max_adu_reporting,
        ))
    }

    async fn electrons_per_adu(&self) -> ASCOMResult<f64> {
        // A ZWO win: a real native value, not the NOT_IMPLEMENTED placeholder.
        // Read live, never from the cached `CameraInfo`: the SDK scales this
        // field by the gain register, so a snapshot would freeze the value at
        // whatever gain the camera held at enumeration (ST2).
        self.ensure_connected()?;
        self.on_handle(|h| {
            h.electrons_per_adu()
                .map(f64::from)
                .map_err(|_| ASCOMError::INVALID_OPERATION)
        })
        .await
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        self.ensure_connected()?;
        Ok(self.info.name.clone())
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
        if !self.info.supported_bins.contains(&u32::from(bin_x)) {
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
        self.info
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
        let area = (*roi).ok_or(ASCOMError::INVALID_VALUE)?;
        *roi = Some(Roi {
            width: num_x,
            ..area
        });
        drop(roi);
        Ok(())
    }

    async fn set_num_y(&self, num_y: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        let area = (*roi).ok_or(ASCOMError::INVALID_VALUE)?;
        *roi = Some(Roi {
            height: num_y,
            ..area
        });
        drop(roi);
        Ok(())
    }

    async fn set_start_x(&self, start_x: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        let area = (*roi).ok_or(ASCOMError::INVALID_VALUE)?;
        *roi = Some(Roi { start_x, ..area });
        drop(roi);
        Ok(())
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        let area = (*roi).ok_or(ASCOMError::INVALID_VALUE)?;
        *roi = Some(Roi { start_y, ..area });
        drop(roi);
        Ok(())
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

    // --- gain / offset ----------------------------------------------------------

    async fn gain(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        if !self.gain_available() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::Gain)
                .map_err(|_| ASCOMError::INVALID_OPERATION)?;
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
                .map_err(|_| ASCOMError::INVALID_OPERATION)
        })
        .await
    }

    async fn offset(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        if !self.offset_available() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::Offset)
                .map_err(|_| ASCOMError::INVALID_OPERATION)?;
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
            h.set_control_value(ControlType::Offset, i64::from(offset))
                .map_err(|_| ASCOMError::INVALID_OPERATION)
        })
        .await
    }

    // --- readout modes ----------------------------------------------------------

    async fn readout_mode(&self) -> ASCOMResult<usize> {
        self.ensure_connected()?;
        Ok(usize::from(self.state.readout_mode.load(Ordering::Acquire)))
    }

    async fn readout_modes(&self) -> ASCOMResult<Vec<String>> {
        // RM1: the camera's own download formats, negotiated at construction.
        self.ensure_connected()?;
        Ok(self
            .readout_formats
            .iter()
            .map(|f| f.name.to_string())
            .collect())
    }

    async fn set_readout_mode(&self, readout_mode: usize) -> ASCOMResult<()> {
        self.ensure_connected()?;
        // The mode selects the download format *and* the MaxADU describing it
        // (RM2), and an in-flight capture already carries the format it was
        // started with — so switching mid-exposure could only produce a frame
        // and a MaxADU that disagree. Validating and storing under
        // `readout_mode_lock` makes that exclusion hold against a
        // concurrently-starting exposure too, not just an already-running one.
        let _guard = self.state.readout_mode_lock.lock();
        let available = self.readout_formats.len();
        if readout_mode >= available {
            return Err(ASCOMError::invalid_value(format!(
                "readout mode {readout_mode} out of range (0..{available})"
            )));
        }
        if self.state.exposure_in_flight() {
            return Err(ASCOMError::invalid_operation(
                "cannot change the readout mode while an exposure is in flight",
            ));
        }
        // Lock order: `readout_mode_lock` (held) then `in_flight_capture` (taken
        // and released by the claim read above) — the same direction
        // `start_exposure` takes them, and the only pair either lock is in.
        //
        // Bounded by the `available` check above, which is itself a `usize`
        // length, so this narrowing has an answer for every index that got here.
        self.state.readout_mode.store(
            u8::try_from(readout_mode).unwrap_or(u8::MAX),
            Ordering::Release,
        );
        Ok(())
    }

    // --- sensor type / bayer ----------------------------------------------------

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        self.ensure_connected()?;
        Ok(if self.info.is_color {
            SensorType::RGGB
        } else {
            SensorType::Monochrome
        })
    }

    async fn bayer_offset_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        if !self.info.is_color {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        Ok(bayer_offsets(self.info.bayer_pattern).0)
    }

    async fn bayer_offset_y(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        if !self.info.is_color {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        Ok(bayer_offsets(self.info.bayer_pattern).1)
    }

    // --- cooling ----------------------------------------------------------------

    async fn can_set_ccd_temperature(&self) -> ASCOMResult<bool> {
        Ok(self.info.is_cooler_cam)
    }

    async fn can_get_cooler_power(&self) -> ASCOMResult<bool> {
        Ok(self.info.is_cooler_cam)
    }

    async fn ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        // Decoupled from cooling: report the sensor temperature whenever the
        // camera advertises the `ASI_TEMPERATURE` control (cached at the open
        // handshake), cooled or not. A camera without it is genuinely
        // `NOT_IMPLEMENTED`.
        if !*self.state.temperature_available.lock() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            h.temperature_celsius().map_err(|_| {
                ASCOMError::new(UNSPECIFIED_ERROR, "failed to read sensor temperature")
            })
        })
        .await
    }

    async fn set_ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if !self.info.is_cooler_cam {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let stored_target = *self.state.target_temperature.lock();
        if let Some(target) = stored_target {
            return Ok(target);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::TargetTemp)
                .map_err(|_| ASCOMError::INVALID_VALUE)?;
            // A temperature outside `i32` is not a temperature; say so rather
            // than widen it lossily.
            i32::try_from(raw).map(f64::from).map_err(|_| {
                ASCOMError::invalid_value(format!("camera reported target temperature {raw}"))
            })
        })
        .await
    }

    async fn set_set_ccd_temperature(&self, set_ccd_temperature: f64) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if !self.info.is_cooler_cam {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if !(-273.15..=80.0).contains(&set_ccd_temperature) {
            return Err(ASCOMError::invalid_value(format!(
                "target temperature {set_ccd_temperature} outside [-273.15, 80]"
            )));
        }
        // Validated to [-273.15, 80] immediately above, so the rounded value is
        // in [-273, 80] — a range `i64` holds with room to spare. No
        // `TryFrom<f64>` exists to spell that, and a fallible form would add an
        // arm the check above already makes unreachable.
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "bounded by the range check above; no TryFrom<f64> for i64 exists"
        )]
        let raw = set_ccd_temperature.round() as i64;
        self.on_handle(move |h| {
            h.set_control_value(ControlType::TargetTemp, raw)
                .map_err(|_| ASCOMError::invalid_operation("failed to set target temperature"))
        })
        .await?;
        *self.state.target_temperature.lock() = Some(set_ccd_temperature);
        Ok(())
    }

    async fn cooler_on(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        if !self.info.is_cooler_cam {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            h.control_value(ControlType::CoolerOn)
                .map(|v| v != 0)
                .map_err(|_| ASCOMError::INVALID_VALUE)
        })
        .await
    }

    async fn set_cooler_on(&self, cooler_on: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if !self.info.is_cooler_cam {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(move |h| {
            h.set_control_value(ControlType::CoolerOn, i64::from(cooler_on))
                .map_err(|_| ASCOMError::invalid_operation("failed to set cooler state"))
        })
        .await
    }

    async fn cooler_power(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if !self.info.is_cooler_cam {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.on_handle(|h| {
            let raw = h
                .control_value(ControlType::CoolerPowerPerc)
                .map_err(|_| ASCOMError::INVALID_VALUE)?;
            i32::try_from(raw).map(f64::from).map_err(|_| {
                ASCOMError::invalid_value(format!("camera reported cooler power {raw}"))
            })
        })
        .await
    }

    // --- shutter / capability flags ---------------------------------------------

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        // ASI sensors are shutterless; darks/bias differ only in client metadata.
        Ok(self.info.has_mechanical_shutter)
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        // ASIStopExposure is a graceful, data-preserving stop (a ZWO win).
        Ok(true)
    }

    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        Ok(self.info.has_st4_port)
    }

    async fn is_pulse_guiding(&self) -> ASCOMResult<bool> {
        // Asynchronous: `pulse_guide` returns immediately and records a deadline;
        // the pulse is in progress until that deadline passes (PG2).
        Ok((*self.state.pulse_guide_until.lock())
            .is_some_and(|deadline| SystemTime::now() < deadline))
    }

    // --- exposure state ---------------------------------------------------------

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        if self.state.last_error.lock().is_some() {
            return Ok(CameraState::Error);
        }
        if self.state.exposure_in_flight() {
            return Ok(CameraState::Exposing);
        }
        Ok(CameraState::Idle)
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        Ok(self.state.image_ready.load(Ordering::Acquire) && !self.state.exposure_in_flight())
    }

    async fn percent_completed(&self) -> ASCOMResult<u8> {
        if !self.state.exposure_in_flight() {
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
        let last_error = self.state.last_error.lock().clone();
        if let Some(msg) = last_error {
            return Err(ASCOMError::new(UNSPECIFIED_ERROR, msg));
        }
        // ASCOM: `ImageArray` is valid only once `ImageReady` is true. Mirror the
        // `image_ready()` condition so a client can never read a stale frame.
        let ready =
            self.state.image_ready.load(Ordering::Acquire) && !self.state.exposure_in_flight();
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

    // --- exposure control -------------------------------------------------------

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if self.state.exposure_in_flight() {
            return Err(ASCOMError::invalid_operation(
                "an exposure is already in flight",
            ));
        }

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
        let roi = self.validated_geometry(bin)?;

        // Pin this frame's download format and claim the device in ONE critical
        // section against `set_readout_mode` (RM1/RM2): reading the format
        // outside the lock lets a mode change land either side of the claim,
        // leaving a frame in one format while `ReadoutMode`/`MaxADU` describe
        // the other.
        //
        // Installing this capture's own stop cell *is* the claim (lose the race
        // → already exposing, E2), so there is no interval in which the device
        // counts as exposing while an abort would find nothing to signal. The
        // generation bump and the flag an abort also writes ride in the same
        // critical section, so a concurrent `cancel_exposure` lands wholly
        // before this exposure exists — and is the no-op it should be — or
        // wholly after it, with full effect. The cell being this capture's own
        // is what keeps it from erasing an abort aimed at an earlier one.
        let (format, stop, generation) = {
            let _readout_guard = self.state.readout_mode_lock.lock();
            // Ordered before the claim so a failed lookup (already validated,
            // so defensive-only) simply never claims the device, rather than
            // having to hand back a claim it took.
            let format = self.selected_format()?;
            let mut slot = self.state.in_flight_capture.lock();
            if slot.is_some() {
                return Err(ASCOMError::invalid_operation(
                    "an exposure is already in flight",
                ));
            }
            let generation = self
                .state
                .exposure_generation
                .fetch_add(1, Ordering::AcqRel)
                + 1;
            self.state.image_ready.store(false, Ordering::Release);
            let stop = Arc::new(StopSignal::new());
            *slot = Some(Arc::clone(&stop));
            // The claim is taken and the cell is installed: everything an
            // abort needs is in place, so the critical section ends here.
            drop(slot);
            (format, stop, generation)
        };

        *self.state.last_error.lock() = None;
        *self.state.last_exposure_start_time.lock() = Some(SystemTime::now());
        *self.state.last_exposure_duration.lock() = Some(duration);

        let request = CaptureRequest {
            width: roi.width,
            height: roi.height,
            bin,
            start_x: roi.start_x,
            start_y: roi.start_y,
            exposure_us,
            image_type: format.image_type,
            duration,
            is_dark: !light,
            stop,
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
        self.ensure_connected()?;
        // Graceful, data-preserving stop: signal the capture to keep the frame.
        // Does NOT bump the generation, so the preserved frame is committed (E8).
        // One read of the cell is both the in-flight test and the signal, and
        // because it hands back that capture's own cell rather than a flag, a
        // stop can only ever reach the capture that was in flight when it was
        // issued — never a successor that started in between. No `result_lock`
        // here: with nothing to bump, there is no second fact to keep in step.
        let stop = self.state.in_flight_capture.lock().clone();
        if let Some(stop) = stop {
            stop.request(true);
        }
        Ok(())
    }

    async fn pulse_guide(&self, direction: GuideDirection, duration: Duration) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if !self.info.has_st4_port {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let dir = guide_direction(direction);

        // ASCOM `PulseGuide` is asynchronous: start the pulse now (so a failed
        // start is reported to the caller) and return immediately, leaving a
        // detached task to end it after `duration`. Blocking it for the whole
        // pulse would exceed ConformU's 1 s response target and stall an
        // autoguider. `IsPulseGuiding` is true until the recorded deadline.
        let on_handle = Arc::clone(&self.handle);
        tokio::task::spawn_blocking(move || on_handle.pulse_guide_on(dir))
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("pulse guide task failed: {e}")))?
            .map_err(|e| ASCOMError::invalid_operation(format!("pulse guide failed: {e}")))?;

        // A guide pulse is milliseconds long, so this cannot overflow the clock;
        // falling back to `now` would simply report the pulse as already done.
        let now = SystemTime::now();
        *self.state.pulse_guide_until.lock() = Some(now.checked_add(duration).unwrap_or(now));

        let off_handle = Arc::clone(&self.handle);
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            match tokio::task::spawn_blocking(move || off_handle.pulse_guide_off(dir)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => debug!(error = %e, "ending the ST4 guide pulse failed"),
                Err(e) => debug!(error = %e, "pulse-guide stop task failed to join"),
            }
            *state.pulse_guide_until.lock() = None;
        });
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::{CaptureOutcome, MockCameraHandle};

    fn roi(start_x: u32, start_y: u32, width: u32, height: u32) -> Roi {
        Roi {
            start_x,
            start_y,
            width,
            height,
        }
    }

    fn connected_device(handle: MockCameraHandle) -> ZwoCamera {
        let device = ZwoCamera::new(Arc::new(handle), None, MaxAduReporting::default());
        device.connect().unwrap();
        device
    }

    // Deadline-bounded polls (no fixed nap count): `tokio::time::timeout` caps
    // the wait in real time, so a contended runtime can't turn a fixed iteration
    // count into an unbounded wall-clock wait. The 30 s cap is sized for
    // heavily-loaded CI runners (spawn_blocking + mock capture threads can
    // stall for seconds under contention); the loop returns the moment the
    // condition holds, so healthy runs never feel the headroom.
    async fn wait_image_ready(device: &ZwoCamera) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !device.image_ready().await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("exposure did not complete");
    }

    /// Deadline-bounded wait for the mock to have entered `count` captures.
    async fn wait_captures_started(handle: &MockCameraHandle, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while handle.capture_outcomes().len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("only {} captures started", handle.capture_outcomes().len()));
    }

    /// Deadline-bounded wait for `count` captures to have *returned*, outcome
    /// recorded. A superseded capture returns on its own blocking thread, which
    /// on a contended runner lands well after the exposure that replaced it has
    /// published its frame — so "the frame is ready" does not imply "the capture
    /// it superseded has finished".
    async fn wait_captures_finished(handle: &MockCameraHandle, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while handle
                .capture_outcomes()
                .iter()
                .filter(|outcome| outcome.is_some())
                .count()
                < count
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("captures did not finish: {:?}", handle.capture_outcomes()));
    }

    async fn wait_camera_state(device: &ZwoCamera, want: CameraState) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while device.camera_state().await.unwrap() != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("camera did not reach {want:?}"));
    }

    // --- pure helpers -----------------------------------------------------------

    /// The threshold belongs to the delivered format, not the ADC — and it sits
    /// one quantization step below that format's full scale.
    ///
    /// **These values are deliberately not the shifted full scale**, so do not
    /// "correct" 65504 to 65520 or 65528 to 65532: that would undo the margin
    /// and silently restore the defect it exists to prevent. A 12-bit ADC
    /// left-shifted into Raw16 tops out at 65520 on the ASI120MC-S but at
    /// 65504 on the ASI1600MM-Cool, and a 14-bit one at 65528 on the ASI178MM —
    /// so the top code is not reachable on every sensor, and reporting it makes
    /// `pixel >= MaxADU` impossible to satisfy on the ones that clip short.
    #[test]
    fn max_adu_is_one_step_below_the_delivered_format_full_scale() {
        let threshold = MaxAduReporting::SaturationThreshold;
        // One quantization step below the shifted full scale, so a sensor that
        // clips short of its top ADC code still trips `pixel >= MaxADU`.
        assert_eq!(max_adu_for(ImageType::Raw16, 12, threshold), 65_504);
        assert_eq!(max_adu_for(ImageType::Raw16, 14, threshold), 65_528);
        // Raw8 delivers 8-bit data whatever the ADC is, and was measured
        // reaching 255, so it takes no margin.
        assert_eq!(max_adu_for(ImageType::Raw8, 12, threshold), 255);
        assert_eq!(max_adu_for(ImageType::Raw8, 16, threshold), 255);
    }

    /// The compatibility mode reports the container's own maximum in Raw16
    /// whatever the ADC depth — the flat 65535 measured from ZWO's own ASCOM
    /// driver on all three cameras.
    #[test]
    fn container_full_scale_reports_the_container_maximum_at_every_depth() {
        let compat = MaxAduReporting::ContainerFullScale;
        // The depths that carry a margin under the default lose it here: this
        // is the whole point of the mode, not an oversight.
        assert_eq!(max_adu_for(ImageType::Raw16, 12, compat), 65_535);
        assert_eq!(max_adu_for(ImageType::Raw16, 14, compat), 65_535);
        // Raw8's container is 8 bits wide, so its maximum is still 255 — the
        // mode reports the *container's* ceiling, not a fixed 65535.
        assert_eq!(max_adu_for(ImageType::Raw8, 12, compat), 255);
    }

    /// The two modes differ only where the shift creates a margin. Stated as a
    /// test so that a future depth arm cannot silently diverge in a branch
    /// where both readings are supposed to agree.
    #[test]
    fn the_modes_agree_wherever_no_margin_is_spent() {
        for (image_type, depth) in [
            (ImageType::Raw16, 16), // fills the container
            (ImageType::Raw16, 0),  // unknown depth
            (ImageType::Raw16, 1),  // degenerate depth
            (ImageType::Raw8, 12),  // 8-bit container
            (ImageType::Y8, 14),    // ditto
        ] {
            assert_eq!(
                max_adu_for(image_type, depth, MaxAduReporting::SaturationThreshold),
                max_adu_for(image_type, depth, MaxAduReporting::ContainerFullScale),
                "{image_type:?} at depth {depth} must not depend on the mode"
            );
        }
    }

    /// The compatibility mode reproduces ZWO's defect on purpose: on a sensor
    /// that clips short, no delivered pixel can satisfy `pixel >= MaxADU`.
    /// Measured through ZWO's own driver, a blown-out frame reports zero
    /// saturated pixels on all three cameras.
    #[test]
    fn container_full_scale_puts_the_measured_ceilings_out_of_reach() {
        for (depth, delivered_ceiling) in [(14u32, 16_382u32 << 2), (12, 4_094 << 4)] {
            let max_adu = max_adu_for(ImageType::Raw16, depth, MaxAduReporting::ContainerFullScale);
            assert!(
                delivered_ceiling < max_adu,
                "compat mode is expected to make saturation undetectable at depth {depth}"
            );
        }
    }

    /// The margin exists only because the shift creates it. Without a shift
    /// there is no slack to spend, so these report the container's ceiling
    /// rather than a value one count below it.
    #[test]
    fn max_adu_spends_no_margin_where_the_container_is_already_full() {
        let threshold = MaxAduReporting::SaturationThreshold;
        // A 16-bit ADC fills the container: step 1, nothing to give back.
        assert_eq!(max_adu_for(ImageType::Raw16, 16, threshold), 65_535);
        // Unknown (0) and degenerate (1) depths give nothing to reason from;
        // 1 in particular would leave `(2 - 2) << 15` = 0 and make every
        // client normalising by MaxADU divide by zero.
        assert_eq!(max_adu_for(ImageType::Raw16, 0, threshold), 65_535);
        assert_eq!(max_adu_for(ImageType::Raw16, 1, threshold), 65_535);
    }

    /// The measured ASI178MM ceiling, stated as the hardware reported it: a
    /// blown-out frame clips at 16382 << 2, and that value must satisfy the
    /// saturation test clients actually write.
    #[test]
    fn measured_asi178mm_ceiling_registers_as_saturated() {
        let max_adu = max_adu_for(ImageType::Raw16, 14, MaxAduReporting::SaturationThreshold);
        let delivered_ceiling = 16_382u32 << 2;
        assert_eq!(delivered_ceiling, 65_528);
        assert!(
            delivered_ceiling >= max_adu,
            "a pixel at the sensor's real ceiling must read as saturated"
        );
    }

    /// The mode is service-wide config, so it has to survive the whole path
    /// from `Config` into the ASCOM property — testing `max_adu_for` alone
    /// would not catch a device that ignored what it was constructed with.
    /// A 14-bit sensor is the case where the two modes genuinely differ.
    #[tokio::test]
    async fn the_configured_mode_reaches_the_max_adu_property() {
        let accurate = ZwoCamera::new(
            Arc::new(MockCameraHandle::default().with_signal(0.25, 14)),
            None,
            MaxAduReporting::SaturationThreshold,
        );
        accurate.connect().unwrap();
        assert_eq!(accurate.max_adu().await.unwrap(), 65_528);

        let compat = ZwoCamera::new(
            Arc::new(MockCameraHandle::default().with_signal(0.25, 14)),
            None,
            MaxAduReporting::ContainerFullScale,
        );
        compat.connect().unwrap();
        assert_eq!(compat.max_adu().await.unwrap(), 65_535);
    }

    /// RM1: the published list is the camera's advertised formats, best first,
    /// with the debayered/luminance formats filtered out (RM4).
    #[test]
    fn negotiated_formats_keep_only_the_raw_ones_best_first() {
        let mut info = crate::backend::mock::MockCameraHandle::default().info();
        // What the physical ASI120MC-S advertises.
        info.supported_video_formats = vec![
            ImageType::Raw8,
            ImageType::Rgb24,
            ImageType::Y8,
            ImageType::Raw16,
        ];
        let names: Vec<&str> = negotiated_formats(&info).iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["Raw16", "Raw8"]);

        info.supported_video_formats = vec![ImageType::Raw8];
        let names: Vec<&str> = negotiated_formats(&info).iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["Raw8"]);

        info.supported_video_formats = vec![ImageType::Rgb24, ImageType::Y8];
        assert_eq!(negotiated_formats(&info), Vec::<ReadoutFormat>::new());
    }

    #[tokio::test]
    async fn electrons_per_adu_tracks_the_current_gain() {
        // The SDK scales the model's gain-0 figure by the gain register, so the
        // property must follow a client's gain writes rather than report the
        // value cached at enumeration. Measured on an ASI1600: 4.96 e⁻/ADU at
        // gain 0, 0.00496 at gain 600. (The mock uses the 0.1 dB law those
        // modern bodies follow; the driver itself assumes no law, since the
        // legacy ASI120MC-S scales differently.)
        let device = connected_device(MockCameraHandle::default().with_signal(4.96, 12));

        device.set_gain(0).await.unwrap();
        let at_zero = device.electrons_per_adu().await.unwrap();
        assert!((at_zero - 4.96).abs() < 1e-6, "{at_zero}");

        // 200 gain units = 20 dB = exactly a factor of 10.
        device.set_gain(200).await.unwrap();
        let at_20db = device.electrons_per_adu().await.unwrap();
        assert!((at_20db - 0.496).abs() < 1e-6, "{at_20db}");
        assert!((at_zero / at_20db - 10.0).abs() < 1e-3);
    }

    #[tokio::test]
    async fn electrons_per_adu_requires_a_connection() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        let err = device.electrons_per_adu().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[test]
    fn a_bin_change_rescales_a_client_set_zero_into_the_error_it_earned() {
        // The rescale arithmetic and its full case list live in
        // `rusty-photon-camera-core`; what this pins is that the two halves
        // are wired together — a 0 the client set survives the bin change and
        // `StartExposure` still answers about that 0, rather than about the %8
        // alignment rule a clamped 1 would trip instead.
        let scaled = camera_core::rescale(roi(0, 0, 0, 0), 1, 2);
        let err = check_geometry(scaled, 6248, 4176, 2).unwrap_err();
        assert!(err.message.contains("greater than 0"), "{}", err.message);
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
    fn geometry_applies_the_asi_alignment_rule_and_reports_it_as_ascom() {
        // The rule set, its order, and the bounds arithmetic are the shared
        // crate's; this pins the two things that are this driver's — that the
        // ASI %8/%2 rule is the one in force, and that a failure arrives as an
        // ASCOM message rather than a `GeometryError`.
        let err = check_geometry(roi(0, 0, 100, 64), 6248, 4176, 1).unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert!(err.message.contains("multiple of 8"), "{}", err.message);
        let err = check_geometry(roi(0, 0, 64, 47), 6248, 4176, 1).unwrap_err();
        assert!(err.message.contains("multiple of 2"), "{}", err.message);
        // A full frame at the reported (aligned) extent passes.
        check_geometry(roi(0, 0, 6240, 4176), 6240, 4176, 1).unwrap();
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

    /// The Raw8 unpack reads one byte per pixel — before this, an 8-bit frame
    /// was rejected as "buffer too small" because the transform assumed 16-bit.
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
    fn to_image_array_rejects_short_buffer() {
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

    /// The device only ever selects a raw format (RM1/RM4), but the transform
    /// stays total: a format it cannot unpack is reported, not mis-read.
    #[test]
    fn to_image_array_rejects_a_format_the_driver_never_selects() {
        let err = to_image_array(vec![0u8; 64 * 48 * 3], 64, 48, ImageType::Rgb24).unwrap_err();
        assert!(err.contains("unsupported download format"), "{err}");
    }

    // --- device behaviour via the mock seam -------------------------------------

    #[tokio::test]
    async fn connect_caches_geometry_and_limits() {
        let device = connected_device(MockCameraHandle::default());
        // CameraXSize is the raw 6248 reduced to 6240 so the binned full frame
        // (CameraXSize/bin) stays a valid ASI ROI at every bin (see
        // `reports_sensor_size_aligned_for_binned_full_frames`); height is
        // already aligned.
        assert_eq!(device.camera_x_size().await.unwrap(), 6240);
        assert_eq!(device.camera_y_size().await.unwrap(), 4176);
        assert_eq!(device.max_adu().await.unwrap(), 65_535);
        assert_eq!(device.max_bin_x().await.unwrap(), 4);
        assert!(!device.can_asymmetric_bin().await.unwrap());
        assert_eq!(device.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert!(!device.has_shutter().await.unwrap());
        assert!(device.electrons_per_adu().await.unwrap() > 0.0);
        assert_eq!(device.gain_min().await.unwrap(), 0);
        assert_eq!(device.gain_max().await.unwrap(), 500);
    }

    #[tokio::test]
    async fn connect_without_an_exposure_control_is_rejected() {
        // The exposure control is mandatory (GO1): a camera that does not
        // advertise it fails the post-open handshake and is left *disconnected*
        // (C2), not opened-but-unusable.
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default().without_control(ControlType::Exposure)),
            None,
            MaxAduReporting::default(),
        );
        assert_eq!(
            device.set_connected(true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn roi_getters_reflect_the_connected_roi() {
        let device = connected_device(MockCameraHandle::default());
        // The default ROI is the aligned full frame at the origin.
        assert_eq!(device.num_x().await.unwrap(), 6240);
        assert_eq!(device.num_y().await.unwrap(), 4176);
        assert_eq!(device.start_x().await.unwrap(), 0);
        assert_eq!(device.start_y().await.unwrap(), 0);
        // The relaxed setters round-trip through the getters (R1).
        device.set_num_x(800).await.unwrap();
        device.set_num_y(600).await.unwrap();
        device.set_start_x(16).await.unwrap();
        device.set_start_y(8).await.unwrap();
        assert_eq!(device.num_x().await.unwrap(), 800);
        assert_eq!(device.num_y().await.unwrap(), 600);
        assert_eq!(device.start_x().await.unwrap(), 16);
        assert_eq!(device.start_y().await.unwrap(), 8);
    }

    #[tokio::test]
    async fn exposure_range_getters_reflect_the_caps() {
        let device = connected_device(MockCameraHandle::default());
        // From the Exposure control cap (32 µs .. 2 000 000 000 µs); the
        // resolution is the ASI 1 µs step.
        assert_eq!(
            device.exposure_min().await.unwrap(),
            Duration::from_micros(32)
        );
        assert_eq!(
            device.exposure_max().await.unwrap(),
            Duration::from_secs(2000)
        );
        assert_eq!(
            device.exposure_resolution().await.unwrap(),
            Duration::from_micros(1)
        );
    }

    #[tokio::test]
    async fn cooling_round_trips_on_a_cooled_model() {
        let device = connected_device(MockCameraHandle::default());
        assert!(device.can_set_ccd_temperature().await.unwrap());
        assert!(device.can_get_cooler_power().await.unwrap());
        // Before any setpoint write, the setpoint getter reads the SDK's
        // target-temperature control...
        assert!((device.set_ccd_temperature().await.unwrap()).abs() < f64::EPSILON);
        // ...and reflects the cached value after a write.
        device.set_set_ccd_temperature(-10.0).await.unwrap();
        assert!((device.set_ccd_temperature().await.unwrap() - (-10.0)).abs() < f64::EPSILON);
        // The cooler toggles and drives the reported sensor temperature + power.
        assert!(!device.cooler_on().await.unwrap());
        device.set_cooler_on(true).await.unwrap();
        assert!(device.cooler_on().await.unwrap());
        assert!((device.ccd_temperature().await.unwrap() - (-10.0)).abs() < f64::EPSILON);
        assert!(device.cooler_power().await.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn bayer_offsets_gate_on_color() {
        // Mono: BayerOffsetX/Y are NOT_IMPLEMENTED (ST1).
        let mono = connected_device(MockCameraHandle::default());
        assert_eq!(mono.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert_eq!(
            mono.bayer_offset_x().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        // Colour: the Bayer pattern maps to BayerOffsetX/Y (Gb → (0, 1)).
        let color = connected_device(MockCameraHandle::default().with_color(BayerPattern::Gb));
        assert_ne!(color.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert_eq!(color.bayer_offset_x().await.unwrap(), 0);
        assert_eq!(color.bayer_offset_y().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pulse_guide_maps_every_direction() {
        // Each ASCOM direction maps onto its zwo-rs counterpart (PG1).
        let device = connected_device(MockCameraHandle::default());
        for dir in [
            GuideDirection::North,
            GuideDirection::South,
            GuideDirection::East,
            GuideDirection::West,
        ] {
            device
                .pulse_guide(dir, Duration::from_millis(1))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn reads_require_connection() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        assert_eq!(
            device.camera_x_size().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    #[tokio::test]
    async fn unique_id_is_serial_derived_and_non_empty() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        assert_ne!(device.unique_id(), "");
        assert!(device.unique_id().contains("0a1b2c3d4e5f6071"));
    }

    #[tokio::test]
    async fn connection_flag_round_trips() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        assert!(!device.connected().await.unwrap());
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn gain_out_of_range_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        let max = device.gain_max().await.unwrap();
        device.set_gain(max).await.unwrap();
        assert_eq!(device.gain().await.unwrap(), max);
        let err = device.set_gain(max + 1).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[test]
    fn ascom_range_has_no_answer_for_bounds_outside_i32() {
        let cap = |min, max| ControlCaps {
            name: "Gain".to_string(),
            control_type: ControlType::Gain,
            min,
            max,
            default: 0,
            is_writable: true,
            is_auto_supported: false,
        };
        assert_eq!(ascom_range(&cap(0, 500)), Some((0, 500)));
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
        let device = connected_device(MockCameraHandle::default().with_control_range(
            ControlType::Gain,
            0,
            i64::from(i32::MAX) + 1,
        ));
        for code in [
            device.gain().await.unwrap_err().code,
            device.gain_min().await.unwrap_err().code,
            device.gain_max().await.unwrap_err().code,
            device.set_gain(5).await.unwrap_err().code,
        ] {
            assert_eq!(code, ASCOMErrorCode::NOT_IMPLEMENTED);
        }
        // Offset is cached independently and is unaffected.
        assert_eq!(device.offset_max().await.unwrap(), 1000);
    }

    #[tokio::test]
    async fn gain_not_implemented_without_control() {
        let device =
            connected_device(MockCameraHandle::default().without_control(ControlType::Gain));
        assert_eq!(
            device.gain().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            device.gain_min().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn offset_out_of_range_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        let min = device.offset_min().await.unwrap();
        let err = device.set_offset(min - 1).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn readout_modes_are_listed_and_out_of_range_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        let modes = device.readout_modes().await.unwrap();
        // RM1: the camera's advertised download formats, best precision first.
        assert_eq!(modes, ["Raw16", "Raw8"]);
        assert!(device.readout_mode().await.unwrap() < modes.len());
        device.set_readout_mode(1).await.unwrap();
        assert_eq!(device.readout_mode().await.unwrap(), 1);
        assert_eq!(
            device.set_readout_mode(9999).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    /// The gap issue #881 filed: a camera without Raw16 must not be handed
    /// Raw16 anyway. It offers only the 8-bit mode, and `MaxADU` follows the
    /// format actually delivered (RM2).
    #[tokio::test]
    async fn a_camera_without_raw16_offers_only_the_8_bit_mode() {
        let device =
            connected_device(MockCameraHandle::default().with_video_formats(vec![ImageType::Raw8]));
        assert_eq!(device.readout_modes().await.unwrap(), ["Raw8"]);
        assert_eq!(device.max_adu().await.unwrap(), 255);
        assert_eq!(
            device.set_readout_mode(1).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    /// RM3: a camera offering neither raw format has nothing this driver's
    /// single-plane contract can describe, so connect fails.
    #[tokio::test]
    async fn connecting_fails_when_no_raw_download_format_is_advertised() {
        let device = ZwoCamera::new(
            Arc::new(
                MockCameraHandle::default()
                    .with_video_formats(vec![ImageType::Rgb24, ImageType::Y8]),
            ),
            None,
            MaxAduReporting::default(),
        );
        assert_eq!(
            device.set_connected(true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert!(!device.connected().await.unwrap());
    }

    /// RM2: selecting the 8-bit mode is what the exposure downloads and what
    /// `MaxADU` describes — the two can never disagree.
    #[tokio::test]
    async fn selecting_the_8_bit_mode_drives_the_download_and_max_adu() {
        let handle = Arc::new(MockCameraHandle::default());
        let device = ZwoCamera::new(handle.clone(), None, MaxAduReporting::default());
        device.set_connected(true).await.unwrap();
        device.set_readout_mode(1).await.unwrap();
        assert_eq!(device.max_adu().await.unwrap(), 255);

        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&device).await;
        assert_eq!(
            handle.last_capture_request().unwrap().image_type,
            ImageType::Raw8
        );
        let image = device.image_array().await.unwrap();
        assert_eq!(image.dim().0, 64);
        assert_eq!(image.dim().1, 48);
    }

    #[tokio::test]
    async fn cooling_turns_on_and_reports_power() {
        let device = connected_device(MockCameraHandle::default());
        assert!(device.can_set_ccd_temperature().await.unwrap());
        device.set_set_ccd_temperature(-10.0).await.unwrap();
        assert_eq!(device.set_ccd_temperature().await.unwrap(), -10.0);
        device.set_cooler_on(true).await.unwrap();
        assert!(device.cooler_on().await.unwrap());
        let power = device.cooler_power().await.unwrap();
        assert!((0.0..=100.0).contains(&power), "{power}");
        assert!(device.ccd_temperature().await.unwrap().is_finite());
    }

    #[tokio::test]
    async fn out_of_range_target_temperature_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(
            device
                .set_set_ccd_temperature(-300.0)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            device
                .set_set_ccd_temperature(100.0)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn cooling_setpoint_members_not_implemented_on_uncooled_model() {
        // The cooler-setpoint members stay gated on `is_cooler_cam`...
        let device = connected_device(MockCameraHandle::default().without_cooler());
        assert!(!device.can_set_ccd_temperature().await.unwrap());
        assert!(!device.can_get_cooler_power().await.unwrap());
        assert_eq!(
            device.set_ccd_temperature().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            device.cooler_power().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn ccd_temperature_reported_on_uncooled_model_with_sensor() {
        // ...but `CCDTemperature` is decoupled: an uncooled camera that still
        // advertises the sensor-temperature control reports a reading (the ASI178
        // hardware behaviour), rather than the old `NOT_IMPLEMENTED` placeholder.
        let device = connected_device(MockCameraHandle::default().without_cooler());
        assert!(device.ccd_temperature().await.unwrap().is_finite());
    }

    #[tokio::test]
    async fn ccd_temperature_not_implemented_without_sensor_control() {
        // A camera that does not advertise `ASI_TEMPERATURE` genuinely has no
        // reading, so `CCDTemperature` is `NOT_IMPLEMENTED`.
        let device =
            connected_device(MockCameraHandle::default().without_control(ControlType::Temperature));
        assert_eq!(
            device.ccd_temperature().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn bin_change_rescales_roi_and_rejects_unsupported() {
        let device = connected_device(MockCameraHandle::default());
        device.set_num_x(3120).await.unwrap();
        device.set_num_y(2088).await.unwrap();
        device.set_bin_x(2).await.unwrap();
        assert_eq!(device.bin_x().await.unwrap(), 2);
        assert_eq!(device.bin_y().await.unwrap(), 2);
        assert_eq!(device.num_x().await.unwrap(), 1560);
        assert_eq!(device.num_y().await.unwrap(), 1044);
        assert_eq!(
            device.set_bin_x(99).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn binned_full_frame_passes_geometry_at_high_bins() {
        // ConformU takes the full frame at every bin via NumX = CameraXSize/bin.
        // With the aligned reported size these are valid ASI ROIs even at the
        // bins where the raw sensor size would not divide cleanly (this is the
        // bug that produced the 3 ConformU StartExposure issues).
        for bin in [2u8, 3, 4] {
            let device = connected_device(MockCameraHandle::default());
            let w = device.camera_x_size().await.unwrap() / u32::from(bin);
            let h = device.camera_y_size().await.unwrap() / u32::from(bin);
            assert_eq!(w % 8, 0);
            assert_eq!(h % 2, 0);
            device.set_bin_x(bin).await.unwrap();
            device.set_start_x(0).await.unwrap();
            device.set_start_y(0).await.unwrap();
            device.set_num_x(w).await.unwrap();
            device.set_num_y(h).await.unwrap();
            device
                .start_exposure(Duration::from_millis(10), true)
                .await
                .unwrap_or_else(|e| panic!("bin {bin} full frame rejected: {e:?}"));
            wait_image_ready(&device).await;
            assert!(device.image_ready().await.unwrap(), "bin {bin} no image");
        }
    }

    #[tokio::test]
    async fn disconnected_start_exposure_is_not_connected() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        let err = device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn dark_frame_is_accepted_on_shutterless_camera() {
        // ZWO divergence from qhy: darks are accepted (E4).
        let device = connected_device(MockCameraHandle::default());
        assert!(!device.has_shutter().await.unwrap());
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), false)
            .await
            .unwrap();
        wait_image_ready(&device).await;
        assert!(device.image_ready().await.unwrap());
    }

    #[tokio::test]
    async fn successful_exposure_produces_image() {
        let device = connected_device(MockCameraHandle::default());
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device.set_start_x(0).await.unwrap();
        device.set_start_y(0).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_image_ready(&device).await;
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
        assert_eq!(device.percent_completed().await.unwrap(), 100);
        device.last_exposure_start_time().await.unwrap();
        let image = device.image_array().await.unwrap();
        assert_eq!(image.dim().0, 64);
        assert_eq!(image.dim().1, 48);
    }

    #[tokio::test]
    async fn out_of_range_duration_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        // 100000 s = 1e11 us, beyond the cached max (2e9 us).
        let err = device
            .start_exposure(Duration::from_secs(100_000), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn mid_exposure_error_transitions_to_error_state() {
        let handle = MockCameraHandle::default();
        handle.fail_capture.store(true, Ordering::SeqCst);
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_camera_state(&device, CameraState::Error).await;
        assert!(!device.image_ready().await.unwrap());
        assert_eq!(
            device.image_array().await.unwrap_err().code,
            UNSPECIFIED_ERROR
        );
    }

    #[tokio::test]
    async fn reconnect_clears_error_state() {
        let handle = Arc::new(MockCameraHandle::default());
        handle.fail_capture.store(true, Ordering::SeqCst);
        let device = ZwoCamera::new(handle.clone(), None, MaxAduReporting::default());
        device.set_connected(true).await.unwrap();
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_camera_state(&device, CameraState::Error).await;
        device.set_connected(false).await.unwrap();
        handle.fail_capture.store(false, Ordering::SeqCst);
        device.set_connected(true).await.unwrap();
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
        assert!(!device.image_ready().await.unwrap());
    }

    #[tokio::test]
    async fn second_exposure_while_in_flight_is_rejected() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_secs(5));
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Exposing);
        let err = device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        device.abort_exposure().await.unwrap();
    }

    #[tokio::test]
    async fn abort_discards_the_frame() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_secs(5));
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        device.abort_exposure().await.unwrap();
        // No fresh frame is ready after an abort. Best-effort, deadline-bounded
        // wait (not a fixed nap count) for the in-flight flag to clear.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while device.state.exposure_in_flight() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(!device.image_ready().await.unwrap());
        assert_eq!(
            device.image_array().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn graceful_stop_preserves_the_frame() {
        // ZWO divergence: StopExposure keeps the frame (E8).
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_secs(5));
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        assert!(device.can_stop_exposure().await.unwrap());
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        device.stop_exposure().await.unwrap();
        wait_image_ready(&device).await;
        assert!(device.image_ready().await.unwrap());
        device.image_array().await.unwrap();
    }

    #[tokio::test]
    async fn pulse_guide_capability_and_branches() {
        let device = connected_device(MockCameraHandle::default());
        assert!(device.can_pulse_guide().await.unwrap());
        device
            .pulse_guide(GuideDirection::North, Duration::from_millis(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pulse_guide_reports_in_flight() {
        // ASCOM PulseGuide returns promptly instead of blocking for the
        // pulse, and IsPulseGuiding is true while one is in flight (PG2).
        // The 60 s pulse outlives the test, so the in-flight read races
        // nothing — a shorter pulse would let a starved runtime delay the
        // read past the deadline. The pulse never ends: runtime shutdown
        // drops the detached stop task, and the mock needs no cleanup.
        // PulseGuide returning promptly is implied by the test finishing.
        let device = connected_device(MockCameraHandle::default());
        assert!(!device.is_pulse_guiding().await.unwrap());
        device
            .pulse_guide(GuideDirection::North, Duration::from_mins(1))
            .await
            .unwrap();
        assert!(device.is_pulse_guiding().await.unwrap());
    }

    #[tokio::test]
    async fn pulse_guide_expires() {
        // Expiry is an event wait, not a point-read: IsPulseGuiding must
        // eventually clear once the wall-clock deadline passes (PG2). The
        // 30 s cap only bounds how long to wait before declaring failure.
        let device = connected_device(MockCameraHandle::default());
        device
            .pulse_guide(GuideDirection::North, Duration::from_millis(50))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while device.is_pulse_guiding().await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("pulse did not expire");
    }

    #[tokio::test]
    async fn pulse_guide_not_implemented_without_st4() {
        let device = connected_device(MockCameraHandle::default().without_st4());
        assert!(!device.can_pulse_guide().await.unwrap());
        assert_eq!(
            device
                .pulse_guide(GuideDirection::North, Duration::from_millis(1))
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn pulse_guide_disconnected_is_not_connected() {
        let device = ZwoCamera::new(
            Arc::new(MockCameraHandle::default()),
            None,
            MaxAduReporting::default(),
        );
        assert_eq!(
            device
                .pulse_guide(GuideDirection::North, Duration::from_millis(1))
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    /// A disconnect and reconnect mid-exposure releases the in-flight slot
    /// (`reset_exposure_state`) while the aborted capture is still draining, so
    /// the next `StartExposure` runs alongside it. The disconnect's abort has to
    /// survive that: with one stop signal shared by the handle, the second
    /// capture reset it, and the first ran on to completion against the camera
    /// the reconnect had just reopened — the *new* exposure's camera.
    #[tokio::test]
    async fn a_reconnect_does_not_erase_a_superseded_captures_abort() {
        let handle = Arc::new(MockCameraHandle::default());
        // Hold each capture before it can read its stop signal, so the
        // interleaving is forced rather than raced.
        handle.set_capture_gate(true);
        let device = ZwoCamera::new(handle.clone(), None, MaxAduReporting::default());
        device.connect().unwrap();
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        wait_captures_started(&handle, 1).await;

        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();
        // Connect resets the ROI to the full frame; keep the second exposure as
        // small as the first.
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        wait_captures_started(&handle, 2).await;

        handle.set_capture_gate(false);
        wait_image_ready(&device).await;
        wait_captures_finished(&handle, 2).await;
        assert_eq!(
            handle.capture_outcomes(),
            vec![Some(CaptureOutcome::Aborted), Some(CaptureOutcome::Frame)],
            "the superseded capture must still see the disconnect's abort"
        );
        device.image_array().await.unwrap();
    }

    /// The same drain must not release the in-flight slot the exposure that
    /// replaced it now owns — the device would report `Idle` mid-exposure and
    /// admit a third capture alongside the running one.
    #[tokio::test]
    async fn a_superseded_capture_does_not_release_the_new_exposures_slot() {
        let handle = Arc::new(MockCameraHandle::default());
        handle.set_capture_gate(true);
        let device = ZwoCamera::new(handle.clone(), None, MaxAduReporting::default());
        device.connect().unwrap();
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        wait_captures_started(&handle, 1).await;

        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        // The second exposure stays in flight while the first drains.
        handle.set_capture_delay(Duration::from_secs(5));
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        wait_captures_started(&handle, 2).await;

        handle.set_capture_gate(false);
        tokio::time::timeout(Duration::from_secs(30), async {
            while handle.capture_outcomes().first() != Some(&Some(CaptureOutcome::Aborted)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the superseded capture never drained, so the assertions below would not test the drain");
        // The drain's post-capture commit runs on the runtime some time after
        // its blocking task returns, so a single sample taken after a fixed nap
        // could sample before it and pass without testing anything. Watch the
        // whole window instead: with the flag released unconditionally the
        // drain clears it at *some* point inside this window, and one `Idle`
        // read is the failure. A longer window can only make the bug easier to
        // catch, never harder.
        for _ in 0..100 {
            assert_eq!(
                device.camera_state().await.unwrap(),
                CameraState::Exposing,
                "the superseded capture's drain released the running exposure's slot"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            device
                .start_exposure(Duration::from_millis(10), true)
                .await
                .unwrap_err()
                .code,
            ASCOMErrorCode::INVALID_OPERATION
        );
        device.abort_exposure().await.unwrap();
    }

    /// The in-flight claim and the capture's stop cell are one piece of
    /// state, so an abort can never reach a device that reports itself
    /// exposing and find nothing to signal. Held apart — a flag taken first, a
    /// cell installed a few statements later — that window is real, and an
    /// abort landing inside it is swallowed: the capture then runs out its
    /// whole deadline with the device still claimed, which is precisely the
    /// stall the per-capture cell exists to prevent.
    ///
    /// The interleaving is forced rather than raced. A parked thread holds a
    /// lock `start_exposure` writes on its way out, stalling it at a point that
    /// is inside that window when the two are held apart and safely after the
    /// claim when they are one — either way the device reports `Exposing`
    /// throughout, so the abort below is never a no-op on an idle device.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abort_reaches_a_capture_the_claim_has_only_just_admitted() {
        let handle = MockCameraHandle::default();
        // Far longer than the drain wait below, so a pass can only come from
        // the abort reaching the capture, never from waiting the capture out.
        handle.set_capture_delay(Duration::from_secs(60));
        let cam = connected_device(handle);
        cam.set_num_x(64).await.unwrap();
        cam.set_num_y(64).await.unwrap();

        let (parked, is_parked) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let state = Arc::clone(&cam.state);
        let stall = std::thread::spawn(move || {
            let _guard = state.last_exposure_start_time.lock();
            parked.send(()).unwrap();
            released.recv().unwrap();
        });
        is_parked.recv().unwrap();

        let starter = {
            let cam = cam.clone();
            tokio::spawn(async move { cam.start_exposure(Duration::from_secs(60), true).await })
        };
        wait_camera_state(&cam, CameraState::Exposing).await;
        cam.abort_exposure().await.unwrap();
        release.send(()).unwrap();
        stall.join().unwrap();
        starter.await.unwrap().unwrap();

        let drained = tokio::time::timeout(Duration::from_secs(30), async {
            while cam.state.exposure_in_flight() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        assert!(!cam.image_ready().await.unwrap());
        // A swallowed abort leaves a capture parked for the rest of its delay;
        // end it here so a failing run reports at once rather than at the
        // runtime's drop, which waits on the blocking pool.
        cam.abort_exposure().await.unwrap();
        assert!(
            drained,
            "the abort never reached the capture: the device stayed claimed"
        );
    }

    #[tokio::test]
    async fn disconnect_cancels_in_flight_exposure() {
        let handle = MockCameraHandle::default();
        handle.set_capture_delay(Duration::from_secs(5));
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
        assert!(!device.image_ready().await.unwrap());
    }
}

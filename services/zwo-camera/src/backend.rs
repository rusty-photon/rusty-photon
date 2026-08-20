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

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use zwo_rs::{CameraInfo, ControlCaps, ControlType, GuideDirection, ImageType};

/// A `zwo-rs` SDK call failed. Carries the underlying message; the ASCOM device
/// decides the `ASCOMError` per call site (the SDK error kind does not map 1:1 to
/// an ASCOM code).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

/// Collapse a [`zwo_rs::Error`] into the typed seam error. The seam keeps only the
/// message string (each call site picks the right `ASCOMError` code); this `From`
/// impl lets `?` convert SDK errors automatically.
impl From<zwo_rs::Error> for BackendError {
    fn from(err: zwo_rs::Error) -> Self {
        Self(err.to_string())
    }
}

impl BackendError {
    fn closed() -> Self {
        Self("camera not open".to_string())
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// The ROI + exposure parameters for a single capture, validated by the device.
#[derive(Debug, Clone, Copy)]
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
    /// Wall-clock integration time the capture honours so an in-flight exposure
    /// is observable (the `zwo-rs` simulation completes after one poll regardless).
    pub duration: Duration,
    /// Request a dark frame (no-op on shutterless ASI sensors).
    pub is_dark: bool,
}

/// Stop request for an in-flight capture: none, abort (discard), or stop (preserve).
const STOP_NONE: u8 = 0;
const STOP_ABORT: u8 = 1;
const STOP_PRESERVE: u8 = 2;

/// Real-clock budget for the post-integration readout-completion poll. Only a
/// safety bound on the stop-responsive courtesy poll — `download_exposure` still
/// blocks until the frame is actually ready — but expressed as a deadline (not a
/// fixed nap count) so it cannot drift under blocking-pool oversubscription.
const READOUT_TIMEOUT: Duration = Duration::from_millis(2500);

/// The blocking camera operations the ASCOM `Camera` device drives. Every method
/// is synchronous (the SDK is blocking C FFI); the device offloads the long
/// [`capture`](CameraHandle::capture) onto `spawn_blocking`.
pub trait CameraHandle: std::fmt::Debug + Send + Sync {
    /// The stable ASCOM `UniqueID` (serial-derived; read once at enumeration).
    fn unique_id(&self) -> String;

    /// The camera's enumeration [`CameraInfo`] (cached; no open required).
    fn info(&self) -> CameraInfo;

    fn is_open(&self) -> bool;
    fn open(&self) -> BackendResult<()>;
    fn close(&self) -> BackendResult<()>;

    /// Enumerate the camera's tunable controls and their ranges (`ASIGetControlCaps`).
    fn control_caps(&self) -> BackendResult<Vec<ControlCaps>>;

    /// Read a control's current value (`ASIGetControlValue`); temperature is in
    /// 0.1 °C units (use [`temperature_celsius`](Self::temperature_celsius)).
    fn control_value(&self, control: ControlType) -> BackendResult<i64>;

    /// Electrons per ADU at the camera's **current gain** — a live read, since
    /// the SDK scales this field by the gain register (ST2).
    fn electrons_per_adu(&self) -> BackendResult<f32>;
    /// Set a control's value (`ASISetControlValue`).
    fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()>;
    /// Sensor temperature in °C (decodes the 0.1 °C `ASI_TEMPERATURE` units).
    fn temperature_celsius(&self) -> BackendResult<f64>;

    /// Run a single-frame capture under one SDK lock: set ROI + exposure, start,
    /// integrate (honouring an abort/stop signal), poll to completion, download.
    /// Returns `Ok(Some(frame))` for a completed or gracefully-stopped exposure,
    /// `Ok(None)` for an aborted one (frame discarded), or `Err` on an SDK error.
    fn capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>>;

    /// Signal an in-flight [`capture`](Self::capture) to stop: `preserve = false`
    /// aborts (discards the frame), `preserve = true` gracefully stops (keeps it).
    fn request_stop(&self, preserve: bool);

    fn pulse_guide_on(&self, direction: GuideDirection) -> BackendResult<()>;
    fn pulse_guide_off(&self, direction: GuideDirection) -> BackendResult<()>;
}

// --- production wrapper over zwo-rs ---------------------------------------------

/// Production [`CameraHandle`] over a real (or `zwo-rs`-simulated) camera.
///
/// Holds the [`zwo_rs::Sdk`] (a ZST) and the enumeration `index` so it can re-open
/// the RAII [`zwo_rs::Camera`] on connect; the open handle lives behind a
/// `Mutex<Option<…>>` because `Camera` is `Send + !Sync`.
#[derive(Debug)]
pub struct ZwoCameraHandle {
    sdk: zwo_rs::Sdk,
    index: usize,
    info: CameraInfo,
    unique_id: String,
    camera: Mutex<Option<zwo_rs::Camera>>,
    /// Abort/stop signal read by an in-flight [`capture`](Self::capture). A plain
    /// atomic (not inside the `Mutex`) so abort/stop can signal while the capture
    /// holds the camera lock.
    stop: AtomicU8,
}

impl ZwoCameraHandle {
    /// Build a handle for the camera at enumeration `index`, with its cached
    /// [`CameraInfo`] and the serial-derived `unique_id` read at enumeration.
    #[must_use]
    pub const fn new(sdk: zwo_rs::Sdk, index: usize, info: CameraInfo, unique_id: String) -> Self {
        Self {
            sdk,
            index,
            info,
            unique_id,
            camera: Mutex::new(None),
            stop: AtomicU8::new(STOP_NONE),
        }
    }

    /// Best-effort `ASIStopExposure`, re-acquiring the lock (the integration loop
    /// runs without it). A no-op if the camera was closed (e.g. by a disconnect).
    fn stop_at_sdk(&self) {
        if let Some(camera) = self.camera.lock().as_ref() {
            let _ = camera.stop_exposure();
        }
    }

    /// Borrow the open camera for one SDK call. Returns the closed error
    /// when the handle slot is empty.
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
        f(camera)
    }
}

impl CameraHandle for ZwoCameraHandle {
    fn unique_id(&self) -> String {
        self.unique_id.clone()
    }

    fn info(&self) -> CameraInfo {
        self.info.clone()
    }

    fn is_open(&self) -> bool {
        self.camera.lock().is_some()
    }

    fn open(&self) -> BackendResult<()> {
        let mut guard = self.camera.lock();
        if guard.is_none() {
            *guard = Some(self.sdk.open_camera(self.index)?);
        }
        drop(guard);
        Ok(())
    }

    fn close(&self) -> BackendResult<()> {
        // Dropping the `Camera` calls `ASICloseCamera`.
        *self.camera.lock() = None;
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
        // Reset the stop signal for this capture. A stop/abort racing in just
        // before this reset is benign: abort bumps the exposure generation (so
        // the device discards the result anyway) and a lost graceful-stop simply
        // lets the full exposure complete — exactly stop's "preserve the frame".
        self.stop.store(STOP_NONE, Ordering::SeqCst);

        // Configure and start the exposure under the lock, then RELEASE it for
        // the integration: holding it for the whole exposure would block every
        // other SDK read — including `is_open()` from a concurrent request — for
        // the full duration. A second exposure is already barred by the device's
        // in-flight CAS, and ASI control/status reads are safe concurrently with
        // an integrating exposure (only ROI/format changes are not, and those
        // happen only here, at the start of a capture).
        self.with_camera(|camera| {
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
            // `ASI_EXPOSURE` is a writable control on every ASI camera, and the
            // `zwo-rs` simulation models it too, so a failure here is a genuine
            // error: fail the capture rather than silently integrate for the
            // wrong exposure time.
            camera.set_control_value(ControlType::Exposure, request.exposure_us, false)?;
            camera.start_exposure(request.is_dark)?;
            Ok(())
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
            match self.stop.load(Ordering::SeqCst) {
                STOP_ABORT => {
                    self.stop_at_sdk();
                    return Ok(None);
                }
                STOP_PRESERVE => {
                    self.stop_at_sdk();
                    preserve = true;
                    break;
                }
                _ => {}
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(step.min(deadline.saturating_duration_since(now)));
        }

        // Poll to completion (unless gracefully stopped) and download, under the
        // lock. If the camera was closed mid-integration (e.g. a disconnect),
        // treat the capture as aborted.
        let guard = self.camera.lock();
        let Some(camera) = guard.as_ref() else {
            return Ok(None);
        };
        if !preserve {
            // Poll the SDK to readout completion against a real-clock DEADLINE,
            // for the same reason as the integration wait above: a fixed nap
            // count drifts unpredictably under blocking-pool oversubscription.
            let readout_start = std::time::Instant::now();
            let readout_deadline = readout_start
                .checked_add(READOUT_TIMEOUT)
                .unwrap_or(readout_start);
            let step = Duration::from_millis(10);
            loop {
                match self.stop.load(Ordering::SeqCst) {
                    STOP_ABORT => {
                        let _ = camera.stop_exposure();
                        return Ok(None);
                    }
                    STOP_PRESERVE => {
                        let _ = camera.stop_exposure();
                        break;
                    }
                    _ => {}
                }
                match camera.exposure_status()? {
                    zwo_rs::ExposureStatus::Success | zwo_rs::ExposureStatus::Idle => break,
                    zwo_rs::ExposureStatus::Failed => {
                        return Err(BackendError("exposure failed".to_string()))
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

        // Size the buffer from the SDK's own view of the ROI, which is
        // exactly what `download_exposure` checks it against. That view now
        // includes the format set above, so the non-Raw16 path this guarded
        // against — an 8-bit readout mode (RM1/RM2) — needs no second
        // bytes-per-pixel constant here.
        let frame_len = camera.roi_format()?.buffer_len().ok_or_else(|| {
            BackendError("frame is too large to address on this target".to_string())
        })?;
        let mut buf = vec![0u8; frame_len];
        camera.download_exposure(&mut buf)?;
        drop(guard);
        Ok(Some(buf))
    }

    fn request_stop(&self, preserve: bool) {
        self.stop.store(
            if preserve { STOP_PRESERVE } else { STOP_ABORT },
            Ordering::SeqCst,
        );
    }

    fn pulse_guide_on(&self, direction: GuideDirection) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.pulse_guide_on(direction)?))
    }

    fn pulse_guide_off(&self, direction: GuideDirection) -> BackendResult<()> {
        self.with_camera(|camera| Ok(camera.pulse_guide_off(direction)?))
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

    fn sim_handle() -> ZwoCameraHandle {
        let sdk = zwo_rs::Sdk::new().expect("simulation SDK");
        let info = sdk.cameras().expect("enumerate")[0].clone();
        ZwoCameraHandle::new(sdk, 0, info, "ZWO:Sim:0a1b2c3d4e5f6071".to_string())
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

    #[test]
    fn production_handle_capture_produces_a_frame() {
        let handle = sim_handle();
        handle.open().unwrap();
        let request = CaptureRequest {
            width: 64,
            height: 64,
            bin: 1,
            start_x: 0,
            start_y: 0,
            exposure_us: 1_000,
            image_type: ImageType::Raw16,
            duration: Duration::from_millis(10),
            is_dark: false,
        };
        let frame = handle.capture(request).unwrap().expect("a completed frame");
        assert_eq!(frame.len(), 64 * 64 * 2);
        handle.close().unwrap();
    }
}

/// A configurable in-memory [`CameraHandle`] for the crate's unit tests, so the
/// device logic — including the paths the `zwo-rs` simulation cannot force, like
/// a mid-exposure SDK error (E9) or a model without an ST4 port (PG2) — is
/// exercised without hardware.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::AtomicBool;

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

    #[derive(Debug)]
    pub struct MockCameraHandle {
        info: CameraInfo,
        caps: Vec<ControlCaps>,
        open: AtomicBool,
        gain: Mutex<i64>,
        offset: Mutex<i64>,
        target_temp: Mutex<i64>,
        cooler_on: AtomicBool,
        stop: AtomicU8,
        /// E9 injection: make the next capture fail at the SDK.
        pub fail_capture: AtomicBool,
        /// Optional artificial integration time (for in-flight tests).
        capture_delay: Mutex<Duration>,
        /// The most recent [`CaptureRequest`] passed to `capture`, so a test can
        /// assert what the device configured — e.g. the negotiated download
        /// format (RM2).
        last_capture_request: Mutex<Option<CaptureRequest>>,
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
                stop: AtomicU8::new(STOP_NONE),
                fail_capture: AtomicBool::new(false),
                capture_delay: Mutex::new(Duration::ZERO),
                last_capture_request: Mutex::new(None),
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
            *self.last_capture_request.lock()
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
    }

    impl CameraHandle for MockCameraHandle {
        fn unique_id(&self) -> String {
            "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071".to_string()
        }

        fn info(&self) -> CameraInfo {
            self.info.clone()
        }

        fn is_open(&self) -> bool {
            self.open.load(Ordering::SeqCst)
        }

        fn open(&self) -> BackendResult<()> {
            self.open.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn close(&self) -> BackendResult<()> {
            self.open.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn control_caps(&self) -> BackendResult<Vec<ControlCaps>> {
            Ok(self.caps.clone())
        }

        fn electrons_per_adu(&self) -> BackendResult<f32> {
            // Mirrors a modern body's SDK response: the gain-0 figure scaled by
            // the gain register in 0.1 dB units. Real models differ (the legacy
            // ASI120MC-S uses another law), hence the live read in the driver.
            let gain = *self.gain.lock();
            let scale = 10f64.powf(gain as f64 / 200.0);
            Ok((f64::from(self.info.e_per_adu) / scale) as f32)
        }

        fn control_value(&self, control: ControlType) -> BackendResult<i64> {
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
                _ => return Err(BackendError("invalid control type".to_string())),
            };
            Ok(value)
        }

        fn set_control_value(&self, control: ControlType, value: i64) -> BackendResult<()> {
            match control {
                ControlType::Gain => *self.gain.lock() = value,
                ControlType::Offset => *self.offset.lock() = value,
                ControlType::TargetTemp => *self.target_temp.lock() = value,
                ControlType::CoolerOn => self.cooler_on.store(value != 0, Ordering::SeqCst),
                ControlType::Exposure => {}
                _ => return Err(BackendError("invalid control type".to_string())),
            }
            Ok(())
        }

        fn temperature_celsius(&self) -> BackendResult<f64> {
            Ok(self.control_value(ControlType::Temperature)? as f64 / 10.0)
        }

        fn capture(&self, request: CaptureRequest) -> BackendResult<Option<Vec<u8>>> {
            *self.last_capture_request.lock() = Some(request);
            self.stop.store(STOP_NONE, Ordering::SeqCst);
            let delay = *self.capture_delay.lock();
            // Mirror the production handle: sleep against a real-clock DEADLINE,
            // not accumulated *intended* nap time, so the simulated capture can't
            // drift under a contended runtime.
            let deadline = std::time::Instant::now() + delay;
            let step = Duration::from_millis(10);
            loop {
                match self.stop.load(Ordering::SeqCst) {
                    STOP_ABORT => return Ok(None),
                    STOP_PRESERVE => break,
                    _ => {}
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::sleep(step.min(deadline.saturating_duration_since(now)));
            }
            if self.stop.load(Ordering::SeqCst) == STOP_ABORT {
                return Ok(None);
            }
            if self.fail_capture.load(Ordering::SeqCst) {
                return Err(BackendError("simulated capture failure".to_string()));
            }
            Ok(Some(vec![
                0u8;
                request.width as usize
                    * request.height as usize
                    * request.image_type.bytes_per_pixel()
            ]))
        }

        fn request_stop(&self, preserve: bool) {
            self.stop.store(
                if preserve { STOP_PRESERVE } else { STOP_ABORT },
                Ordering::SeqCst,
            );
        }

        fn pulse_guide_on(&self, _direction: GuideDirection) -> BackendResult<()> {
            Ok(())
        }

        fn pulse_guide_off(&self, _direction: GuideDirection) -> BackendResult<()> {
            Ok(())
        }
    }
}

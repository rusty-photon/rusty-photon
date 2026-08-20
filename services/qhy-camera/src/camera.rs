//! `QhyCameraDevice` — the ASCOM `Device` + `Camera` implementation over the
//! [`CameraHandle`](crate::backend::CameraHandle) seam.
//!
//! Behaviour is ported from the author's standalone `qhyccd-alpaca` driver and
//! re-expressed against rusty-photon conventions, with these deliberate
//! divergences (see `docs/services/qhy-camera.md`):
//! - **`MaxADU`** = `2^transfer_bits - 1` (65535 for the 16-bit container we set at
//!   connect), from `GetQHYCCDChipInfo`'s reported bit depth — *not*
//!   `2^OutputDataActualBits - 1`. The SDK left-shifts each raw sensor reading to
//!   fill the container (12-bit IMX290 → values up to 0xFFF0, 14-bit IMX178 →
//!   0xFFFC; SDK manual §14), so the container max is what a client receives;
//!   `OutputDataActualBits` is the sensor ADC depth (and is 0 on the QHY5III715C).
//! - **ROI validation** rejects a zero or out-of-bounds sub-frame via
//!   `StartX + NumX > CameraXSize / BinX` (contract R2), not the reference's
//!   `StartX > NumX`.
//! - **Dark frames** return `NOT_IMPLEMENTED` (qhyccd-rs 0.1.9 has no shutter
//!   actuation; contract E4 degraded form).
//! - A real **`Error` `CameraState`** (E9) when a mid-exposure SDK call fails.
//! - **`PercentCompleted`** is percent *done*, clamped, 100 when idle (E6).
//!
//! Blocking exposure SDK calls run on `spawn_blocking` inside a detached task; a
//! generation counter lets abort/disconnect invalidate a late-completing task.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use parking_lot::Mutex;
use qhyccd_rs::{BayerPattern, CCDChipArea, ControlType};
use rusty_photon_camera_core::{self as camera_core, Alignment, PixelDepth, Roi};
use rusty_photon_driver::ConfigActionCtx;
use tracing::{debug, warn};

use crate::backend::{BackendError, CameraHandle, ImageData};
use crate::config::DeviceOverride;
use crate::config_actions::QhyCameraDriver;

/// 0x500 — driver-specific catch-all for an asynchronous capture failure
/// surfaced lazily via `image_array`.
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

/// How long an abort/disconnect waits for the capture task to leave the SDK.
/// Sized for a readout, not an exposure: the task's wait for the exposure to
/// elapse is cancellable, so the only uninterruptible stretch is
/// `GetQHYCCDSingleFrame` itself (a full-frame USB transfer, well under a second
/// on the QHY178M). Reaching this deadline means the SDK is genuinely stuck, and
/// the caller then declines to close the handle rather than close it unsafely.
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a single wait between `GetQHYCCDExposureRemaining` polls. Bounds how
/// stale the camera-side confirmation can get without making a long exposure
/// chatter over USB — a cancel does not wait for it, since `cancel_signal` wakes
/// the sleep immediately.
const EXPOSURE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the capture task keeps asking the camera whether the exposure is
/// over before entering the readout anyway. A camera that never reports 0 must
/// not strand the frame forever; entering the readout is what the driver did
/// unconditionally before, so this is a bounded fallback, not a regression.
const EXPOSURE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-device runtime state: caches populated at connect plus the exposure state
/// machine. Atomics for the hot/simple flags; `parking_lot::Mutex` for the
/// `Option<…>` caches and the captured image. Locks are never held across an
/// `await`.
#[derive(Debug)]
struct DeviceState {
    /// Current symmetric bin (init 1).
    bin: AtomicU8,
    valid_bins: Mutex<Vec<u8>>,
    ccd_info: Mutex<Option<CachedCcdInfo>>,
    /// Intended ROI in *binned* pixel coordinates (rescaled on bin change).
    intended_roi: Mutex<Option<CCDChipArea>>,
    exposure_range_us: Mutex<Option<(f64, f64, f64)>>,
    /// Gain range in ASCOM's own width, converted once at connect (see
    /// [`cache_range`]). `None` means the control is not advertised — either the
    /// model lacks it, or its range has no `i32` spelling.
    gain_min_max: Mutex<Option<(i32, i32)>>,
    /// Offset range, on the same terms as [`DeviceState::gain_min_max`].
    offset_min_max: Mutex<Option<(i32, i32)>>,
    target_temperature: Mutex<Option<f64>>,
    /// Tracked independently of the SDK's `CurPWM` readback: neither real
    /// hardware nor the simulation backend updates `CurPWM` synchronously
    /// when `ControlType::Cooler` (auto-regulation) is (re-)asserted, and a
    /// settled regulation loop can legitimately read back 0% PWM while still
    /// engaged.
    cooler_engaged: AtomicBool,

    exposure_in_flight: AtomicBool,
    image_ready: AtomicBool,
    /// Bumped on each start / abort / disconnect so a late-completing capture
    /// task can tell it has been superseded and discard its result.
    exposure_generation: AtomicU64,
    /// Asks the capture task to stop *before* it enters the uninterruptible
    /// readout. Checked only between the task's phases — never mid-readout.
    cancel_requested: AtomicBool,
    /// Wakes the capture task's exposure wait the instant a cancel is requested,
    /// so abort latency tracks the readout rather than the exposure length.
    cancel_signal: tokio::sync::Notify,
    expected_duration_us: AtomicU64,
    last_exposure_start_time: Mutex<Option<SystemTime>>,
    last_exposure_duration: Mutex<Option<Duration>>,
    last_image: Mutex<Option<ImageArray>>,
    /// Set on a mid-exposure SDK failure → `CameraState::Error` (E9).
    last_error: Mutex<Option<String>>,
    /// Serializes the capture task's "check generation + commit result" against
    /// `cancel_exposure`'s "bump generation + clear `image_ready`", so an abort
    /// landing at the wrong instant can't leave a stale `ImageReady = true`.
    result_lock: Mutex<()>,
    /// Notified the instant the detached capture task clears
    /// `exposure_in_flight`. Lets `disconnect` (and tests) await the drain on a
    /// deadline via `tokio::time::timeout` instead of a polling sleep loop —
    /// busy-waits have bitten us with scheduler stalls under load.
    exposure_drained: tokio::sync::Notify,
}

/// Cached sensor geometry. `image_width`/`image_height` track the active readout
/// mode (mutated by `set_readout_mode`); the rest is fixed at connect.
#[derive(Debug, Clone, Copy)]
struct CachedCcdInfo {
    image_width: u32,
    image_height: u32,
    pixel_width: f64,
    pixel_height: f64,
    bits_per_pixel: u32,
}

impl DeviceState {
    fn new() -> Self {
        Self {
            bin: AtomicU8::new(1),
            valid_bins: Mutex::new(Vec::new()),
            ccd_info: Mutex::new(None),
            intended_roi: Mutex::new(None),
            exposure_range_us: Mutex::new(None),
            gain_min_max: Mutex::new(None),
            offset_min_max: Mutex::new(None),
            target_temperature: Mutex::new(None),
            cooler_engaged: AtomicBool::new(false),
            exposure_in_flight: AtomicBool::new(false),
            image_ready: AtomicBool::new(false),
            exposure_generation: AtomicU64::new(0),
            cancel_requested: AtomicBool::new(false),
            cancel_signal: tokio::sync::Notify::new(),
            expected_duration_us: AtomicU64::new(0),
            last_exposure_start_time: Mutex::new(None),
            last_exposure_duration: Mutex::new(None),
            last_image: Mutex::new(None),
            last_error: Mutex::new(None),
            result_lock: Mutex::new(()),
            exposure_drained: tokio::sync::Notify::new(),
        }
    }

    /// Reset the exposure state machine to a clean idle state. Called on connect
    /// so a stale `Error` / `ImageReady` / image from a previous session does not
    /// survive a reconnect (C3).
    fn reset_exposure_state(&self) {
        let _guard = self.result_lock.lock();
        self.exposure_generation.fetch_add(1, Ordering::AcqRel);
        self.exposure_in_flight.store(false, Ordering::Release);
        self.cancel_requested.store(false, Ordering::Release);
        self.image_ready.store(false, Ordering::Release);
        self.expected_duration_us.store(0, Ordering::Release);
        *self.last_image.lock() = None;
        *self.last_error.lock() = None;
        *self.last_exposure_start_time.lock() = None;
        *self.last_exposure_duration.lock() = None;
    }
}

/// One ASCOM Camera device per discovered QHY camera.
#[derive(Clone, derive_more::Debug)]
pub struct QhyCameraDevice {
    #[debug(skip)]
    handle: Arc<dyn CameraHandle>,
    unique_id: String,
    name: String,
    description: String,
    state: Arc<DeviceState>,
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<QhyCameraDriver>>,
    /// How long an abort/disconnect waits for the capture task to leave the SDK
    /// ([`CAPTURE_DRAIN_TIMEOUT`]). A field so tests can shorten it and exercise
    /// the refuse-to-close branch without a 30 s wait.
    drain_timeout: Duration,
}

impl QhyCameraDevice {
    /// Build a device from an SDK handle and an optional per-serial config
    /// override. The ASCOM `UniqueID` is the SDK serial; `name`/`description`
    /// fall back to SDK-derived defaults.
    pub fn new(handle: Arc<dyn CameraHandle>, overrides: Option<&DeviceOverride>) -> Self {
        let id = handle.id();
        let name = overrides
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| id.clone());
        let description = overrides
            .and_then(|o| o.description.clone())
            .unwrap_or_else(|| "QHYCCD camera".to_string());
        Self {
            handle,
            unique_id: id,
            name,
            description,
            state: Arc::new(DeviceState::new()),
            config_ctx: None,
            drain_timeout: CAPTURE_DRAIN_TIMEOUT,
        }
    }

    /// Shorten the SDK drain deadline so a test can reach the refuse-to-close
    /// branch without waiting out [`CAPTURE_DRAIN_TIMEOUT`].
    #[cfg(test)]
    const fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Attach config-action wiring (enables `config.get`/`apply`/`schema`).
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<QhyCameraDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        match self.handle.is_open() {
            Ok(true) => Ok(()),
            _ => Err(ASCOMError::NOT_CONNECTED),
        }
    }

    fn connect(&self) -> ASCOMResult<()> {
        // `handle.open()` refcounts the shared physical connection
        // (`backend::SharedCameraConnection`): the open + refcount transition is
        // atomic. The post-open handshake below is not serialized against a racing
        // connect on the same device, but it is idempotent (re-applies stream mode
        // / readout / cached geometry on the shared handle), so a redundant run
        // from a concurrent connect is harmless.
        self.handle.open().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        // If any step of the post-open handshake fails, close the handle before
        // propagating so a failed connect leaves Connected == false (C2) rather
        // than an opened-but-unusable camera.
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

    /// The open → single-frame → readout-mode-0 → init → 16-bit → cache handshake,
    /// run after `open()`. Caches CCD info, effective area, valid binning modes,
    /// and the exposure/gain/offset limits.
    fn open_handshake(&self) -> ASCOMResult<()> {
        let h = &self.handle;
        let nc = |_e: BackendError| ASCOMError::NOT_CONNECTED;
        if h.is_control_available(ControlType::CamSingleFrameMode)
            .is_none()
        {
            warn!("camera does not advertise single-frame mode");
            return Err(ASCOMError::NOT_CONNECTED);
        }
        h.set_stream_mode_single().map_err(nc)?;
        h.set_readout_mode(0).map_err(nc)?;
        h.init().map_err(nc)?;
        // Best-effort 16-bit transfer; not every model exposes the control.
        if let Err(e) = h.set_transfer_bit_16() {
            debug!(error = %e, "16-bit transfer not set");
        }

        let ccd = h.get_ccd_info().map_err(nc)?;
        *self.state.ccd_info.lock() = Some(CachedCcdInfo {
            image_width: ccd.image_width,
            image_height: ccd.image_height,
            pixel_width: ccd.pixel_width,
            pixel_height: ccd.pixel_height,
            bits_per_pixel: ccd.bits_per_pixel,
        });
        let area = h.get_effective_area().map_err(nc)?;
        *self.state.intended_roi.lock() = Some(area);
        *self.state.valid_bins.lock() = self.valid_binning_modes();

        let exposure = h.exposure_range_us().map_err(nc)?;
        *self.state.exposure_range_us.lock() = Some(exposure);

        let gain_range = if h.is_control_available(ControlType::Gain).is_some() {
            let (min, max, _) = h.gain_range().map_err(nc)?;
            Some((min, max))
        } else {
            None
        };
        cache_range(&self.state.gain_min_max, "gain", gain_range);

        let offset_range = if h.is_control_available(ControlType::Offset).is_some() {
            let (min, max, _) = h.offset_range().map_err(nc)?;
            Some((min, max))
        } else {
            None
        };
        cache_range(&self.state.offset_min_max, "offset", offset_range);

        self.state.bin.store(1, Ordering::Release);
        Ok(())
    }

    /// Wait until no capture task is in flight, bounded by `timeout`. Returns
    /// `true` once `exposure_in_flight` is observed clear, `false` on timeout.
    ///
    /// Event-driven, not a polling sleep loop: the detached `run_exposure` task
    /// fires `exposure_drained` the instant it clears the flag, and we await that
    /// against a single `tokio::time::timeout` deadline. Uses the canonical tokio
    /// `Notify` pattern — pin the `Notified` future and `enable()` it *before*
    /// re-checking the flag — so a drain landing between the check and the await
    /// can never be lost, and a spurious/stale wakeup just re-checks the flag.
    async fn wait_until_drained(&self, timeout: Duration) -> bool {
        let drained = async {
            let notified = self.state.exposure_drained.notified();
            tokio::pin!(notified);
            loop {
                notified.as_mut().enable();
                if !self.state.exposure_in_flight.load(Ordering::Acquire) {
                    return;
                }
                notified.as_mut().await;
                notified.set(self.state.exposure_drained.notified());
            }
        };
        tokio::time::timeout(timeout, drained).await.is_ok()
    }

    async fn disconnect(&self) -> ASCOMResult<()> {
        // An in-flight exposure is cancelled (C3) before the handle closes, and
        // `cancel_exposure` returns only once the capture task is out of the SDK.
        if !self.cancel_exposure().await {
            // CRITICAL: never close a handle a capture task may still be using.
            // Closing frees it under a live USB transfer — a use-after-free that
            // trips libusb's `usbi_mutex_lock` assertion and can corrupt the SDK's
            // shared libusb context. A failed disconnect is the lesser evil; the
            // device stays logically connected and a later disconnect can retry.
            warn!(camera = %self.unique_id, "capture task still inside the SDK; refusing to close the handle");
            return Err(ASCOMError::invalid_operation(
                "an exposure is still inside the SDK; the handle cannot be closed safely",
            ));
        }
        // Refcounted close (`backend::SharedCameraConnection`): when a CFW device
        // shares this camera's SDK id, the physical handle is closed only once
        // both devices have disconnected, so disconnecting the camera no longer
        // breaks a concurrently-connected filter wheel. See the design doc.
        self.handle.close().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        debug!(camera = %self.unique_id, "camera disconnected");
        Ok(())
    }

    /// Cancel any in-flight exposure and leave the device quiescent. Returns
    /// `false` if the capture task could not be got out of the SDK, in which case
    /// no SDK cancel was issued and the handle must not be closed.
    ///
    /// **Ordering is the whole point.** `CancelQHYCCDExposingAndReadout` is
    /// documented (`qhyccd.h`) as *"the camera does not send back the image data.
    /// Host software must not readout the data"*, so it may only be issued once
    /// nothing is inside `GetQHYCCDSingleFrame` — otherwise that call is left
    /// waiting on image data the camera has been told never to send. So this
    /// raises `cancel_requested`, wakes the capture task, waits for it to leave
    /// the SDK, and only then touches the device. indi-qhy keeps the same
    /// discipline: its `AbortExposure` blocks on the imaging thread leaving
    /// `StateExposure` before calling the SDK cancel.
    ///
    /// Deliberately does NOT clear `exposure_in_flight` — the capture task clears
    /// that once its blocking chain has drained, so a new exposure cannot start
    /// and race the still-running SDK calls (the design's "one logical owner per
    /// device").
    async fn cancel_exposure(&self) -> bool {
        if !self.state.exposure_in_flight.load(Ordering::Acquire) {
            return true;
        }
        {
            // Atomic with the capture task's commit (run_exposure) so an abort can
            // never be overwritten by a just-completing capture.
            let _guard = self.state.result_lock.lock();
            self.state
                .exposure_generation
                .fetch_add(1, Ordering::AcqRel);
            self.state.image_ready.store(false, Ordering::Release);
            *self.state.last_error.lock() = None;
        }
        // Ask the capture task to stop short of its readout, and wake it now so a
        // long exposure does not have to elapse first.
        self.state.cancel_requested.store(true, Ordering::Release);
        self.state.cancel_signal.notify_waiters();

        if !self.wait_until_drained(self.drain_timeout).await {
            warn!(
                camera = %self.unique_id,
                timeout = ?self.drain_timeout,
                "capture task still inside the SDK; withholding the SDK cancel \
                 rather than issuing it under a live readout"
            );
            return false;
        }

        // Re-claim the in-flight slot before touching the device. The capture task
        // released it as it drained, so without this a concurrent `start_exposure`
        // could slip in and have its brand-new exposure killed by the cancel below
        // (or, on the disconnect path, run against a handle about to close).
        // Losing the CAS means a newer exposure already owns the device, and this
        // cancel is no longer ours to issue.
        if self
            .state
            .exposure_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return true;
        }
        // Safe now: nothing is inside the SDK for this device. On a cancel taken
        // during the exposure this stops the integration; on one that arrived
        // during the readout the frame has already been read out and this is the
        // harmless pre-close reset the SDK's own SingleFrameSample performs.
        let handle = Arc::clone(&self.handle);
        match tokio::task::spawn_blocking(move || handle.abort_exposure_and_readout()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => debug!(error = %e, "abort_exposure_and_readout failed"),
            Err(e) => warn!(error = %e, "abort task panicked"),
        }
        self.state
            .exposure_in_flight
            .store(false, Ordering::Release);
        self.state.exposure_drained.notify_waiters();
        true
    }

    fn valid_binning_modes(&self) -> Vec<u8> {
        let mut bins = Vec::new();
        for (control, bin) in [
            (ControlType::CamBin1x1mode, 1u8),
            (ControlType::CamBin2x2mode, 2),
            (ControlType::CamBin3x3mode, 3),
            (ControlType::CamBin4x4mode, 4),
            (ControlType::CamBin6x6mode, 6),
            (ControlType::CamBin8x8mode, 8),
        ] {
            if self.handle.is_control_available(control).is_some() {
                bins.push(bin);
            }
        }
        bins
    }

    /// Validate the cached ROI against the binned sensor geometry (R2), returning
    /// the `CCDChipArea` to push to the SDK.
    fn validated_roi(&self) -> ASCOMResult<CCDChipArea> {
        let roi = (*self.state.intended_roi.lock())
            .ok_or_else(|| ASCOMError::invalid_value("no ROI defined for camera"))?;
        let ccd = (*self.state.ccd_info.lock()).ok_or(ASCOMError::VALUE_NOT_SET)?;
        let bin = u32::from(self.state.bin.load(Ordering::Acquire)).max(1);
        check_geometry(roi, ccd.image_width, ccd.image_height, bin)?;
        Ok(roi)
    }
}

/// QHY imposes **no** sub-frame alignment rule — contrast `zwo-camera` and
/// `svbony-camera`, which both require `NumX % 8 == 0` and `NumY % 2 == 0`.
/// That absence is the whole of this driver's geometry difference from the
/// other two; the rules themselves are shared.
const ALIGNMENT: Option<Alignment> = None;

/// The SDK's `CCDChipArea` as the shared geometry's [`Roi`], and back.
///
/// This driver keeps `CCDChipArea` in state because that is the type
/// `SetQHYCCDResolution` takes, while the shared rules speak a vendor-free
/// `Roi`. The two meet in these two functions rather than inside either of
/// them.
const fn to_roi(area: CCDChipArea) -> Roi {
    Roi {
        start_x: area.start_x,
        start_y: area.start_y,
        width: area.width,
        height: area.height,
    }
}

const fn from_roi(roi: Roi) -> CCDChipArea {
    CCDChipArea {
        start_x: roi.start_x,
        start_y: roi.start_y,
        width: roi.width,
        height: roi.height,
    }
}

/// Geometry validation shared by `validated_roi` (R2), as the ASCOM error a
/// client sees.
///
/// The rules, their order, and the message text all live in
/// `rusty-photon-camera-core`, shared with `zwo-camera` and `svbony-camera`,
/// as does the ASCOM code it becomes. What this driver contributes is
/// [`ALIGNMENT`] — which for QHY is the *absence* of a rule.
fn check_geometry(roi: CCDChipArea, ccd_w: u32, ccd_h: u32, bin: u32) -> ASCOMResult<()> {
    Ok(camera_core::check(
        to_roi(roi),
        ccd_w,
        ccd_h,
        bin,
        ALIGNMENT,
    )?)
}

/// A bin change rescales the cached ROI (B3); see `camera_core::rescale` for why a
/// sub-pixel extent clamps to 1 while a client-set 0 does not.
fn rescale_roi(roi: CCDChipArea, old: u8, new: u8) -> CCDChipArea {
    from_roi(camera_core::rescale(to_roi(roi), old, new))
}

/// The ASCOM spelling of a gain or offset bound the SDK reports as `f64`.
///
/// The QHY SDK carries every control through one `f64` parameter, but a gain
/// bound is an integer underneath and ASCOM's `Gain`/`GainMin`/`GainMax` are
/// `i32`. Rounding to nearest recovers the integer the SDK meant whichever side
/// the float representation lands on; truncating would turn a 100 that arrives
/// as 99.999… into 99 and advertise a maximum one below the one the camera
/// accepts.
///
/// `None` is the case a cast has to invent a value for: a bound outside `i32`
/// is a control ASCOM has no vocabulary for. NaN lands there too — a range
/// check written as two comparisons would let it through to a `0` bound.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "no TryFrom<f64> for i32 exists; the range check is what makes this total"
)]
fn ascom_bound(value: f64) -> Option<i32> {
    let rounded = value.round();
    if (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&rounded) {
        Some(rounded as i32)
    } else {
        None
    }
}

/// Cache a control's ASCOM-describable range, or clear it and say why.
///
/// `range` is `None` when the control is not advertised at all. Either way this
/// **always writes**, because the cache is what the accessors gate on: it has to
/// describe the camera on *this* connect, not the last one. A control that has
/// gone away, or whose bounds this connect cannot name, must clear the cell
/// rather than leave a previous session's bounds standing — the same reconnect
/// hygiene as C3.
///
/// An unset cell reports `NOT_IMPLEMENTED` for the control, rather than
/// advertising a clamped bound the camera would then reject.
fn cache_range(cell: &Mutex<Option<(i32, i32)>>, control: &str, range: Option<(f64, f64)>) {
    let cached = if let Some((min, max)) = range {
        if let (Some(min), Some(max)) = (ascom_bound(min), ascom_bound(max)) {
            debug!(control, min, max, "cached control range");
            Some((min, max))
        } else {
            warn!(
                control,
                min, max, "range has no i32 spelling; not advertised"
            );
            None
        }
    } else {
        debug!(control, "control not available; not advertised");
        None
    };
    *cell.lock() = cached;
}

/// `MaxADU = 2^bits - 1` (e.g. 65535 for a 16-bit sensor), saturating.
fn max_adu_from_bits(bits: u32) -> u32 {
    2u32.checked_pow(bits)
        .map_or(u32::MAX, |full| full.saturating_sub(1))
}

/// Bayer-pattern → ASCOM `BayerOffsetX/Y`.
///
/// The SDK spells the quad out in full, so this maps four names onto the same
/// four; where the red photosite then sits is the shared crate's rule.
const fn bayer_offsets(mode: BayerPattern) -> (u8, u8) {
    match mode {
        BayerPattern::GBRG => camera_core::BayerPattern::Gbrg,
        BayerPattern::GRBG => camera_core::BayerPattern::Grbg,
        BayerPattern::BGGR => camera_core::BayerPattern::Bggr,
        BayerPattern::RGGB => camera_core::BayerPattern::Rggb,
    }
    .offsets()
}

/// Convert a single-plane SDK frame into an ASCOM `ImageArray` with `[x][y]`
/// axis order (ASCOM stores width-major).
///
/// This driver's share is deciding which frames are unpackable at all — the
/// SDK reports a channel count and a bit depth rather than a format enum — and
/// naming the depth in the message. The unpack itself is
/// `rusty-photon-camera-core`'s, shared with `zwo-camera` and `svbony-camera`.
fn to_image_array(image: ImageData) -> Result<ImageArray, String> {
    if image.channels != 1 {
        return Err(format!("unsupported channel count {}", image.channels));
    }
    let depth = match image.bits_per_pixel {
        8 => PixelDepth::Eight,
        16 => PixelDepth::Sixteen,
        other => return Err(format!("unsupported bit depth {other}")),
    };
    camera_core::to_image_array(image.data, image.width, image.height, depth)
        .map_err(|error| format!("{}-bit {error}", image.bits_per_pixel))
}

/// What a capture attempt produced. `Cancelled` is a first-class outcome, not an
/// error: an aborted frame leaves the device idle with nothing to report.
enum Capture {
    Frame(ImageData),
    Cancelled,
    Failed(String),
}

/// Sleep for `duration`, waking early if a cancel is requested. Returns `false`
/// if the capture should stop. Uses the canonical tokio `Notify` pattern (pin,
/// `enable()`, then re-check) so a cancel landing between the check and the
/// await is never lost.
async fn sleep_unless_cancelled(state: &DeviceState, duration: Duration) -> bool {
    let notified = state.cancel_signal.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if state.cancel_requested.load(Ordering::Acquire) {
        return false;
    }
    tokio::select! {
        () = tokio::time::sleep(duration) => !state.cancel_requested.load(Ordering::Acquire),
        () = notified => false,
    }
}

/// Wait for the exposure to finish integrating, returning `false` if a cancel
/// arrived first. This is the *only* window in which an abort takes effect, and
/// it spans everything except the readout.
///
/// Timing is host-side first (so a long exposure costs no USB traffic at all),
/// after which the camera's own `GetQHYCCDExposureRemaining` has to agree before
/// we commit to the uninterruptible readout — if our clock ran ahead,
/// `get_single_frame` would block inside the readout for the remainder,
/// re-opening the very window this split exists to close.
async fn wait_for_exposure(handle: &Arc<dyn CameraHandle>, state: &DeviceState) -> bool {
    let expected = Duration::from_micros(state.expected_duration_us.load(Ordering::Acquire));
    if !sleep_unless_cancelled(state, expected).await {
        return false;
    }
    // A deadline this far out cannot overflow the clock; falling back to `now`
    // if it somehow did just skips straight to the readout, which blocks until
    // the frame really is ready.
    let now = tokio::time::Instant::now();
    let deadline = now.checked_add(EXPOSURE_CONFIRM_TIMEOUT).unwrap_or(now);
    while tokio::time::Instant::now() < deadline {
        let polled = Arc::clone(handle);
        let Ok(Ok(remaining)) =
            tokio::task::spawn_blocking(move || polled.get_remaining_exposure_us()).await
        else {
            // A failed or panicking poll is not worth abandoning the frame
            // over: fall through to the readout, which blocks until the frame
            // really is ready.
            break;
        };
        if remaining == 0 {
            break;
        }
        let nap = Duration::from_micros(u64::from(remaining)).min(EXPOSURE_POLL_INTERVAL);
        if !sleep_unless_cancelled(state, nap).await {
            return false;
        }
    }
    // Re-check: a cancel may have landed while the last poll was in flight.
    !state.cancel_requested.load(Ordering::Acquire)
}

/// Run one capture in three phases — start, a *cancellable* wait, then an
/// *uninterruptible* readout.
///
/// The split exists because `CancelQHYCCDExposingAndReadout` tells the camera not
/// to send the frame, and `qhyccd.h` requires that the host then not read it out.
/// So the readout is entered only once we know no cancel is outstanding, and once
/// entered it runs to completion; `cancel_exposure` waits for that before it
/// touches the device. Each blocking SDK call gets its own `spawn_blocking`, so
/// no runtime worker is parked across the exposure.
async fn capture_once(handle: &Arc<dyn CameraHandle>, state: &DeviceState) -> Capture {
    let starter = Arc::clone(handle);
    match tokio::task::spawn_blocking(move || starter.start_single_frame_exposure()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Capture::Failed(e.0),
        Err(e) => return Capture::Failed(format!("exposure task failed: {e}")),
    }

    if !wait_for_exposure(handle, state).await {
        return Capture::Cancelled;
    }

    let reader = Arc::clone(handle);
    match tokio::task::spawn_blocking(move || -> Result<ImageData, BackendError> {
        let size = reader.get_image_size()?;
        reader.get_single_frame(size)
    })
    .await
    {
        Ok(Ok(image)) => Capture::Frame(image),
        Ok(Err(e)) => Capture::Failed(e.0),
        Err(e) => Capture::Failed(format!("exposure task failed: {e}")),
    }
}

/// The detached capture task: runs one capture, then stores the image (or
/// records the failure as the `Error` state) — unless a newer generation has
/// superseded it.
async fn run_exposure(handle: Arc<dyn CameraHandle>, state: Arc<DeviceState>, generation: u64) {
    let result = capture_once(&handle, &state).await;

    // Commit the outcome under the result lock so this "check generation +
    // record" is atomic against cancel_exposure's "bump generation + clear
    // image_ready" — an abort can never be overwritten by a just-completing
    // capture. (No await is held across the lock: the capture is awaited above.)
    {
        let _guard = state.result_lock.lock();
        // Discard silently if a newer start / abort / disconnect superseded us.
        if state.exposure_generation.load(Ordering::Acquire) == generation {
            match result {
                Capture::Frame(image) => match to_image_array(image) {
                    Ok(array) => {
                        *state.last_image.lock() = Some(array);
                        *state.last_error.lock() = None;
                        state.image_ready.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to transform captured image");
                        *state.last_image.lock() = None;
                        *state.last_error.lock() = Some(format!("image transform failed: {e}"));
                    }
                },
                // A cancel that beat the generation bump: nothing to record, and
                // `cancel_exposure` has already cleared `image_ready`.
                Capture::Cancelled => {}
                Capture::Failed(e) => {
                    warn!(error = %e, "mid-exposure SDK error");
                    *state.last_error.lock() = Some(e);
                }
            }
        }
    }
    // The CAS in start_exposure guarantees only one capture task runs at a time,
    // so this task owns clearing the flag once its SDK chain has fully drained —
    // even when superseded. Until it does, a new start_exposure is rejected.
    state.exposure_in_flight.store(false, Ordering::Release);
    // Wake any deadline-bounded waiter (abort/disconnect drain, tests) now that
    // the SDK calls have fully returned and the handle is safe to close.
    state.exposure_drained.notify_waiters();
}

#[async_trait::async_trait]
impl Device for QhyCameraDevice {
    fn static_name(&self) -> &str {
        &self.name
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        // A `Connected` GET must be a safe boolean so health/management polling
        // never throws (matches every sibling driver, e.g. zwo-camera /
        // sky-survey-camera / pa-falcon-rotator). Report `false` if the seam ever
        // fails rather than erroring. `is_open()` is infallible in every current
        // backend (it reads an atomic), so the fallback is purely defensive — the
        // *mutating* `set_connected` below intentionally still propagates the error,
        // since a misread there would drive a wrong open/close.
        Ok(self.handle.is_open().unwrap_or_else(|e| {
            debug!(camera = %self.unique_id, error = %e, "is_open() failed; reporting disconnected");
            false
        }))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        let current = self
            .handle
            .is_open()
            .map_err(|_| ASCOMError::NOT_CONNECTED)?;
        if current == connected {
            return Ok(());
        }
        if connected {
            self.connect()
        } else {
            self.disconnect().await
        }
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.description.clone())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon qhy-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<QhyCameraDriver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait::async_trait]
impl Camera for QhyCameraDevice {
    // --- geometry ---------------------------------------------------------------

    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.ccd_info.lock())
            .map(|c| c.image_width)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        (*self.state.ccd_info.lock())
            .map(|c| c.image_height)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        (*self.state.ccd_info.lock())
            .map(|c| c.pixel_width)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        (*self.state.ccd_info.lock())
            .map(|c| c.pixel_height)
            .ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        // MaxADU is the largest value a client can actually receive in `ImageArray`
        // — which is governed by the *transfer container* depth, not the sensor's
        // ADC depth. The driver forces a 16-bit container at connect
        // (`set_transfer_bit_16`), and the SDK left-shifts each sensor reading to
        // fill it: on hardware the 12-bit IMX290 returns values up to 0xFFF0 and
        // the 14-bit IMX178 up to 0xFFFC, both quantised in steps of
        // 2^(16 - sensor_bits). So the container max (65535 for 16-bit) is correct.
        //
        // `OutputDataActualBits` is therefore the *wrong* source: it reports the
        // sensor ADC depth (which would give 4095 on the IMX290 — 16x too small —
        // and 0 on the QHY5III715C, whose firmware reports actual-bits = 0). Use
        // the container depth cached from `GetQHYCCDChipInfo` (8-bit if a model
        // ever rejects the 16-bit transfer), defaulting to 16 so MaxADU is never 0.
        let container_bits = (*self.state.ccd_info.lock())
            .map(|c| c.bits_per_pixel)
            .filter(|&b| b > 0)
            .unwrap_or(16);
        Ok(max_adu_from_bits(container_bits))
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        self.ensure_connected()?;
        self.unique_id
            .split('-')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ASCOMError::invalid_operation("could not derive sensor name"))
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
        let valid = self.state.valid_bins.lock().clone();
        if !valid.contains(&bin_x) {
            return Err(ASCOMError::invalid_value(format!(
                "bin {bin_x} is not a supported binning mode"
            )));
        }
        let old = self.state.bin.load(Ordering::Acquire);
        if old == bin_x {
            return Ok(());
        }
        self.handle
            .set_bin_mode(u32::from(bin_x), u32::from(bin_x))
            .map_err(|e| {
                ASCOMError::invalid_operation(format!("failed to set binning mode: {e}"))
            })?;
        {
            let mut roi = self.state.intended_roi.lock();
            if let Some(area) = *roi {
                *roi = Some(rescale_roi(area, old, bin_x));
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
        self.state
            .valid_bins
            .lock()
            .iter()
            .copied()
            .max()
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
        *roi = Some(CCDChipArea {
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
        *roi = Some(CCDChipArea {
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
        *roi = Some(CCDChipArea { start_x, ..area });
        drop(roi);
        Ok(())
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        let mut roi = self.state.intended_roi.lock();
        let area = (*roi).ok_or(ASCOMError::INVALID_VALUE)?;
        *roi = Some(CCDChipArea { start_y, ..area });
        drop(roi);
        Ok(())
    }

    // --- exposure range ---------------------------------------------------------

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        let (min, _, _) =
            (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        // `Duration` takes the seconds directly, so the SDK's `f64` never has to
        // become an integer here; a value it cannot represent is an error rather
        // than a saturated number.
        Duration::try_from_secs_f64(min / 1_000_000.0).map_err(|_| ASCOMError::INVALID_VALUE)
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        let (_, max, _) =
            (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        Duration::try_from_secs_f64(max / 1_000_000.0).map_err(|_| ASCOMError::INVALID_VALUE)
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        self.ensure_connected()?;
        let (_, _, step) =
            (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
        Duration::try_from_secs_f64(step / 1_000_000.0).map_err(|_| ASCOMError::INVALID_VALUE)
    }

    // --- gain / offset ----------------------------------------------------------

    async fn gain(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        // The cache holds a range only for a control that is both available and
        // describable in ASCOM's width, so it answers both questions at once —
        // and without an SDK round-trip on every read.
        if self.state.gain_min_max.lock().is_none() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let raw = self
            .handle
            .gain()
            .map_err(|_| ASCOMError::INVALID_OPERATION)?;
        ascom_bound(raw)
            .ok_or_else(|| ASCOMError::invalid_operation(format!("camera reported gain {raw}")))
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
        self.handle
            .set_gain(f64::from(gain))
            .map_err(|_| ASCOMError::INVALID_OPERATION)
    }

    async fn offset(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        if self.state.offset_min_max.lock().is_none() {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let raw = self
            .handle
            .offset()
            .map_err(|_| ASCOMError::INVALID_OPERATION)?;
        ascom_bound(raw)
            .ok_or_else(|| ASCOMError::invalid_operation(format!("camera reported offset {raw}")))
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
        self.handle
            .set_offset(f64::from(offset))
            .map_err(|_| ASCOMError::INVALID_OPERATION)
    }

    // --- readout modes ----------------------------------------------------------

    async fn readout_mode(&self) -> ASCOMResult<usize> {
        self.ensure_connected()?;
        let mode = self
            .handle
            .get_readout_mode()
            .map_err(|_| ASCOMError::INVALID_OPERATION)?;
        // The SDK numbers modes in `u32`; ASCOM indexes `ReadoutModes` with a
        // `usize`.
        usize::try_from(mode).map_err(|_| ASCOMError::INVALID_OPERATION)
    }

    async fn readout_modes(&self) -> ASCOMResult<Vec<String>> {
        self.ensure_connected()?;
        let count = self
            .handle
            .get_number_of_readout_modes()
            .map_err(|_| ASCOMError::INVALID_OPERATION)?;
        // Capacity is only a hint, so a count too large to address just means no
        // preallocation — the loop below is bounded by that same count.
        let mut modes = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        for index in 0..count {
            modes.push(
                self.handle
                    .get_readout_mode_name(index)
                    .map_err(|_| ASCOMError::INVALID_OPERATION)?,
            );
        }
        Ok(modes)
    }

    async fn set_readout_mode(&self, readout_mode: usize) -> ASCOMResult<()> {
        self.ensure_connected()?;
        // An index the SDK's `u32` cannot hold is out of range by definition,
        // and the count check below is where that is reported.
        let mode = u32::try_from(readout_mode).unwrap_or(u32::MAX);
        let count = self
            .handle
            .get_number_of_readout_modes()
            .map_err(|_| ASCOMError::INVALID_VALUE)?;
        if mode >= count {
            return Err(ASCOMError::invalid_value(format!(
                "readout mode {readout_mode} out of range (0..{count})"
            )));
        }
        let (width, height) = self
            .handle
            .get_readout_mode_resolution(mode)
            .map_err(|_| ASCOMError::INVALID_VALUE)?;
        self.handle.set_readout_mode(mode).map_err(|e| {
            ASCOMError::invalid_operation(format!("failed to set readout mode: {e}"))
        })?;
        if let Some(info) = self.state.ccd_info.lock().as_mut() {
            info.image_width = width;
            info.image_height = height;
        }
        Ok(())
    }

    // --- sensor type / bayer ----------------------------------------------------

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::CamIsColor)
            .is_none()
        {
            return Ok(SensorType::Monochrome);
        }
        match self.handle.is_control_available(ControlType::CamColor) {
            Some(_) => Ok(SensorType::RGGB),
            None => Err(ASCOMError::INVALID_VALUE),
        }
    }

    async fn bayer_offset_x(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::CamIsColor)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let raw = self
            .handle
            .is_control_available(ControlType::CamColor)
            .ok_or(ASCOMError::INVALID_VALUE)?;
        let mode = BayerPattern::try_from(raw).map_err(|()| ASCOMError::INVALID_VALUE)?;
        Ok(bayer_offsets(mode).0)
    }

    async fn bayer_offset_y(&self) -> ASCOMResult<u8> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::CamIsColor)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let raw = self
            .handle
            .is_control_available(ControlType::CamColor)
            .ok_or(ASCOMError::INVALID_VALUE)?;
        let mode = BayerPattern::try_from(raw).map_err(|()| ASCOMError::INVALID_VALUE)?;
        Ok(bayer_offsets(mode).1)
    }

    // --- cooling ----------------------------------------------------------------

    async fn can_set_ccd_temperature(&self) -> ASCOMResult<bool> {
        Ok(self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_some())
    }

    async fn can_get_cooler_power(&self) -> ASCOMResult<bool> {
        self.can_set_ccd_temperature().await
    }

    async fn ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        self.handle
            .current_temperature_celsius()
            .map_err(|_| ASCOMError::INVALID_VALUE)
    }

    async fn set_ccd_temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let stored_target = *self.state.target_temperature.lock();
        if let Some(target) = stored_target {
            return Ok(target);
        }
        self.handle
            .current_temperature_celsius()
            .map_err(|_| ASCOMError::INVALID_VALUE)
    }

    async fn set_set_ccd_temperature(&self, set_ccd_temperature: f64) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if !(-273.15..=80.0).contains(&set_ccd_temperature) {
            return Err(ASCOMError::invalid_value(format!(
                "target temperature {set_ccd_temperature} outside [-273.15, 80]"
            )));
        }
        self.handle
            .set_target_temperature_celsius(set_ccd_temperature)
            .map_err(|_| ASCOMError::invalid_operation("failed to set target temperature"))?;
        *self.state.target_temperature.lock() = Some(set_ccd_temperature);
        Ok(())
    }

    async fn cooler_on(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        Ok(self.state.cooler_engaged.load(Ordering::Acquire))
    }

    async fn set_cooler_on(&self, cooler_on: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if cooler_on {
            // Engage the SDK's auto-regulation via `set_target_temperature_celsius`
            // (`ControlType::Cooler`) at the stored target — `set_manual_cooler_pwm`
            // (`ControlType::ManualPWM`) instead pins a fixed duty cycle and does
            // not regulate — falling back to the current CCD temperature if
            // SetCCDTemperature was never called.
            let cached_target = *self.state.target_temperature.lock();
            let target = match cached_target {
                Some(target) => target,
                None => self
                    .handle
                    .current_temperature_celsius()
                    .map_err(|_| ASCOMError::INVALID_VALUE)?,
            };
            self.handle
                .set_target_temperature_celsius(target)
                .map_err(|_| ASCOMError::invalid_operation("failed to set cooler state"))?;
            *self.state.target_temperature.lock() = Some(target);
        } else {
            self.handle
                .set_manual_cooler_pwm(0.0)
                .map_err(|_| ASCOMError::invalid_operation("failed to set cooler state"))?;
        }
        self.state
            .cooler_engaged
            .store(cooler_on, Ordering::Release);
        Ok(())
    }

    async fn cooler_power(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        if self
            .handle
            .is_control_available(ControlType::Cooler)
            .is_none()
        {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        let pwm = self
            .handle
            .cooler_power_raw()
            .map_err(|_| ASCOMError::INVALID_VALUE)?;
        Ok(pwm / 255.0 * 100.0)
    }

    // --- shutter / capability flags ---------------------------------------------

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        Ok(self
            .handle
            .is_control_available(ControlType::CamMechanicalShutter)
            .is_some())
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    // --- exposure state ---------------------------------------------------------

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        if self.state.last_error.lock().is_some() {
            return Ok(CameraState::Error);
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
        if !self.state.exposure_in_flight.load(Ordering::Acquire) {
            // Idle: 100 once a frame is ready, 0 in the Error state (so a camera
            // reporting CameraState::Error never also reports 100% complete).
            return Ok(if self.state.last_error.lock().is_some() {
                0
            } else {
                100
            });
        }
        // A zero expected duration has no ratio to report. `NonZeroU64` carries
        // that answer down into the division below, so the guard and the divisor
        // are one fact rather than two that could drift apart.
        let expected = self.state.expected_duration_us.load(Ordering::Acquire);
        let Some(expected) = NonZeroU64::new(expected) else {
            return Ok(0);
        };
        // `get_remaining_exposure_us` reads 0 both just-before the SDK exposure
        // actually begins and at completion; while still in flight, never report
        // 100 (that is reserved for the Idle/ready state above).
        let remaining = u64::from(
            self.handle
                .get_remaining_exposure_us()
                .map_err(|_| ASCOMError::invalid_operation("failed to read remaining exposure"))?,
        );
        let done = expected.get().saturating_sub(remaining);
        // Never 100 while in flight — that answer belongs to the Idle/ready
        // state above, and the cap is shared with the sibling drivers.
        Ok(camera_core::progress_percent(
            Duration::from_micros(done),
            Duration::from_micros(expected.get()),
        ))
    }

    async fn last_exposure_start_time(&self) -> ASCOMResult<SystemTime> {
        (*self.state.last_exposure_start_time.lock()).ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn last_exposure_duration(&self) -> ASCOMResult<Duration> {
        // Return the stored Duration as-is; round-tripping through secs_f64 only
        // introduces floating-point rounding (the value is already exact).
        (*self.state.last_exposure_duration.lock()).ok_or(ASCOMError::VALUE_NOT_SET)
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        self.ensure_connected()?;
        let last_error = self.state.last_error.lock().clone();
        if let Some(msg) = last_error {
            return Err(ASCOMError::new(UNSPECIFIED_ERROR, msg));
        }
        // ASCOM: `ImageArray` is only valid once `ImageReady` is true. Mirror the
        // `image_ready()` condition (a frame is committed and no exposure is in
        // flight) and error otherwise, so a client can never read a stale frame
        // from a previous exposure during a new capture or after an abort.
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

    // --- exposure control -------------------------------------------------------

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if self.state.exposure_in_flight.load(Ordering::Acquire) {
            return Err(ASCOMError::invalid_operation(
                "an exposure is already in flight",
            ));
        }
        if !light {
            // Dark/bias frames need the mechanical shutter closed. qhyccd-rs 0.1.9
            // exposes shutter *presence* (CamMechanicalShutter) but no open/close
            // actuation, so v0 cannot capture a true dark on any model — darks are
            // rejected. See docs/services/qhy-camera.md E4 / Future Work. The
            // simulated QHY178M-Simulated is shutterless.
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }

        let (min_us, max_us) = {
            let (min, max, _) =
                (*self.state.exposure_range_us.lock()).ok_or(ASCOMError::INVALID_VALUE)?;
            (min, max)
        };
        let exposure_us = (duration.as_secs_f64() * 1_000_000.0).round();
        if exposure_us < min_us || exposure_us > max_us {
            return Err(ASCOMError::invalid_value(format!(
                "exposure {exposure_us}us outside [{min_us}, {max_us}]"
            )));
        }

        let roi = self.validated_roi()?;

        // Claim the in-flight slot; lose the race → already exposing.
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
        let generation = self
            .state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        // Clear any cancel left over from the previous frame; the in-flight CAS
        // above means the previous capture task has already drained.
        self.state.cancel_requested.store(false, Ordering::Release);

        if let Err(e) = self.handle.set_roi(roi) {
            self.state
                .exposure_in_flight
                .store(false, Ordering::Release);
            return Err(ASCOMError::invalid_value(format!("failed to set ROI: {e}")));
        }
        if let Err(e) = self.handle.set_exposure_us(exposure_us) {
            self.state
                .exposure_in_flight
                .store(false, Ordering::Release);
            return Err(ASCOMError::invalid_operation(format!(
                "failed to set exposure time: {e}"
            )));
        }

        self.state.image_ready.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        *self.state.last_exposure_start_time.lock() = Some(SystemTime::now());
        *self.state.last_exposure_duration.lock() = Some(duration);
        // Exact microseconds from the `Duration` itself rather than a narrowed
        // copy of the float sent to the SDK.
        self.state.expected_duration_us.store(
            u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            Ordering::Release,
        );

        let handle = Arc::clone(&self.handle);
        let state = Arc::clone(&self.state);
        tokio::spawn(run_exposure(handle, state, generation));
        Ok(())
    }

    async fn abort_exposure(&self) -> ASCOMResult<()> {
        self.ensure_connected()?;
        // Returns only once the capture task is out of the SDK and the device has
        // been told to stop, so a client that aborts and immediately re-exposes
        // cannot collide with the previous frame's SDK calls.
        if !self.cancel_exposure().await {
            return Err(ASCOMError::invalid_operation(
                "the exposure could not be aborted; the SDK did not return",
            ));
        }
        Ok(())
    }

    async fn stop_exposure(&self) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::MockCameraHandle;
    use std::sync::atomic::Ordering;

    fn area(start_x: u32, start_y: u32, width: u32, height: u32) -> CCDChipArea {
        CCDChipArea {
            start_x,
            start_y,
            width,
            height,
        }
    }

    fn connected_device(handle: MockCameraHandle) -> QhyCameraDevice {
        let device = QhyCameraDevice::new(Arc::new(handle), None);
        device.connect().unwrap();
        device
    }

    /// Like [`connected_device`] but keeps a handle to the mock, so a test can
    /// inspect which SDK calls the driver made and when.
    fn connected_device_with_handle(
        handle: MockCameraHandle,
    ) -> (QhyCameraDevice, Arc<MockCameraHandle>) {
        let handle = Arc::new(handle);
        let device = QhyCameraDevice::new(Arc::<MockCameraHandle>::clone(&handle), None);
        device.connect().unwrap();
        (device, handle)
    }

    /// Blocks until the mock is actually executing its readout.
    ///
    /// Sleeping a fixed budget instead does not establish this: CI overcommits
    /// CPU deliberately (`.bazelrc`: `--local_resources=cpu=HOST_CPUS*2`), so a
    /// starved test can wake past the whole readout and assert against a window
    /// that has already closed. Pair with
    /// [`MockCameraHandle::hold_readout`](crate::backend::mock::MockCameraHandle::hold_readout)
    /// so the window cannot close while the test is inside it.
    async fn await_readout(handle: &MockCameraHandle) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !handle.is_in_readout() {
            assert!(
                std::time::Instant::now() < deadline,
                "the readout never started"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    // --- pure helpers -----------------------------------------------------------

    #[test]
    fn max_adu_is_two_pow_bits_minus_one() {
        assert_eq!(max_adu_from_bits(16), 65535);
        assert_eq!(max_adu_from_bits(8), 255);
        // Saturating, not panicking, at 32 bits.
        assert_eq!(max_adu_from_bits(32), u32::MAX);
    }

    #[test]
    fn a_bin_change_rescales_a_client_set_zero_into_the_error_it_earned() {
        // The rescale arithmetic and its full case list live in
        // `rusty-photon-camera-core`; what this pins is that the two halves
        // are wired together through the `CCDChipArea` conversion — a 0 the
        // client set survives the bin change, and `StartExposure` still answers
        // about that 0.
        let scaled = rescale_roi(area(0, 0, 0, 0), 1, 2);
        assert_eq!((scaled.width, scaled.height), (0, 0));
        let err = check_geometry(scaled, 3072, 2048, 2).unwrap_err();
        assert!(err.message.contains("greater than 0"), "{}", err.message);
    }

    #[test]
    fn geometry_imposes_no_alignment_rule() {
        // The rule set, its order, and the bounds arithmetic are the shared
        // crate's; what is this driver's is the *absence* of an alignment rule.
        // A ROI that both siblings reject as misaligned is valid here.
        check_geometry(area(0, 0, 100, 47), 3072, 2048, 1).unwrap();
        // The conversion carries all four fields, so a bound is still enforced.
        let err = check_geometry(area(3000, 0, 100, 48), 3072, 2048, 1).unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert!(err.message.contains("StartX + NumX"), "{}", err.message);
    }

    /// The offsets come from the shared crate, so these values are not a
    /// restatement of anything in this file — they pin the vendor mapping end
    /// to end. `GRBG` and `GBRG` are the pair worth having a test for: they are
    /// anagrams and their offsets are transposes of each other.
    #[test]
    fn bayer_offset_mapping() {
        assert_eq!(bayer_offsets(BayerPattern::RGGB), (0, 0));
        assert_eq!(bayer_offsets(BayerPattern::GBRG), (0, 1));
        assert_eq!(bayer_offsets(BayerPattern::GRBG), (1, 0));
        assert_eq!(bayer_offsets(BayerPattern::BGGR), (1, 1));
    }

    #[test]
    fn to_image_array_16bit_has_width_major_axes() {
        let image = ImageData {
            data: vec![0u8; 64 * 48 * 2],
            width: 64,
            height: 48,
            bits_per_pixel: 16,
            channels: 1,
        };
        let array = to_image_array(image).unwrap();
        // ASCOM [x][y]: first axis = width.
        assert_eq!(array.dim().0, 64);
        assert_eq!(array.dim().1, 48);
    }

    #[test]
    fn to_image_array_16bit_reads_the_wire_order() {
        // The camera puts 16-bit pixels on the wire low byte first, so `34 12`
        // is 0x1234. This pins the driver's route into the shared unpack; the
        // wire contract itself is pinned there.
        let mut data = vec![0u8; 64 * 48 * 2];
        data[0] = 0x34;
        data[1] = 0x12;
        let image = ImageData {
            data,
            width: 64,
            height: 48,
            bits_per_pixel: 16,
            channels: 1,
        };
        let array = to_image_array(image).unwrap();
        assert_eq!(array[(0, 0, 0)], 0x1234_i32);
    }

    #[test]
    fn to_image_array_rejects_multichannel() {
        let image = ImageData {
            data: vec![0u8; 64 * 48 * 4],
            width: 64,
            height: 48,
            bits_per_pixel: 16,
            channels: 4,
        };
        assert!(to_image_array(image).is_err());
    }

    #[test]
    fn to_image_array_8bit_has_width_major_axes() {
        let image = ImageData {
            data: vec![0u8; 64 * 48],
            width: 64,
            height: 48,
            bits_per_pixel: 8,
            channels: 1,
        };
        let array = to_image_array(image).unwrap();
        assert_eq!(array.dim().0, 64);
        assert_eq!(array.dim().1, 48);
    }

    #[test]
    fn to_image_array_rejects_undersized_buffers() {
        for bits in [8, 16] {
            let image = ImageData {
                data: vec![0u8; 10], // far too small for a 64×48 frame
                width: 64,
                height: 48,
                bits_per_pixel: bits,
                channels: 1,
            };
            let err = to_image_array(image).unwrap_err();
            assert!(
                err.contains("buffer too small"),
                "{bits}-bit undersized buffer must be rejected: {err}"
            );
        }
    }

    // --- device behaviour via the mock seam -------------------------------------

    #[tokio::test]
    async fn connect_caches_geometry_and_limits() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(device.camera_x_size().await.unwrap(), 3072);
        assert_eq!(device.camera_y_size().await.unwrap(), 2048);
        assert_eq!(device.max_adu().await.unwrap(), 65535);
        assert_eq!(device.max_bin_x().await.unwrap(), 2);
        assert!(!device.can_asymmetric_bin().await.unwrap());
        assert_eq!(device.sensor_type().await.unwrap(), SensorType::Monochrome);
        assert!(!device.has_shutter().await.unwrap());
    }

    #[tokio::test]
    async fn max_adu_is_container_depth_independent_of_actual_bits() {
        // MaxADU must track the 16-bit *transfer container* (GetQHYCCDChipInfo bpp,
        // 16 in the mock), never the sensor's OutputDataActualBits: the SDK
        // left-shifts raw data to fill the container (SDK manual §14). So whether
        // the sensor reports a real depth (14) or the QHY5III715C's bogus 0, MaxADU
        // is 65535 — and is never the 2^0 - 1 = 0 that ConformU flagged.
        for actual_bits in [0.0, 12.0, 14.0] {
            let handle = MockCameraHandle::default()
                .with_param(ControlType::OutputDataActualBits, actual_bits);
            let device = connected_device(handle);
            assert_eq!(
                device.max_adu().await.unwrap(),
                65535,
                "actual_bits={actual_bits} must not change the 16-bit container MaxADU"
            );
        }
    }

    #[tokio::test]
    async fn failed_connect_leaves_camera_closed() {
        // C2: a handshake failure after open() must not leave the camera open.
        let handle = MockCameraHandle::default();
        handle.fail_handshake.store(true, Ordering::SeqCst);
        let device = QhyCameraDevice::new(Arc::new(handle), None);
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn reconnect_clears_error_state() {
        // E9 puts the camera in Error; a disconnect + reconnect must clear it (C3).
        let mock = Arc::new(MockCameraHandle::default());
        mock.fail_single_frame.store(true, Ordering::SeqCst);
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(
            device.wait_until_drained(Duration::from_secs(30)).await,
            "capture task did not drain in time"
        );
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Error);

        device.set_connected(false).await.unwrap();
        mock.fail_single_frame.store(false, Ordering::SeqCst);
        device.set_connected(true).await.unwrap();
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
        assert!(!device.image_ready().await.unwrap());
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

    #[tokio::test]
    async fn gain_not_implemented_without_control() {
        let device =
            connected_device(MockCameraHandle::default().without_control(ControlType::Gain));
        let err = device.gain().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn color_sensor_reports_rggb_and_bayer_offsets() {
        // A colour model the mono simulation backend cannot exercise via BDD.
        let handle = MockCameraHandle::default()
            .with_control(ControlType::CamIsColor, 1)
            .with_control(ControlType::CamColor, BayerPattern::RGGB as u32);
        let device = connected_device(handle);
        assert_eq!(device.sensor_type().await.unwrap(), SensorType::RGGB);
        assert_eq!(device.bayer_offset_x().await.unwrap(), 0);
        assert_eq!(device.bayer_offset_y().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn shutter_model_reports_has_shutter() {
        let device = connected_device(
            MockCameraHandle::default().with_control(ControlType::CamMechanicalShutter, 1),
        );
        assert!(device.has_shutter().await.unwrap());
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
    }

    #[tokio::test]
    async fn cooler_on_reasserts_target_not_manual_pwm() {
        let mock = Arc::new(MockCameraHandle::default());
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();
        device.set_set_ccd_temperature(-10.0).await.unwrap();

        device.set_cooler_on(true).await.unwrap();

        assert_eq!(mock.param(ControlType::Cooler), Some(-10.0));
        // The mock mirrors a ControlType::ManualPWM write into CurPWM; it must
        // stay at its untouched default, proving set_cooler_on(true) never
        // wrote ControlType::ManualPWM.
        assert_eq!(mock.param(ControlType::CurPWM), Some(0.0));
    }

    #[tokio::test]
    async fn cooler_on_without_prior_target_falls_back_to_current_temperature() {
        let mock = Arc::new(MockCameraHandle::default());
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();

        device.set_cooler_on(true).await.unwrap();

        // MockCameraHandle::default() seeds CurTemp at 20.0.
        assert_eq!(mock.param(ControlType::Cooler), Some(20.0));
        assert_eq!(device.set_ccd_temperature().await.unwrap(), 20.0);
    }

    #[tokio::test]
    async fn cooler_off_clears_manual_pwm_and_engaged_state() {
        // Seed CurPWM nonzero, as if the cooler had been actively regulating,
        // so a subsequent drop to 0.0 is observable evidence of the
        // ControlType::ManualPWM = 0.0 write (the mock mirrors ManualPWM into
        // CurPWM; see MockCameraHandle::param).
        let mock = Arc::new(MockCameraHandle::default().with_param(ControlType::CurPWM, 50.0));
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();
        device.set_cooler_on(true).await.unwrap();
        assert!(device.cooler_on().await.unwrap());

        device.set_cooler_on(false).await.unwrap();

        assert!(!device.cooler_on().await.unwrap());
        assert_eq!(mock.param(ControlType::CurPWM), Some(0.0));
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
    async fn gain_min_max_reflect_cached_range() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(device.gain_min().await.unwrap(), 0);
        assert_eq!(device.gain_max().await.unwrap(), 100);
    }

    #[tokio::test]
    async fn a_control_that_vanishes_on_reconnect_stops_being_advertised() {
        // The accessors gate solely on the cache, so the cache has to describe
        // *this* connect: a range left over from the last one would advertise a
        // control the camera no longer has. Same reconnect hygiene as C3.
        let (device, handle) = connected_device_with_handle(MockCameraHandle::default());
        assert_eq!(device.gain_max().await.unwrap(), 100);

        device.set_connected(false).await.unwrap();
        handle.remove_control(ControlType::Gain);
        device.set_connected(true).await.unwrap();

        assert_eq!(
            device.gain_max().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        // The sibling control, still present, is untouched.
        assert_eq!(device.offset_max().await.unwrap(), 255);
    }

    #[test]
    fn ascom_bound_rounds_to_nearest_from_either_side() {
        // The SDK's `f64` carries an integer; float representation can land on
        // either side of it, and truncation only recovers one of them.
        assert_eq!(ascom_bound(99.999_999), Some(100));
        assert_eq!(ascom_bound(100.000_001), Some(100));
        assert_eq!(ascom_bound(100.4), Some(100));
        assert_eq!(ascom_bound(-0.6), Some(-1));
    }

    #[test]
    fn ascom_bound_has_no_answer_outside_i32() {
        // The case a cast had to invent a value for: `as` saturates here, which
        // would advertise `i32::MAX` as a gain the camera never offered.
        assert_eq!(ascom_bound(f64::from(i32::MAX) + 1.0), None);
        assert_eq!(ascom_bound(f64::from(i32::MIN) - 1.0), None);
        assert_eq!(ascom_bound(f64::INFINITY), None);
        // NaN compares false against both ends, so a range check written as two
        // comparisons would let it through to `NaN as i32` — which is 0.
        assert_eq!(ascom_bound(f64::NAN), None);
    }

    #[test]
    fn ascom_bound_keeps_the_extremes_it_can_name() {
        assert_eq!(ascom_bound(f64::from(i32::MAX)), Some(i32::MAX));
        assert_eq!(ascom_bound(f64::from(i32::MIN)), Some(i32::MIN));
    }

    #[tokio::test]
    async fn a_gain_range_outside_i32_leaves_the_control_unadvertised() {
        // Degrade rather than lie: a clamped bound would advertise a maximum the
        // camera then rejects.
        let device = connected_device(
            MockCameraHandle::default()
                .with_range(ControlType::Gain, (0.0, f64::from(i32::MAX) + 1.0, 1.0)),
        );
        for code in [
            device.gain().await.unwrap_err().code,
            device.gain_min().await.unwrap_err().code,
            device.gain_max().await.unwrap_err().code,
            device.set_gain(5).await.unwrap_err().code,
        ] {
            assert_eq!(code, ASCOMErrorCode::NOT_IMPLEMENTED);
        }
        // Offset is cached independently and is unaffected.
        assert_eq!(device.offset_max().await.unwrap(), 255);
    }

    #[tokio::test]
    async fn a_gain_the_camera_cannot_name_is_an_error_not_a_number() {
        let device = connected_device(
            MockCameraHandle::default().with_param(ControlType::Gain, f64::from(i32::MAX) + 1.0),
        );
        assert_eq!(
            device.gain().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn offset_round_trips_and_rejects_out_of_range() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(device.offset_min().await.unwrap(), 0);
        let max = device.offset_max().await.unwrap();
        assert_eq!(max, 255);
        device.set_offset(max).await.unwrap();
        assert_eq!(device.offset().await.unwrap(), max);
        assert_eq!(
            device.set_offset(max + 1).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn offset_not_implemented_without_control() {
        let device =
            connected_device(MockCameraHandle::default().without_control(ControlType::Offset));
        assert_eq!(
            device.offset().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            device.set_offset(5).await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn exposure_limits_reflect_cached_range() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(
            device.exposure_min().await.unwrap(),
            Duration::from_micros(1)
        );
        assert_eq!(
            device.exposure_max().await.unwrap(),
            Duration::from_hours(1)
        );
        assert_eq!(
            device.exposure_resolution().await.unwrap(),
            Duration::from_micros(1)
        );
    }

    #[tokio::test]
    async fn readout_modes_list_select_and_reject_out_of_range() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(
            device.readout_modes().await.unwrap(),
            vec!["Standard".to_string()]
        );
        assert_eq!(device.readout_mode().await.unwrap(), 0);
        device.set_readout_mode(0).await.unwrap();
        // Only one mode (0); selecting 1 is out of range.
        assert_eq!(
            device.set_readout_mode(1).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn sensor_name_is_the_serial_prefix() {
        // unique_id "SIM-QHY178M" → "SIM".
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(device.sensor_name().await.unwrap(), "SIM");
    }

    #[tokio::test]
    async fn device_metadata_reports_expected_strings() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(
            device.driver_info().await.unwrap(),
            "rusty-photon qhy-camera"
        );
        assert_eq!(
            device.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(device.description().await.unwrap(), "QHYCCD camera");
        // Delegates to rusty_photon_driver; exercise the path (always Ok).
        device.supported_actions().await.unwrap();
    }

    #[tokio::test]
    async fn capability_flags_are_fixed() {
        let device = connected_device(MockCameraHandle::default());
        assert!(device.can_abort_exposure().await.unwrap());
        assert!(!device.can_stop_exposure().await.unwrap());
        assert!(!device.can_pulse_guide().await.unwrap());
    }

    #[tokio::test]
    async fn cooling_capabilities_and_temperature_readback() {
        let device = connected_device(MockCameraHandle::default());
        assert!(device.can_get_cooler_power().await.unwrap());
        // CurTemp default is 20.0 °C on the simulated mono model.
        assert_eq!(device.ccd_temperature().await.unwrap(), 20.0);
    }

    #[tokio::test]
    async fn cooling_is_not_implemented_without_cooler_control() {
        let device =
            connected_device(MockCameraHandle::default().without_control(ControlType::Cooler));
        assert!(!device.can_set_ccd_temperature().await.unwrap());
        assert!(!device.can_get_cooler_power().await.unwrap());
        for code in [
            device.ccd_temperature().await.unwrap_err().code,
            device.cooler_on().await.unwrap_err().code,
            device.set_cooler_on(true).await.unwrap_err().code,
            device.cooler_power().await.unwrap_err().code,
            device
                .set_set_ccd_temperature(-10.0)
                .await
                .unwrap_err()
                .code,
        ] {
            assert_eq!(code, ASCOMErrorCode::NOT_IMPLEMENTED);
        }
    }

    #[tokio::test]
    async fn geometry_roi_and_bin_getters_report_cached_values() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(device.pixel_size_x().await.unwrap(), 2.4);
        assert_eq!(device.pixel_size_y().await.unwrap(), 2.4);
        // The intended ROI defaults to the full effective area at connect.
        assert_eq!(device.num_x().await.unwrap(), 3072);
        assert_eq!(device.num_y().await.unwrap(), 2048);
        assert_eq!(device.start_x().await.unwrap(), 0);
        assert_eq!(device.start_y().await.unwrap(), 0);
        assert_eq!(device.bin_x().await.unwrap(), 1);
        assert_eq!(device.bin_y().await.unwrap(), 1);
        assert_eq!(device.max_bin_y().await.unwrap(), 2);
        assert!(!device.can_asymmetric_bin().await.unwrap());
    }

    #[tokio::test]
    async fn roi_setters_round_trip() {
        let device = connected_device(MockCameraHandle::default());
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device.set_start_x(10).await.unwrap();
        device.set_start_y(20).await.unwrap();
        assert_eq!(device.num_x().await.unwrap(), 64);
        assert_eq!(device.num_y().await.unwrap(), 48);
        assert_eq!(device.start_x().await.unwrap(), 10);
        assert_eq!(device.start_y().await.unwrap(), 20);
    }

    #[tokio::test]
    async fn set_bin_y_mirrors_bin_x() {
        let device = connected_device(MockCameraHandle::default());
        device.set_bin_y(2).await.unwrap();
        assert_eq!(device.bin_x().await.unwrap(), 2);
        assert_eq!(device.bin_y().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn last_exposure_metadata_is_unset_before_first_exposure() {
        let device = connected_device(MockCameraHandle::default());
        assert_eq!(
            device.last_exposure_start_time().await.unwrap_err().code,
            ASCOMErrorCode::VALUE_NOT_SET
        );
        assert_eq!(
            device.last_exposure_duration().await.unwrap_err().code,
            ASCOMErrorCode::VALUE_NOT_SET
        );
    }

    #[tokio::test]
    async fn set_bin_surfaces_sdk_failure_as_invalid_operation() {
        let mock = Arc::new(MockCameraHandle::default());
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();
        mock.fail_set_controls.store(true, Ordering::SeqCst);
        // bin 2 is valid and differs from the current 1, so it reaches the SDK.
        assert_eq!(
            device.set_bin_x(2).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn set_readout_mode_surfaces_sdk_failure_as_invalid_operation() {
        let mock = Arc::new(MockCameraHandle::default());
        let device = QhyCameraDevice::new(mock.clone(), None);
        device.set_connected(true).await.unwrap();
        mock.fail_set_controls.store(true, Ordering::SeqCst);
        assert_eq!(
            device.set_readout_mode(0).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[tokio::test]
    async fn handshake_rejects_camera_without_single_frame_mode() {
        // open() succeeds, but a camera that doesn't advertise single-frame mode
        // can't be driven — connect must fail and leave it disconnected.
        let handle = MockCameraHandle::default().without_control(ControlType::CamSingleFrameMode);
        let device = QhyCameraDevice::new(Arc::new(handle), None);
        assert_eq!(
            device.set_connected(true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn bin_change_rescales_roi_and_rejects_unsupported() {
        let device = connected_device(MockCameraHandle::default());
        device.set_num_x(3072).await.unwrap();
        device.set_num_y(2048).await.unwrap();
        device.set_bin_x(2).await.unwrap();
        assert_eq!(device.bin_x().await.unwrap(), 2);
        assert_eq!(device.num_x().await.unwrap(), 1536);
        assert_eq!(device.num_y().await.unwrap(), 1024);
        assert_eq!(
            device.set_bin_x(99).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
    }

    #[tokio::test]
    async fn disconnected_start_exposure_is_not_connected() {
        let device = QhyCameraDevice::new(Arc::new(MockCameraHandle::default()), None);
        let err = device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn dark_frame_is_not_implemented() {
        let device = connected_device(MockCameraHandle::default());
        let err = device
            .start_exposure(Duration::from_millis(10), false)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
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
        // Wait (on a deadline, not a polling sleep) for the detached capture
        // task. The 30 s cap is sized for contended CI runners; the wait
        // returns the moment the task drains, so healthy runs never feel it.
        assert!(
            device.wait_until_drained(Duration::from_secs(30)).await,
            "capture task did not drain in time"
        );
        assert!(device.image_ready().await.unwrap());
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
        assert_eq!(device.percent_completed().await.unwrap(), 100);
        let image = device.image_array().await.unwrap();
        assert_eq!(image.dim().0, 64);
        assert_eq!(image.dim().1, 48);
    }

    #[tokio::test]
    async fn mid_exposure_error_transitions_to_error_state() {
        let handle = MockCameraHandle::default();
        handle.fail_single_frame.store(true, Ordering::SeqCst);
        let device = connected_device(handle);
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(
            device.wait_until_drained(Duration::from_secs(30)).await,
            "capture task did not drain in time"
        );
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Error);
        assert!(!device.image_ready().await.unwrap());
        assert_eq!(
            device.image_array().await.unwrap_err().code,
            UNSPECIFIED_ERROR
        );
    }

    #[tokio::test]
    async fn second_exposure_while_in_flight_is_rejected() {
        let device = connected_device(MockCameraHandle::default());
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        // Give the background task a moment to enter its exposure wait.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Exposing);
        let err = device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        device.abort_exposure().await.unwrap();
    }

    #[tokio::test]
    async fn stop_exposure_is_not_implemented() {
        let device = connected_device(MockCameraHandle::default());
        assert!(!device.can_stop_exposure().await.unwrap());
        assert_eq!(
            device.stop_exposure().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn image_array_errors_after_abort_instead_of_returning_stale_frame() {
        let device = connected_device(MockCameraHandle::default());
        // Long enough that the abort lands during the cancellable wait.
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        device.abort_exposure().await.unwrap();
        // No fresh frame is ready after an abort, so ImageArray must error
        // rather than hand back a stale image from a previous exposure.
        assert!(!device.image_ready().await.unwrap());
        assert_eq!(
            device.image_array().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    /// `qhyccd.h`: `CancelQHYCCDExposingAndReadout` means "the camera does not
    /// send back the image data. Host software must not readout the data." An
    /// abort taken while the camera is still integrating must therefore skip the
    /// readout entirely.
    #[tokio::test]
    async fn abort_during_the_exposure_skips_the_readout() {
        let (device, handle) = connected_device_with_handle(MockCameraHandle::default());
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        device.abort_exposure().await.unwrap();

        assert_eq!(
            handle.single_frame_calls.load(Ordering::SeqCst),
            0,
            "the readout must not run for an exposure aborted mid-integration"
        );
        assert!(
            handle.aborted.load(Ordering::SeqCst),
            "the SDK cancel must still reach the camera"
        );
        assert!(!device.image_ready().await.unwrap());
    }

    /// The mirror case: an abort arriving *during* the readout must wait for that
    /// readout to finish before the SDK cancel is issued, never land on top of it.
    /// This is the discipline indi-qhy keeps by blocking until its imaging thread
    /// leaves `StateExposure`.
    #[tokio::test]
    async fn abort_during_the_readout_waits_for_it_to_finish() {
        let handle = MockCameraHandle::default();
        handle.hold_readout();
        let (device, handle) = connected_device_with_handle(handle);
        let device = Arc::new(device);
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        await_readout(&handle).await;

        // The readout is held open, so the abort cannot complete until it is
        // released — no clock is involved in establishing that.
        let abort = {
            let device = Arc::clone(&device);
            tokio::spawn(async move { device.abort_exposure().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.aborted.load(Ordering::SeqCst),
            "the SDK cancel must not be issued while the readout is still running"
        );
        assert!(!abort.is_finished(), "abort returned without waiting");

        handle.release_readout();
        abort.await.unwrap().unwrap();

        assert!(
            !handle.aborted_during_readout.load(Ordering::SeqCst),
            "the SDK cancel was issued while a readout was in flight"
        );
        assert!(handle.aborted.load(Ordering::SeqCst));
    }

    /// If the capture task cannot be got out of the SDK, closing the handle would
    /// free it under a live USB transfer. Refusing to close is the safe outcome:
    /// no SDK cancel, no close, and an honest error to the client.
    #[tokio::test]
    async fn disconnect_refuses_to_close_while_a_readout_is_stuck() {
        let handle = MockCameraHandle::default();
        handle.hold_readout();
        let handle = Arc::new(handle);
        let device = QhyCameraDevice::new(Arc::<MockCameraHandle>::clone(&handle), None)
            .with_drain_timeout(Duration::from_millis(50));
        device.connect().unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        await_readout(&handle).await;

        let err = device.disconnect().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            !handle.aborted.load(Ordering::SeqCst),
            "no SDK cancel may be issued while the readout is still running"
        );
        assert!(
            handle.is_open().unwrap(),
            "the handle must stay open rather than close under a live transfer"
        );

        // Let the readout finish so the task does not outlive the test.
        handle.release_readout();
        assert!(device.wait_until_drained(Duration::from_secs(30)).await);
    }

    /// An exposure the SDK refuses to start is reported as the `Error` state,
    /// not left looking like a capture in progress.
    #[tokio::test]
    async fn a_failed_exposure_start_becomes_the_error_state() {
        let handle = MockCameraHandle::default();
        handle.fail_start.store(true, Ordering::SeqCst);
        let (device, handle) = connected_device_with_handle(handle);
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(device.wait_until_drained(Duration::from_secs(30)).await);

        assert_eq!(device.camera_state().await.unwrap(), CameraState::Error);
        assert!(!device.image_ready().await.unwrap());
        assert_eq!(
            handle.single_frame_calls.load(Ordering::SeqCst),
            0,
            "no readout may follow an exposure that never started"
        );
    }

    /// A camera that will not report its progress must not strand the frame: the
    /// driver falls through to the readout, which blocks until the data is ready.
    #[tokio::test]
    async fn a_failed_remaining_poll_falls_through_to_the_readout() {
        let handle = MockCameraHandle::default();
        handle.fail_remaining.store(true, Ordering::SeqCst);
        let device = connected_device(handle);
        device.set_num_x(64).await.unwrap();
        device.set_num_y(48).await.unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(device.wait_until_drained(Duration::from_secs(30)).await);

        assert!(device.image_ready().await.unwrap());
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
    }

    /// An SDK that refuses the cancel is logged, not propagated: the capture is
    /// already out of the SDK by then, so the device is still safe to close.
    #[tokio::test]
    async fn a_refused_sdk_cancel_still_completes_the_abort() {
        let handle = MockCameraHandle::default();
        handle.fail_abort.store(true, Ordering::SeqCst);
        let device = connected_device(handle);
        device
            .start_exposure(Duration::from_secs(5), true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        device.abort_exposure().await.unwrap();
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Idle);
        // Still closable: nothing is inside the SDK.
        device.disconnect().await.unwrap();
    }

    /// An SDK call that dies on its blocking thread must not strand the device:
    /// the slot has to clear and the drain has to fire, or a later disconnect
    /// would wait out its whole deadline for a task that is already gone.
    #[tokio::test]
    async fn a_panicking_readout_does_not_strand_the_exposure() {
        let handle = MockCameraHandle::default();
        handle.panic_in_readout.store(true, Ordering::SeqCst);
        let device = connected_device(handle);
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(
            device.wait_until_drained(Duration::from_secs(30)).await,
            "a panicking SDK call must still release the in-flight slot"
        );

        assert_eq!(device.camera_state().await.unwrap(), CameraState::Error);
        assert!(!device.image_ready().await.unwrap());
        device.disconnect().await.unwrap();
    }

    /// The abort mirror of the disconnect case: if the capture cannot be got out
    /// of the SDK, `AbortExposure` reports that rather than claiming success.
    #[tokio::test]
    async fn abort_reports_failure_when_the_sdk_will_not_return() {
        let handle = MockCameraHandle::default();
        handle.hold_readout();
        let handle = Arc::new(handle);
        let device = QhyCameraDevice::new(Arc::<MockCameraHandle>::clone(&handle), None)
            .with_drain_timeout(Duration::from_millis(50));
        device.connect().unwrap();
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();
        await_readout(&handle).await;

        let err = device.abort_exposure().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            !handle.aborted.load(Ordering::SeqCst),
            "no SDK cancel may be issued while the readout is still running"
        );

        handle.release_readout();
        assert!(device.wait_until_drained(Duration::from_secs(30)).await);
    }

    /// A camera that keeps reporting time remaining holds the driver in the
    /// cancellable wait — the abort still lands promptly and skips the readout.
    #[tokio::test]
    async fn a_camera_still_reporting_time_remaining_stays_cancellable() {
        let handle = MockCameraHandle::default();
        handle.set_remaining_exposure_us(500_000);
        let (device, handle) = connected_device_with_handle(handle);
        // Host-side timing is satisfied almost at once; the camera's own counter
        // is what keeps the task waiting.
        device
            .start_exposure(Duration::from_millis(10), true)
            .await
            .unwrap();

        // Wait for the poll loop to be entered rather than sleeping a fixed
        // span and asserting afterwards. The claim under test is that the
        // driver polls the camera's counter at all, not that a loaded host
        // schedules the spawned exposure within any particular budget — and
        // the mock reports a CONSTANT 500 ms remaining, so once the loop is
        // entered it stays entered and this cannot overshoot into a later
        // state.
        let entered = tokio::time::timeout(Duration::from_secs(30), async {
            while handle.remaining_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        assert!(
            entered,
            "the driver must have entered its poll loop, not passed straight \
             through on host-side timing"
        );
        assert_eq!(device.camera_state().await.unwrap(), CameraState::Exposing);

        device.abort_exposure().await.unwrap();
        assert_eq!(
            handle.single_frame_calls.load(Ordering::SeqCst),
            0,
            "the readout must not run while the camera says it is still exposing"
        );
    }
}

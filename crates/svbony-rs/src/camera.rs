//! `SVBony` camera enumeration and device handle.
//!
//! [`Sdk::cameras`] lists every connected camera's [`CameraInfo`] — including
//! its serial number, which (unlike ZWO's ASI cameras) arrives at
//! enumeration time, before the camera is opened — without opening it.
//! [`Sdk::open_camera`] opens a camera, returning a [`Camera`] RAII handle
//! that closes the device on drop. The handle covers property/capability
//! queries ([`Camera::property`] / [`Camera::property_ex`] /
//! [`Camera::control_caps`]), ROI ([`Camera::set_roi_format`]), controls
//! ([`Camera::control_value`] / [`Camera::set_control_value`], plus typed
//! convenience wrappers for gain/exposure/black-level/cooling), and the
//! **video-capture exposure model** — `SVBony`'s SDK has no snap-exposure API;
//! every exposure is a video frame ([`Camera::start_video_capture`] /
//! [`Camera::send_soft_trigger`] / [`Camera::get_video_data`]), plus ST4
//! guiding ([`Camera::pulse_guide`]). With the `simulation` feature a single
//! fabricated `SV605CC-Simulated` camera is presented and the SDK is never
//! called.

#[cfg(not(feature = "simulation"))]
use crate::ffi_util::{c_long_field, c_string_field};
#[cfg(not(feature = "simulation"))]
use crate::{svb_check, sys};
#[cfg(not(feature = "simulation"))]
use std::os::raw::{c_int, c_long};

use crate::{Error, Result, Sdk, SvbError};

/// Bayer colour-filter pattern (`SVB_BAYER_PATTERN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BayerPattern {
    /// `SVB_BAYER_RG`.
    Rg,
    /// `SVB_BAYER_BG`.
    Bg,
    /// `SVB_BAYER_GR`.
    Gr,
    /// `SVB_BAYER_GB`.
    Gb,
}

impl BayerPattern {
    #[cfg(not(feature = "simulation"))]
    #[must_use]
    const fn from_raw(v: i32) -> Self {
        match v {
            0 => Self::Rg,
            1 => Self::Bg,
            2 => Self::Gr,
            _ => Self::Gb,
        }
    }
}

/// Output image format (`SVB_IMG_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// `SVB_IMG_RAW8` — 8-bit raw (1 byte/pixel).
    Raw8,
    /// `SVB_IMG_RAW10` — 10-bit raw.
    Raw10,
    /// `SVB_IMG_RAW12` — 12-bit raw.
    Raw12,
    /// `SVB_IMG_RAW14` — 14-bit raw.
    Raw14,
    /// `SVB_IMG_RAW16` — 16-bit raw (2 bytes/pixel).
    Raw16,
    /// `SVB_IMG_Y8` — 8-bit luminance (1 byte/pixel).
    Y8,
    /// `SVB_IMG_Y10` — 10-bit luminance.
    Y10,
    /// `SVB_IMG_Y12` — 12-bit luminance.
    Y12,
    /// `SVB_IMG_Y14` — 14-bit luminance.
    Y14,
    /// `SVB_IMG_Y16` — 16-bit luminance.
    Y16,
    /// `SVB_IMG_RGB24` — 8-bit RGB (3 bytes/pixel).
    Rgb24,
    /// `SVB_IMG_RGB32` — 8-bit RGBA/BGRA (assumed 4 bytes/pixel).
    Rgb32,
}

impl ImageType {
    /// Bytes per pixel for this format.
    ///
    /// The verified SDK ground truth (`docs/plans/archive/svbony-camera.md`) gives
    /// the buffer-size formula only for 8-bit mono (`w*h`), 16-bit mono
    /// (`w*h*2`), and RGB24 (`w*h*3`). RAW10/12/14 and Y10/12/14 are
    /// **assumed** packed into 16-bit containers (2 bytes/pixel) — the
    /// common convention for sub-16-bit raw sensor readout — and RGB32 is
    /// **assumed** 4 bytes/pixel. Both assumptions need confirmation against
    /// a real SV605CC capture (Phase G, hardware validation); this is noted
    /// rather than silently resolved.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Raw8 | Self::Y8 => 1,
            Self::Raw10
            | Self::Raw12
            | Self::Raw14
            | Self::Raw16
            | Self::Y10
            | Self::Y12
            | Self::Y14
            | Self::Y16 => 2,
            Self::Rgb24 => 3,
            Self::Rgb32 => 4,
        }
    }

    #[cfg(not(feature = "simulation"))]
    const fn to_raw(self) -> i32 {
        match self {
            Self::Raw8 => 0,
            Self::Raw10 => 1,
            Self::Raw12 => 2,
            Self::Raw14 => 3,
            Self::Raw16 => 4,
            Self::Y8 => 5,
            Self::Y10 => 6,
            Self::Y12 => 7,
            Self::Y14 => 8,
            Self::Y16 => 9,
            Self::Rgb24 => 10,
            Self::Rgb32 => 11,
        }
    }

    #[cfg(not(feature = "simulation"))]
    const fn from_raw(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Raw8),
            1 => Some(Self::Raw10),
            2 => Some(Self::Raw12),
            3 => Some(Self::Raw14),
            4 => Some(Self::Raw16),
            5 => Some(Self::Y8),
            6 => Some(Self::Y10),
            7 => Some(Self::Y12),
            8 => Some(Self::Y14),
            9 => Some(Self::Y16),
            10 => Some(Self::Rgb24),
            11 => Some(Self::Rgb32),
            _ => None,
        }
    }
}

/// Safe view of `SVB_CAMERA_INFO`. Readable without opening the camera (via
/// [`Sdk::cameras`]); also cached on an open [`Camera`].
///
/// Unlike ZWO's ASI cameras, the serial number arrives at enumeration time —
/// no open-to-mint-identity dance is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// `CameraID` — the handle used by all per-camera SDK calls.
    pub id: i32,
    /// Model/friendly name, e.g. `"SV605CC"`.
    pub friendly_name: String,
    /// The camera's serial number, read pre-open from `CameraSN`.
    pub serial: String,
    /// Connection port type, e.g. `"USB3"`.
    pub port_type: String,
    /// Raw USB device id.
    pub device_id: u32,
}

/// Safe view of `SVB_CAMERA_PROPERTY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraProperty {
    /// Full sensor width in pixels.
    pub max_width: i64,
    /// Full sensor height in pixels.
    pub max_height: i64,
    /// Colour (Bayer) sensor when `true`, monochrome when `false`.
    pub is_color: bool,
    /// Bayer pattern (meaningful only when [`is_color`](Self::is_color)).
    pub bayer_pattern: BayerPattern,
    /// Supported symmetric binning factors (e.g. `[1, 2, 3, 4]`).
    pub supported_bins: Vec<u32>,
    /// Supported output image formats.
    pub supported_video_formats: Vec<ImageType>,
    /// ADC bit depth (e.g. `14`).
    pub max_bit_depth: i32,
    /// Whether the camera supports the trigger (industrial) modes.
    pub is_trigger_cam: bool,
}

/// Safe view of `SVB_CAMERA_PROPERTY_EX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraPropertyEx {
    /// Whether the camera exposes ST4 pulse guiding.
    pub supports_pulse_guide: bool,
    /// Whether the camera supports cooling/temperature control.
    pub supports_control_temp: bool,
}

/// `SVBony` control type (`SVB_CONTROL_TYPE`).
///
/// Unrecognised control types are preserved as [`ControlType::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlType {
    /// `SVB_GAIN`.
    Gain,
    /// `SVB_EXPOSURE`, in microseconds (µs) — **hardware-confirmed** on a
    /// physical SV605CC (2026-07-26): a 3 s value integrates ~3 s
    /// wall-clock, and the SDK's own quantization reads back at µs scale
    /// (200 000 → 199 997). Matches ZWO's ASI `ASI_EXPOSURE` convention.
    Exposure,
    /// `SVB_GAMMA`.
    Gamma,
    /// `SVB_GAMMA_CONTRAST`.
    GammaContrast,
    /// `SVB_WB_R`.
    WbR,
    /// `SVB_WB_G`.
    WbG,
    /// `SVB_WB_B`.
    WbB,
    /// `SVB_FLIP`.
    Flip,
    /// `SVB_FRAME_SPEED_MODE`.
    FrameSpeedMode,
    /// `SVB_CONTRAST`.
    Contrast,
    /// `SVB_SHARPNESS`.
    Sharpness,
    /// `SVB_SATURATION`.
    Saturation,
    /// `SVB_AUTO_TARGET_BRIGHTNESS`.
    AutoTargetBrightness,
    /// `SVB_BLACK_LEVEL` — the ASCOM *Offset*-equivalent control.
    BlackLevel,
    /// `SVB_COOLER_ENABLE`.
    CoolerEnable,
    /// `SVB_TARGET_TEMPERATURE` (cooler set-point, 0.1 °C units).
    TargetTemperature,
    /// `SVB_CURRENT_TEMPERATURE` (sensor temperature, 0.1 °C units).
    CurrentTemperature,
    /// `SVB_COOLER_POWER` (read-only, percent).
    CoolerPower,
    /// `SVB_BAD_PIXEL_CORRECTION_ENABLE`.
    BadPixelCorrectionEnable,
    /// `SVB_BAD_PIXEL_CORRECTION_THRESHOLD`.
    BadPixelCorrectionThreshold,
    /// A control type outside the subset named above; carries the raw value.
    Other(i32),
}

impl ControlType {
    #[cfg(not(feature = "simulation"))]
    #[must_use]
    const fn from_raw(v: i32) -> Self {
        match v {
            0 => Self::Gain,
            1 => Self::Exposure,
            2 => Self::Gamma,
            3 => Self::GammaContrast,
            4 => Self::WbR,
            5 => Self::WbG,
            6 => Self::WbB,
            7 => Self::Flip,
            8 => Self::FrameSpeedMode,
            9 => Self::Contrast,
            10 => Self::Sharpness,
            11 => Self::Saturation,
            12 => Self::AutoTargetBrightness,
            13 => Self::BlackLevel,
            14 => Self::CoolerEnable,
            15 => Self::TargetTemperature,
            16 => Self::CurrentTemperature,
            17 => Self::CoolerPower,
            18 => Self::BadPixelCorrectionEnable,
            19 => Self::BadPixelCorrectionThreshold,
            other => Self::Other(other),
        }
    }

    #[cfg(not(feature = "simulation"))]
    const fn to_raw(self) -> i32 {
        match self {
            Self::Gain => 0,
            Self::Exposure => 1,
            Self::Gamma => 2,
            Self::GammaContrast => 3,
            Self::WbR => 4,
            Self::WbG => 5,
            Self::WbB => 6,
            Self::Flip => 7,
            Self::FrameSpeedMode => 8,
            Self::Contrast => 9,
            Self::Sharpness => 10,
            Self::Saturation => 11,
            Self::AutoTargetBrightness => 12,
            Self::BlackLevel => 13,
            Self::CoolerEnable => 14,
            Self::TargetTemperature => 15,
            Self::CurrentTemperature => 16,
            Self::CoolerPower => 17,
            Self::BadPixelCorrectionEnable => 18,
            Self::BadPixelCorrectionThreshold => 19,
            Self::Other(v) => v,
        }
    }
}

/// Safe view of `SVB_CONTROL_CAPS` — one tunable control's range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlCaps {
    /// Control name, e.g. `"Gain"`.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Which control this describes.
    pub control_type: ControlType,
    /// Minimum value.
    pub min: i64,
    /// Maximum value.
    pub max: i64,
    /// Default value.
    pub default: i64,
    /// Whether the control can be written (some, e.g. temperature, are read-only).
    pub is_writable: bool,
    /// Whether the control supports the SDK's auto mode.
    pub is_auto_supported: bool,
}

/// A control's current value and whether it is in SDK auto mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlValue {
    /// The raw control value (e.g. gain, black level; temperature is in
    /// 0.1 °C units).
    pub value: i64,
    /// Whether the control is currently in the SDK's auto mode.
    pub is_auto: bool,
}

/// The region-of-interest format: frame position, size, and binning.
///
/// `width`/`height` are **post-binning** pixel counts. The SDK requires
/// `width % 8 == 0` and `height % 2 == 0`. Unlike ASI's `ASISetROIFormat`,
/// `SVBony`'s `SVBSetROIFormat` does not bundle the output image format — that
/// is set independently via [`Camera::set_output_image_type`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoiFormat {
    /// ROI start X (top-left), in post-binning coordinates.
    pub start_x: u32,
    /// ROI start Y (top-left), in post-binning coordinates.
    pub start_y: u32,
    /// Post-binning frame width in pixels (`width % 8 == 0`).
    pub width: u32,
    /// Post-binning frame height in pixels (`height % 2 == 0`).
    pub height: u32,
    /// Symmetric binning factor (1 = no binning).
    pub bin: u32,
}

/// Camera acquisition mode (`SVB_CAMERA_MODE`).
///
/// `SVBony` has no snap-exposure API: every exposure rides video capture. In
/// [`CameraMode::Normal`] frames are free-running/continuous; in
/// [`CameraMode::TrigSoft`] a frame is only produced after
/// [`Camera::send_soft_trigger`]. See the module docs and
/// `docs/plans/archive/svbony-camera.md` ("Exposure state machine over video mode").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraMode {
    /// `SVB_MODE_NORMAL` — free-running video capture.
    Normal,
    /// `SVB_MODE_TRIG_SOFT` — a frame is captured on each
    /// [`Camera::send_soft_trigger`] call.
    TrigSoft,
    /// `SVB_MODE_TRIG_RISE_EDGE`.
    TrigRiseEdge,
    /// `SVB_MODE_TRIG_FALL_EDGE`.
    TrigFallEdge,
    /// `SVB_MODE_TRIG_DOUBLE_EDGE`.
    TrigDoubleEdge,
    /// `SVB_MODE_TRIG_HIGH_LEVEL`.
    TrigHighLevel,
    /// `SVB_MODE_TRIG_LOW_LEVEL`.
    TrigLowLevel,
    /// A mode value outside the set named above; carries the raw value.
    Other(i32),
}

impl CameraMode {
    #[cfg(not(feature = "simulation"))]
    #[must_use]
    const fn from_raw(v: i32) -> Self {
        match v {
            0 => Self::Normal,
            1 => Self::TrigSoft,
            2 => Self::TrigRiseEdge,
            3 => Self::TrigFallEdge,
            4 => Self::TrigDoubleEdge,
            5 => Self::TrigHighLevel,
            6 => Self::TrigLowLevel,
            other => Self::Other(other),
        }
    }

    #[cfg(not(feature = "simulation"))]
    const fn to_raw(self) -> i32 {
        match self {
            Self::Normal => 0,
            Self::TrigSoft => 1,
            Self::TrigRiseEdge => 2,
            Self::TrigFallEdge => 3,
            Self::TrigDoubleEdge => 4,
            Self::TrigHighLevel => 5,
            Self::TrigLowLevel => 6,
            Self::Other(v) => v,
        }
    }
}

/// ST4 guide-pulse direction (`SVB_GUIDE_DIRECTION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideDirection {
    /// `SVB_GUIDE_NORTH` (+Dec).
    North,
    /// `SVB_GUIDE_SOUTH` (−Dec).
    South,
    /// `SVB_GUIDE_EAST` (+RA).
    East,
    /// `SVB_GUIDE_WEST` (−RA).
    West,
}

impl GuideDirection {
    #[cfg(not(feature = "simulation"))]
    const fn to_raw(self) -> i32 {
        match self {
            Self::North => 0,
            Self::South => 1,
            Self::East => 2,
            Self::West => 3,
        }
    }
}

/// An open `SVBony` camera. Closes the device on drop.
///
/// The SDK's thread-safety is undocumented — treat it as unsafe for
/// concurrent calls on one handle, the same posture `qhyccd-rs`/`zwo-rs`
/// take. `Camera` is `Send` but **not** `Sync`: move it between threads
/// freely, but to share it across threads put it behind a `Mutex` so the SDK
/// calls serialise.
#[derive(Debug)]
pub struct Camera {
    info: CameraInfo,
    property: CameraProperty,
    property_ex: CameraPropertyEx,
    #[cfg(feature = "simulation")]
    state: std::sync::Mutex<SimState>,
    /// Makes `Camera` `!Sync` (see the type docs) while leaving it `Send`.
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl Sdk {
    /// Enumerate every connected camera's [`CameraInfo`] without opening it.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK fails to read a camera's info.
    pub fn cameras(&self) -> Result<Vec<CameraInfo>> {
        crate::with_sdk_lock(|| {
            #[cfg(feature = "simulation")]
            let infos = (0..crate::SIM_CAMERA_COUNT)
                .map(|_| sim_camera_info())
                .collect();
            #[cfg(not(feature = "simulation"))]
            let infos = {
                // `crate::camera_count_raw`, not `self.camera_count()`: the
                // latter takes `SDK_CALL_LOCK` itself, and it is not
                // reentrant.
                let n = crate::camera_count_raw();
                (0..n)
                    .map(|index| {
                        let idx =
                            i32::try_from(index).map_err(|_| Error::Svb(SvbError::InvalidIndex))?;
                        read_camera_info(idx)
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            Ok(infos)
        })
    }

    /// Open the camera at enumeration `index`.
    ///
    /// On the real path this calls `SVBOpenCamera`, then reads
    /// `SVB_CAMERA_PROPERTY`/`SVB_CAMERA_PROPERTY_EX`; the returned
    /// [`Camera`] closes the device on drop.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the index is out of range or the SDK fails
    /// to open the camera or read its properties.
    pub fn open_camera(&self, index: usize) -> Result<Camera> {
        // `SVBOpenCamera` mutates the SDK's global camera/handle table, so
        // this — like `cameras()` — serializes on the process-wide
        // `SDK_CALL_LOCK` (see its doc comment): two `Sdk` instances (one per
        // discovered camera, minted freely by callers) opening different
        // cameras concurrently would otherwise race with no synchronization
        // at all.
        crate::with_sdk_lock(|| {
            #[cfg(feature = "simulation")]
            let camera = {
                if index >= crate::SIM_CAMERA_COUNT {
                    return Err(Error::Svb(SvbError::InvalidIndex));
                }
                let info = sim_camera_info();
                let property = sim_camera_property();
                let property_ex = sim_camera_property_ex();
                let state = std::sync::Mutex::new(SimState::new(&property));
                Camera {
                    info,
                    property,
                    property_ex,
                    state,
                    _not_sync: std::marker::PhantomData,
                }
            };
            #[cfg(not(feature = "simulation"))]
            let camera = {
                let idx = i32::try_from(index).map_err(|_| Error::Svb(SvbError::InvalidIndex))?;
                let info = read_camera_info(idx)?;
                // SAFETY: `info.id` is a valid CameraID from enumeration; open it.
                svb_check(unsafe { sys::SVBOpenCamera(info.id) })?;
                let property = match read_camera_property(info.id) {
                    Ok(p) => p,
                    Err(e) => {
                        // SAFETY: closing what was just successfully opened, on
                        // the property-read failure path, so the handle is not
                        // leaked.
                        unsafe {
                            let _ = sys::SVBCloseCamera(info.id);
                        }
                        return Err(e);
                    }
                };
                let property_ex = match read_camera_property_ex(info.id) {
                    Ok(p) => p,
                    Err(e) => {
                        // SAFETY: as above.
                        unsafe {
                            let _ = sys::SVBCloseCamera(info.id);
                        }
                        return Err(e);
                    }
                };
                Camera {
                    info,
                    property,
                    property_ex,
                    _not_sync: std::marker::PhantomData,
                }
            };
            Ok(camera)
        })
    }
}

impl Camera {
    /// The camera's cached [`CameraInfo`] (including its serial number).
    #[must_use]
    pub const fn info(&self) -> &CameraInfo {
        &self.info
    }

    /// The camera's `CameraID`.
    #[must_use]
    pub const fn id(&self) -> i32 {
        self.info.id
    }

    /// The camera's cached [`CameraProperty`].
    #[must_use]
    pub const fn property(&self) -> &CameraProperty {
        &self.property
    }

    /// The camera's cached [`CameraPropertyEx`].
    #[must_use]
    pub const fn property_ex(&self) -> &CameraPropertyEx {
        &self.property_ex
    }

    /// Enumerate this camera's tunable controls and their ranges.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK fails to read the control list.
    pub fn control_caps(&self) -> Result<Vec<ControlCaps>> {
        #[cfg(feature = "simulation")]
        let caps = sim_control_caps();
        #[cfg(not(feature = "simulation"))]
        let caps = {
            let mut n: c_int = 0;
            // SAFETY: `self.info.id` is an open camera; the SDK writes the count.
            svb_check(unsafe { sys::SVBGetNumOfControls(self.info.id, &raw mut n) })?;
            let count = usize::try_from(n).unwrap_or(0);
            (0..count)
                .map(|i| {
                    let idx = i32::try_from(i).map_err(|_| Error::Svb(SvbError::InvalidIndex))?;
                    // SAFETY: POD struct filled by the SDK for a valid index.
                    let mut raw: sys::SvbControlCaps = unsafe { std::mem::zeroed() };
                    svb_check(unsafe { sys::SVBGetControlCaps(self.info.id, idx, &raw mut raw) })?;
                    Ok(control_caps_from_raw(&raw))
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(caps)
    }

    /// Read a control's current value and auto flag.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control type is invalid for this camera.
    pub fn control_value(&self, control: ControlType) -> Result<ControlValue> {
        #[cfg(feature = "simulation")]
        let value = self.sim_control_value(control)?;
        #[cfg(not(feature = "simulation"))]
        let value = {
            let mut v: c_long = 0;
            let mut auto: sys::SvbBool = 0;
            // SAFETY: open camera id; the SDK writes the value and auto flag.
            svb_check(unsafe {
                sys::SVBGetControlValue(self.info.id, control.to_raw(), &raw mut v, &raw mut auto)
            })?;
            ControlValue {
                value: c_long_field(v),
                is_auto: auto != 0,
            }
        };
        Ok(value)
    }

    /// Set a control's value (and auto mode).
    ///
    /// **Gain is gated by the SDK's auto-exposure state.** The SDK refuses
    /// a `Gain` write (as [`SvbError::GeneralError`], its catch-all code)
    /// while that state is on; it is on after open and after
    /// [`Camera::restore_default_param`], and the SDK's only path that
    /// clears it is an `Exposure` write with `auto = false` (an `Exposure`
    /// write with `auto = true` turns it back on). Drivers therefore issue
    /// one manual `Exposure` write in their connect handshake before any
    /// gain write — `indi_svbony_ccd` does exactly this ("fix for SDK gain
    /// error issue"). The simulation reproduces the gate.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control type is invalid, read-only, or
    /// the value is rejected by the camera.
    pub fn set_control_value(&self, control: ControlType, value: i64, auto: bool) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_set_control_value(control, value, auto)?;
        #[cfg(not(feature = "simulation"))]
        {
            let v = c_long::try_from(value).map_err(|_| Error::Svb(SvbError::GeneralError))?;
            let auto_flag: sys::SvbBool = i32::from(auto);
            // SAFETY: open camera id; the SDK validates control/value.
            svb_check(unsafe {
                sys::SVBSetControlValue(self.info.id, control.to_raw(), v, auto_flag)
            })?;
        }
        Ok(())
    }

    /// Current gain (`SVB_GAIN`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn gain(&self) -> Result<i64> {
        Ok(self.control_value(ControlType::Gain)?.value)
    }

    /// Set gain (`SVB_GAIN`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the value is rejected.
    pub fn set_gain(&self, gain: i64) -> Result<()> {
        self.set_control_value(ControlType::Gain, gain, false)
    }

    /// Current exposure time in microseconds (`SVB_EXPOSURE`; see
    /// [`ControlType::Exposure`]'s doc for the unit assumption).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn exposure_us(&self) -> Result<i64> {
        Ok(self.control_value(ControlType::Exposure)?.value)
    }

    /// Set exposure time in microseconds (`SVB_EXPOSURE`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the value is rejected.
    pub fn set_exposure_us(&self, exposure_us: i64) -> Result<()> {
        self.set_control_value(ControlType::Exposure, exposure_us, false)
    }

    /// Current black level (`SVB_BLACK_LEVEL`, the ASCOM *Offset*-equivalent).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn black_level(&self) -> Result<i64> {
        Ok(self.control_value(ControlType::BlackLevel)?.value)
    }

    /// Set black level (`SVB_BLACK_LEVEL`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the value is rejected.
    pub fn set_black_level(&self, black_level: i64) -> Result<()> {
        self.set_control_value(ControlType::BlackLevel, black_level, false)
    }

    /// Whether the TEC cooler is enabled (`SVB_COOLER_ENABLE`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn cooler_enable(&self) -> Result<bool> {
        Ok(self.control_value(ControlType::CoolerEnable)?.value != 0)
    }

    /// Enable/disable the TEC cooler (`SVB_COOLER_ENABLE`).
    ///
    /// Actuates hardware — callers must respect the workspace's "no
    /// actuation on connect" tenet (never call this from a connect/reconnect
    /// path; see `docs/workspace.md`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the value is rejected.
    pub fn set_cooler_enable(&self, enable: bool) -> Result<()> {
        self.set_control_value(ControlType::CoolerEnable, i64::from(enable), false)
    }

    /// Cooler set-point in °C (decodes the 0.1 °C `SVB_TARGET_TEMPERATURE` units).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn target_temperature_celsius(&self) -> Result<f64> {
        let raw = self.control_value(ControlType::TargetTemperature)?;
        Ok(tenths_to_celsius(raw.value))
    }

    /// Set the cooler set-point in °C (encodes to the 0.1 °C
    /// `SVB_TARGET_TEMPERATURE` units).
    ///
    /// Actuates hardware — see [`Camera::set_cooler_enable`]'s tenet note.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the value is rejected; a non-finite
    /// `celsius` (NaN or an infinity) is rejected with
    /// [`SvbError::GeneralError`] before anything reaches the SDK.
    pub fn set_target_temperature_celsius(&self, celsius: f64) -> Result<()> {
        // NaN would otherwise encode to `0` — a plausible real set-point —
        // through the cast below, silently actuating the cooler to 0 degC.
        if !celsius.is_finite() {
            return Err(Error::Svb(SvbError::GeneralError));
        }
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "no TryFrom<f64> exists; the guard above rejects non-finite input, and `as` saturates any remaining out-of-range result"
        )]
        let tenths = (celsius * 10.0).round() as i64;
        self.set_control_value(ControlType::TargetTemperature, tenths, false)
    }

    /// Sensor temperature in °C (decodes the 0.1 °C `SVB_CURRENT_TEMPERATURE`
    /// units). Reported independently of whether cooling is on.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn current_temperature_celsius(&self) -> Result<f64> {
        let raw = self.control_value(ControlType::CurrentTemperature)?;
        Ok(tenths_to_celsius(raw.value))
    }

    /// Cooler power, 0-100 % (`SVB_COOLER_POWER`, read-only).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the control cannot be read.
    pub fn cooler_power_percent(&self) -> Result<i64> {
        Ok(self.control_value(ControlType::CoolerPower)?.value)
    }

    /// Read the current output image format.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK call fails or reports an unknown
    /// image type.
    pub fn output_image_type(&self) -> Result<ImageType> {
        #[cfg(feature = "simulation")]
        let t = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output_image_type;
        #[cfg(not(feature = "simulation"))]
        let t = {
            let mut raw: sys::SvbImgType = 0;
            // SAFETY: open camera id; the SDK writes the image type.
            svb_check(unsafe { sys::SVBGetOutputImageType(self.info.id, &raw mut raw) })?;
            ImageType::from_raw(raw).ok_or(Error::Svb(SvbError::InvalidImgType))?
        };
        Ok(t)
    }

    /// Set the output image format.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the format is unsupported by this camera.
    pub fn set_output_image_type(&self, image_type: ImageType) -> Result<()> {
        #[cfg(feature = "simulation")]
        {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .output_image_type = image_type;
        }
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; the SDK validates the format.
        svb_check(unsafe { sys::SVBSetOutputImageType(self.info.id, image_type.to_raw()) })?;
        Ok(())
    }

    /// Read the current ROI format.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK call fails.
    pub fn roi_format(&self) -> Result<RoiFormat> {
        #[cfg(feature = "simulation")]
        let roi = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roi;
        #[cfg(not(feature = "simulation"))]
        let roi = {
            let mut sx: c_int = 0;
            let mut sy: c_int = 0;
            let mut w: c_int = 0;
            let mut h: c_int = 0;
            let mut b: c_int = 0;
            // SAFETY: open camera id; the SDK writes the five out-params.
            svb_check(unsafe {
                sys::SVBGetROIFormat(
                    self.info.id,
                    &raw mut sx,
                    &raw mut sy,
                    &raw mut w,
                    &raw mut h,
                    &raw mut b,
                )
            })?;
            RoiFormat {
                start_x: u32::try_from(sx).unwrap_or(0),
                start_y: u32::try_from(sy).unwrap_or(0),
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
                bin: u32::try_from(b).unwrap_or(0),
            }
        };
        Ok(roi)
    }

    /// Set the ROI format: start position, size (post-binning), and binning.
    ///
    /// `width` must be a multiple of 8 and `height` a multiple of 2 (SDK
    /// requirements); violating either is rejected as a typed error here,
    /// before any SDK call.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] (`SvbError::InvalidSize`) if `width`/`height`
    /// fail the alignment rule or the binning/size is unsupported by this
    /// camera; `SvbError::OutOfBoundary` if the start position places the
    /// frame off-sensor.
    pub fn set_roi_format(
        &self,
        start_x: u32,
        start_y: u32,
        width: u32,
        height: u32,
        bin: u32,
    ) -> Result<()> {
        if width % 8 != 0 || height % 2 != 0 {
            return Err(Error::Svb(SvbError::InvalidSize));
        }
        #[cfg(feature = "simulation")]
        self.sim_set_roi_format(start_x, start_y, width, height, bin)?;
        #[cfg(not(feature = "simulation"))]
        {
            let sx = c_int::try_from(start_x).map_err(|_| Error::Svb(SvbError::OutOfBoundary))?;
            let sy = c_int::try_from(start_y).map_err(|_| Error::Svb(SvbError::OutOfBoundary))?;
            let w = c_int::try_from(width).map_err(|_| Error::Svb(SvbError::InvalidSize))?;
            let h = c_int::try_from(height).map_err(|_| Error::Svb(SvbError::InvalidSize))?;
            let b = c_int::try_from(bin).map_err(|_| Error::Svb(SvbError::InvalidSize))?;
            // SAFETY: open camera id; the SDK validates size/binning.
            svb_check(unsafe { sys::SVBSetROIFormat(self.info.id, sx, sy, w, h, b) })?;
        }
        Ok(())
    }

    /// Byte length of one full frame at the current ROI and output image
    /// type (`roi.width × roi.height × image_type.bytes_per_pixel()`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if either underlying read fails, or
    /// [`SvbError::InvalidSize`] if the ROI describes a frame this target
    /// cannot address.
    pub fn frame_buffer_len(&self) -> Result<usize> {
        let roi = self.roi_format()?;
        let image_type = self.output_image_type()?;
        // The ROI is device state and arrives fixed-width; the length it
        // describes is a `usize`, so the conversion belongs here. Fallible
        // rather than saturating because callers allocate this many bytes
        // as well as compare against it — `usize::MAX` would serve the
        // comparison and abort the allocation.
        let too_large = || Error::Svb(SvbError::InvalidSize);
        let width = usize::try_from(roi.width).map_err(|_| too_large())?;
        let height = usize::try_from(roi.height).map_err(|_| too_large())?;
        width
            .checked_mul(height)
            .and_then(|px| px.checked_mul(image_type.bytes_per_pixel()))
            .ok_or_else(too_large)
    }

    /// Read the current [`CameraMode`].
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK call fails.
    pub fn camera_mode(&self) -> Result<CameraMode> {
        #[cfg(feature = "simulation")]
        let mode = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .camera_mode;
        #[cfg(not(feature = "simulation"))]
        let mode = {
            let mut raw: sys::SvbCameraMode = 0;
            // SAFETY: open camera id; the SDK writes the mode.
            svb_check(unsafe { sys::SVBGetCameraMode(self.info.id, &raw mut raw) })?;
            CameraMode::from_raw(raw)
        };
        Ok(mode)
    }

    /// Set the [`CameraMode`].
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the mode is unsupported by this camera.
    pub fn set_camera_mode(&self, mode: CameraMode) -> Result<()> {
        #[cfg(feature = "simulation")]
        {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .camera_mode = mode;
        }
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; the SDK validates the mode.
        svb_check(unsafe { sys::SVBSetCameraMode(self.info.id, mode.to_raw()) })?;
        Ok(())
    }

    /// The camera modes this camera supports (only meaningful when
    /// [`CameraProperty::is_trigger_cam`] is true).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK call fails.
    pub fn support_modes(&self) -> Result<Vec<CameraMode>> {
        #[cfg(feature = "simulation")]
        let modes = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .support_modes
            .clone();
        #[cfg(not(feature = "simulation"))]
        let modes = {
            // SAFETY: POD struct filled by the SDK.
            let mut raw: sys::SvbSupportedMode = unsafe { std::mem::zeroed() };
            svb_check(unsafe { sys::SVBGetCameraSupportMode(self.info.id, &raw mut raw) })?;
            raw.supported_camera_mode
                .iter()
                .take_while(|&&m| m != sys::SVB_MODE_END)
                .map(|&m| CameraMode::from_raw(m))
                .collect()
        };
        Ok(modes)
    }

    /// Start video capture. In [`CameraMode::Normal`] frames free-run; in
    /// [`CameraMode::TrigSoft`] each frame needs [`Camera::send_soft_trigger`].
    ///
    /// # Errors
    /// Returns [`Error::Svb`] (`SvbError::VideoModeActive`) if capture is
    /// already running.
    pub fn start_video_capture(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_start_video_capture()?;
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; starts video capture.
        svb_check(unsafe { sys::SVBStartVideoCapture(self.info.id) })?;
        Ok(())
    }

    /// How many times video capture has been started on this simulated
    /// camera.
    ///
    /// A read-only observable with no side effect, for tests that need to know
    /// a driver has actually issued its per-exposure capture restart (the
    /// non-trigger path) rather than to guess from elapsed time. Nothing on
    /// the real SDK corresponds to it, so it is `simulation`-only.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn video_capture_starts(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .video_capture_starts
    }

    /// How many times [`Camera::get_video_data`] has been called on this
    /// simulated camera, frame or timeout.
    ///
    /// A read-only observable with no side effect, and the only way a test can
    /// know a driver's retrieval loop has actually started rather than infer
    /// it from elapsed time — the call before the first poll is the last thing
    /// that happens before the loop's clock starts. Nothing on the real SDK
    /// corresponds to it, so it is `simulation`-only.
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn get_video_data_calls(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_video_data_calls
    }

    /// Stop video capture. There is no graceful, data-preserving stop at the
    /// SDK level — any in-flight frame is discarded.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the call fails.
    pub fn stop_video_capture(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_stop_video_capture();
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; stops video capture.
        svb_check(unsafe { sys::SVBStopVideoCapture(self.info.id) })?;
        Ok(())
    }

    /// Trigger a single frame in [`CameraMode::TrigSoft`].
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if capture is not running or the camera is not
    /// in soft-trigger mode.
    pub fn send_soft_trigger(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_send_soft_trigger()?;
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; requests one triggered frame.
        svb_check(unsafe { sys::SVBSendSoftTrigger(self.info.id) })?;
        Ok(())
    }

    /// Fetch the most recent video frame into `buf`.
    ///
    /// `buf` must be at least [`Camera::frame_buffer_len`] bytes; a short
    /// buffer is rejected with [`SvbError::BufferTooSmall`] **before** the
    /// SDK is called. `timeout_ms` follows the SDK convention: `-1` waits
    /// forever; the SDK's own recommendation is `exposure*2 + 500ms`.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if `buf` is too small, no frame becomes ready
    /// within `timeout_ms` (`SvbError::Timeout`), or the download fails.
    pub fn get_video_data(&self, buf: &mut [u8], timeout_ms: i32) -> Result<()> {
        let need = self.frame_buffer_len()?;
        if buf.len() < need {
            return Err(Error::Svb(SvbError::BufferTooSmall));
        }
        #[cfg(feature = "simulation")]
        {
            // The simulation never literally waits out `timeout_ms` (see
            // `sim_get_video_data`'s doc comment) — it reports readiness or a
            // timeout immediately, so the parameter is unused on this path.
            let _ = timeout_ms;
            self.sim_get_video_data(buf, need)?;
        }
        #[cfg(not(feature = "simulation"))]
        {
            let len =
                c_long::try_from(buf.len()).map_err(|_| Error::Svb(SvbError::BufferTooSmall))?;
            // SAFETY: `buf` is at least `need` bytes (checked above) and `len`
            // equals its length, so the SDK writes within bounds.
            svb_check(unsafe {
                sys::SVBGetVideoData(self.info.id, buf.as_mut_ptr(), len, timeout_ms)
            })?;
        }
        Ok(())
    }

    /// Whether this camera supports ST4 pulse guiding (`SVBCanPulseGuide`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the call fails.
    // Const only in the `simulation` shape: the real-FFI body makes SDK
    // calls, and the signature must stay identical in both shapes (the
    // sibling qhyccd-rs sim-twin convention).
    #[allow(clippy::missing_const_for_fn)]
    pub fn can_pulse_guide(&self) -> Result<bool> {
        #[cfg(feature = "simulation")]
        let can = self.property_ex.supports_pulse_guide;
        #[cfg(not(feature = "simulation"))]
        let can = {
            let mut b: sys::SvbBool = 0;
            // SAFETY: open camera id; the SDK writes the capability flag.
            svb_check(unsafe { sys::SVBCanPulseGuide(self.info.id, &raw mut b) })?;
            b != 0
        };
        Ok(can)
    }

    /// Issue an ST4 guide pulse in `direction` for `duration_ms` milliseconds
    /// (blocking at the SDK level for the pulse duration).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the call fails.
    // Const only in the `simulation` shape: the real-FFI body makes SDK
    // calls, and the signature must stay identical in both shapes (the
    // sibling qhyccd-rs sim-twin convention).
    #[allow(clippy::missing_const_for_fn)]
    pub fn pulse_guide(&self, direction: GuideDirection, duration_ms: i32) -> Result<()> {
        #[cfg(feature = "simulation")]
        {
            let _ = (direction, duration_ms);
        }
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; issues a blocking guide pulse.
        svb_check(unsafe { sys::SVBPulseGuide(self.info.id, direction.to_raw(), duration_ms) })?;
        Ok(())
    }

    /// Sensor pixel size in microns (`SVBGetSensorPixelSize`).
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the call fails.
    // Const only in the `simulation` shape: the real-FFI body makes SDK
    // calls, and the signature must stay identical in both shapes (the
    // sibling qhyccd-rs sim-twin convention).
    #[allow(clippy::missing_const_for_fn)]
    pub fn pixel_size_microns(&self) -> Result<f32> {
        #[cfg(feature = "simulation")]
        let size = SIM_PIXEL_SIZE_UM;
        #[cfg(not(feature = "simulation"))]
        let size = {
            let mut px: f32 = 0.0;
            // SAFETY: open camera id; the SDK writes the pixel size.
            svb_check(unsafe { sys::SVBGetSensorPixelSize(self.info.id, &raw mut px) })?;
            px
        };
        Ok(size)
    }

    /// Restore the camera's default parameters (`SVBRestoreDefaultParam`).
    ///
    /// The SDK reloads the device's default parameter block — gain,
    /// exposure, black level, and its own **auto-exposure state**, which
    /// the defaults leave *on* (see [`Camera::set_control_value`]'s
    /// gain/exposure note) — and then, unconditionally, persists it as
    /// `<model>_Cfg_A.bin` in the process's current working directory. A
    /// read-only working directory makes that persist step, and so this
    /// call, return [`SvbError::GeneralError`] even though the restore
    /// itself took effect; callers that only need the restore should treat
    /// the error as advisory.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK rejects the call.
    pub fn restore_default_param(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_restore_default_param();
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open camera id; the SDK reloads and re-persists its
        // parameter block.
        svb_check(unsafe { sys::SVBRestoreDefaultParam(self.info.id) })?;
        Ok(())
    }

    /// Enable or disable the SDK's parameter auto-save
    /// (`SVBSetAutoSaveParam`).
    ///
    /// With auto-save on — the SDK default — the SDK persists the whole
    /// camera parameter block (gain, exposure, black level, auto-exposure
    /// state, …) to `<model>_Cfg_SAVE.bin` in the process's current
    /// working directory when the camera closes, and reloads it at the next
    /// open: session state travels through the launcher's working directory
    /// and the state a camera connects in depends on the previous session.
    /// Turn it off right after opening for deterministic connect state.
    ///
    /// # Errors
    /// Returns [`Error::Svb`] if the SDK rejects the call.
    pub fn set_auto_save_param(&self, enable: bool) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_set_auto_save_param(enable);
        #[cfg(not(feature = "simulation"))]
        {
            let flag: sys::SvbBool = i32::from(enable);
            // SAFETY: open camera id; toggles an SDK-internal flag.
            svb_check(unsafe { sys::SVBSetAutoSaveParam(self.info.id, flag) })?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "simulation"))]
impl Drop for Camera {
    fn drop(&mut self) {
        // SAFETY: closing an open camera by id.
        unsafe {
            let _ = sys::SVBCloseCamera(self.info.id);
        }
    }
}

// ---- real FFI helpers --------------------------------------------------------

#[cfg(not(feature = "simulation"))]
fn read_camera_info(index: i32) -> Result<CameraInfo> {
    // SAFETY: `SvbCameraInfo` is POD; the SDK fills it for a valid index.
    let mut raw: sys::SvbCameraInfo = unsafe { std::mem::zeroed() };
    svb_check(unsafe { sys::SVBGetCameraInfo(&raw mut raw, index) })?;
    Ok(camera_info_from_raw(&raw))
}

#[cfg(not(feature = "simulation"))]
fn camera_info_from_raw(raw: &sys::SvbCameraInfo) -> CameraInfo {
    CameraInfo {
        id: raw.camera_id,
        friendly_name: c_string_field(&raw.friendly_name),
        serial: c_string_field(&raw.camera_sn),
        port_type: c_string_field(&raw.port_type),
        device_id: raw.device_id,
    }
}

#[cfg(not(feature = "simulation"))]
fn read_camera_property(camera_id: i32) -> Result<CameraProperty> {
    // SAFETY: `SvbCameraProperty` is POD; the SDK fills it for an open camera.
    let mut raw: sys::SvbCameraProperty = unsafe { std::mem::zeroed() };
    svb_check(unsafe { sys::SVBGetCameraProperty(camera_id, &raw mut raw) })?;
    Ok(camera_property_from_raw(&raw))
}

#[cfg(not(feature = "simulation"))]
fn camera_property_from_raw(raw: &sys::SvbCameraProperty) -> CameraProperty {
    CameraProperty {
        max_width: c_long_field(raw.max_width),
        max_height: c_long_field(raw.max_height),
        is_color: raw.is_color_cam != 0,
        bayer_pattern: BayerPattern::from_raw(raw.bayer_pattern),
        supported_bins: raw
            .supported_bins
            .iter()
            .take_while(|&&b| b != 0)
            // Drop an entry that is not a bin rather than mapping it to 0: a
            // 0 in this list reads as a supported bin factor, and a driver
            // validating a client's `BinX` against the list would accept it
            // and then divide the sensor extent by it.
            .filter_map(|&b| u32::try_from(b).ok())
            .collect(),
        supported_video_formats: raw
            .supported_video_format
            .iter()
            .take_while(|&&f| f != sys::SVB_IMG_END)
            .filter_map(|&f| ImageType::from_raw(f))
            .collect(),
        max_bit_depth: raw.max_bit_depth,
        is_trigger_cam: raw.is_trigger_cam != 0,
    }
}

#[cfg(not(feature = "simulation"))]
fn read_camera_property_ex(camera_id: i32) -> Result<CameraPropertyEx> {
    // SAFETY: `SvbCameraPropertyEx` is POD; the SDK fills it for an open camera.
    let mut raw: sys::SvbCameraPropertyEx = unsafe { std::mem::zeroed() };
    svb_check(unsafe { sys::SVBGetCameraPropertyEx(camera_id, &raw mut raw) })?;
    Ok(CameraPropertyEx {
        supports_pulse_guide: raw.support_pulse_guide != 0,
        supports_control_temp: raw.support_control_temp != 0,
    })
}

#[cfg(not(feature = "simulation"))]
fn control_caps_from_raw(raw: &sys::SvbControlCaps) -> ControlCaps {
    ControlCaps {
        name: c_string_field(&raw.name),
        description: c_string_field(&raw.description),
        control_type: ControlType::from_raw(raw.control_type),
        min: c_long_field(raw.min_value),
        max: c_long_field(raw.max_value),
        default: c_long_field(raw.default_value),
        is_writable: raw.is_writable != 0,
        is_auto_supported: raw.is_auto_supported != 0,
    }
}

// ---- simulation backend ------------------------------------------------------

#[cfg(feature = "simulation")]
const SIM_SERIAL: &str = "SVB0123456789AB";
#[cfg(feature = "simulation")]
const SIM_PIXEL_SIZE_UM: f32 = 3.76;
#[cfg(feature = "simulation")]
const SIM_MAX_WIDTH: u32 = 3008;
#[cfg(feature = "simulation")]
const SIM_MAX_HEIGHT: u32 = 3008;
/// Ambient (cooler-off) sensor temperature, in 0.1 °C units.
#[cfg(feature = "simulation")]
const SIM_AMBIENT_TENTHS: i64 = 200;
/// Per-poll cooling-ramp step, in 0.1 °C units (1.0 °C/poll) — mirrors
/// `zwo-rs`'s EAF focuser position ramp: advance-on-poll, not on wall-clock
/// time, so tests are deterministic and don't sleep.
#[cfg(feature = "simulation")]
const SIM_COOLING_STEP_TENTHS: i64 = 10;

/// The fabricated simulated camera. Mirrors the SV605CC (this plan's first
/// hardware target): a cooled, colour (OSC) camera on the Sony IMX533
/// sensor — 3008×3008, 3.76 µm pixels, 14-bit ADC, no ST4 port, trigger-cam
/// capable (so the simulation can exercise the soft-trigger flow).
#[cfg(feature = "simulation")]
fn sim_camera_info() -> CameraInfo {
    CameraInfo {
        id: 0,
        friendly_name: "SV605CC-Simulated".to_owned(),
        serial: SIM_SERIAL.to_owned(),
        port_type: "USB3".to_owned(),
        device_id: 0,
    }
}

#[cfg(feature = "simulation")]
fn sim_camera_property() -> CameraProperty {
    CameraProperty {
        max_width: i64::from(SIM_MAX_WIDTH),
        max_height: i64::from(SIM_MAX_HEIGHT),
        is_color: true,
        bayer_pattern: BayerPattern::Rg,
        supported_bins: vec![1, 2, 3, 4],
        supported_video_formats: vec![ImageType::Raw8, ImageType::Raw16],
        max_bit_depth: 14,
        is_trigger_cam: true,
    }
}

#[cfg(feature = "simulation")]
const fn sim_camera_property_ex() -> CameraPropertyEx {
    CameraPropertyEx {
        // The SV605CC has no ST4 port (docs/plans/archive/svbony-camera.md).
        supports_pulse_guide: false,
        supports_control_temp: true,
    }
}

#[cfg(feature = "simulation")]
fn sim_control_caps() -> Vec<ControlCaps> {
    let cap =
        |name: &str, control_type, min, max, default, is_writable, is_auto_supported| ControlCaps {
            name: name.to_owned(),
            description: String::new(),
            control_type,
            min,
            max,
            default,
            is_writable,
            is_auto_supported,
        };
    vec![
        cap("Gain", ControlType::Gain, 0, 400, 100, true, false),
        cap(
            "Exposure",
            ControlType::Exposure,
            32,
            2_000_000_000,
            10_000,
            true,
            false,
        ),
        cap(
            "BlackLevel",
            ControlType::BlackLevel,
            0,
            255,
            0,
            true,
            false,
        ),
        cap(
            "CoolerEnable",
            ControlType::CoolerEnable,
            0,
            1,
            0,
            true,
            false,
        ),
        cap(
            "TargetTemperature",
            ControlType::TargetTemperature,
            -500,
            500,
            0,
            true,
            false,
        ),
        cap(
            "CurrentTemperature",
            ControlType::CurrentTemperature,
            -500,
            1000,
            SIM_AMBIENT_TENTHS,
            false,
            false,
        ),
        cap(
            "CoolerPower",
            ControlType::CoolerPower,
            0,
            100,
            0,
            false,
            false,
        ),
    ]
}

/// Mutable state for the simulated camera, behind a `Mutex` so the `&self`
/// device methods can update it.
/// Decode a 0.1 °C tenths reading to °C. Saturating: 0.1-degree units
/// never approach the i32 range, but a drifted SDK value pins to the
/// nearer bound instead of folding to zero.
fn tenths_to_celsius(tenths: i64) -> f64 {
    let saturated = i32::try_from(tenths).unwrap_or_else(|_| {
        if tenths.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    });
    f64::from(saturated) / 10.0
}

#[cfg(feature = "simulation")]
// The flags mirror independent SDK per-camera states (capture running,
// frame armed, cooler, auto-exposure, auto-save): the bool count is the
// SDK's state model, not a design choice (the zwo-rs ASI_CAMERA_INFO
// precedent).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct SimState {
    gain: i64,
    exposure_us: i64,
    black_level: i64,
    cooler_enable: bool,
    target_temp_tenths: i64,
    current_temp_tenths: i64,
    output_image_type: ImageType,
    roi: RoiFormat,
    camera_mode: CameraMode,
    support_modes: Vec<CameraMode>,
    /// `SVBStartVideoCapture`/`SVBStopVideoCapture` state.
    capturing: bool,
    /// How many times `SVBStartVideoCapture` has succeeded. `capturing`
    /// alone cannot tell a caller that a stop-then-start restart happened —
    /// both edges land inside one lock acquisition — so this monotonic count
    /// is what [`Camera::video_capture_starts`] exposes.
    video_capture_starts: u64,
    /// Whether a frame is currently armed and ready for `get_video_data`.
    frame_ready: bool,
    /// How many `get_video_data` calls this camera has served, frame or
    /// timeout. Counted for the same reason as `video_capture_starts`: a
    /// retrieval loop's first poll is otherwise unobservable, leaving a test
    /// to guess at it from the clock. Exposed by
    /// [`Camera::get_video_data_calls`].
    get_video_data_calls: u64,
    /// The SDK's auto-exposure state: on after open and after
    /// `restore_default_param`, cleared only by a manual `Exposure` write,
    /// and refusing `Gain` writes while on (see
    /// [`Camera::set_control_value`]).
    auto_exposure: bool,
    /// The SDK's parameter auto-save flag (`SVBSetAutoSaveParam`); the
    /// simulation records it but persists nothing.
    auto_save: bool,
}

#[cfg(feature = "simulation")]
impl SimState {
    fn new(property: &CameraProperty) -> Self {
        Self {
            gain: 100,
            // Matches the "Exposure" control cap default (microseconds).
            exposure_us: 10_000,
            black_level: 0,
            cooler_enable: false,
            target_temp_tenths: 0,
            current_temp_tenths: SIM_AMBIENT_TENTHS,
            output_image_type: ImageType::Raw16,
            roi: RoiFormat {
                start_x: 0,
                start_y: 0,
                width: u32::try_from(property.max_width).unwrap_or(0),
                height: u32::try_from(property.max_height).unwrap_or(0),
                bin: 1,
            },
            camera_mode: CameraMode::Normal,
            support_modes: vec![CameraMode::Normal, CameraMode::TrigSoft],
            capturing: false,
            video_capture_starts: 0,
            get_video_data_calls: 0,
            frame_ready: false,
            auto_exposure: true,
            auto_save: true,
        }
    }

    /// The SDK's device-default parameter block: what `restore_default_param`
    /// reinstates. Only the tunable imaging controls and the auto-exposure
    /// state are defaults; cooling, ROI, mode and capture state are left
    /// alone (a restore must never actuate anything).
    fn restore_defaults(&mut self) {
        let fresh = Self::new(&sim_camera_property());
        self.gain = fresh.gain;
        self.exposure_us = fresh.exposure_us;
        self.black_level = fresh.black_level;
        self.auto_exposure = fresh.auto_exposure;
    }
}

#[cfg(feature = "simulation")]
impl Camera {
    fn sim_control_value(&self, control: ControlType) -> Result<ControlValue> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = match control {
            ControlType::Gain => st.gain,
            ControlType::Exposure => st.exposure_us,
            ControlType::BlackLevel => st.black_level,
            ControlType::CoolerEnable => i64::from(st.cooler_enable),
            ControlType::TargetTemperature => st.target_temp_tenths,
            ControlType::CurrentTemperature => {
                // Poll-based ramp: advance one step toward the target (or
                // back toward ambient when the cooler is off) per read —
                // mirrors zwo-rs's EAF focuser position ramp (advance on
                // poll, not on wall-clock time: deterministic, no sleeping
                // in tests).
                let target = if st.cooler_enable {
                    st.target_temp_tenths
                } else {
                    SIM_AMBIENT_TENTHS
                };
                let delta = target.saturating_sub(st.current_temp_tenths);
                let step = delta.clamp(-SIM_COOLING_STEP_TENTHS, SIM_COOLING_STEP_TENTHS);
                st.current_temp_tenths = st.current_temp_tenths.saturating_add(step);
                st.current_temp_tenths
            }
            ControlType::CoolerPower => {
                if st.cooler_enable {
                    // Ordered difference: max minus min is non-negative, and
                    // saturating_sub caps the full-span case — unlike
                    // `(a - b).abs()`, whose abs(i64::MIN) panics when the
                    // set-point sits saturated at a bound.
                    let current = st.current_temp_tenths;
                    let target = st.target_temp_tenths;
                    current
                        .max(target)
                        .saturating_sub(current.min(target))
                        .min(100)
                } else {
                    0
                }
            }
            _ => return Err(Error::Svb(SvbError::InvalidControlType)),
        };
        Ok(ControlValue {
            value,
            is_auto: control == ControlType::Exposure && st.auto_exposure,
        })
    }

    fn sim_set_control_value(&self, control: ControlType, value: i64, auto: bool) -> Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match control {
            // The SDK's gate: manual gain is refused while auto-exposure is
            // on, with its catch-all error code.
            ControlType::Gain if st.auto_exposure => {
                return Err(Error::Svb(SvbError::GeneralError));
            }
            ControlType::Gain => st.gain = value,
            // The SDK's only auto-exposure switch: `auto = true` turns it on
            // (the value is ignored), `auto = false` turns it off and sets
            // the exposure.
            ControlType::Exposure => {
                st.auto_exposure = auto;
                if !auto {
                    st.exposure_us = value;
                }
            }
            ControlType::BlackLevel => st.black_level = value,
            ControlType::CoolerEnable => st.cooler_enable = value != 0,
            ControlType::TargetTemperature => st.target_temp_tenths = value,
            // Read-only (CurrentTemperature/CoolerPower) and unknown/other
            // controls are rejected.
            _ => return Err(Error::Svb(SvbError::InvalidControlType)),
        }
        drop(st);
        Ok(())
    }

    fn sim_restore_default_param(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restore_defaults();
    }

    fn sim_set_auto_save_param(&self, enable: bool) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auto_save = enable;
    }

    fn sim_set_roi_format(
        &self,
        start_x: u32,
        start_y: u32,
        width: u32,
        height: u32,
        bin: u32,
    ) -> Result<()> {
        if !self.property.supported_bins.contains(&bin) {
            return Err(Error::Svb(SvbError::InvalidSize));
        }
        let max_w = u32::try_from(self.property.max_width)
            .unwrap_or(0)
            .checked_div(bin)
            .unwrap_or(0);
        let max_h = u32::try_from(self.property.max_height)
            .unwrap_or(0)
            .checked_div(bin)
            .unwrap_or(0);
        if width == 0 || height == 0 || width > max_w || height > max_h {
            return Err(Error::Svb(SvbError::InvalidSize));
        }
        if start_x.saturating_add(width) > max_w || start_y.saturating_add(height) > max_h {
            return Err(Error::Svb(SvbError::OutOfBoundary));
        }
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.roi = RoiFormat {
            start_x,
            start_y,
            width,
            height,
            bin,
        };
        drop(st);
        Ok(())
    }

    fn sim_start_video_capture(&self) -> Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.capturing {
            return Err(Error::Svb(SvbError::VideoModeActive));
        }
        st.capturing = true;
        st.video_capture_starts = st.video_capture_starts.saturating_add(1);
        // Free-running (Normal) mode has a frame ready as soon as capture
        // starts (continuous acquisition); soft-trigger mode requires an
        // explicit `send_soft_trigger` first.
        st.frame_ready = st.camera_mode != CameraMode::TrigSoft;
        drop(st);
        Ok(())
    }

    fn sim_stop_video_capture(&self) {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.capturing = false;
        st.frame_ready = false;
    }

    fn sim_send_soft_trigger(&self) -> Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !st.capturing {
            return Err(Error::Svb(SvbError::InvalidSequence));
        }
        if st.camera_mode != CameraMode::TrigSoft {
            return Err(Error::Svb(SvbError::InvalidMode));
        }
        st.frame_ready = true;
        drop(st);
        Ok(())
    }

    fn sim_get_video_data(&self, buf: &mut [u8], need: usize) -> Result<()> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Counted before the readiness verdict: a poll that times out is still
        // a poll, and it is the timing-out ones a retrieval loop makes first.
        st.get_video_data_calls = st.get_video_data_calls.saturating_add(1);
        if !st.capturing || !st.frame_ready {
            // A real camera would eventually time out waiting for a frame
            // that never becomes ready; the simulation reports it
            // immediately rather than sleeping for `timeout_ms` in tests
            // (the same "don't literally wait" simplification `zwo-rs` uses
            // for its exposure-completes-on-poll simulation).
            return Err(Error::Svb(SvbError::Timeout));
        }
        // Free-running (Normal) mode stays ready for the next frame
        // (continuous stream); soft-trigger mode consumes the armed frame
        // and requires another `send_soft_trigger` before the next one.
        if st.camera_mode == CameraMode::TrigSoft {
            st.frame_ready = false;
        }
        drop(st);
        let dst = buf
            .get_mut(..need)
            .ok_or(Error::Svb(SvbError::BufferTooSmall))?;
        crate::simulation::fill_noise(dst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_per_pixel_matches_every_image_type() {
        // Pins the current mapping (`bytes_per_pixel`'s doc comment flags
        // RAW10/12/14, Y10/12/14, and RGB32 as assumptions pending hardware
        // confirmation) so an accidental edit is caught here rather than
        // only once real hardware disagrees.
        let cases = [
            (ImageType::Raw8, 1),
            (ImageType::Raw10, 2),
            (ImageType::Raw12, 2),
            (ImageType::Raw14, 2),
            (ImageType::Raw16, 2),
            (ImageType::Y8, 1),
            (ImageType::Y10, 2),
            (ImageType::Y12, 2),
            (ImageType::Y14, 2),
            (ImageType::Y16, 2),
            (ImageType::Rgb24, 3),
            (ImageType::Rgb32, 4),
        ];
        for (image_type, want) in cases {
            assert_eq!(image_type.bytes_per_pixel(), want, "{image_type:?}");
        }
    }

    #[test]
    fn camera_is_send() {
        // The SVBony SDK's thread-safety is undocumented, so `Camera` is
        // `Send` (movable between threads) but deliberately not `Sync`. Lock
        // in `Send` here — the multi-threaded tokio runtime the driver uses
        // requires it.
        fn assert_send<T: Send>() {}
        assert_send::<Camera>();
    }

    #[test]
    fn cameras_enumerates() {
        let sdk = Sdk::new().unwrap();
        let cams = sdk.cameras().unwrap();
        #[cfg(feature = "simulation")]
        {
            assert_eq!(cams.len(), crate::SIM_CAMERA_COUNT);
            let info = &cams[0];
            assert_eq!(info.friendly_name, "SV605CC-Simulated");
            assert_eq!(info.serial, "SVB0123456789AB");
        }
        #[cfg(not(feature = "simulation"))]
        {
            let _ = cams;
        }
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_camera_exposes_info_property_and_controls() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert_eq!(cam.id(), 0);
        assert_eq!(cam.info().friendly_name, "SV605CC-Simulated");
        assert_eq!(cam.info().serial, "SVB0123456789AB");

        let property = cam.property();
        assert_eq!(property.max_width, 3008);
        assert_eq!(property.max_height, 3008);
        assert!(property.is_color);
        assert!(property.is_trigger_cam);
        assert_eq!(property.supported_bins, vec![1, 2, 3, 4]);

        assert!(!cam.property_ex().supports_pulse_guide);
        assert!(cam.property_ex().supports_control_temp);

        let caps = cam.control_caps().unwrap();
        let gain = caps
            .iter()
            .find(|c| c.control_type == ControlType::Gain)
            .unwrap();
        assert_eq!(gain.max, 400);
        let exposure = caps
            .iter()
            .find(|c| c.control_type == ControlType::Exposure)
            .unwrap();
        assert_eq!(exposure.min, 32);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_camera_out_of_range_is_rejected() {
        let sdk = Sdk::new().unwrap();
        assert_eq!(
            sdk.open_camera(99).unwrap_err(),
            Error::Svb(SvbError::InvalidIndex)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn roi_format_round_trips() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        let default = cam.roi_format().unwrap();
        assert_eq!(default.width, 3008);
        assert_eq!(default.height, 3008);
        assert_eq!(default.bin, 1);

        cam.set_roi_format(100, 100, 800, 600, 2).unwrap();
        let roi = cam.roi_format().unwrap();
        assert_eq!(roi.start_x, 100);
        assert_eq!(roi.start_y, 100);
        assert_eq!(roi.width, 800);
        assert_eq!(roi.height, 600);
        assert_eq!(roi.bin, 2);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn set_roi_format_rejects_misaligned_size() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // width not a multiple of 8
        assert_eq!(
            cam.set_roi_format(0, 0, 801, 600, 1).unwrap_err(),
            Error::Svb(SvbError::InvalidSize)
        );
        // height not a multiple of 2
        assert_eq!(
            cam.set_roi_format(0, 0, 800, 601, 1).unwrap_err(),
            Error::Svb(SvbError::InvalidSize)
        );
        // unsupported binning
        assert_eq!(
            cam.set_roi_format(0, 0, 800, 600, 5).unwrap_err(),
            Error::Svb(SvbError::InvalidSize)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn set_roi_format_rejects_out_of_bounds_start() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert_eq!(
            cam.set_roi_format(2900, 0, 800, 600, 1).unwrap_err(),
            Error::Svb(SvbError::OutOfBoundary)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn control_values_round_trip() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // A manual exposure write first: it clears the SDK's auto-exposure
        // state, which otherwise refuses the gain write below.
        cam.set_exposure_us(1_500_000).unwrap();
        assert_eq!(cam.exposure_us().unwrap(), 1_500_000);
        cam.set_gain(200).unwrap();
        assert_eq!(cam.gain().unwrap(), 200);
        cam.set_black_level(30).unwrap();
        assert_eq!(cam.black_level().unwrap(), 30);
    }

    /// The SDK's gain gate, reproduced: a freshly opened camera has
    /// auto-exposure on and refuses a manual gain write with the SDK's
    /// catch-all error; black level is not gated.
    #[cfg(feature = "simulation")]
    #[test]
    fn gain_is_refused_while_auto_exposure_is_on() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert!(cam.control_value(ControlType::Exposure).unwrap().is_auto);
        assert_eq!(
            cam.set_gain(200).unwrap_err(),
            Error::Svb(SvbError::GeneralError)
        );
        cam.set_black_level(30).unwrap();
        assert_eq!(cam.black_level().unwrap(), 30);
    }

    /// Only a manual exposure write clears the gate; an auto one re-arms it.
    #[cfg(feature = "simulation")]
    #[test]
    fn manual_exposure_write_clears_auto_exposure_and_an_auto_one_restores_it() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_exposure_us(20_000).unwrap();
        assert!(!cam.control_value(ControlType::Exposure).unwrap().is_auto);
        cam.set_gain(200).unwrap();

        cam.set_control_value(ControlType::Exposure, 5, true)
            .unwrap();
        assert!(cam.control_value(ControlType::Exposure).unwrap().is_auto);
        // The auto write leaves the manual exposure value alone.
        assert_eq!(cam.exposure_us().unwrap(), 20_000);
        assert_eq!(
            cam.set_gain(150).unwrap_err(),
            Error::Svb(SvbError::GeneralError)
        );
    }

    /// `restore_default_param` reinstates the device defaults for the
    /// imaging controls — including auto-exposure back on — and touches
    /// nothing that could actuate (cooling stays as set).
    #[cfg(feature = "simulation")]
    #[test]
    fn restore_default_param_reinstates_the_imaging_defaults_only() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_exposure_us(20_000).unwrap();
        cam.set_gain(200).unwrap();
        cam.set_black_level(30).unwrap();
        cam.set_target_temperature_celsius(-10.0).unwrap();
        cam.set_cooler_enable(true).unwrap();

        cam.restore_default_param().unwrap();

        assert_eq!(cam.gain().unwrap(), 100);
        assert_eq!(cam.exposure_us().unwrap(), 10_000);
        assert_eq!(cam.black_level().unwrap(), 0);
        assert!(cam.control_value(ControlType::Exposure).unwrap().is_auto);
        assert_eq!(
            cam.set_gain(200).unwrap_err(),
            Error::Svb(SvbError::GeneralError)
        );
        assert!(cam.cooler_enable().unwrap());
        assert!((cam.target_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn non_finite_target_temperature_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // A non-default set-point first, so the assertion below can tell
        // "untouched" apart from "reset to the sim default".
        cam.set_target_temperature_celsius(-10.0).unwrap();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                cam.set_target_temperature_celsius(bad).unwrap_err(),
                Error::Svb(SvbError::GeneralError)
            );
        }
        // Every rejected write left the set-point untouched.
        assert!((cam.target_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn tenths_to_celsius_pins_out_of_range_values_to_the_i32_bounds() {
        assert!((tenths_to_celsius(0) - 0.0).abs() < f64::EPSILON);
        assert!((tenths_to_celsius(-105) - (-10.5)).abs() < f64::EPSILON);
        assert!((tenths_to_celsius(i64::MAX) - f64::from(i32::MAX) / 10.0).abs() < f64::EPSILON);
        assert!((tenths_to_celsius(i64::MIN) - f64::from(i32::MIN) / 10.0).abs() < f64::EPSILON);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn cooler_power_survives_a_saturated_set_point() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // A huge finite set-point saturates the encoded tenths to i64::MAX.
        cam.set_target_temperature_celsius(1e300).unwrap();
        cam.set_cooler_enable(true).unwrap();
        // The proxy pins to the 100 % ceiling instead of overflowing.
        assert_eq!(cam.cooler_power_percent().unwrap(), 100);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn set_auto_save_param_is_accepted_either_way() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_auto_save_param(false).unwrap();
        cam.set_auto_save_param(true).unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn set_read_only_control_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert_eq!(
            cam.set_control_value(ControlType::CurrentTemperature, 0, false)
                .unwrap_err(),
            Error::Svb(SvbError::InvalidControlType)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn reading_an_unsupported_control_type_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // `Gamma` is a real `ControlType` the header defines, but the
        // simulation's read side (unlike its write side, already covered by
        // `set_read_only_control_is_rejected`'s sibling case) does not model
        // it, so it should hit the same catch-all rejection.
        assert_eq!(
            cam.control_value(ControlType::Gamma).unwrap_err(),
            Error::Svb(SvbError::InvalidControlType)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn target_temperature_celsius_reads_back_the_set_point() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_target_temperature_celsius(-15.5).unwrap();
        assert!((cam.target_temperature_celsius().unwrap() - (-15.5)).abs() < f64::EPSILON);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn cooling_ramps_toward_target_over_polls() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert!((cam.current_temperature_celsius().unwrap() - 20.0).abs() < f64::EPSILON);

        cam.set_target_temperature_celsius(-10.0).unwrap();
        cam.set_cooler_enable(true).unwrap();
        assert!(cam.cooler_enable().unwrap());

        // 20.0 -> -10.0 at 1.0 C/poll takes 30 polls; verify it's dropping
        // and never overshoots, then converges exactly.
        let mut previous = cam.current_temperature_celsius().unwrap();
        for _ in 0..29 {
            let next = cam.current_temperature_celsius().unwrap();
            assert!(next <= previous);
            assert!(next >= -10.0);
            previous = next;
        }
        assert!((cam.current_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON);
        // Once converged, further polls stay put (no overshoot).
        assert!((cam.current_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON);
        assert!(cam.cooler_power_percent().unwrap() >= 0);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn cooler_off_drifts_back_toward_ambient() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_target_temperature_celsius(-10.0).unwrap();
        cam.set_cooler_enable(true).unwrap();
        for _ in 0..30 {
            let _ = cam.current_temperature_celsius().unwrap();
        }
        assert!((cam.current_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON);

        cam.set_cooler_enable(false).unwrap();
        for _ in 0..30 {
            let _ = cam.current_temperature_celsius().unwrap();
        }
        assert!((cam.current_temperature_celsius().unwrap() - 20.0).abs() < f64::EPSILON);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn normal_mode_video_capture_is_continuous() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_roi_format(0, 0, 800, 600, 1).unwrap();
        cam.set_output_image_type(ImageType::Raw8).unwrap();
        assert_eq!(cam.camera_mode().unwrap(), CameraMode::Normal);

        cam.start_video_capture().unwrap();
        let mut buf = vec![0u8; cam.frame_buffer_len().unwrap()];
        // No soft trigger needed in Normal mode; every call succeeds.
        cam.get_video_data(&mut buf, 1000).unwrap();
        cam.get_video_data(&mut buf, 1000).unwrap();
        cam.stop_video_capture().unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn soft_trigger_mode_requires_a_trigger_per_frame() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_roi_format(0, 0, 800, 600, 1).unwrap();
        cam.set_output_image_type(ImageType::Raw8).unwrap();
        assert!(cam.support_modes().unwrap().contains(&CameraMode::TrigSoft));
        cam.set_camera_mode(CameraMode::TrigSoft).unwrap();

        cam.start_video_capture().unwrap();
        let mut buf = vec![0u8; cam.frame_buffer_len().unwrap()];
        // No frame armed yet: times out immediately (simulation
        // simplification — see sim_get_video_data's doc comment).
        assert_eq!(
            cam.get_video_data(&mut buf, 1000).unwrap_err(),
            Error::Svb(SvbError::Timeout)
        );

        cam.send_soft_trigger().unwrap();
        cam.get_video_data(&mut buf, 1000).unwrap();
        // The armed frame was consumed; another trigger is required.
        assert_eq!(
            cam.get_video_data(&mut buf, 1000).unwrap_err(),
            Error::Svb(SvbError::Timeout)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn soft_trigger_outside_trig_soft_mode_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.start_video_capture().unwrap();
        assert_eq!(
            cam.send_soft_trigger().unwrap_err(),
            Error::Svb(SvbError::InvalidMode)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn soft_trigger_without_capture_running_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_camera_mode(CameraMode::TrigSoft).unwrap();
        assert_eq!(
            cam.send_soft_trigger().unwrap_err(),
            Error::Svb(SvbError::InvalidSequence)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn starting_video_capture_twice_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.start_video_capture().unwrap();
        assert_eq!(
            cam.start_video_capture().unwrap_err(),
            Error::Svb(SvbError::VideoModeActive)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn get_video_data_rejects_short_buffer() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_roi_format(0, 0, 800, 600, 1).unwrap();
        cam.set_output_image_type(ImageType::Raw8).unwrap();
        cam.start_video_capture().unwrap();
        let mut buf = vec![0u8; 10];
        assert_eq!(
            cam.get_video_data(&mut buf, 1000).unwrap_err(),
            Error::Svb(SvbError::BufferTooSmall)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn frame_buffer_len_matches_roi_and_format() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        cam.set_roi_format(0, 0, 800, 600, 1).unwrap();
        cam.set_output_image_type(ImageType::Raw8).unwrap();
        assert_eq!(cam.frame_buffer_len().unwrap(), 800 * 600);
        cam.set_output_image_type(ImageType::Raw16).unwrap();
        assert_eq!(cam.frame_buffer_len().unwrap(), 800 * 600 * 2);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn can_pulse_guide_reflects_property_ex() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        // The simulated SV605CC has no ST4 port.
        assert!(!cam.can_pulse_guide().unwrap());
        // Issuing a pulse anyway (e.g. driven by a stale capability cache)
        // must not panic.
        cam.pulse_guide(GuideDirection::North, 100).unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn pixel_size_matches_sim_sensor() {
        let sdk = Sdk::new().unwrap();
        let cam = sdk.open_camera(0).unwrap();
        assert!((cam.pixel_size_microns().unwrap() - 3.76).abs() < f32::EPSILON);
    }
}

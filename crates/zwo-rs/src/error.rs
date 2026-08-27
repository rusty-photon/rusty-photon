//! Error types for the safe `zwo-rs` API.
//!
//! The SDK reports failures as small integer codes (`ASI_ERROR_CODE` /
//! `EFW_ERROR_CODE` / `EAF_ERROR_CODE`). We map them to typed errors **by
//! numeric value** (the values are fixed by the vendored headers) rather than
//! by the generated `bindgen` constant names, so the mapping is stable across
//! bindgen versions.

use crate::sys;
use thiserror::Error;

/// Result alias for the safe API.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the safe `zwo-rs` API.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// An ASI camera SDK call returned a non-success code.
    #[error("ASI camera SDK error: {0}")]
    Asi(#[from] AsiError),
    /// An EFW filter-wheel SDK call returned a non-success code.
    #[error("EFW filter-wheel SDK error: {0}")]
    Efw(#[from] EfwError),
    /// An EAF focuser SDK call returned a non-success code.
    #[error("EAF focuser SDK error: {0}")]
    Eaf(#[from] EafError),
}

/// ASI camera SDK error codes (`ASI_ERROR_CODE`), mapped from the raw `int`.
///
/// `0` is `ASI_SUCCESS` and is **not** represented here — handle it via
/// [`asi_check`] before constructing an `AsiError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AsiError {
    /// `ASI_ERROR_INVALID_INDEX` — no camera connected or index out of range.
    #[error("invalid index: no camera connected or index out of range")]
    InvalidIndex,
    /// `ASI_ERROR_INVALID_ID`.
    #[error("invalid camera ID")]
    InvalidId,
    /// `ASI_ERROR_INVALID_CONTROL_TYPE`.
    #[error("invalid control type")]
    InvalidControlType,
    /// `ASI_ERROR_CAMERA_CLOSED` — the camera was not opened.
    #[error("camera not open")]
    CameraClosed,
    /// `ASI_ERROR_CAMERA_REMOVED`.
    #[error("camera removed")]
    CameraRemoved,
    /// `ASI_ERROR_INVALID_PATH`.
    #[error("invalid path")]
    InvalidPath,
    /// `ASI_ERROR_INVALID_FILEFORMAT`.
    #[error("invalid file format")]
    InvalidFileFormat,
    /// `ASI_ERROR_INVALID_SIZE` — wrong video-format size.
    #[error("invalid size: wrong video-format size")]
    InvalidSize,
    /// `ASI_ERROR_INVALID_IMGTYPE`.
    #[error("invalid (unsupported) image type")]
    InvalidImgType,
    /// `ASI_ERROR_OUTOF_BOUNDARY` — the start position is out of boundary.
    #[error("start position out of boundary")]
    OutOfBoundary,
    /// `ASI_ERROR_TIMEOUT`.
    #[error("timeout")]
    Timeout,
    /// `ASI_ERROR_INVALID_SEQUENCE` — stop capture first.
    #[error("invalid sequence: stop capture first")]
    InvalidSequence,
    /// `ASI_ERROR_BUFFER_TOO_SMALL`.
    #[error("buffer too small")]
    BufferTooSmall,
    /// `ASI_ERROR_VIDEO_MODE_ACTIVE`.
    #[error("video mode active")]
    VideoModeActive,
    /// `ASI_ERROR_EXPOSURE_IN_PROGRESS`.
    #[error("exposure in progress")]
    ExposureInProgress,
    /// `ASI_ERROR_GENERAL_ERROR` — e.g. a value out of valid range.
    #[error("general error (e.g. value out of valid range)")]
    GeneralError,
    /// `ASI_ERROR_INVALID_MODE`.
    #[error("invalid mode")]
    InvalidMode,
    /// `ASI_ERROR_GPS_NOT_SUPPORTED`.
    #[error("GPS not supported")]
    GpsNotSupported,
    /// `ASI_ERROR_GPS_VER_ERR`.
    #[error("GPS firmware version error")]
    GpsVerErr,
    /// `ASI_ERROR_GPS_FPGA_ERR`.
    #[error("GPS FPGA error")]
    GpsFpgaErr,
    /// `ASI_ERROR_GPS_PARAM_OUT_OF_RANGE`.
    #[error("GPS parameter out of range")]
    GpsParamOutOfRange,
    /// `ASI_ERROR_GPS_DATA_INVALID`.
    #[error("GPS data invalid")]
    GpsDataInvalid,
    /// A code outside the range known to this binding's vendored header.
    #[error("unknown ASI error code {0}")]
    Unknown(i32),
}

impl AsiError {
    /// Map a raw non-zero `ASI_ERROR_CODE` to a typed error.
    ///
    /// `0` (`ASI_SUCCESS`) maps to [`AsiError::Unknown(0)`] here; callers should
    /// route success through [`asi_check`] instead of calling this directly.
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            1 => Self::InvalidIndex,
            2 => Self::InvalidId,
            3 => Self::InvalidControlType,
            4 => Self::CameraClosed,
            5 => Self::CameraRemoved,
            6 => Self::InvalidPath,
            7 => Self::InvalidFileFormat,
            8 => Self::InvalidSize,
            9 => Self::InvalidImgType,
            10 => Self::OutOfBoundary,
            11 => Self::Timeout,
            12 => Self::InvalidSequence,
            13 => Self::BufferTooSmall,
            14 => Self::VideoModeActive,
            15 => Self::ExposureInProgress,
            16 => Self::GeneralError,
            17 => Self::InvalidMode,
            18 => Self::GpsNotSupported,
            19 => Self::GpsVerErr,
            20 => Self::GpsFpgaErr,
            21 => Self::GpsParamOutOfRange,
            22 => Self::GpsDataInvalid,
            other => Self::Unknown(other),
        }
    }
}

/// EFW filter-wheel SDK error codes (`EFW_ERROR_CODE`), mapped from the raw `int`.
///
/// `0` is `EFW_SUCCESS`; handle it via [`efw_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EfwError {
    /// `EFW_ERROR_INVALID_INDEX`.
    #[error("invalid index")]
    InvalidIndex,
    /// `EFW_ERROR_INVALID_ID`.
    #[error("invalid ID")]
    InvalidId,
    /// `EFW_ERROR_INVALID_VALUE`.
    #[error("invalid value")]
    InvalidValue,
    /// `EFW_ERROR_REMOVED`.
    #[error("filter wheel removed")]
    Removed,
    /// `EFW_ERROR_MOVING` — the wheel is moving (distinct from the `-1`
    /// position sentinel `EFWGetPosition` writes while moving).
    #[error("filter wheel is moving")]
    Moving,
    /// `EFW_ERROR_ERROR_STATE`.
    #[error("filter wheel is in error state")]
    ErrorState,
    /// `EFW_ERROR_GENERAL_ERROR`.
    #[error("general error")]
    GeneralError,
    /// `EFW_ERROR_NOT_SUPPORTED`.
    #[error("operation not supported by the firmware")]
    NotSupported,
    /// `EFW_ERROR_CLOSED` — the wheel was not opened.
    #[error("filter wheel not open")]
    Closed,
    /// A code outside the range known to this binding's vendored header.
    #[error("unknown EFW error code {0}")]
    Unknown(i32),
}

impl EfwError {
    /// Map a raw non-zero `EFW_ERROR_CODE` to a typed error.
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            1 => Self::InvalidIndex,
            2 => Self::InvalidId,
            3 => Self::InvalidValue,
            4 => Self::Removed,
            5 => Self::Moving,
            6 => Self::ErrorState,
            7 => Self::GeneralError,
            8 => Self::NotSupported,
            9 => Self::Closed,
            other => Self::Unknown(other),
        }
    }
}

/// EAF focuser SDK error codes (`EAF_ERROR_CODE`), mapped from the raw `int`.
///
/// `0` is `EAF_SUCCESS`; handle it via [`eaf_check`]. The BLE-specific codes
/// (`EAF_BLE_*`, starting at 50) are out of scope for this driver (see
/// `docs/services/zwo-focuser.md` "MVP scope") and fold into [`Self::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EafError {
    /// `EAF_ERROR_INVALID_INDEX`.
    #[error("invalid index")]
    InvalidIndex,
    /// `EAF_ERROR_INVALID_ID`.
    #[error("invalid ID")]
    InvalidId,
    /// `EAF_ERROR_INVALID_VALUE`.
    #[error("invalid value")]
    InvalidValue,
    /// `EAF_ERROR_REMOVED` — failed to find the focuser; it may have been
    /// unplugged.
    #[error("focuser removed")]
    Removed,
    /// `EAF_ERROR_MOVING` — the focuser is moving.
    #[error("focuser is moving")]
    Moving,
    /// `EAF_ERROR_ERROR_STATE`.
    #[error("focuser is in error state")]
    ErrorState,
    /// `EAF_ERROR_GENERAL_ERROR`.
    #[error("general error")]
    GeneralError,
    /// `EAF_ERROR_NOT_SUPPORTED`.
    #[error("operation not supported by the firmware")]
    NotSupported,
    /// `EAF_ERROR_CLOSED` — the focuser was not opened.
    #[error("focuser not open")]
    Closed,
    /// `EAF_ERROR_BATTER_INFO` (the header's own spelling).
    #[error("battery info error")]
    BatteryInfo,
    /// `EAF_ERROR_INVALID_LENGTH`.
    #[error("invalid length")]
    InvalidLength,
    /// A code outside the range known to this binding's vendored header
    /// (includes the out-of-scope `EAF_BLE_*` codes, 50+).
    #[error("unknown EAF error code {0}")]
    Unknown(i32),
}

impl EafError {
    /// Map a raw non-zero `EAF_ERROR_CODE` to a typed error.
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            1 => Self::InvalidIndex,
            2 => Self::InvalidId,
            3 => Self::InvalidValue,
            4 => Self::Removed,
            5 => Self::Moving,
            6 => Self::ErrorState,
            7 => Self::GeneralError,
            8 => Self::NotSupported,
            9 => Self::Closed,
            10 => Self::BatteryInfo,
            11 => Self::InvalidLength,
            other => Self::Unknown(other),
        }
    }
}

/// Convert a raw `ASI_ERROR_CODE` into `Result<()>` — `0` is success.
///
/// # Errors
/// Returns [`Error::Asi`] for any non-zero code.
pub fn asi_check(code: sys::ASI_ERROR_CODE) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Asi(AsiError::from_code(
            i32::try_from(code).unwrap_or(i32::MAX),
        )))
    }
}

/// Convert a raw `EFW_ERROR_CODE` into `Result<()>` — `0` is success.
///
/// # Errors
/// Returns [`Error::Efw`] for any non-zero code.
pub const fn efw_check(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Efw(EfwError::from_code(code)))
    }
}

/// Convert a raw `EAF_ERROR_CODE` into `Result<()>` — `0` is success.
///
/// # Errors
/// Returns [`Error::Eaf`] for any non-zero code.
pub const fn eaf_check(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Eaf(EafError::from_code(code)))
    }
}

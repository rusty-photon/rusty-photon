//! Error types for the safe `svbony-rs` API.
//!
//! The SDK reports failures as small integer codes (`SVB_ERROR_CODE`). We map
//! them to a typed error **by numeric value** (fixed by the hand-transcribed
//! header — see `libsvbony-sys`'s crate docs) rather than by named constants,
//! so the mapping is stable regardless of how the sys crate's bindings are
//! generated.

use thiserror::Error;

/// Result alias for the safe API.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the safe `svbony-rs` API.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// An `SVBony` camera SDK call returned a non-success code.
    #[error("SVBony camera SDK error: {0}")]
    Svb(#[from] SvbError),
}

/// `SVBony` camera SDK error codes (`SVB_ERROR_CODE`), mapped from the raw `int`.
///
/// `0` is `SVB_SUCCESS` and is **not** represented here — handle it via
/// [`svb_check`] before constructing an `SvbError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SvbError {
    /// `SVB_ERROR_INVALID_INDEX` — no camera connected or index out of range.
    #[error("invalid index: no camera connected or index out of range")]
    InvalidIndex,
    /// `SVB_ERROR_INVALID_ID`.
    #[error("invalid camera ID")]
    InvalidId,
    /// `SVB_ERROR_INVALID_CONTROL_TYPE`.
    #[error("invalid control type")]
    InvalidControlType,
    /// `SVB_ERROR_CAMERA_CLOSED` — the camera was not opened.
    #[error("camera not open")]
    CameraClosed,
    /// `SVB_ERROR_CAMERA_REMOVED`.
    #[error("camera removed")]
    CameraRemoved,
    /// `SVB_ERROR_INVALID_PATH`.
    #[error("invalid path")]
    InvalidPath,
    /// `SVB_ERROR_INVALID_FILEFORMAT`.
    #[error("invalid file format")]
    InvalidFileFormat,
    /// `SVB_ERROR_INVALID_SIZE` — wrong ROI size (`width % 8 != 0`,
    /// `height % 2 != 0`, or unsupported binning/dimensions).
    #[error("invalid size: wrong ROI size or unsupported binning")]
    InvalidSize,
    /// `SVB_ERROR_INVALID_IMGTYPE`.
    #[error("invalid (unsupported) image type")]
    InvalidImgType,
    /// `SVB_ERROR_OUTOF_BOUNDARY` — the start position is out of boundary.
    #[error("start position out of boundary")]
    OutOfBoundary,
    /// `SVB_ERROR_TIMEOUT`.
    #[error("timeout")]
    Timeout,
    /// `SVB_ERROR_INVALID_SEQUENCE` — stop capture first.
    #[error("invalid sequence: stop capture first")]
    InvalidSequence,
    /// `SVB_ERROR_BUFFER_TOO_SMALL`.
    #[error("buffer too small")]
    BufferTooSmall,
    /// `SVB_ERROR_VIDEO_MODE_ACTIVE`.
    #[error("video mode active")]
    VideoModeActive,
    /// `SVB_ERROR_EXPOSURE_IN_PROGRESS`.
    #[error("exposure in progress")]
    ExposureInProgress,
    /// `SVB_ERROR_GENERAL_ERROR` — e.g. a value out of valid range.
    #[error("general error (e.g. value out of valid range)")]
    GeneralError,
    /// `SVB_ERROR_INVALID_MODE`.
    #[error("invalid mode")]
    InvalidMode,
    /// `SVB_ERROR_INVALID_DIRECTION`.
    #[error("invalid direction")]
    InvalidDirection,
    /// `SVB_ERROR_UNKNOW_SENSOR_TYPE` (the header's own spelling).
    #[error("unknown sensor type")]
    UnknownSensorType,
    /// A code outside the range known to this binding's hand-transcribed header.
    #[error("unknown SVBony error code {0}")]
    Unknown(i32),
}

impl SvbError {
    /// Map a raw non-zero `SVB_ERROR_CODE` to a typed error.
    ///
    /// `0` (`SVB_SUCCESS`) maps to [`SvbError::Unknown(0)`] here; callers
    /// should route success through [`svb_check`] instead of calling this
    /// directly.
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
            18 => Self::InvalidDirection,
            19 => Self::UnknownSensorType,
            other => Self::Unknown(other),
        }
    }
}

/// Convert a raw `SVB_ERROR_CODE` into `Result<()>` — `0` is success.
///
/// # Errors
/// Returns [`Error::Svb`] for any non-zero code.
pub const fn svb_check(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Svb(SvbError::from_code(code)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svb_check_maps_known_and_unknown_codes() {
        svb_check(0).unwrap();
        assert_eq!(
            svb_check(1).unwrap_err(),
            Error::Svb(SvbError::InvalidIndex)
        );
        assert_eq!(
            svb_check(16).unwrap_err(),
            Error::Svb(SvbError::GeneralError)
        );
        assert_eq!(
            svb_check(19).unwrap_err(),
            Error::Svb(SvbError::UnknownSensorType)
        );
        assert_eq!(
            svb_check(999).unwrap_err(),
            Error::Svb(SvbError::Unknown(999))
        );
    }

    /// Every code 1-19 in the hand-transcribed header maps to its own
    /// distinct, non-`Unknown` variant — a full sweep so a future header
    /// edit that drops or reorders an arm (and silently falls through to
    /// `Unknown`) fails here instead of only being caught by chance in
    /// whichever single code an existing test happened to exercise.
    #[test]
    fn from_code_maps_every_known_code_to_a_distinct_variant() {
        let known = [
            (1, SvbError::InvalidIndex),
            (2, SvbError::InvalidId),
            (3, SvbError::InvalidControlType),
            (4, SvbError::CameraClosed),
            (5, SvbError::CameraRemoved),
            (6, SvbError::InvalidPath),
            (7, SvbError::InvalidFileFormat),
            (8, SvbError::InvalidSize),
            (9, SvbError::InvalidImgType),
            (10, SvbError::OutOfBoundary),
            (11, SvbError::Timeout),
            (12, SvbError::InvalidSequence),
            (13, SvbError::BufferTooSmall),
            (14, SvbError::VideoModeActive),
            (15, SvbError::ExposureInProgress),
            (16, SvbError::GeneralError),
            (17, SvbError::InvalidMode),
            (18, SvbError::InvalidDirection),
            (19, SvbError::UnknownSensorType),
        ];
        for (code, want) in known {
            let got = SvbError::from_code(code);
            assert_eq!(got, want, "code {code}");
            assert_ne!(got, SvbError::Unknown(code), "code {code} fell through");
        }
    }

    #[test]
    fn from_code_maps_codes_outside_the_known_range_to_unknown() {
        for code in [0, 20, -1, i32::MAX, i32::MIN] {
            assert_eq!(SvbError::from_code(code), SvbError::Unknown(code));
        }
    }
}

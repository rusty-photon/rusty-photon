use thiserror::Error;

use crate::ControlType;

/// Errors returned by the safe `qhyccd-rs` API.
///
/// Most QHYCCD SDK functions return a bare `u32` — `0` (`QHYCCD_SUCCESS`) on
/// success and `u32::MAX` (`QHYCCD_ERROR`) on failure — with **no
/// discriminating error codes**. Unlike the sibling `zwo-rs` / `svbony-rs`
/// crates, whose SDKs return a rich error-code enum worth mapping (`asi_check` /
/// `svb_check`), there is no numeric code to preserve here. A failed SDK call is
/// therefore reported as [`QHYError::Sdk`] carrying a `'static` operation label
/// that names the wrapper method which failed; route such calls through
/// [`check`]. The remaining variants capture the genuinely-distinct conditions
/// the wrapper detects itself: a closed camera, the three control-scoped
/// parameter failures (where the [`ControlType`] is real information), an
/// argument outside the range the SDK can address, the two FFI
/// string-conversion errors, an id that names no enumerated camera, and a
/// second `Sdk` refused while one is alive.
///
/// This mirrors the flat shape of the sibling crates' error enums (`zwo-rs`'s
/// `Error`, `svbony-rs`'s `Error`); it carries no per-call-site variant. The
/// operation that failed is conveyed by the `op` label, not the enum shape.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum QHYError {
    /// A QHYCCD SDK call returned its failure sentinel (in practice `u32::MAX`,
    /// `QHYCCD_ERROR`).
    ///
    /// `op` is a `'static` label for the wrapper operation that failed (usually
    /// the method name, e.g. `"set_stream_mode"`). The SDK exposes no error code
    /// beyond the success/failure bit, so none is carried.
    #[error("QHYCCD SDK operation '{op}' failed")]
    Sdk {
        /// `'static` label for the wrapper operation that failed.
        op: &'static str,
    },
    /// The camera is not open (the shared handle cell is `None`, or the
    /// simulated backend is closed).
    #[error("camera is not open")]
    CameraNotOpen,
    /// Reading a camera parameter for `control` failed.
    #[error("error getting camera parameter for control {control:?}")]
    GetParameter {
        /// The control whose value could not be read.
        control: ControlType,
    },
    /// Determining whether `control` is supported failed.
    #[error("error determining support for camera feature {control:?}")]
    IsControlAvailable {
        /// The control whose availability could not be determined.
        control: ControlType,
    },
    /// Reading the min/max/step range for `control` failed.
    #[error("error getting camera min, max, step for control {control:?}")]
    GetMinMaxStep {
        /// The control whose range could not be read.
        control: ControlType,
    },
    /// The caller-owned buffer passed to
    /// [`Camera::get_single_frame`](crate::Camera::get_single_frame) /
    /// [`Camera::get_live_frame`](crate::Camera::get_live_frame) is smaller than
    /// the frame to download. Detected by the wrapper *before* the SDK is called
    /// (the FFI would otherwise write out of bounds), so it carries the sizes
    /// rather than an SDK operation label.
    #[error("frame buffer too small: need {needed} bytes, got {got}")]
    BufferTooSmall {
        /// bytes the frame requires
        needed: usize,
        /// bytes the caller provided
        got: usize,
    },
    /// A filter-wheel slot outside the sixteen the SDK can address. Slots travel
    /// over `CONTROL_CFWPORT` as a single hex digit (`'0'`..`'F'`), so slot 16
    /// and up have no code to be commanded with, and reporting one back would
    /// mean the wheel is at a position this encoding cannot name.
    #[error("filter-wheel slot {slot} is outside the SDK's 16 addressable positions")]
    InvalidFilterSlot {
        /// The slot that has no `CONTROL_CFWPORT` code.
        slot: u32,
    },
    /// A string returned by the SDK was not valid UTF-8.
    #[error("invalid UTF-8 in a string returned by the SDK: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// The camera id contains an interior NUL byte and cannot be passed to the
    /// SDK as a C string.
    #[error("camera id contains an interior NUL byte: {0}")]
    InvalidCameraId(#[from] std::ffi::NulError),
    /// [`Sdk::probe_filter_wheels`](crate::Sdk::probe_filter_wheels) was asked
    /// about an id that names no camera the SDK enumerated.
    #[error("no camera with id '{id}' was enumerated")]
    UnknownCameraId {
        /// The id that matched no enumerated camera.
        id: String,
    },
    /// [`Sdk::try_new`](crate::Sdk::try_new) found another
    /// [`Sdk`](crate::Sdk) alive in this process. The SDK's resource is the
    /// process's and is not counted, so one `Sdk` at a time holds it.
    #[error("another QHYCCD SDK instance is alive in this process")]
    SdkInUse,
}

/// Convenience alias for fallible QHYCCD SDK operations: `Result<T, QHYError>`.
pub type Result<T> = core::result::Result<T, QHYError>;

/// Convert a raw QHYCCD status word into `Result<()>`, tagging any failure with
/// an operation label.
///
/// Most QHYCCD SDK entry points return a bare `u32` where `0` (`QHYCCD_SUCCESS`)
/// is success and any other value (in practice `u32::MAX`, `QHYCCD_ERROR`) is the
/// sole failure indication — there are no discriminating error codes. This is the
/// QHY analogue of `zwo-rs`'s `asi_check` / `svbony-rs`'s `svb_check`; because the
/// QHY ABI carries no code to map, it takes a `'static` `op` label instead, which
/// becomes [`QHYError::Sdk`]'s operation name on failure. On the failure path it
/// logs the error via `tracing::error!`, centralising the per-call-site logging
/// the void SDK wrappers previously did by hand.
///
/// The status is compared against the literal `0` (rather than the `ffi`/`sys`
/// `QHYCCD_SUCCESS` constant) so the check does not depend on how the bindings
/// are generated — matching the sibling crates' hard-coded `code == 0`. It is
/// **not** used for the value-returning entry points (`GetQHYCCDMemLength`,
/// `GetQHYCCDParam`, `GetQHYCCDType`, …) whose `u32::MAX` / `u32::MAX as f64`
/// sentinel must be distinguished from a valid returned value at the call site;
/// those keep their explicit `match` and build the error directly.
///
/// # Errors
/// Returns [`QHYError::Sdk`] tagged with `op` for any non-`0` status word.
pub fn check(status: u32, op: &'static str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        let error = QHYError::Sdk { op };
        tracing::error!(error = ?error);
        Err(error)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::ControlType;

    #[test]
    fn check_returns_ok_on_success_status() {
        check(0, "set_stream_mode").unwrap();
    }

    #[test]
    fn check_returns_sdk_error_tagged_with_op_on_failure() {
        let error = check(u32::MAX, "set_stream_mode").unwrap_err();
        assert_eq!(
            error,
            QHYError::Sdk {
                op: "set_stream_mode"
            }
        );
        assert_eq!(
            error.to_string(),
            "QHYCCD SDK operation 'set_stream_mode' failed"
        );
    }

    #[test]
    fn check_treats_any_nonzero_status_as_failure() {
        // The SDK's documented failure sentinel is u32::MAX, but `check` treats
        // every non-zero status as failure so a stray non-sentinel code is not
        // silently read as success.
        check(1, "set_bin_mode").unwrap_err();
    }

    #[test]
    fn control_scoped_errors_display_the_control() {
        assert_eq!(
            QHYError::GetParameter {
                control: ControlType::Gain,
            }
            .to_string(),
            "error getting camera parameter for control Gain"
        );
        assert_eq!(
            QHYError::GetMinMaxStep {
                control: ControlType::Exposure,
            }
            .to_string(),
            "error getting camera min, max, step for control Exposure"
        );
        assert_eq!(
            QHYError::IsControlAvailable {
                control: ControlType::Cooler,
            }
            .to_string(),
            "error determining support for camera feature Cooler"
        );
    }

    #[test]
    fn camera_not_open_displays_without_a_code() {
        assert_eq!(QHYError::CameraNotOpen.to_string(), "camera is not open");
    }

    #[test]
    fn sdk_in_use_says_another_sdk_is_alive_here() {
        assert_eq!(
            QHYError::SdkInUse.to_string(),
            "another QHYCCD SDK instance is alive in this process"
        );
    }

    #[test]
    fn buffer_too_small_displays_both_sizes() {
        assert_eq!(
            QHYError::BufferTooSmall {
                needed: 2048,
                got: 1000,
            }
            .to_string(),
            "frame buffer too small: need 2048 bytes, got 1000"
        );
    }

    #[test]
    fn utf8_error_converts_via_from() {
        // Built at runtime (not a literal) so the `invalid_from_utf8` lint does not
        // fire on a known-invalid literal.
        let invalid = vec![0xff_u8, 0xff];
        let utf8_error = std::str::from_utf8(&invalid).unwrap_err();
        let error: QHYError = utf8_error.into();
        assert_eq!(error, QHYError::InvalidUtf8(utf8_error));
    }

    #[test]
    fn nul_error_converts_via_from() {
        let nul_error = std::ffi::CString::new(vec![b'a', 0, b'b']).unwrap_err();
        let error: QHYError = nul_error.clone().into();
        assert_eq!(error, QHYError::InvalidCameraId(nul_error));
    }
}

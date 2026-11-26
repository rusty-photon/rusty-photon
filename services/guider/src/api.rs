//! The API's error envelope, stop reasons and request bodies.
//!
//! See docs/services/guider.md § Error envelope, § Stopping on its own and
//! § HTTP API.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::calibration_store::{check_name, leg_name};
use rp_guiding::{CalibrationError, Check, Leg};

/// Why the loop, or the run, stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A `guiding/stop`.
    Requested,
    /// `camera.max_frame_failures` failed frames in a row, or a camera
    /// that came back with another sensor.
    CameraError,
    /// The mount check found `Tracking` false.
    MountNotTracking,
    /// The mount check found `Slewing` true.
    MountSlewing,
    /// The mount check found `AtPark` true.
    MountParked,
    /// The mount check found another pier side than the run's.
    PierSideChanged,
    /// `mount.max_failures` cycles in a row with a failed mount read or
    /// pulse call.
    MountUnreachable,
    /// A pulse still in progress `mount.pulse_timeout` after its duration.
    PulseTimeout,
    /// Corrections in one direction that the error did not follow.
    Runaway,
    /// A calibration failed inside a run that was already guiding.
    CalibrationFailed,
}

impl StopReason {
    /// The reason as the API names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::CameraError => "camera_error",
            Self::MountNotTracking => "mount_not_tracking",
            Self::MountSlewing => "mount_slewing",
            Self::MountParked => "mount_parked",
            Self::PierSideChanged => "pier_side_changed",
            Self::MountUnreachable => "mount_unreachable",
            Self::PulseTimeout => "pulse_timeout",
            Self::Runaway => "runaway",
            Self::CalibrationFailed => "calibration_failed",
        }
    }
}

/// A stop, with what it says of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub reason: StopReason,
    pub message: String,
}

impl Stop {
    /// A stop a client asked for.
    #[must_use]
    pub fn requested() -> Self {
        Self {
            reason: StopReason::Requested,
            message: "a client asked the guider to stop".to_owned(),
        }
    }

    /// A stop for the camera's sake, saying why.
    #[must_use]
    pub const fn camera_error(message: String) -> Self {
        Self {
            reason: StopReason::CameraError,
            message,
        }
    }

    /// A stop for `reason`, saying why.
    #[must_use]
    pub fn new(reason: StopReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// The error codes of the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    NoFrame,
    NotConfigured,
    NotGuiding,
    NotIdle,
    NotExposing,
    DeviceNotConnected,
    MountNotReady,
    Stopped,
    DitherAborted,
    CameraUnsupported,
    NoStars,
    CalibrationFailed,
    GuideFailed,
    DeviceUnreachable,
    SettleTimeout,
    StopTimeout,
    Internal,
}

impl ErrorCode {
    const fn name(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::NoFrame => "no_frame",
            Self::NotConfigured => "not_configured",
            Self::NotGuiding => "not_guiding",
            Self::NotIdle => "not_idle",
            Self::NotExposing => "not_exposing",
            Self::DeviceNotConnected => "device_not_connected",
            Self::MountNotReady => "mount_not_ready",
            Self::Stopped => "stopped",
            Self::DitherAborted => "dither_aborted",
            Self::CameraUnsupported => "camera_unsupported",
            Self::NoStars => "no_stars",
            Self::CalibrationFailed => "calibration_failed",
            Self::GuideFailed => "guide_failed",
            Self::DeviceUnreachable => "device_unreachable",
            Self::SettleTimeout => "settle_timeout",
            Self::StopTimeout => "stop_timeout",
            Self::Internal => "internal",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::NoFrame => StatusCode::NOT_FOUND,
            Self::NotConfigured
            | Self::NotGuiding
            | Self::NotIdle
            | Self::NotExposing
            | Self::DeviceNotConnected
            | Self::MountNotReady
            | Self::Stopped
            | Self::DitherAborted => StatusCode::CONFLICT,
            Self::CameraUnsupported
            | Self::NoStars
            | Self::CalibrationFailed
            | Self::GuideFailed => StatusCode::UNPROCESSABLE_ENTITY,
            Self::DeviceUnreachable => StatusCode::BAD_GATEWAY,
            Self::SettleTimeout | Self::StopTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// What an error attaches as its `details`.
#[derive(Debug, Clone, PartialEq)]
pub enum Details {
    /// The reason of the stop a `guide_failed` reports.
    StopReason(StopReason),
    /// The leg of a calibration that failed.
    Leg(Leg),
    /// The cross-check of a calibration that failed.
    Check {
        check: Check,
        value: f64,
        threshold: f64,
    },
}

/// An error the API answers with.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<Details>,
}

impl ApiError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// The reason of the stop a `guide_failed` reports.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<StopReason> {
        match self.details {
            Some(Details::StopReason(reason)) => Some(reason),
            _ => None,
        }
    }

    /// A calibration that failed, with the leg or the cross-check.
    #[must_use]
    pub fn calibration_failed(error: &CalibrationError) -> Self {
        let details = match *error {
            CalibrationError::OutOfSteps { leg } | CalibrationError::TimedOut { leg } => {
                Details::Leg(leg)
            }
            CalibrationError::CheckFailed {
                check,
                value,
                threshold,
            } => Details::Check {
                check,
                value,
                threshold,
            },
        };
        Self {
            code: ErrorCode::CalibrationFailed,
            message: format!("the calibration failed: {error}"),
            details: Some(details),
        }
    }

    /// A frame with fewer guide stars than `stars.min`.
    #[must_use]
    pub fn no_stars() -> Self {
        Self::new(
            ErrorCode::NoStars,
            "the frame has fewer guide stars than stars.min",
        )
    }

    /// A request that needs frames while the guider takes none.
    #[must_use]
    pub fn not_exposing() -> Self {
        Self::new(ErrorCode::NotExposing, "the guider takes no frames")
    }

    /// A request that needs a run that guides, outside one.
    #[must_use]
    pub fn not_guiding() -> Self {
        Self::new(
            ErrorCode::NotGuiding,
            "the guider is not in a run that guides",
        )
    }

    /// A `dither/abort` ended the dither the request waited on.
    #[must_use]
    pub fn dither_aborted() -> Self {
        Self::new(
            ErrorCode::DitherAborted,
            "a dither/abort arrived while the dither waited to settle",
        )
    }

    /// A `guiding/stop` arrived while the request waited or queued.
    #[must_use]
    pub fn stopped() -> Self {
        Self::new(
            ErrorCode::Stopped,
            "a guiding/stop arrived while the request waited",
        )
    }

    /// The loop stopped by itself while the request waited.
    #[must_use]
    pub fn guide_failed(stop: &Stop) -> Self {
        Self {
            code: ErrorCode::GuideFailed,
            message: format!("the loop stopped: {}", stop.message),
            details: Some(Details::StopReason(stop.reason)),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let details = self.details.map(|details| match details {
            Details::StopReason(reason) => json!({ "stop_reason": reason.as_str() }),
            Details::Leg(leg) => json!({ "leg": leg_name(leg) }),
            Details::Check {
                check,
                value,
                threshold,
            } => json!({ "check": check_name(check), "value": value, "threshold": threshold }),
        });
        let body = match details {
            Some(details) => json!({
                "error": self.code.name(),
                "message": self.message,
                "details": details,
            }),
            None => json!({ "error": self.code.name(), "message": self.message }),
        };
        (self.code.status(), Json(body)).into_response()
    }
}

/// The body of a request that takes no fields: an empty JSON object.
///
/// It converts from a JSON map rather than deriving a field-less struct with
/// `deny_unknown_fields`, whose derive expands to an empty enum that
/// `clippy::empty_enums` denies.
#[derive(Debug, Deserialize)]
#[serde(try_from = "serde_json::Map<String, serde_json::Value>")]
pub struct NoFields;

impl TryFrom<serde_json::Map<String, serde_json::Value>> for NoFields {
    type Error = &'static str;

    fn try_from(fields: serde_json::Map<String, serde_json::Value>) -> Result<Self, Self::Error> {
        if fields.is_empty() {
            Ok(Self)
        } else {
            Err("the request takes no fields")
        }
    }
}

/// The size of a settle tolerance or a dither: pixels of the measured frame,
/// or arcseconds at the pixel scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    Pixels(f64),
    Arcsec(f64),
}

impl Size {
    /// The size in pixels at `pixel_scale`, in arcseconds a pixel.
    #[must_use]
    pub fn pixels(self, pixel_scale: f64) -> f64 {
        match self {
            Self::Pixels(pixels) => pixels,
            Self::Arcsec(arcsec) => arcsec / pixel_scale,
        }
    }
}

/// A settle's terms, the configuration's `settling` filling what the
/// request leaves out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettleSpec {
    pub tolerance: Size,
    pub time: Duration,
    pub timeout: Duration,
}

/// `settle` of `guiding/start` and `dither`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettleBody {
    pixels: Option<f64>,
    arcsec: Option<f64>,
    time: Option<String>,
    timeout: Option<String>,
}

/// A size given as at most one of a pixel and an arcsecond field.
///
/// # Errors
///
/// `invalid_request` for both, and for a size that is not a positive
/// number.
fn size(pixels: Option<f64>, arcsec: Option<f64>, what: &str) -> Result<Option<Size>, ApiError> {
    let positive = |value: f64| value.is_finite() && value > 0.0;
    match (pixels, arcsec) {
        (Some(_), Some(_)) => Err(invalid(format!(
            "{what} takes one of its pixel and its arcsecond field, not both"
        ))),
        (Some(pixels), None) if positive(pixels) => Ok(Some(Size::Pixels(pixels))),
        (None, Some(arcsec)) if positive(arcsec) => Ok(Some(Size::Arcsec(arcsec))),
        (None, None) => Ok(None),
        _ => Err(invalid(format!("{what} must be a positive number"))),
    }
}

/// `invalid_request` with `message`.
fn invalid(message: String) -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest, message)
}

/// A duration field of a settle.
fn duration(text: &str, what: &str) -> Result<Duration, ApiError> {
    humantime::parse_duration(text).map_err(|e| invalid(format!("settle.{what} {text:?}: {e}")))
}

impl SettleBody {
    /// The settle's terms, each field the body leaves out taken from
    /// `default`.
    ///
    /// # Errors
    ///
    /// `invalid_request` for both a pixel and an arcsecond tolerance, a
    /// tolerance that is not a positive number, a duration that does not
    /// parse, and a timeout of zero.
    pub fn resolve(&self, default: SettleSpec) -> Result<SettleSpec, ApiError> {
        let tolerance = size(self.pixels, self.arcsec, "settle")?.unwrap_or(default.tolerance);
        let time = self
            .time
            .as_deref()
            .map(|text| duration(text, "time"))
            .transpose()?
            .unwrap_or(default.time);
        let timeout = self
            .timeout
            .as_deref()
            .map(|text| duration(text, "timeout"))
            .transpose()?
            .unwrap_or(default.timeout);
        if timeout.is_zero() {
            return Err(invalid(
                "settle.timeout must be longer than zero".to_owned(),
            ));
        }
        Ok(SettleSpec {
            tolerance,
            time,
            timeout,
        })
    }
}

/// The body of `guiding/start`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StartBody {
    pub recalibrate: bool,
    pub settle: SettleBody,
}

/// The body of `dither`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DitherBody {
    amount_px: Option<f64>,
    amount_arcsec: Option<f64>,
    #[serde(default)]
    pub ra_only: bool,
    #[serde(default)]
    pub settle: SettleBody,
}

impl DitherBody {
    /// The dither's size.
    ///
    /// # Errors
    ///
    /// `invalid_request` for neither or both of `amount_px` and
    /// `amount_arcsec`, and an amount that is not a positive number.
    pub fn amount(&self) -> Result<Size, ApiError> {
        size(self.amount_px, self.amount_arcsec, "the dither's amount")?
            .ok_or_else(|| invalid("a dither takes one of amount_px and amount_arcsec".to_owned()))
    }
}

/// The body of `guiding/pause`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PauseBody {
    pub full: bool,
}

/// `body` read as `T`, an empty body as `{}`.
///
/// # Errors
///
/// `invalid_request` for a body that does not match `T`.
pub fn read_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    let text: &[u8] = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        body
    };
    serde_json::from_slice(text).map_err(|e| {
        ApiError::new(
            ErrorCode::InvalidRequest,
            format!("the body does not match the request: {e}"),
        )
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    async fn answer(error: ApiError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn an_error_answers_its_status_with_the_envelope() {
        let (status, body) = answer(ApiError::new(ErrorCode::NoFrame, "no frame yet")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body,
            json!({ "error": "no_frame", "message": "no frame yet" })
        );
    }

    #[tokio::test]
    async fn guide_failed_carries_the_stop_reason_in_its_details() {
        let stop = Stop::camera_error("3 frames failed in a row".to_owned());
        let (status, body) = answer(ApiError::guide_failed(&stop)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            json!({
                "error": "guide_failed",
                "message": "the loop stopped: 3 frames failed in a row",
                "details": { "stop_reason": "camera_error" }
            })
        );
    }

    #[tokio::test]
    async fn a_failed_calibration_names_its_leg_or_its_cross_check() {
        let (status, body) = answer(ApiError::calibration_failed(
            &CalibrationError::OutOfSteps {
                leg: Leg::ClearBacklash,
            },
        ))
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["details"], json!({ "leg": "clear_backlash" }));
        let (_, body) = answer(ApiError::calibration_failed(
            &CalibrationError::CheckFailed {
                check: Check::RateRatio,
                value: 0.75,
                threshold: 0.5,
            },
        ))
        .await;
        assert_eq!(
            body["details"],
            json!({ "check": "rate_ratio", "value": 0.75, "threshold": 0.5 })
        );
    }

    #[test]
    fn every_stop_reason_has_its_api_name() {
        let names = [
            StopReason::Requested,
            StopReason::CameraError,
            StopReason::MountNotTracking,
            StopReason::MountSlewing,
            StopReason::MountParked,
            StopReason::PierSideChanged,
            StopReason::MountUnreachable,
            StopReason::PulseTimeout,
            StopReason::Runaway,
            StopReason::CalibrationFailed,
        ]
        .map(StopReason::as_str);
        assert_eq!(
            names,
            [
                "requested",
                "camera_error",
                "mount_not_tracking",
                "mount_slewing",
                "mount_parked",
                "pier_side_changed",
                "mount_unreachable",
                "pulse_timeout",
                "runaway",
                "calibration_failed"
            ]
        );
    }

    #[test]
    fn every_code_has_the_status_of_the_design() {
        for (code, name, status) in [
            (ErrorCode::InvalidRequest, "invalid_request", 400),
            (ErrorCode::NoFrame, "no_frame", 404),
            (ErrorCode::NotConfigured, "not_configured", 409),
            (ErrorCode::NotGuiding, "not_guiding", 409),
            (ErrorCode::DitherAborted, "dither_aborted", 409),
            (ErrorCode::SettleTimeout, "settle_timeout", 504),
            (ErrorCode::NotIdle, "not_idle", 409),
            (ErrorCode::NotExposing, "not_exposing", 409),
            (ErrorCode::DeviceNotConnected, "device_not_connected", 409),
            (ErrorCode::MountNotReady, "mount_not_ready", 409),
            (ErrorCode::Stopped, "stopped", 409),
            (ErrorCode::CameraUnsupported, "camera_unsupported", 422),
            (ErrorCode::NoStars, "no_stars", 422),
            (ErrorCode::CalibrationFailed, "calibration_failed", 422),
            (ErrorCode::GuideFailed, "guide_failed", 422),
            (ErrorCode::DeviceUnreachable, "device_unreachable", 502),
            (ErrorCode::StopTimeout, "stop_timeout", 504),
            (ErrorCode::Internal, "internal", 500),
        ] {
            assert_eq!((code.name(), code.status().as_u16()), (name, status));
        }
    }

    const DEFAULT: SettleSpec = SettleSpec {
        tolerance: Size::Pixels(0.5),
        time: Duration::from_secs(10),
        timeout: Duration::from_secs(60),
    };

    fn settle(body: &str) -> Result<SettleSpec, ApiError> {
        read_body::<SettleBody>(&Bytes::from(body.to_owned()))?.resolve(DEFAULT)
    }

    #[test]
    fn a_settle_takes_each_field_it_leaves_out_from_the_configuration() {
        assert_eq!(settle("{}").unwrap(), DEFAULT);
        assert_eq!(
            settle(r#"{"arcsec": 2.0, "time": "1s"}"#).unwrap(),
            SettleSpec {
                tolerance: Size::Arcsec(2.0),
                time: Duration::from_secs(1),
                ..DEFAULT
            }
        );
    }

    #[test]
    fn a_settle_the_guider_cannot_read_is_invalid() {
        for body in [
            r#"{"pixels": 1.0, "arcsec": 1.0}"#,
            r#"{"pixels": 0.0}"#,
            r#"{"arcsec": -1.0}"#,
            r#"{"time": "soon"}"#,
            r#"{"timeout": "0s"}"#,
            r#"{"seconds": 1}"#,
        ] {
            assert_eq!(
                settle(body).unwrap_err().code,
                ErrorCode::InvalidRequest,
                "{body}"
            );
        }
    }

    #[test]
    fn a_dither_takes_exactly_one_positive_amount() {
        let amount = |body: &str| {
            read_body::<DitherBody>(&Bytes::from(body.to_owned())).and_then(|body| body.amount())
        };
        assert_eq!(amount(r#"{"amount_px": 3.0}"#).unwrap(), Size::Pixels(3.0));
        assert_eq!(
            amount(r#"{"amount_arcsec": 6.0}"#).unwrap(),
            Size::Arcsec(6.0)
        );
        for body in [
            "{}",
            r#"{"amount_px": 3.0, "amount_arcsec": 6.0}"#,
            r#"{"amount_px": 0.0}"#,
        ] {
            assert_eq!(
                amount(body).unwrap_err().code,
                ErrorCode::InvalidRequest,
                "{body}"
            );
        }
    }

    #[test]
    fn a_size_in_arcseconds_is_taken_at_the_pixel_scale() {
        assert_eq!(Size::Arcsec(6.0).pixels(2.0), 3.0);
        assert_eq!(Size::Pixels(6.0).pixels(2.0), 6.0);
    }

    #[test]
    fn an_empty_body_is_read_as_an_empty_object() {
        read_body::<NoFields>(&Bytes::new()).unwrap();
        read_body::<NoFields>(&Bytes::from_static(b" \n")).unwrap();
        read_body::<NoFields>(&Bytes::from_static(b"{}")).unwrap();
    }

    #[test]
    fn a_body_with_a_field_the_request_does_not_have_is_invalid() {
        let error = read_body::<NoFields>(&Bytes::from_static(br#"{"x": 1}"#)).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn a_body_that_is_not_json_is_invalid() {
        let error = read_body::<NoFields>(&Bytes::from_static(b"loop")).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
}

//! Error types and HTTP error mapping for the guider HTTP service.
//!
//! Mirrors `services/plate-solver/src/error.rs`: a typed error enum,
//! frozen `snake_case` wire codes, and the shared structured envelope
//! `{ "error": <code>, "message": <text>, "details": <value|omitted> }`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

use crate::error::Phd2Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("cannot dither: PHD2 is not guiding (application state: {0})")]
    NotGuiding(String),

    #[error("camera busy: {0}")]
    CameraBusy(String),

    #[error("settle failed: {0}")]
    GuideFailed(String),

    #[error("settle timed out: no SettleDone within the {0} backstop")]
    SettleTimeout(String),

    #[error("stop timed out: PHD2 did not reach Stopped within {0}")]
    StopTimeout(String),

    #[error("PHD2 unreachable: {0}")]
    Phd2Unreachable(String),

    #[error("internal: {0}")]
    Internal(String),
}

/// Wire-format error codes (frozen by
/// `docs/services/phd2-guider.md` § "Error envelope").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    NotGuiding,
    CameraBusy,
    GuideFailed,
    SettleTimeout,
    StopTimeout,
    Phd2Unreachable,
    Internal,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ServiceError {
    /// Map this error to its wire-format code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::NotGuiding(_) => ErrorCode::NotGuiding,
            Self::CameraBusy(_) => ErrorCode::CameraBusy,
            Self::GuideFailed(_) => ErrorCode::GuideFailed,
            Self::SettleTimeout(_) => ErrorCode::SettleTimeout,
            Self::StopTimeout(_) => ErrorCode::StopTimeout,
            Self::Phd2Unreachable(_) => ErrorCode::Phd2Unreachable,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// HTTP status the service returns for this error.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotGuiding(_) | Self::CameraBusy(_) => StatusCode::CONFLICT,
            Self::GuideFailed(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::SettleTimeout(_) | Self::StopTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::Phd2Unreachable(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Map a client-layer PHD2 error onto the wire taxonomy: connection
/// and transport failures are `phd2_unreachable`; an error response
/// PHD2 itself produced for the RPC (equipment not connected, invalid
/// state, …) is `guide_failed` carrying PHD2's message; anything else
/// is `internal`.
impl From<Phd2Error> for ServiceError {
    fn from(e: Phd2Error) -> Self {
        match e {
            Phd2Error::NotConnected
            | Phd2Error::ConnectionFailed(_)
            | Phd2Error::Phd2NotRunning
            | Phd2Error::Io(_)
            | Phd2Error::SendError(_)
            | Phd2Error::ReceiveError
            | Phd2Error::Timeout(_)
            | Phd2Error::ReconnectFailed(_) => Self::Phd2Unreachable(e.to_string()),
            Phd2Error::RpcError { .. }
            | Phd2Error::EquipmentNotConnected
            | Phd2Error::NotCalibrated
            | Phd2Error::InvalidState(_) => Self::GuideFailed(e.to_string()),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorResponse {
            error: self.code(),
            message: self.to_string(),
            details: serde_json::Value::Null,
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_frozen_code_and_status() {
        let cases: Vec<(ServiceError, ErrorCode, StatusCode)> = vec![
            (
                ServiceError::InvalidRequest("x".into()),
                ErrorCode::InvalidRequest,
                StatusCode::BAD_REQUEST,
            ),
            (
                ServiceError::NotGuiding("Stopped".into()),
                ErrorCode::NotGuiding,
                StatusCode::CONFLICT,
            ),
            (
                ServiceError::CameraBusy("frame in flight".into()),
                ErrorCode::CameraBusy,
                StatusCode::CONFLICT,
            ),
            (
                ServiceError::GuideFailed("star lost".into()),
                ErrorCode::GuideFailed,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ServiceError::SettleTimeout("70s".into()),
                ErrorCode::SettleTimeout,
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                ServiceError::StopTimeout("10s".into()),
                ErrorCode::StopTimeout,
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                ServiceError::Phd2Unreachable("refused".into()),
                ErrorCode::Phd2Unreachable,
                StatusCode::BAD_GATEWAY,
            ),
            (
                ServiceError::Internal("bug".into()),
                ErrorCode::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, code, status) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.status(), status);
        }
    }

    #[test]
    fn wire_codes_serialize_as_snake_case() {
        let json = serde_json::to_string(&ErrorCode::Phd2Unreachable).unwrap();
        assert_eq!(json, "\"phd2_unreachable\"");
        let json = serde_json::to_string(&ErrorCode::NotGuiding).unwrap();
        assert_eq!(json, "\"not_guiding\"");
        let json = serde_json::to_string(&ErrorCode::CameraBusy).unwrap();
        assert_eq!(json, "\"camera_busy\"");
    }

    #[test]
    fn the_envelope_omits_null_details() {
        let body = ErrorResponse {
            error: ErrorCode::GuideFailed,
            message: "settle failed: Star lost".into(),
            details: serde_json::Value::Null,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["error"], "guide_failed");
        assert_eq!(json["message"], "settle failed: Star lost");
        assert!(json.get("details").is_none());
    }

    #[test]
    fn connection_failures_map_to_phd2_unreachable() {
        let err: ServiceError = Phd2Error::NotConnected.into();
        assert_eq!(err.code(), ErrorCode::Phd2Unreachable);
        let err: ServiceError = Phd2Error::Timeout("cmd".into()).into();
        assert_eq!(err.code(), ErrorCode::Phd2Unreachable);
    }

    #[test]
    fn phd2_rpc_errors_map_to_guide_failed() {
        let err: ServiceError = Phd2Error::RpcError {
            code: 1,
            message: "camera not connected".into(),
        }
        .into();
        assert_eq!(err.code(), ErrorCode::GuideFailed);
    }
}

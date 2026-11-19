//! Error types and HTTP error mapping for plate-solver.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("FITS file not found: {0}")]
    FitsNotFound(String),

    #[error("solve failed: {message}")]
    SolveFailed {
        message: String,
        exit_code: Option<i32>,
        stderr_tail: Option<String>,
    },

    #[error("solve timed out (terminated)")]
    SolveTimeoutTerminated,

    #[error("solve timed out (killed)")]
    SolveTimeoutKilled,

    #[error("internal: {0}")]
    Internal(String),
}

/// Wire-format error codes, the five of
/// [§ HTTP API](../../../docs/services/plate-solver.md#http-api).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    FitsNotFound,
    SolveFailed,
    SolveTimeout,
    Internal,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl AppError {
    /// Map this error to its wire-format code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::FitsNotFound(_) => ErrorCode::FitsNotFound,
            Self::SolveFailed { .. } => ErrorCode::SolveFailed,
            Self::SolveTimeoutTerminated | Self::SolveTimeoutKilled => ErrorCode::SolveTimeout,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// HTTP status the wrapper returns for this error.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::FitsNotFound(_) => StatusCode::NOT_FOUND,
            Self::SolveFailed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::SolveTimeoutTerminated | Self::SolveTimeoutKilled => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn details(&self) -> serde_json::Value {
        match self {
            Self::SolveFailed {
                exit_code,
                stderr_tail,
                ..
            } => {
                let mut map = serde_json::Map::new();
                if let Some(code) = exit_code {
                    map.insert("exit_code".into(), serde_json::Value::from(*code));
                }
                if let Some(tail) = stderr_tail {
                    map.insert("stderr_tail".into(), serde_json::Value::from(tail.clone()));
                }
                if map.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(map)
                }
            }
            _ => serde_json::Value::Null,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorResponse {
            error: self.code(),
            message: self.to_string(),
            details: self.details(),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn invalid_request_maps_to_400() {
        let err = AppError::InvalidRequest("bad".into());
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn fits_not_found_maps_to_404() {
        let err = AppError::FitsNotFound("/does/not/exist".into());
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.code(), ErrorCode::FitsNotFound);
    }

    #[test]
    fn solve_failed_maps_to_422_with_details() {
        let err = AppError::SolveFailed {
            message: "ASTAP exited 1".into(),
            exit_code: Some(1),
            stderr_tail: Some("no stars detected".into()),
        };
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.code(), ErrorCode::SolveFailed);
        let details = err.details();
        assert_eq!(details["exit_code"], 1);
        assert_eq!(details["stderr_tail"], "no stars detected");
    }

    #[test]
    fn timeout_terminated_and_killed_share_code() {
        assert_eq!(
            AppError::SolveTimeoutTerminated.code(),
            ErrorCode::SolveTimeout
        );
        assert_eq!(AppError::SolveTimeoutKilled.code(), ErrorCode::SolveTimeout);
        assert_eq!(
            AppError::SolveTimeoutTerminated.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn error_code_serializes_to_snake_case() {
        let json = serde_json::to_string(&ErrorCode::FitsNotFound).unwrap();
        assert_eq!(json, "\"fits_not_found\"");
        let json = serde_json::to_string(&ErrorCode::SolveTimeout).unwrap();
        assert_eq!(json, "\"solve_timeout\"");
    }

    #[test]
    fn internal_maps_to_500() {
        let err = AppError::Internal("broken pipe".into());
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.code(), ErrorCode::Internal);
    }
}

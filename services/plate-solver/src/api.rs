//! Axum router and HTTP handlers for `POST /api/v1/solve` and
//! `GET /health`.
//!
//! Frozen contract details live in
//! `docs/plans/archive/plate-solver.md` §"HTTP contract" and the
//! behavior contract is in `docs/services/plate-solver.md`.

use crate::error::AppError;
use crate::runner::{AstapRunner, RunnerError, SolveOutcome, SolveRequest, WcsMatrix};
use axum::{extract::State, response::IntoResponse, routing::get, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Shared application state held by axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub runner: Arc<dyn AstapRunner>,
    /// Single-flight semaphore. Capacity = `max_concurrency`. Default 1
    /// per the design contract; serializes overlapping solves.
    pub semaphore: Arc<Semaphore>,
    pub default_solve_timeout: Duration,
    pub max_solve_timeout: Duration,
    /// Paths re-validated by `/health` on every probe.
    pub astap_binary_path: PathBuf,
    pub astap_db_directory: PathBuf,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/solve", post(solve))
        .route("/health", get(health))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SolveRequestBody {
    fits_path: PathBuf,
    #[serde(default)]
    ra_hint: Option<f64>,
    #[serde(default)]
    dec_hint: Option<f64>,
    #[serde(default)]
    fov_hint_deg: Option<f64>,
    #[serde(default)]
    search_radius_deg: Option<f64>,
    #[serde(default, with = "humantime_serde::option")]
    timeout: Option<Duration>,
}

#[derive(Debug, Serialize)]
struct SolveResponseBody {
    ra_center: f64,
    dec_center: f64,
    pixel_scale_arcsec: f64,
    rotation_deg: f64,
    solver: String,
    /// Serialized as a plain `Option` — an incomplete CRPIX + CD set
    /// is an explicit `null` on the wire, not an absent key.
    wcs_matrix: Option<WcsMatrix>,
}

impl From<SolveOutcome> for SolveResponseBody {
    fn from(o: SolveOutcome) -> Self {
        Self {
            ra_center: o.ra_center,
            dec_center: o.dec_center,
            pixel_scale_arcsec: o.pixel_scale_arcsec,
            rotation_deg: o.rotation_deg,
            solver: o.solver,
            wcs_matrix: o.wcs_matrix,
        }
    }
}

async fn solve(
    State(state): State<AppState>,
    body: Result<Json<SolveRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SolveResponseBody>, AppError> {
    // 1. JSON parse / schema validation.
    let body = body.map_err(|e| AppError::InvalidRequest(format!("malformed body: {e}")))?;
    let body = body.0;

    // 2. fits_path must be absolute.
    if !body.fits_path.is_absolute() {
        return Err(AppError::InvalidRequest(format!(
            "fits_path must be absolute: {}",
            body.fits_path.display()
        )));
    }

    // 3. fits_path must exist, be a regular file (not a directory),
    //    and be readable. Each failure surfaces as `fits_not_found`
    //    with a message describing which check failed — `internal` is
    //    reserved for genuine wrapper bugs.
    let fits_meta = std::fs::metadata(&body.fits_path)
        .map_err(|e| AppError::FitsNotFound(format!("{}: {}", body.fits_path.display(), e)))?;
    if !fits_meta.is_file() {
        return Err(AppError::FitsNotFound(format!(
            "{}: not a regular file",
            body.fits_path.display()
        )));
    }
    // Open-check confirms read permissions without reading content.
    if let Err(e) = std::fs::File::open(&body.fits_path) {
        return Err(AppError::FitsNotFound(format!(
            "{}: {}",
            body.fits_path.display(),
            e
        )));
    }

    // 4. Determine timeout. Caller-supplied is capped at max; missing
    //    falls back to default.
    let timeout = body
        .timeout
        .unwrap_or(state.default_solve_timeout)
        .min(state.max_solve_timeout);

    // 5. Acquire semaphore. Overlapping requests queue behind this; the
    //    queue wait is not counted against the per-request timeout.
    let _permit = state
        .semaphore
        .acquire()
        .await
        .map_err(|e| AppError::Internal(format!("semaphore closed: {e}")))?;

    // 6. Drive the runner. Map RunnerError → AppError per the frozen
    //    error-code table.
    let request = SolveRequest {
        fits_path: body.fits_path,
        ra_hint: body.ra_hint,
        dec_hint: body.dec_hint,
        fov_hint_deg: body.fov_hint_deg,
        search_radius_deg: body.search_radius_deg,
        timeout,
    };

    match state.runner.solve(request).await {
        Ok(outcome) => Ok(Json(SolveResponseBody::from(outcome))),
        Err(RunnerError::ExitStatus {
            status,
            stderr_tail,
        }) => Err(AppError::SolveFailed {
            message: format!("ASTAP exited with code {status}"),
            exit_code: Some(status),
            stderr_tail: Some(stderr_tail),
        }),
        Err(RunnerError::NoWcs) => Err(AppError::SolveFailed {
            message: "ASTAP did not write a .wcs sidecar (exit was clean)".to_string(),
            exit_code: None,
            stderr_tail: None,
        }),
        Err(RunnerError::MalformedWcs(msg)) => Err(AppError::SolveFailed {
            message: format!("malformed .wcs: {msg}"),
            exit_code: None,
            stderr_tail: None,
        }),
        Err(RunnerError::TimedOutTerminated) => Err(AppError::SolveTimeoutTerminated),
        Err(RunnerError::TimedOutKilled) => Err(AppError::SolveTimeoutKilled),
        Err(e @ RunnerError::Process(_)) => Err(AppError::Internal(e.to_string())),
    }
}

#[derive(Debug, Serialize)]
struct HealthBody {
    status: &'static str,
    /// Names the missing dependency on a 503 — the opaque explanation
    /// sentinel displays for a degraded service without interpreting it
    /// (docs/services/sentinel.md §Service Health Supervision).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Cheap readiness probe: stats both runtime dependencies. Returns 200
/// with `{"status": "ok"}` when both pass; 503 otherwise — a degraded
/// answer, since a missing binary or database is not curable by a
/// service restart.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    // SECURITY: `state.astap_binary_path` and `state.astap_db_directory`
    // come from operator-supplied config, validated at startup. Health
    // re-validates them against the same rules. The wrapper isn't
    // multi-tenant; "user-controlled path" alerts on these fields are
    // by-design and dismissed.
    if !path_is_executable_file(&state.astap_binary_path) {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthBody {
                status: "binary_unavailable",
                message: Some(format!(
                    "ASTAP binary missing or not executable: {}",
                    state.astap_binary_path.display()
                )),
            }),
        )
            .into_response();
    }
    if !state.astap_db_directory.is_dir() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthBody {
                status: "db_unavailable",
                message: Some(format!(
                    "ASTAP star database directory missing: {}",
                    state.astap_db_directory.display()
                )),
            }),
        )
            .into_response();
    }
    (
        axum::http::StatusCode::OK,
        Json(HealthBody {
            status: "ok",
            message: None,
        }),
    )
        .into_response()
}

fn path_is_executable_file(path: &std::path::Path) -> bool {
    // SECURITY: `path` is operator-supplied via config; see comment in
    // `health()`. CodeQL's "user-controlled data in path expression"
    // alert on this `metadata` call is by-design.
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

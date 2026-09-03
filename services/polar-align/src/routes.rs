//! HTTP routes: POST /runs to start an alignment and GET /status for
//! the live alignment state.
//!
//! `/runs` is design § Runs; `/status` is where the outcome lands —
//! nothing is posted back to `rp`, which has no notion of a session
//! (mcp-sessionless D6).
//!
//! The operator-facing rest: POST /measure/continue to confirm a manual
//! rotation, POST /adjust/finish to end the adjustment loop, and
//! GET /preview.png for the latest frame rendered for the UI.

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::PolarAlignConfig;
use crate::mcp_client::McpClient;
use crate::preview::{self, PreviewError};
use crate::workflow::{self, Phase, WorkflowShared};

#[derive(Clone)]
pub struct AppState {
    pub config: PolarAlignConfig,
    pub shared: WorkflowShared,
}

pub fn build_router(config: PolarAlignConfig) -> Router {
    let state = AppState {
        config,
        shared: WorkflowShared::default(),
    };
    Router::new()
        .route("/health", get(health))
        .route("/runs", post(start_run))
        .route("/status", get(status_handler))
        .route("/measure/continue", post(continue_handler))
        .route("/adjust/finish", post(finish_handler))
        .route("/preview.png", get(preview_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "polar-align healthy"
}

async fn status_handler(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let status = state.shared.status.read().await;
    match serde_json::to_value(&*status) {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(e) => {
            warn!(error = %e, "status serialization failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "status serialization failed" })),
            )
        }
    }
}

/// Confirms a manual rotation: 202 while a `manual_rotation` wait is
/// active, 409 otherwise (including in the other modes, which never
/// wait). Gating on `awaiting_point` means a duplicate post lands
/// 409 instead of banking a permit for the next wait.
async fn continue_handler(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let awaiting = state.shared.status.read().await.awaiting_point;
    match awaiting {
        Some(point) => {
            debug!(point, "operator confirmed the manual rotation");
            state.shared.signal_proceed().await;
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "point": point })),
            )
        }
        None => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "no manual-rotation wait is active"
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct PreviewParams {
    width: Option<u32>,
}

/// The most recent captured frame as a grayscale PNG. 404 before any
/// capture or when the frame vanished from disk; rendering runs on a
/// blocking thread (frames are tens of megabytes).
async fn preview_handler(
    State(state): State<AppState>,
    Query(params): Query<PreviewParams>,
) -> Response {
    let Some(path) = state.shared.latest_image.read().await.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no frame has been captured yet" })),
        )
            .into_response();
    };
    let width = params.width.unwrap_or(preview::DEFAULT_PREVIEW_WIDTH);
    let rendered = tokio::task::spawn_blocking(move || preview::render_png(&path, width)).await;
    // Error bodies stay generic: the path and decoder detail go to
    // the logs, not to HTTP clients.
    match rendered {
        Ok(Ok(png)) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        Ok(Err(e @ PreviewError::NoFrameOnDisk(_))) => {
            debug!(error = %e, "preview frame missing");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "the captured frame no longer exists on disk" })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            warn!(error = %e, "preview rendering failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to render the preview" })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "preview rendering task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to render the preview" })),
            )
                .into_response()
        }
    }
}

async fn finish_handler(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let phase = state.shared.status.read().await.phase;
    if phase != Phase::Adjusting {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("no adjustment in progress (phase {:?})", phase)
            })),
        );
    }
    state.shared.signal_finish().await;
    (StatusCode::ACCEPTED, Json(serde_json::json!({})))
}

/// `POST /runs`: start an alignment against the configured
/// `mcp_server_url`. `409` while a workflow is measuring or adjusting,
/// `400` without an `rp` to reach. The outcome is `/status` — nothing is
/// posted anywhere.
async fn start_run(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let Some(mcp_url) = state.config.mcp_server_url.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "no `mcp_server_url` configured — a run cannot reach rp"
            })),
        );
    };
    let run_id = format!("run-{}", uuid::Uuid::new_v4().simple());
    if let Err(response) = reserve_slot(&state, &run_id).await {
        return response;
    }
    info!(run_id, "run started");
    launch(state, mcp_url, run_id.clone());
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    )
}

/// Reserve the single workflow slot for `id` before spawning: a second
/// start while one alignment runs is meaningless (one mount, one
/// camera) and is rejected with 409.
async fn reserve_slot(state: &AppState, id: &str) -> Result<(), (StatusCode, Json<Value>)> {
    {
        let mut status = state.shared.status.write().await;
        if matches!(status.phase, Phase::Measuring | Phase::Adjusting) {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "a polar-alignment workflow is already running",
                    "active_run_id": status.workflow_id,
                })),
            ));
        }
        *status = workflow::StatusState {
            phase: Phase::Measuring,
            workflow_id: Some(id.to_owned()),
            ..Default::default()
        };
    }
    // A `/adjust/finish` that raced the previous run's end may have
    // left a permit on the old signal; a fresh one discards it.
    state.shared.arm_finish().await;
    Ok(())
}

/// Spawn the workflow task for `id`; the run reports on `/status`
/// alone.
fn launch(state: AppState, mcp_url: String, id: String) {
    tokio::spawn(async move {
        let config = &state.config;
        let mcp = match McpClient::new(&mcp_url, config.rp_auth(), config.rp_ca()).await {
            Ok(c) => c,
            Err(e) => {
                warn!(run_id = %id, error = %e, "failed to connect MCP client");
                {
                    let mut status = state.shared.status.write().await;
                    status.phase = Phase::Error;
                    status.error = Some(e.to_string());
                }
                return;
            }
        };

        // A failed run has already moved `/status` to `error` with its
        // message inside `workflow::run`; only the success is logged here.
        if let Ok(summary) = workflow::run(&mcp, config, &state.shared).await {
            info!(
                run_id = %id,
                azimuth_error_arcmin = summary.final_measurement.azimuth_error_arcmin,
                altitude_error_arcmin = summary.final_measurement.altitude_error_arcmin,
                iterations = summary.adjustment_iterations,
                "polar alignment completed"
            );
        }
    });
}

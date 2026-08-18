//! HTTP routes: POST /invoke for orchestrator invocation, GET /status
//! for the live alignment state, POST /measure/continue to confirm a
//! manual rotation, POST /adjust/finish to end the adjustment loop,
//! GET /preview.png for the latest frame rendered for the UI.

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::{MeasurementMode, PolarAlignConfig};
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
        .route("/invoke", post(invoke_handler))
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

async fn invoke_handler(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let workflow_id = body
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mcp_server_url = body
        .get("mcp_server_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Reject before reserving the workflow slot: a malformed request
    // must not flip the service into an error state.
    if workflow_id.is_empty() || mcp_server_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "workflow_id and mcp_server_url are required"
            })),
        );
    }

    debug!(
        workflow_id = %workflow_id,
        mcp_server_url = %mcp_server_url,
        "received invocation"
    );

    // Reserve the single workflow slot before spawning: a second
    // /invoke while one alignment runs is meaningless (one mount, one
    // camera) and is rejected with 409.
    {
        let mut status = state.shared.status.write().await;
        if matches!(status.phase, Phase::Measuring | Phase::Adjusting) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "a polar-alignment workflow is already running"
                })),
            );
        }
        *status = workflow::StatusState {
            phase: Phase::Measuring,
            workflow_id: Some(workflow_id.clone()),
            ..Default::default()
        };
    }
    // A `/adjust/finish` that raced the previous run's end may have
    // left a permit on the old signal; a fresh one discards it.
    state.shared.arm_finish().await;

    let wf_id = workflow_id.clone();
    let mcp_url = mcp_server_url.clone();
    let state_clone = state.clone();

    tokio::spawn(async move {
        let config = &state_clone.config;
        let mcp = match McpClient::new(&mcp_url, config.rp_auth(), config.rp_ca()).await {
            Ok(c) => c,
            Err(e) => {
                warn!(workflow_id = %wf_id, error = %e, "failed to connect MCP client");
                {
                    let mut status = state_clone.shared.status.write().await;
                    status.phase = Phase::Error;
                    status.error = Some(e.to_string());
                }
                post_failure(&mcp_url, &wf_id, &e.to_string(), config).await;
                return;
            }
        };

        match workflow::run(&mcp, config, &state_clone.shared).await {
            Ok(summary) => {
                info!(
                    workflow_id = %wf_id,
                    azimuth_error_arcmin = summary.final_measurement.azimuth_error_arcmin,
                    altitude_error_arcmin = summary.final_measurement.altitude_error_arcmin,
                    iterations = summary.adjustment_iterations,
                    "polar alignment completed"
                );
                post_completion(&mcp_url, &wf_id, &summary, config).await;
            }
            Err(e) => {
                post_failure(&mcp_url, &wf_id, &e.to_string(), config).await;
            }
        }
    });

    (StatusCode::OK, Json(invocation_ack(&state.config)))
}

/// The `/invoke` ack's timing estimates: three slew/capture/solve
/// legs plus the adjustment window, plus the two operator waits in
/// manual-rotation mode (halved for the estimate). Saturating
/// arithmetic so a pathological config cannot crash the ack.
fn invocation_ack(config: &PolarAlignConfig) -> Value {
    let per_point = config
        .measurement
        .exposure
        .saturating_add(config.measurement.settle)
        .saturating_add(std::time::Duration::from_secs(30));
    let measurement = per_point.saturating_mul(3);
    let half_max_duration = config
        .adjustment
        .max_duration
        .checked_div(2)
        .unwrap_or_default();
    let mut estimated = measurement.saturating_add(half_max_duration);
    let mut max = measurement
        .saturating_add(config.adjustment.max_duration)
        .saturating_add(std::time::Duration::from_mins(1));
    if config.measurement.mode == MeasurementMode::ManualRotation {
        estimated = estimated.saturating_add(config.measurement.manual_timeout);
        max = max.saturating_add(config.measurement.manual_timeout.saturating_mul(2));
    }

    serde_json::json!({
        "estimated_duration": humantime::format_duration(estimated).to_string(),
        "max_duration": humantime::format_duration(max).to_string(),
    })
}

async fn post_completion(
    mcp_server_url: &str,
    workflow_id: &str,
    summary: &workflow::WorkflowSummary,
    config: &PolarAlignConfig,
) {
    let body = serde_json::json!({
        "status": "complete",
        "result": {
            "reason": "polar_alignment_complete",
            "azimuth_error_arcmin": summary.final_measurement.azimuth_error_arcmin,
            "altitude_error_arcmin": summary.final_measurement.altitude_error_arcmin,
            "total_error_arcmin": summary.final_measurement.total_error_arcmin,
            "adjustment_iterations": summary.adjustment_iterations,
        }
    });
    post_to_rp(mcp_server_url, workflow_id, &body, config).await;
}

async fn post_failure(
    mcp_server_url: &str,
    workflow_id: &str,
    error: &str,
    config: &PolarAlignConfig,
) {
    let body = serde_json::json!({
        "status": "error",
        "result": {
            "reason": "polar_alignment_failed",
            "error": error,
        }
    });
    post_to_rp(mcp_server_url, workflow_id, &body, config).await;
}

/// The rp API base for an MCP endpoint URL, tolerating a trailing
/// slash (an operator-configured advertised URL may carry one).
fn rp_base_url(mcp_server_url: &str) -> &str {
    let trimmed = mcp_server_url.trim_end_matches('/');
    trimmed.strip_suffix("/mcp").unwrap_or(trimmed)
}

/// POST a completion body to `rp`, trusting and authenticating per
/// the ADR-017 policy — the same legs the MCP client uses.
async fn post_to_rp(
    mcp_server_url: &str,
    workflow_id: &str,
    body: &Value,
    config: &PolarAlignConfig,
) {
    let base_url = rp_base_url(mcp_server_url);
    let url = format!("{base_url}/api/plugins/{workflow_id}/complete");

    let client = match rusty_photon_tls::client::build_reqwest_client(config.rp_ca()) {
        Ok(client) => client,
        Err(e) => {
            warn!(%url, error = %e, "cannot build HTTP client for the completion post");
            return;
        }
    };
    let auth_header = rp_mcp_client::basic_authorization(&url, config.rp_auth(), config.rp_ca())
        .unwrap_or_else(|e| {
            warn!(%url, error = %e, "cannot build the completion Authorization header");
            None
        });
    let mut request = client.post(&url).json(body);
    if let Some(header) = auth_header {
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    let _ = request.send().await;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_rp_base_url_strips_the_mcp_suffix_with_and_without_a_trailing_slash() {
        assert_eq!(rp_base_url("https://rig:11170/mcp"), "https://rig:11170");
        assert_eq!(rp_base_url("https://rig:11170/mcp/"), "https://rig:11170");
        assert_eq!(rp_base_url("https://rig:11170"), "https://rig:11170");
    }
}

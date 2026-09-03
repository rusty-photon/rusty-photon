//! HTTP routes: `POST /runs`, `GET /status`, `GET /health`.
//!
//! `/runs` and `/status` are the run surface of design § Runs. A run
//! reports on `/status` only — nothing is posted back to `rp`, which has
//! no notion of a session (mcp-sessionless D6).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::FlatPlan;
use crate::mcp_client::McpClient;
use crate::workflow;

/// Where the service is in its one-run-at-a-time lifecycle, serialized
/// verbatim into `/status.phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Running,
    Complete,
    Error,
}

/// Everything `GET /status` serves.
#[derive(Debug, Clone, Serialize)]
pub struct RunStatus {
    pub phase: Phase,
    /// The run id (`POST /runs`); `null` before the first run.
    pub run_id: Option<String>,
    /// The result payload once a run completed.
    pub result: Option<Value>,
    /// The failure message once a run failed.
    pub error: Option<String>,
}

impl Default for RunStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            run_id: None,
            result: None,
            error: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub plan: FlatPlan,
    pub status: Arc<RwLock<RunStatus>>,
}

pub fn build_router(plan: FlatPlan) -> Router {
    let state = AppState {
        plan,
        status: Arc::new(RwLock::new(RunStatus::default())),
    };
    Router::new()
        .route("/health", get(health))
        .route("/runs", post(start_run))
        .route("/status", get(status_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "calibrator-flats healthy"
}

async fn status_handler(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let status = state.status.read().await;
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

/// `POST /runs`: start a run from the configured plan against the
/// configured `mcp_server_url`. `409` while a run is in progress, `400`
/// without an `rp` to reach.
async fn start_run(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let Some(mcp_url) = state.plan.mcp_server_url.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "no `mcp_server_url` configured — a run cannot reach rp"
            })),
        );
    };
    let run_id = format!("run-{}", uuid::Uuid::new_v4().simple());
    match reserve_slot(&state, &run_id).await {
        Ok(()) => {}
        Err(response) => return response,
    }
    info!(run_id, "run started");
    tokio::spawn(run_workflow(state.clone(), mcp_url, run_id.clone()));
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "run_id": run_id })),
    )
}

/// Take the single run slot for `run_id`, or answer `409` naming the run
/// that holds it.
async fn reserve_slot(state: &AppState, run_id: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let mut status = state.status.write().await;
    if status.phase == Phase::Running {
        let active = status.run_id.clone();
        drop(status);
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "a flat-calibration run is already in progress",
                "active_run_id": active,
            })),
        ));
    }
    *status = RunStatus {
        phase: Phase::Running,
        run_id: Some(run_id.to_owned()),
        result: None,
        error: None,
    };
    drop(status);
    Ok(())
}

/// The run task: connect, run the workflow, record the outcome on
/// `/status`.
async fn run_workflow(state: AppState, mcp_url: String, run_id: String) {
    let plan = &state.plan;
    let mcp = match McpClient::new(&mcp_url, plan.rp_auth(), plan.rp_ca()).await {
        Ok(c) => c,
        Err(e) => {
            warn!(run_id, error = %e, "failed to connect MCP client");
            record_error(&state, &e.to_string()).await;
            return;
        }
    };

    match workflow::run(&mcp, plan).await {
        Ok(result) => {
            info!(
                run_id,
                total_frames = result.total_frames,
                "flat calibration completed"
            );
            let mut status = state.status.write().await;
            status.phase = Phase::Complete;
            status.result = Some(result_payload(&result));
        }
        Err(e) => {
            warn!(run_id, error = %e, "flat calibration failed");
            record_error(&state, &e.to_string()).await;
        }
    }
}

async fn record_error(state: &AppState, error: &str) {
    let mut status = state.status.write().await;
    status.phase = Phase::Error;
    status.error = Some(error.to_owned());
}

/// The `/status.result` payload of a completed run.
fn result_payload(result: &workflow::WorkflowResult) -> Value {
    let filters: Vec<Value> = result
        .filters_completed
        .iter()
        .map(|f| {
            serde_json::json!({
                "filter": f.filter_name,
                "duration": humantime::format_duration(f.duration).to_string(),
                "median_adu": f.median_adu,
                "frames": f.frames_captured,
                "converged": f.converged,
            })
        })
        .collect();
    serde_json::json!({
        "reason": "flat_calibration_complete",
        "filters_completed": filters,
        "total_frames": result.total_frames,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::config::{FilterPlan, ServerConfig};

    fn plan(mcp_server_url: Option<&str>) -> FlatPlan {
        FlatPlan {
            server: ServerConfig::new(0),
            mcp_server_url: mcp_server_url.map(str::to_owned),
            camera_id: "main-cam".to_owned(),
            filter_wheel_id: None,
            calibrator_id: "flat-panel".to_owned(),
            target_adu_fraction: 0.5,
            tolerance: 0.05,
            max_iterations: 1,
            initial_duration: std::time::Duration::from_millis(100),
            brightness: None,
            filters: vec![FilterPlan {
                name: "OSC".to_owned(),
                count: 1,
            }],
            service_auth: None,
            ca_cert: None,
        }
    }

    async fn serve(plan: FlatPlan) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, build_router(plan)).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn status_is_idle_before_any_run() {
        let addr = serve(plan(None)).await;
        let body: Value = reqwest::get(format!("http://{addr}/status"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["phase"], "idle");
        assert_eq!(body["run_id"], Value::Null);
    }

    #[tokio::test]
    async fn a_run_without_a_configured_rp_is_refused() {
        let addr = serve(plan(None)).await;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/runs"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        let body: Value = response.json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("mcp_server_url"),
            "{body}"
        );
    }

    /// The run task connects to rp; an unreachable rp surfaces on
    /// `/status` as an error, not on the start response — and the slot
    /// is free again afterwards.
    #[tokio::test]
    async fn an_unreachable_rp_lands_on_status_as_an_error() {
        let addr = serve(plan(Some("http://127.0.0.1:1/mcp"))).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{addr}/runs"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
        let started: Value = response.json().await.unwrap();
        let run_id = started["run_id"].as_str().unwrap().to_owned();
        assert!(run_id.starts_with("run-"), "{run_id}");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let body: Value = client
                .get(format!("http://{addr}/status"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if body["phase"] == "error" {
                assert_eq!(body["run_id"], run_id);
                assert!(body["error"].is_string(), "{body}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run never reported its error: {body}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // The slot is free: a new run is accepted.
        let response = client
            .post(format!("http://{addr}/runs"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
    }

    #[tokio::test]
    async fn a_second_run_is_refused_while_one_is_running() {
        let state = AppState {
            plan: plan(Some("http://127.0.0.1:1/mcp")),
            status: Arc::new(RwLock::new(RunStatus {
                phase: Phase::Running,
                run_id: Some("run-busy".to_owned()),
                result: None,
                error: None,
            })),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/runs", post(start_run))
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/runs"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 409);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["active_run_id"], "run-busy");
    }
}

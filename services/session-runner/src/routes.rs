//! HTTP routes: the run family, `/validate`, and `/health`.
//!
//! The run family — `POST /runs`, `GET /runs`, `GET /runs/{id}`, `POST
//! /runs/{id}/stop` — is design § Runs; `POST /validate` runs layers 1–2
//! as a service.
//!
//! `POST /runs` runs **all three validation layers before answering**
//! (design tenet 3). Deliberately local-first: schema
//! (layer 1) and parameters (layer 3) run before the catalog check
//! (layer 2), which needs a network round-trip to `rp` — a document or
//! parameter error is diagnosed without touching `rp` at all. Any
//! failure is the error response — the run fails to start loudly, before
//! any hardware moves. Only then is the run spawned and the response
//! returned.
//!
//! The happy paths (a real MCP server on the other end) are exercised by
//! the BDD suite; the unit tests here cover the validation/error paths
//! and the run registry's HTTP surface.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::config::{Config, RpConnection};
use crate::document::{
    bind_parameters, resolve_workflow_path, validate_against_catalog, Document, ToolSpec,
    ValidationIssue,
};
use crate::mcp_client::McpClient;
use crate::runs::{
    self, completion_result, mint_run_id, mint_session_id, valid_session_id, AppState, Launch,
    RunManifest, StopError,
};

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runs", post(start_run).get(list_runs))
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/stop", post(stop_run))
        .route("/validate", post(validate))
        .with_state(state)
}

async fn health() -> &'static str {
    "session-runner healthy"
}

fn error_response(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message })))
}

fn issues_response(
    status: StatusCode,
    message: &str,
    issues: &[ValidationIssue],
) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message, "issues": issues })))
}

// --- /validate --------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateRequest {
    /// A workflow document, inline.
    #[serde(default)]
    document: Option<Value>,
    /// A workflow name resolved in the configured `workflows_dir`.
    #[serde(default)]
    workflow: Option<String>,
}

fn validate_report(valid: bool, errors: &[ValidationIssue], catalog: &str) -> Json<Value> {
    Json(json!({ "valid": valid, "errors": errors, "catalog_validation": catalog }))
}

async fn validate(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ValidateRequest>,
) -> (StatusCode, Json<Value>) {
    let config = &state.config;
    // Exactly one input form. Failures carry the reason the catalog
    // check was skipped: a workflow that could not be loaded is not a
    // schema failure.
    let document = match (request.document, request.workflow) {
        (Some(document), None) => {
            Document::from_value(&document).map_err(|issues| (issues, "schema validation failed"))
        }
        (None, Some(name)) => match load_workflow_source(config, &name).await {
            Ok(src) => Document::parse(&src).map_err(|issues| (issues, "schema validation failed")),
            Err(message) => Err((
                vec![ValidationIssue {
                    pointer: String::new(),
                    message,
                    expr_span: None,
                }],
                "the workflow could not be loaded",
            )),
        },
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "provide exactly one of `document` (inline) or `workflow` (a name in \
                 workflows_dir)",
            )
        }
    };

    let document = match document {
        Ok(document) => document,
        Err((issues, reason)) => {
            return (
                StatusCode::OK,
                validate_report(false, &issues, &format!("skipped: {reason}")),
            )
        }
    };

    // Layer 2, best-effort: standalone /validate reaches rp through the
    // configured mcp_server_url; unreachable (or unconfigured) is not an
    // error — the response says the catalog check was skipped.
    let Some(mcp_url) = config.mcp_server_url.as_deref() else {
        return (
            StatusCode::OK,
            validate_report(true, &[], "skipped: no mcp_server_url configured"),
        );
    };
    let catalog = match fetch_catalog(mcp_url, &config.rp_connection()).await {
        Ok(catalog) => catalog,
        Err(message) => {
            return (
                StatusCode::OK,
                validate_report(true, &[], &format!("skipped: {message}")),
            )
        }
    };
    let issues = validate_against_catalog(&document, &catalog);
    (
        StatusCode::OK,
        validate_report(issues.is_empty(), &issues, "checked"),
    )
}

async fn load_workflow_source(config: &Config, name: &str) -> Result<String, String> {
    let path = resolve_workflow_path(&config.workflows_dir, name)?;
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("cannot read workflow `{name}` at {}: {e}", path.display()))
}

async fn fetch_catalog(mcp_url: &str, connection: &RpConnection) -> Result<Vec<ToolSpec>, String> {
    let client = McpClient::connect(mcp_url, connection.auth(), connection.ca_path())
        .await
        .map_err(|e| format!("rp unreachable: {e}"))?;
    client.list_tools().await.map_err(|e| e.to_string())
}

// --- /runs ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRunRequest {
    /// Document name resolved in `workflows_dir` (or an absolute path).
    workflow: String,
    /// The document's parameters; absent means `{}`.
    #[serde(default)]
    params: Option<Value>,
    /// Names the blackboard file; minted when absent.
    #[serde(default)]
    session_id: Option<String>,
}

/// `POST /runs`: validate (schema, parameters, live catalog), reserve
/// the one active-run slot, write the manifest, spawn the supervisor.
///
/// A body the extractor rejects (not JSON, an unknown key, a wrong
/// type) is a `400` in the module's JSON error shape, not the
/// extractor's plain-text `422`.
async fn start_run(
    State(state): State<Arc<AppState>>,
    request: Result<Json<StartRunRequest>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return body_rejected(&rejection),
    };
    let config = &state.config;
    // A fast refusal before any validation work; the reserving call
    // below is the authoritative one.
    if let Some(active) = state.runs.active() {
        return run_conflict(&active);
    }
    let Some(mcp_url) = config.mcp_server_url.clone() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "no `mcp_server_url` configured — a run cannot reach rp",
        );
    };
    let now = Utc::now();
    let session_id = request.session_id.unwrap_or_else(|| mint_session_id(now));
    if !valid_session_id(&session_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid session_id `{session_id}`"),
        );
    }
    let params = request.params.unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return error_response(StatusCode::BAD_REQUEST, "`params` must be a JSON object");
    }

    // Layer 1: schema.
    let source = match load_workflow_source(config, &request.workflow).await {
        Ok(source) => source,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    let document = match Document::parse(&source) {
        Ok(document) => document,
        Err(issues) => {
            return issues_response(
                StatusCode::BAD_REQUEST,
                "document failed validation",
                &issues,
            )
        }
    };
    // Layer 3: parameters — bound again by the supervisor on every
    // (re)execution; checked here so the request fails loudly.
    if let Err(issues) = bind_parameters(&document.parameters, Some(&params)) {
        return issues_response(
            StatusCode::BAD_REQUEST,
            "parameter validation failed",
            &issues,
        );
    }
    // Layer 2: the live catalog.
    let connection = config.rp_connection();
    let mcp = match validate_with_live_catalog(&document, &mcp_url, &connection).await {
        Ok(mcp) => mcp,
        Err(response) => return response,
    };

    let run_id = mint_run_id();
    let stop = match state
        .runs
        .start(&run_id, &session_id, &request.workflow, now)
    {
        Ok(stop) => stop,
        Err(active) => return run_conflict(&active),
    };
    let manifest = RunManifest {
        run_id: run_id.clone(),
        session_id: session_id.clone(),
        workflow: request.workflow,
        params,
        started_at: now,
    };
    if let Err(message) = manifest.write(&config.state_dir).await {
        // Release the slot: the run never started. It stays listed as
        // failed, so the response names it.
        state.runs.mark_ended(
            &run_id,
            runs::RunState::Failed,
            completion_result(&manifest.workflow, "failed", Some(&message), None),
            Utc::now(),
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": message,
                "run_id": run_id,
                "session_id": session_id,
            })),
        );
    }
    info!(run_id, session_id, workflow = %manifest.workflow, "run started");
    tokio::spawn(runs::supervise(
        state.clone(),
        manifest,
        stop,
        Some(Launch { document, mcp }),
    ));
    (
        StatusCode::ACCEPTED,
        Json(json!({ "run_id": run_id, "session_id": session_id })),
    )
}

/// The extractor's rejection in the module's JSON error shape.
fn body_rejected(rejection: &JsonRejection) -> (StatusCode, Json<Value>) {
    error_response(
        StatusCode::BAD_REQUEST,
        &format!("invalid request body: {}", rejection.body_text()),
    )
}

fn run_conflict(active: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": format!("run `{active}` is still active; one run at a time"),
            "active_run_id": active,
        })),
    )
}

fn record_json(record: &runs::RunRecord) -> Value {
    serde_json::to_value(record).unwrap_or_else(|e| json!({ "error": e.to_string() }))
}

async fn list_runs(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(Value::Array(
        state.runs.list().iter().map(record_json).collect(),
    ))
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    AxumPath(run_id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    state.runs.get(&run_id).map_or_else(
        || error_response(StatusCode::NOT_FOUND, &format!("unknown run `{run_id}`")),
        |record| (StatusCode::OK, Json(record_json(&record))),
    )
}

async fn stop_run(
    State(state): State<Arc<AppState>>,
    AxumPath(run_id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    match state.runs.stop(&run_id) {
        Ok(record) => {
            info!(run_id, "stop requested");
            (StatusCode::ACCEPTED, Json(record_json(&record)))
        }
        Err(StopError::NotFound) => {
            error_response(StatusCode::NOT_FOUND, &format!("unknown run `{run_id}`"))
        }
        Err(StopError::AlreadyEnded(run_state)) => error_response(
            StatusCode::CONFLICT,
            &format!(
                "run `{run_id}` has already ended ({})",
                serde_json::to_value(run_state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
        ),
    }
}

/// Layer 2: the live tool catalog. Unlike `/validate`, an unreachable
/// rp is a hard error here — the run cannot start without it.
async fn validate_with_live_catalog(
    document: &Document,
    mcp_server_url: &str,
    connection: &RpConnection,
) -> Result<McpClient, (StatusCode, Json<Value>)> {
    let mcp =
        match McpClient::connect(mcp_server_url, connection.auth(), connection.ca_path()).await {
            Ok(mcp) => mcp,
            Err(e) => return Err(error_response(StatusCode::BAD_GATEWAY, &e.to_string())),
        };
    let catalog = match mcp.list_tools().await {
        Ok(catalog) => catalog,
        Err(e) => return Err(error_response(StatusCode::BAD_GATEWAY, &e.to_string())),
    };
    let issues = validate_against_catalog(document, &catalog);
    if issues.is_empty() {
        Ok(mcp)
    } else {
        Err(issues_response(
            StatusCode::BAD_REQUEST,
            "catalog validation failed",
            &issues,
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::*;

    async fn serve(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    /// Spawn the app with the given config; returns its base URL.
    async fn spawn_app(config: Config) -> String {
        let (base, _) = spawn_app_with_state(config).await;
        base
    }

    /// Spawn the app and keep its state, for the run-registry routes.
    async fn spawn_app_with_state(config: Config) -> (String, Arc<AppState>) {
        let state = Arc::new(AppState::new(config));
        let addr = serve(build_router(state.clone())).await;
        (format!("http://{addr}"), state)
    }

    fn test_config(dir: &tempfile::TempDir) -> Config {
        let workflows_dir = dir.path().join("workflows");
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        Config {
            server: crate::config::ServerConfig::new(0),
            workflows_dir,
            state_dir,
            mcp_server_url: None,
            events_url: None,
            service_auth: None,
            ca_cert: None,
            rp_outage_grace: Duration::from_secs(1),
            safety_poll_interval: Duration::from_millis(50),
            resume_on_start: false,
        }
    }

    async fn get_json(url: &str) -> (StatusCode, Value) {
        let response = reqwest::get(url).await.unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (status, response.json().await.unwrap())
    }

    fn minimal_document() -> Value {
        json!({ "version": 1, "name": "t", "root": { "log": { "message": "m" } } })
    }

    async fn post_json(url: &str, body: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(url)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (status, response.json().await.unwrap())
    }

    // --- /health and /validate ---------------------------------------------

    #[tokio::test]
    async fn test_health_reports() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let body = reqwest::get(format!("{base}/health"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "session-runner healthy");
    }

    #[tokio::test]
    async fn test_validate_requires_exactly_one_input_form() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        for body in [
            json!({}),
            json!({ "document": minimal_document(), "workflow": "x" }),
        ] {
            let (status, response) = post_json(&format!("{base}/validate"), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(
                response["error"]
                    .as_str()
                    .unwrap()
                    .contains("exactly one of"),
                "{response}"
            );
        }
    }

    #[tokio::test]
    async fn test_validate_reports_schema_issues_with_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let (status, response) = post_json(
            &format!("{base}/validate"),
            json!({ "document": { "version": 1, "name": "t",
                                   "root": { "tool": "x", "typo_key": 1 } } }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["valid"], json!(false));
        assert_eq!(
            response["catalog_validation"],
            json!("skipped: schema validation failed")
        );
        let errors = response["errors"].as_array().unwrap();
        assert_ne!(errors.as_slice(), Vec::<serde_json::Value>::new());
        assert_eq!(errors[0]["pointer"], json!("/root/typo_key"));
    }

    #[tokio::test]
    async fn test_validate_without_mcp_url_skips_the_catalog_check() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let (status, response) = post_json(
            &format!("{base}/validate"),
            json!({ "document": minimal_document() }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["valid"], json!(true));
        assert_eq!(response["errors"], json!([]));
        assert_eq!(
            response["catalog_validation"],
            json!("skipped: no mcp_server_url configured")
        );
    }

    #[tokio::test]
    async fn test_validate_with_unreachable_rp_skips_the_catalog_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(&dir);
        // A bound-then-dropped listener guarantees a refusing port.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        config.mcp_server_url = Some(format!("http://{dead_addr}/mcp"));
        let base = spawn_app(config).await;
        let (status, response) = post_json(
            &format!("{base}/validate"),
            json!({ "document": minimal_document() }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["valid"], json!(true));
        assert!(
            response["catalog_validation"]
                .as_str()
                .unwrap()
                .starts_with("skipped: rp unreachable"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_validate_resolves_workflow_names_from_workflows_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir);
        std::fs::write(
            config.workflows_dir.join("nightly.json"),
            serde_json::to_vec(&minimal_document()).unwrap(),
        )
        .unwrap();
        let base = spawn_app(config).await;

        let (status, response) = post_json(
            &format!("{base}/validate"),
            json!({ "workflow": "nightly" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["valid"], json!(true));

        let (status, response) = post_json(
            &format!("{base}/validate"),
            json!({ "workflow": "missing" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["valid"], json!(false));
        // A load failure is not a schema failure — the skip reason says so.
        assert_eq!(
            response["catalog_validation"],
            json!("skipped: the workflow could not be loaded")
        );
        let message = response["errors"][0]["message"].as_str().unwrap();
        assert!(message.contains("missing"), "{message}");
    }

    // --- /runs ---------------------------------------------------------------

    #[tokio::test]
    async fn test_start_run_without_mcp_url_is_refused_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let (status, response) =
            post_json(&format!("{base}/runs"), json!({ "workflow": "anything" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("mcp_server_url"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_start_run_validates_locally_before_reaching_rp() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(&dir);
        // Unreachable on purpose: every failure below must be diagnosed
        // without the catalog round-trip.
        config.mcp_server_url = Some("http://127.0.0.1:1/mcp".to_owned());
        std::fs::write(
            config.workflows_dir.join("p.json"),
            serde_json::to_vec(&json!({
                "version": 1, "name": "p",
                "parameters": { "camera_id": { "type": "string", "required": true } },
                "root": { "log": { "message": "m" } }
            }))
            .unwrap(),
        )
        .unwrap();
        let base = spawn_app(config).await;
        let runs = format!("{base}/runs");

        let (status, response) = post_json(&runs, json!({ "workflow": "nope" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"].as_str().unwrap().contains("nope"),
            "{response}"
        );

        let (status, response) = post_json(&runs, json!({ "workflow": "p", "params": {} })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"], json!("parameter validation failed"));
        assert!(
            response["issues"][0]["message"]
                .as_str()
                .unwrap()
                .contains("camera_id"),
            "{response}"
        );

        let (status, response) = post_json(
            &runs,
            json!({ "workflow": "p", "params": { "camera_id": "c" }, "session_id": "../x" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"].as_str().unwrap().contains("session_id"),
            "{response}"
        );

        // An unknown key is rejected in the module's JSON error shape,
        // not the extractor's plain-text 422.
        let (status, response) = post_json(
            &runs,
            json!({ "workflow": "p", "params": { "camera_id": "c" }, "typo": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(
            response["error"].as_str().unwrap().contains("typo"),
            "{response}"
        );

        // Everything local passed: only now is rp reached, and it is down.
        let (status, _) = post_json(
            &runs,
            json!({ "workflow": "p", "params": { "camera_id": "c" } }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            std::fs::read_dir(dir.path().join("state"))
                .unwrap()
                .next()
                .is_none(),
            "no manifest or blackboard is written for a run that never started"
        );
    }

    #[tokio::test]
    async fn test_run_routes_report_the_registry_and_stop_fires_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let (base, state) = spawn_app_with_state(test_config(&dir)).await;
        let (status, _) = get_json(&format!("{base}/runs/run-missing")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, response) =
            post_json(&format!("{base}/runs/run-missing/stop"), json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{response}");

        let token = state
            .runs
            .start("run-1", "session-1", "flats", Utc::now())
            .unwrap();
        state.runs.mark_paused("run-1", "safety", "clouds");

        let (status, response) = get_json(&format!("{base}/runs/run-1")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["state"], json!("paused"));
        assert_eq!(response["paused_reason"], json!("safety"));
        assert_eq!(response["paused_detail"], json!("clouds"));
        assert_eq!(response["session_id"], json!("session-1"));
        assert_eq!(response["outcome"], Value::Null);

        let (status, response) = get_json(&format!("{base}/runs")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.as_array().unwrap().len(), 1);

        // A second run is refused while the first is paused.
        let (status, response) =
            post_json(&format!("{base}/runs"), json!({ "workflow": "x" })).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(response["active_run_id"], json!("run-1"));

        let (status, _) = post_json(&format!("{base}/runs/run-1/stop"), json!({})).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(token.is_cancelled());

        state.runs.mark_ended(
            "run-1",
            runs::RunState::Stopped,
            json!({ "outcome": "stopped" }),
            Utc::now(),
        );
        let (status, response) = post_json(&format!("{base}/runs/run-1/stop"), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{response}");
        let (_, response) = get_json(&format!("{base}/runs/run-1")).await;
        assert_eq!(response["state"], json!("stopped"));
        assert_eq!(response["outcome"]["outcome"], json!("stopped"));
        assert!(response["ended_at"].is_string());
    }
}

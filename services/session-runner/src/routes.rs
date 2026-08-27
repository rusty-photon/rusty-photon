//! HTTP routes: `POST /invoke` (the orchestrator protocol), `POST
//! /validate` (layers 1–2 as a service), `GET /health`.
//!
//! `/invoke` runs **all three validation layers before acknowledging**
//! (design tenet 3). Deliberately local-first: schema (layer 1) and
//! invocation parameters (layer 3) run before the catalog check
//! (layer 2), which needs a network round-trip to `rp` — a document or
//! parameter error is diagnosed without touching `rp` at all. Any
//! failure is the error response — the session fails to start loudly,
//! before any hardware moves. Only then is the engine run spawned and the
//! acknowledgment returned.
//!
//! The full `/invoke` happy path (a real MCP server on the other end) is
//! exercised by the Phase C BDD suite; the unit tests here cover the
//! validation/error paths and the completion contract against a captured
//! `rp` stand-in.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use crate::blackboard::Blackboard;
use crate::config::{Config, RpConnection};
use crate::document::{
    bind_parameters, resolve_workflow_path, validate_against_catalog, Document, ToolSpec,
    ValidationIssue,
};
use crate::engine::{run, EventIntake, RunOutcome, SystemClock, ToolClient};
use crate::events;
use crate::mcp_client::McpClient;

/// Engine defaults for the acknowledgment durations when the document
/// omits them (design § Invocation): `max_duration` must comfortably
/// exceed a full night because `rp` treats its expiry as plugin timeout.
const DEFAULT_ESTIMATED_DURATION: Duration = Duration::from_hours(1);
const DEFAULT_MAX_DURATION: Duration = Duration::from_hours(14);

pub fn build_router(config: Arc<Config>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/invoke", post(invoke))
        .route("/validate", post(validate))
        .with_state(config)
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
    State(config): State<Arc<Config>>,
    Json(request): Json<ValidateRequest>,
) -> (StatusCode, Json<Value>) {
    // Exactly one input form. Failures carry the reason the catalog
    // check was skipped: a workflow that could not be loaded is not a
    // schema failure.
    let document = match (request.document, request.workflow) {
        (Some(document), None) => {
            Document::from_value(&document).map_err(|issues| (issues, "schema validation failed"))
        }
        (None, Some(name)) => match load_workflow_source(&config, &name).await {
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

// --- /invoke ----------------------------------------------------------------

#[derive(Deserialize)]
struct InvokeRequest {
    workflow_id: String,
    session_id: String,
    mcp_server_url: String,
    #[serde(default)]
    recovery: Option<Value>,
    #[serde(default)]
    config: Option<Value>,
}

/// The plugin's registered `config` object, forwarded verbatim by `rp`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestratorConfig {
    /// Document name resolved in `workflows_dir` (or an absolute path).
    workflow: String,
    #[serde(default)]
    parameters: Option<Value>,
}

async fn invoke(
    State(config): State<Arc<Config>>,
    Json(request): Json<InvokeRequest>,
) -> (StatusCode, Json<Value>) {
    info!(
        workflow_id = %request.workflow_id,
        session_id = %request.session_id,
        recovery = request.recovery.is_some(),
        "invocation received"
    );

    let orchestrator_config = match parse_orchestrator_config(request.config) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };

    // The session id names the blackboard file — it must not traverse.
    if !valid_session_id(&request.session_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid session_id `{}`", request.session_id),
        );
    }

    // Layer 1: schema.
    let source = match load_workflow_source(&config, &orchestrator_config.workflow).await {
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

    // Layer 3: invocation parameters.
    let mut params = match bind_parameters(
        &document.parameters,
        orchestrator_config.parameters.as_ref(),
    ) {
        Ok(params) => params,
        Err(issues) => {
            return issues_response(
                StatusCode::BAD_REQUEST,
                "parameter validation failed",
                &issues,
            )
        }
    };

    let connection = config.rp_connection();
    let mcp =
        match validate_with_live_catalog(&document, &request.mcp_server_url, &connection).await {
            Ok(mcp) => mcp,
            Err(response) => return response,
        };

    // Blackboard: reloaded on recovery; fresh otherwise, with any
    // leftover file deleted eagerly — a termination before the first
    // persist must not resurrect stale state on the recovery invocation.
    let blackboard_path = config
        .state_dir
        .join(format!("{}.json", request.session_id));
    let blackboard = if request.recovery.is_some() {
        Blackboard::load(blackboard_path).await
    } else {
        Blackboard::replace(blackboard_path).await
    };
    let blackboard = match blackboard {
        Ok(blackboard) => blackboard,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if let Some(recovery) = &request.recovery {
        // Exposed as `params._recovery.*`; declared parameter names cannot
        // collide (`_`-prefixed declarations are a validation error).
        info!(recovery = %recovery, "recovery invocation; re-executing from the root");
        if let Value::Object(map) = &mut params {
            map.insert("_recovery".to_owned(), recovery.clone());
        }
    }

    let estimated = document
        .estimated_duration
        .unwrap_or(DEFAULT_ESTIMATED_DURATION);
    let max = document.max_duration.unwrap_or(DEFAULT_MAX_DURATION);
    let ack = json!({
        "estimated_duration": humantime::format_duration(estimated).to_string(),
        "max_duration": humantime::format_duration(max).to_string(),
    });

    // Subscribe to rp's SSE stream before the first instruction runs —
    // the initial connect completes inside `subscribe`, so an event
    // emitted during an early instruction still satisfies a later
    // `until_event` wait and feeds trigger evaluation. The client task
    // lives exactly as long as the run: it exits when the intake drops.
    let events_url = config.events_url.clone().unwrap_or_else(|| {
        format!(
            "{}/api/events/subscribe",
            rp_base_url(&request.mcp_server_url)
        )
    });
    let intake = events::subscribe(events_url, &connection).await;

    tokio::spawn(run_session(
        document,
        params,
        blackboard,
        mcp,
        intake,
        request.workflow_id,
        request.mcp_server_url,
        connection,
    ));

    (StatusCode::OK, Json(ack))
}

/// The plugin-registration `config` object: required, and parsed
/// strictly — a malformed value surfaces rather than silently becoming
/// defaults.
fn parse_orchestrator_config(
    config: Option<Value>,
) -> Result<OrchestratorConfig, (StatusCode, Json<Value>)> {
    let Some(raw) = config else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invocation carries no `config` object — session-runner needs \
             `config.workflow` (and optional `config.parameters`) from the plugin \
             registration, forwarded by rp",
        ));
    };
    serde_json::from_value(raw)
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, &format!("invalid `config`: {e}")))
}

/// The session id names the blackboard file — it must not traverse.
fn valid_session_id(session_id: &str) -> bool {
    !(session_id.is_empty()
        || session_id.contains(['/', '\\'])
        || session_id == "."
        || session_id == "..")
}

/// Layer 2: the live tool catalog. Unlike `/validate`, an unreachable
/// rp is a hard error here — the invocation cannot proceed without it.
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

/// `rp`'s HTTP origin, derived from the invocation's MCP endpoint — the
/// base for the completion POST and the default SSE stream URL. Tolerates
/// a trailing slash on the endpoint (`…/mcp/`), which would otherwise
/// survive the suffix strip and double the slash in derived URLs.
fn rp_base_url(mcp_server_url: &str) -> &str {
    let trimmed = mcp_server_url.trim_end_matches('/');
    trimmed.strip_suffix("/mcp").unwrap_or(trimmed)
}

/// Run the engine to completion and honor the completion contract:
/// `Completed`/`Failed` post to `rp` and delete the blackboard once
/// acknowledged; `Terminated` posts nothing and keeps the blackboard for
/// the recovery invocation.
#[allow(clippy::too_many_arguments)]
async fn run_session<T: ToolClient + Sync>(
    document: Document,
    params: Value,
    mut blackboard: Blackboard,
    tools: T,
    events: EventIntake,
    workflow_id: String,
    mcp_server_url: String,
    connection: RpConnection,
) {
    let outcome = run(
        &document,
        &params,
        &mut blackboard,
        &tools,
        &SystemClock,
        events,
    )
    .await;
    let (status, result) = match &outcome {
        RunOutcome::Terminated => return,
        RunOutcome::Completed => (
            "complete",
            completion_result(&document.name, "complete", None, &blackboard),
        ),
        RunOutcome::Failed(error) => (
            "error",
            completion_result(&document.name, "failed", Some(&error.message), &blackboard),
        ),
    };
    let acknowledged =
        post_completion(&mcp_server_url, &workflow_id, status, result, &connection).await;
    if acknowledged {
        if let Err(e) = tokio::fs::remove_file(blackboard.path()).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %blackboard.path().display(), error = %e,
                      "could not delete completed session's blackboard");
            }
        }
    }
}

/// The completion `result` payload: `workflow` / `outcome` / `error`,
/// plus any values the document accumulated under `session.report.*`
/// (fixed keys win on a name collision).
fn completion_result(
    workflow: &str,
    outcome: &str,
    error: Option<&str>,
    blackboard: &Blackboard,
) -> Value {
    let mut result = Map::new();
    result.insert("workflow".to_owned(), json!(workflow));
    result.insert("outcome".to_owned(), json!(outcome));
    if let Some(error) = error {
        result.insert("error".to_owned(), json!(error));
    }
    if let Some(Value::Object(report)) = blackboard.value().get("report") {
        for (key, value) in report {
            if !result.contains_key(key) {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(result)
}

/// How long the completion POST may take before it counts as
/// unacknowledged. A stalled `rp` (accepted connection, no response) must
/// not wedge the session task forever — timeout expiry lands in the
/// unacknowledged path: blackboard kept, warning logged.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

/// POST the completion to `rp`; `true` when acknowledged (2xx). Trusts
/// and authenticates per the ADR-017 policy, like every other rp leg.
async fn post_completion(
    mcp_server_url: &str,
    workflow_id: &str,
    status: &str,
    result: Value,
    connection: &RpConnection,
) -> bool {
    let url = format!(
        "{}/api/plugins/{workflow_id}/complete",
        rp_base_url(mcp_server_url)
    );
    let body = json!({ "status": status, "result": result });
    debug!(%url, %status, "posting completion");
    let client = match rusty_photon_tls::client::build_reqwest_client(connection.ca_path()) {
        Ok(client) => client,
        Err(e) => {
            warn!(%url, error = %e, "cannot build HTTP client for the completion post");
            return false;
        }
    };
    let auth_header =
        rp_mcp_client::basic_authorization(&url, connection.auth(), connection.ca_path())
            .unwrap_or_else(|e| {
                warn!(%url, error = %e, "cannot build the completion Authorization header");
                None
            });
    let mut request = client.post(&url).timeout(COMPLETION_TIMEOUT).json(&body);
    if let Some(header) = auth_header {
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            warn!(%url, status = %response.status(), "completion was not acknowledged");
            false
        }
        Err(e) => {
            warn!(%url, error = %e, "completion post failed");
            false
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;

    use axum::extract::Path as AxumPath;
    use tokio::sync::mpsc;

    use super::*;
    use crate::engine::ToolCallError;

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
        let addr = serve(build_router(Arc::new(config))).await;
        format!("http://{addr}")
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
        }
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

    // --- rp URL derivation -----------------------------------------------------

    #[test]
    fn test_rp_base_url_strips_the_mcp_suffix_with_or_without_a_trailing_slash() {
        for url in ["http://host:11115/mcp", "http://host:11115/mcp/"] {
            assert_eq!(rp_base_url(url), "http://host:11115", "{url}");
        }
        // No /mcp suffix: only trailing slashes are dropped.
        assert_eq!(rp_base_url("http://host:11115/"), "http://host:11115");
    }

    // --- /invoke error paths -------------------------------------------------

    fn invoke_body(config: Option<Value>) -> Value {
        let mut body = json!({
            "workflow_id": "wf-1",
            "session_id": "session-1",
            "mcp_server_url": "http://127.0.0.1:1/mcp",
            "recovery": null
        });
        if let Some(config) = config {
            body["config"] = config;
        }
        body
    }

    #[tokio::test]
    async fn test_invoke_without_config_names_the_missing_forwarding() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let (status, response) = post_json(&format!("{base}/invoke"), invoke_body(None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("no `config` object"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_invoke_with_unknown_workflow_fails_before_anything_moves() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let (status, response) = post_json(
            &format!("{base}/invoke"),
            invoke_body(Some(json!({ "workflow": "nope" }))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"].as_str().unwrap().contains("nope"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_invoke_reports_parameter_validation_issues() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir);
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
        let (status, response) = post_json(
            &format!("{base}/invoke"),
            invoke_body(Some(json!({ "workflow": "p", "parameters": {} }))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["error"], json!("parameter validation failed"));
        // Missing required parameters point at the supplied object itself
        // (the key is absent), per the document layer's pinned behavior.
        assert_eq!(response["issues"][0]["pointer"], json!("/parameters"));
        assert!(
            response["issues"][0]["message"]
                .as_str()
                .unwrap()
                .contains("camera_id"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_invoke_rejects_traversing_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        let base = spawn_app(test_config(&dir)).await;
        let mut body = invoke_body(Some(json!({ "workflow": "x" })));
        body["session_id"] = json!("../escape");
        let (status, response) = post_json(&format!("{base}/invoke"), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            response["error"].as_str().unwrap().contains("session_id"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_invoke_with_unreachable_rp_is_a_bad_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir);
        std::fs::write(
            config.workflows_dir.join("w.json"),
            serde_json::to_vec(&minimal_document()).unwrap(),
        )
        .unwrap();
        let base = spawn_app(config).await;
        let (status, _) = post_json(
            &format!("{base}/invoke"),
            invoke_body(Some(json!({ "workflow": "w" }))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    // --- the completion contract ----------------------------------------------

    /// A stand-in for `rp`'s completion endpoint: captures posted bodies.
    async fn spawn_completion_capture() -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let router = Router::new()
            .route(
                "/api/plugins/{workflow_id}/complete",
                post(
                    |State(tx): State<mpsc::UnboundedSender<(String, Value)>>,
                     AxumPath(workflow_id): AxumPath<String>,
                     Json(body): Json<Value>| async move {
                        tx.send((workflow_id, body)).unwrap();
                        StatusCode::OK
                    },
                ),
            )
            .with_state(tx);
        let addr = serve(router).await;
        (format!("http://{addr}/mcp"), rx)
    }

    struct StubTools(Result<Value, ToolCallError>);

    impl ToolClient for StubTools {
        async fn call(
            &self,
            _tool: &str,
            _args: Map<String, Value>,
        ) -> Result<Value, ToolCallError> {
            self.0.clone()
        }
    }

    fn doc(document: Value) -> Document {
        Document::from_value(&document).unwrap()
    }

    async fn run_session_with(
        document: Value,
        tools: StubTools,
        state_dir: &Path,
    ) -> (std::path::PathBuf, mpsc::UnboundedReceiver<(String, Value)>) {
        let (mcp_url, rx) = spawn_completion_capture().await;
        let blackboard_path = state_dir.join("session-1.json");
        let blackboard = Blackboard::new_empty(blackboard_path.clone());
        run_session(
            doc(document),
            json!({}),
            blackboard,
            tools,
            EventIntake::disconnected(),
            "wf-1".to_owned(),
            mcp_url,
            RpConnection::default(),
        )
        .await;
        (blackboard_path, rx)
    }

    #[tokio::test]
    async fn test_completion_carries_the_session_report_and_deletes_the_blackboard() {
        let dir = tempfile::tempdir().unwrap();
        let document = json!({
            "version": 1, "name": "flats",
            "root": { "set": { "session.report.frames": "30",
                                "session.report.outcome": "'cannot-shadow-fixed-keys'" } }
        });
        let (blackboard_path, mut rx) =
            run_session_with(document, StubTools(Ok(json!({}))), dir.path()).await;

        let (workflow_id, body) = rx.try_recv().unwrap();
        assert_eq!(workflow_id, "wf-1");
        assert_eq!(body["status"], json!("complete"));
        assert_eq!(body["result"]["workflow"], json!("flats"));
        assert_eq!(body["result"]["outcome"], json!("complete"));
        assert_eq!(body["result"]["frames"], json!(30.0));
        assert!(
            !blackboard_path.exists(),
            "acknowledged completion deletes the blackboard"
        );
    }

    #[tokio::test]
    async fn test_failed_runs_post_an_error_completion() {
        let dir = tempfile::tempdir().unwrap();
        let document = json!({
            "version": 1, "name": "flats",
            "root": { "sequence": [
                { "set": { "session.report.frames": "3" } },
                { "tool": "capture" }
            ] }
        });
        let tools = StubTools(Err(ToolCallError::Failed("lens cap on".to_owned())));
        let (blackboard_path, mut rx) = run_session_with(document, tools, dir.path()).await;

        let (_, body) = rx.try_recv().unwrap();
        assert_eq!(body["status"], json!("error"));
        assert_eq!(body["result"]["outcome"], json!("failed"));
        assert_eq!(
            body["result"]["error"],
            json!("tool `capture` failed: lens cap on")
        );
        assert_eq!(body["result"]["frames"], json!(3.0));
        assert!(!blackboard_path.exists());
    }

    #[tokio::test]
    async fn test_terminated_runs_post_nothing_and_keep_the_blackboard() {
        let dir = tempfile::tempdir().unwrap();
        let document = json!({
            "version": 1, "name": "flats",
            "root": { "sequence": [
                { "set": { "session.progress": "1" } },
                { "tool": "capture" }
            ] }
        });
        let tools = StubTools(Err(ToolCallError::SessionTerminated("safety".to_owned())));
        let (blackboard_path, mut rx) = run_session_with(document, tools, dir.path()).await;

        assert!(rx.try_recv().is_err(), "no completion may be posted");
        assert!(
            blackboard_path.exists(),
            "the blackboard must survive for the recovery invocation"
        );
    }
}

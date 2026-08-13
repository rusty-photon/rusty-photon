//! In-process HTTP stub of the guider rp-managed service.
//!
//! Spawned by rp's BDD scenarios so the guiding MCP tools' HTTP
//! client (`rp-guider`) has something to talk to without standing up
//! the real `phd2-guider serve` process (whose own contract is
//! covered by its self-contained BDD suite against `mock_phd2`).
//! Records each incoming request as `(path, body)` so subsequent
//! `Then` steps can assert on settle/dither plumbing, and returns
//! either canned guiding responses or a structured error envelope
//! based on the configured behavior.
//!
//! The wire shape mirrors `services/phd2-guider/src/service/api.rs`
//! and `.../error.rs` exactly — see `docs/services/phd2-guider.md`
//! § "HTTP Service Mode". The stub does not depend on the
//! `phd2-guider` crate (Tenet 4: remote interfaces only) — the
//! duplication is the point.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tokio::sync::RwLock;

/// Canned guiding numbers used for both the settle responses
/// (`guiding/start`, `dither`) and the `guiding/stats` body. The
/// defaults are the Pythagorean triple the `mock_phd2` fixture also
/// produces (0.3 / 0.4 / 0.5), so numbers stay recognizable across
/// the two test layers.
#[derive(Debug, Clone)]
pub struct CannedGuiding {
    pub rms_ra_px: f64,
    pub rms_dec_px: f64,
    pub total_rms_px: f64,
    pub sample_count: u32,
    /// Star SNR reported by `guiding/stats`.
    pub snr: f64,
    /// Star mass reported by `guiding/stats`.
    pub star_mass: f64,
    /// Whether `guiding/stats` reports an active guiding loop
    /// (`guiding: true`, `app_state: "Guiding"`). `false` reports a
    /// stopped guider (`app_state: "Stopped"`) — the state
    /// `refocus_train`'s pause/resume handshake checks before pausing.
    pub guiding: bool,
    /// Delay applied before answering the settle-blocking endpoints
    /// (`guiding/start`, `dither`). Zero (the default) answers
    /// immediately; rp's motion-gate scenarios use a non-zero delay
    /// to hold a dither in flight while a concurrent capture is
    /// issued.
    pub settle_delay: std::time::Duration,
    /// Whether `GET /api/v1/equipment` reports a connected PHD2
    /// rotator slot (`{"name": "Stub Rotator", "connected": true}`)
    /// or `null` (the default) — the branch rp's
    /// rotate-while-guiding ladder takes.
    pub phd2_rotator_connected: bool,
    /// HFD values served by `GET /api/v1/guiding/metrics`, one entry
    /// per *request*: request N uses `metrics_hfd_script[min(N-1,
    /// len-1)]` for every frame in that response (the last value
    /// repeats). Each request advances the stub's frame counter by 5
    /// and returns the trailing 12 frames at the current value, so
    /// pollers always see fresh frame numbers. A one-element script
    /// (the default, `[2.5]`) yields perfectly flat HFD — rp's
    /// guide-sweep scenarios lean on the deterministic
    /// no-minimum fit error that produces; a two-element script like
    /// `[2.0, 3.0]` drives the focus-watch degradation scenarios.
    pub metrics_hfd_script: Vec<f64>,
    /// When `true`, the stub models PHD2's guide-loop lifecycle
    /// instead of the static `guiding` flag: stats and metrics report
    /// no active loop (and metrics serve **no frames**, consuming no
    /// script entries) until a `/guiding/start` request lands, and go
    /// inactive again on `/guiding/stop`. This is what lets a
    /// focus-watch scenario fire its events only after the document
    /// under test starts guiding — with the static flag the watch
    /// would degrade before the session even begins and the events
    /// would be lost to the engine's from-run-start intake.
    pub lifecycle: bool,
}

impl Default for CannedGuiding {
    fn default() -> Self {
        Self {
            rms_ra_px: 0.3,
            rms_dec_px: 0.4,
            total_rms_px: 0.5,
            sample_count: 12,
            snr: 25.0,
            star_mass: 5432.0,
            guiding: true,
            settle_delay: std::time::Duration::ZERO,
            phd2_rotator_connected: false,
            metrics_hfd_script: vec![2.5],
            lifecycle: false,
        }
    }
}

/// Canned response variants the stub can return.
#[derive(Debug, Clone)]
pub enum GuiderStubBehavior {
    /// Every endpoint succeeds: settle-blocking calls return the
    /// canned RMS snapshot, state-only calls return their state, and
    /// stats reports `app_state: "Guiding"`.
    Canned(CannedGuiding),
    /// Every endpoint returns a structured error envelope. `code`
    /// matches the service's `ErrorCode` (`invalid_request`,
    /// `not_guiding`, `guide_failed`, `settle_timeout`,
    /// `stop_timeout`, `phd2_unreachable`, `internal`); the
    /// appropriate HTTP status is selected per the service's mapping.
    Error { code: String, message: String },
}

/// In-process axum stub of the guider service. Hold the handle alive
/// for the scenario duration; on drop the listener task is cancelled.
#[derive(Debug)]
pub struct GuiderStub {
    pub url: String,
    pub port: u16,
    requests: Arc<RwLock<Vec<(String, Value)>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Clone)]
struct StubState {
    behavior: GuiderStubBehavior,
    requests: Arc<RwLock<Vec<(String, Value)>>>,
    /// Count of `guiding/metrics` requests served — indexes
    /// `CannedGuiding::metrics_hfd_script` and advances the fake
    /// frame counter (see the field's doc). In lifecycle mode only
    /// active-loop requests count.
    metrics_polls: Arc<std::sync::atomic::AtomicU64>,
    /// Whether the lifecycle-mode guide loop is currently active
    /// (`CannedGuiding::lifecycle`); unused for the static flag.
    lifecycle_active: Arc<std::sync::atomic::AtomicBool>,
}

impl StubState {
    /// The guiding state reported by stats and metrics: the lifecycle
    /// state in lifecycle mode, the static `guiding` flag otherwise.
    fn effective_guiding(&self, c: &CannedGuiding) -> bool {
        if c.lifecycle {
            self.lifecycle_active
                .load(std::sync::atomic::Ordering::SeqCst)
        } else {
            c.guiding
        }
    }
}

impl GuiderStub {
    /// Spawn an axum router on `127.0.0.1:0` serving the guider
    /// service's endpoints with the configured behavior.
    pub async fn start(behavior: GuiderStubBehavior) -> Self {
        let requests: Arc<RwLock<Vec<(String, Value)>>> = Arc::new(RwLock::new(Vec::new()));
        let state = StubState {
            behavior,
            requests: requests.clone(),
            metrics_polls: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            lifecycle_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let app = Router::new()
            .route("/api/v1/guiding/start", post(settle_handler))
            .route("/api/v1/guiding/stop", post(stop_handler))
            .route("/api/v1/guiding/pause", post(pause_handler))
            .route("/api/v1/guiding/resume", post(resume_handler))
            .route("/api/v1/dither", post(settle_handler))
            .route("/api/v1/guiding/stats", get(stats_handler))
            .route("/api/v1/guiding/metrics", get(metrics_handler))
            .route("/api/v1/equipment", get(equipment_handler))
            .route("/api/v1/calibration/clear", post(calibration_clear_handler))
            .route("/api/v1/star/reselect", post(reselect_handler))
            .route("/health", get(health_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind guider stub");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("guider stub failed");
        });

        Self {
            url,
            port,
            requests,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// All `(path, body)` pairs the stub has received, in order. The
    /// body is `Value::Null` for endpoints called without a JSON body
    /// (`stop`, `resume`).
    pub async fn requests(&self) -> Vec<(String, Value)> {
        self.requests.read().await.clone()
    }

    /// The recorded bodies of requests whose path ends with
    /// `path_suffix` (e.g. `"/guiding/start"`, `"/dither"`).
    pub async fn requests_to(&self, path_suffix: &str) -> Vec<Value> {
        self.requests
            .read()
            .await
            .iter()
            .filter(|(path, _)| path.ends_with(path_suffix))
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// Stop the stub server.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for GuiderStub {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Record the request and hand back the behavior-appropriate error,
/// or `None` when the behavior is `Canned` (the caller then builds
/// its endpoint-specific success body).
async fn record_and_check(
    state: &StubState,
    uri: &axum::http::Uri,
    body: axum::body::Bytes,
) -> Option<axum::response::Response> {
    let parsed = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    state
        .requests
        .write()
        .await
        .push((uri.path().to_string(), parsed));

    match &state.behavior {
        GuiderStubBehavior::Canned(_) => None,
        GuiderStubBehavior::Error { code, message } => {
            let status = match code.as_str() {
                "invalid_request" => StatusCode::BAD_REQUEST,
                "not_guiding" => StatusCode::CONFLICT,
                "guide_failed" => StatusCode::UNPROCESSABLE_ENTITY,
                "settle_timeout" | "stop_timeout" => StatusCode::GATEWAY_TIMEOUT,
                "phd2_unreachable" => StatusCode::BAD_GATEWAY,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let body = Json(serde_json::json!({
                "error": code,
                "message": message,
            }));
            Some((status, body).into_response())
        }
    }
}

fn canned(state: &StubState) -> &CannedGuiding {
    match &state.behavior {
        GuiderStubBehavior::Canned(c) => c,
        // record_and_check already returned the error response for
        // the Error behavior; this branch is unreachable by
        // construction.
        GuiderStubBehavior::Error { .. } => unreachable!("canned() called on Error behavior"),
    }
}

async fn settle_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    let c = canned(&state);
    if !c.settle_delay.is_zero() {
        tokio::time::sleep(c.settle_delay).await;
    }
    if c.lifecycle && uri.path().ends_with("/guiding/start") {
        state
            .lifecycle_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Json(serde_json::json!({
        "state": "guiding",
        "rms_ra_px": c.rms_ra_px,
        "rms_dec_px": c.rms_dec_px,
        "total_rms_px": c.total_rms_px,
        "sample_count": c.sample_count,
    }))
    .into_response()
}

async fn stop_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    if canned(&state).lifecycle {
        state
            .lifecycle_active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    Json(serde_json::json!({ "state": "stopped" })).into_response()
}

async fn pause_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    Json(serde_json::json!({ "state": "paused" })).into_response()
}

async fn resume_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    Json(serde_json::json!({ "state": "resumed" })).into_response()
}

async fn stats_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    let c = canned(&state);
    let guiding = state.effective_guiding(c);
    Json(serde_json::json!({
        "app_state": if guiding { "Guiding" } else { "Stopped" },
        "guiding": guiding,
        "rms_ra_px": c.rms_ra_px,
        "rms_dec_px": c.rms_dec_px,
        "total_rms_px": c.total_rms_px,
        "snr": c.snr,
        "star_mass": c.star_mass,
        "sample_count": c.sample_count,
    }))
    .into_response()
}

async fn health_handler(State(_state): State<StubState>) -> axum::response::Response {
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

async fn metrics_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    let c = canned(&state);
    if c.lifecycle && !state.effective_guiding(c) {
        // No guide loop, no frames — and no script consumption, so
        // the script's first entry is what the watch sees once the
        // document under test actually starts guiding.
        return Json(serde_json::json!({ "guiding": false, "frames": [] })).into_response();
    }
    let poll = state
        .metrics_polls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let script = &c.metrics_hfd_script;
    // Clamped at the last entry; an empty script yields NaN (null on the
    // wire), which no HFD watch converges on — loud, not silently plausible.
    let idx = usize::try_from(poll)
        .unwrap_or(usize::MAX)
        .min(script.len().saturating_sub(1));
    let hfd = script.get(idx).copied().unwrap_or(f64::NAN);
    // Advance 5 fake frames per poll and return the trailing 12, so
    // watermark-based freshness checks always find new frames.
    let frame_end = poll.saturating_add(1).saturating_mul(5);
    let frames: Vec<Value> = (frame_end.saturating_sub(11).max(1)..=frame_end)
        .map(|frame| {
            serde_json::json!({
                "frame": frame,
                "hfd": hfd,
                "snr": c.snr,
                "star_mass": c.star_mass,
                "star_lost": false,
            })
        })
        .collect();
    Json(serde_json::json!({ "guiding": state.effective_guiding(c), "frames": frames }))
        .into_response()
}

async fn equipment_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    let c = canned(&state);
    let rotator = if c.phd2_rotator_connected {
        serde_json::json!({ "name": "Stub Rotator", "connected": true })
    } else {
        Value::Null
    };
    Json(serde_json::json!({
        "camera": { "name": "Stub Camera", "connected": true },
        "mount": { "name": "Stub Mount", "connected": true },
        "aux_mount": null,
        "ao": null,
        "rotator": rotator,
    }))
    .into_response()
}

async fn calibration_clear_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    Json(serde_json::json!({ "state": "cleared" })).into_response()
}

async fn reselect_handler(
    State(state): State<StubState>,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    if let Some(err) = record_and_check(&state, &uri, body).await {
        return err;
    }
    Json(serde_json::json!({ "state": "selected" })).into_response()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canned_stub_answers_every_endpoint_and_records_requests() {
        let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding::default())).await;
        let client = reqwest::Client::new();

        let start: Value = client
            .post(format!("{}/api/v1/guiding/start", stub.url))
            .json(&serde_json::json!({ "recalibrate": true }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(start["state"], "guiding");
        assert_eq!(start["total_rms_px"], 0.5);

        let stop: Value = client
            .post(format!("{}/api/v1/guiding/stop", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stop["state"], "stopped");

        let stats: Value = client
            .get(format!("{}/api/v1/guiding/stats", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stats["app_state"], "Guiding");
        assert_eq!(stats["snr"], 25.0);

        let requests = stub.requests().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, "/api/v1/guiding/start");
        assert_eq!(requests[0].1["recalibrate"], true);
        // A body-less stop records Null.
        assert_eq!(requests[1].1, Value::Null);

        let starts = stub.requests_to("/guiding/start").await;
        assert_eq!(starts.len(), 1);
    }

    #[tokio::test]
    async fn the_t4_endpoints_answer_with_their_canned_shapes() {
        let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
            phd2_rotator_connected: true,
            metrics_hfd_script: vec![2.0, 3.0],
            ..CannedGuiding::default()
        }))
        .await;
        let client = reqwest::Client::new();

        let equipment: Value = client
            .get(format!("{}/api/v1/equipment", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(equipment["rotator"]["name"], "Stub Rotator");

        // Metrics advance 5 frames per poll and follow the script.
        let m1: Value = client
            .get(format!("{}/api/v1/guiding/metrics", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(m1["guiding"], true);
        let f1 = m1["frames"].as_array().unwrap();
        assert_eq!(f1.last().unwrap()["frame"], 5);
        assert_eq!(f1[0]["hfd"], 2.0);
        let m2: Value = client
            .get(format!("{}/api/v1/guiding/metrics", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let f2 = m2["frames"].as_array().unwrap();
        assert_eq!(f2.last().unwrap()["frame"], 10);
        assert_eq!(f2[0]["hfd"], 3.0);

        let cleared: Value = client
            .post(format!("{}/api/v1/calibration/clear", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cleared["state"], "cleared");

        let selected: Value = client
            .post(format!("{}/api/v1/star/reselect", stub.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(selected["state"], "selected");

        let clear_requests = stub.requests_to("/calibration/clear").await;
        assert_eq!(clear_requests.len(), 1);
    }

    #[tokio::test]
    async fn the_t4_endpoints_map_the_error_behavior() {
        let stub = GuiderStub::start(GuiderStubBehavior::Error {
            code: "internal".to_string(),
            message: "boom".to_string(),
        })
        .await;
        let client = reqwest::Client::new();
        for (method, path) in [
            ("get", "/api/v1/guiding/metrics"),
            ("get", "/api/v1/equipment"),
            ("post", "/api/v1/calibration/clear"),
            ("post", "/api/v1/star/reselect"),
        ] {
            let request = if method == "get" {
                client.get(format!("{}{}", stub.url, path))
            } else {
                client.post(format!("{}{}", stub.url, path))
            };
            let response = request.send().await.unwrap();
            assert_eq!(response.status().as_u16(), 500, "{path}");
        }
    }

    #[tokio::test]
    async fn error_behavior_maps_codes_to_the_service_statuses() {
        for (code, expected_status) in [
            ("invalid_request", 400),
            ("not_guiding", 409),
            ("guide_failed", 422),
            ("settle_timeout", 504),
            ("stop_timeout", 504),
            ("phd2_unreachable", 502),
            ("internal", 500),
        ] {
            let stub = GuiderStub::start(GuiderStubBehavior::Error {
                code: code.to_string(),
                message: "boom".to_string(),
            })
            .await;
            let resp = reqwest::Client::new()
                .post(format!("{}/api/v1/dither", stub.url))
                .json(&serde_json::json!({ "amount_px": 3.0 }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), expected_status, "code {code}");
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body["error"], code);
            assert_eq!(body["message"], "boom");
        }
    }

    #[tokio::test]
    async fn lifecycle_mode_follows_start_and_stop_and_serves_no_frames_while_inactive() {
        let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
            lifecycle: true,
            metrics_hfd_script: vec![2.0, 3.0],
            ..CannedGuiding::default()
        }))
        .await;
        let client = reqwest::Client::new();
        let get = |path: &str| {
            let url = format!("{}{path}", stub.url);
            let client = client.clone();
            async move {
                client
                    .get(url)
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            }
        };

        // Inactive: stats and metrics say so, no frames, no script use.
        assert_eq!(get("/api/v1/guiding/stats").await["guiding"], false);
        let idle = get("/api/v1/guiding/metrics").await;
        assert_eq!(idle["guiding"], false);
        assert!(idle["frames"].as_array().unwrap().is_empty());

        // Start activates the loop; the first active metrics request
        // serves the script's FIRST entry — idle polls consumed none.
        client
            .post(format!("{}/api/v1/guiding/start", stub.url))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(get("/api/v1/guiding/stats").await["app_state"], "Guiding");
        let active = get("/api/v1/guiding/metrics").await;
        assert_eq!(active["guiding"], true);
        assert_eq!(active["frames"][0]["hfd"], 2.0);

        // Stop deactivates again.
        client
            .post(format!("{}/api/v1/guiding/stop", stub.url))
            .send()
            .await
            .unwrap();
        assert_eq!(get("/api/v1/guiding/stats").await["guiding"], false);
    }
}

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! HTTP client for the guider rp-managed service.
//!
//! Wraps the guider service's frozen HTTP contract (the
//! `phd2-guider` binary's `serve` mode — `POST
//! /api/v1/guiding/{start,stop,pause,resume}`, `POST /api/v1/dither`,
//! `GET /api/v1/guiding/stats`) behind a small, mockable Rust API.
//! Used by `rp`'s guiding MCP tools (`start_guiding`, `stop_guiding`,
//! `dither`, `pause_guiding`, `resume_guiding`, `get_guiding_stats`)
//! and by the safety enforcer's stop-guiding-on-unsafe path.
//!
//! Wire types are defined here, not pulled from the `phd2-guider`
//! crate — the inter-service contract is HTTP, not in-process Rust
//! types (Tenet 4: remote interfaces only). The contract lives in
//! `docs/services/phd2-guider.md` § "HTTP Service Mode"; this module
//! is a thin transport. All positional quantities are **guide-camera
//! pixels** (`*_px`, `settle.pixels`), matching the service.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rp_auth::config::ClientAuthConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Extra head-room added on top of a request's settle `timeout` when
/// sizing the client-side HTTP deadline for `start_guiding` /
/// `dither`. The service holds those requests open until settling
/// resolves, bounded by its own backstop of `settle.timeout` plus a
/// 10 s grace (see `phd2-guider.md` § "HTTP Service Mode"); 15 s of
/// margin keeps the service's legitimate `settle_timeout` envelope
/// arriving before rp's transport deadline cuts the connection.
const SETTLE_BACKSTOP_MARGIN: Duration = Duration::from_secs(15);

/// Partial settle override forwarded to the service. Omitted fields
/// fall back to the *service's* configured `settling` block, field by
/// field — `None` here means "omit from the wire", not "zero".
#[derive(Debug, Clone, Default, Serialize)]
pub struct SettleOverride {
    /// Settle threshold in guide-camera pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixels: Option<f64>,
    /// How long guiding must hold within the threshold.
    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<Duration>,
    /// PHD2-enforced deadline for the settle to complete.
    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

impl SettleOverride {
    /// `true` when every field is unset — the caller should send no
    /// `settle` object at all so the service's defaults apply.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pixels.is_none() && self.time.is_none() && self.timeout.is_none()
    }
}

/// Request body for `POST /api/v1/guiding/start`. Matches the
/// service's `StartBody` field-for-field.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartGuidingRequest {
    /// Force a recalibration before guiding starts.
    pub recalibrate: bool,
    /// Per-call settle override; `None` ⇒ the service's configured
    /// `settling` defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle: Option<SettleOverride>,
}

/// Request body for `POST /api/v1/dither`. Matches the service's
/// `DitherBody` field-for-field.
#[derive(Debug, Clone, Serialize)]
pub struct DitherRequest {
    /// Dither offset in guide-camera pixels.
    pub amount_px: f64,
    /// Restrict the dither to the RA axis.
    pub ra_only: bool,
    /// Per-call settle override; `None` ⇒ the service's configured
    /// `settling` defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle: Option<SettleOverride>,
}

/// Success body shared by `guiding/start` and `dither` — the state of
/// guiding after the settle completed. RMS fields are `None` until
/// the service has sampled at least one guide step on each axis.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettledOutcome {
    /// Always `"guiding"` on success.
    pub state: String,
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    /// Guide steps in the service's rolling RMS window.
    pub sample_count: u32,
}

/// Success body of `GET /api/v1/guiding/stats`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuidingStats {
    /// PHD2 application state (`Guiding`, `Looping`, `Stopped`, ...).
    pub app_state: String,
    /// `true` iff `app_state == "Guiding"`.
    pub guiding: bool,
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    /// Star SNR from the most recent guide step; `None` when never
    /// sampled or omitted from the latest step.
    pub snr: Option<f64>,
    /// Star mass from the most recent guide step.
    pub star_mass: Option<f64>,
    pub sample_count: u32,
}

/// One entry of `GET /api/v1/guiding/metrics`: a PHD2 guide frame's
/// star metrics, or a star-lost marker (`star_lost: true`, no HFD).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FrameMetrics {
    /// PHD2's own frame counter — poll for freshness by watermark,
    /// never by array position.
    pub frame: u64,
    /// Half-flux diameter in guide-camera pixels; `None` when the
    /// step omitted it (older PHD2) or the star was lost.
    pub hfd: Option<f64>,
    pub snr: Option<f64>,
    pub star_mass: Option<f64>,
    pub star_lost: bool,
}

/// Success body of `GET /api/v1/guiding/metrics`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuidingMetrics {
    /// `true` iff PHD2's application state is `Guiding`.
    pub guiding: bool,
    /// The service's per-frame ring, oldest first (up to 50 entries,
    /// cleared when guiding starts).
    pub frames: Vec<FrameMetrics>,
}

/// One equipment slot of `GET /api/v1/equipment`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EquipmentSlot {
    pub name: String,
    pub connected: bool,
}

/// Success body of `GET /api/v1/equipment` — PHD2's current
/// equipment; slots the profile does not configure are `None`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PhdEquipment {
    pub camera: Option<EquipmentSlot>,
    pub mount: Option<EquipmentSlot>,
    pub aux_mount: Option<EquipmentSlot>,
    pub ao: Option<EquipmentSlot>,
    pub rotator: Option<EquipmentSlot>,
}

/// Success body of the state-only endpoints (`stop`, `pause`,
/// `resume`, `calibration/clear`, `star/reselect`). Parsed to
/// validate the response shape; callers get `()`.
#[derive(Debug, Deserialize)]
struct StateOutcome {
    #[allow(dead_code)]
    state: String,
}

/// Structured error envelope returned by the service (matches
/// `ErrorResponse` in `services/phd2-guider/src/service/error.rs`).
#[derive(Debug, Clone, Deserialize)]
struct ErrorResponseBody {
    error: String,
    message: String,
    #[serde(default)]
    details: serde_json::Value,
}

/// Errors surfaced by the client. Variants:
/// - `ServiceUnreachable` — connection refused, DNS failure,
///   connection timeout. The caller cannot distinguish further.
/// - `Service { code, message, details }` — the service returned a
///   structured error envelope (`invalid_request`, `not_guiding`,
///   `guide_failed`, `settle_timeout`, `stop_timeout`,
///   `phd2_unreachable`, `internal`). Propagate verbatim.
/// - `Internal` — a service-shaped HTTP error that didn't parse as
///   the structured envelope, or a Rust-side bug (rare).
#[derive(Debug, Error)]
pub enum GuiderError {
    #[error("service unreachable: {0}")]
    ServiceUnreachable(String),

    #[error("{code}: {message}")]
    Service {
        code: String,
        message: String,
        details: serde_json::Value,
    },

    #[error("internal: {0}")]
    Internal(String),
}

/// HTTP client to the guider service.
#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
#[async_trait]
pub trait GuiderClient: Send + Sync {
    /// `POST /api/v1/guiding/start` — start the guiding loop and
    /// block until the post-start settle completes.
    async fn start_guiding(
        &self,
        request: StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError>;

    /// `POST /api/v1/guiding/stop` — stop guiding, blocking until the
    /// service confirms the loop is down. Idempotent: stopping an
    /// already-stopped guider succeeds.
    async fn stop_guiding(&self) -> Result<(), GuiderError>;

    /// `POST /api/v1/guiding/pause` — pause guide corrections.
    /// `full` also pauses guide-camera looping.
    async fn pause_guiding(&self, full: bool) -> Result<(), GuiderError>;

    /// `POST /api/v1/guiding/resume` — resume after a pause.
    async fn resume_guiding(&self) -> Result<(), GuiderError>;

    /// `POST /api/v1/dither` — dither and block until re-settled.
    async fn dither(&self, request: DitherRequest) -> Result<SettledOutcome, GuiderError>;

    /// `GET /api/v1/guiding/stats` — read the current guiding state
    /// and rolling RMS statistics.
    async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError>;

    /// `GET /api/v1/guiding/metrics` — the service's per-frame star
    /// metrics ring (HFD/SNR/star mass + star-lost markers).
    async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError>;

    /// `GET /api/v1/equipment` — PHD2's current equipment slots.
    async fn current_equipment(&self) -> Result<PhdEquipment, GuiderError>;

    /// `POST /api/v1/calibration/clear` — clear PHD2's stored mount
    /// calibration; PHD2 recalibrates on the next guide start.
    async fn clear_calibration(&self) -> Result<(), GuiderError>;

    /// `POST /api/v1/star/reselect` — auto-select a guide star on
    /// the current frame (after a rotation moved it).
    async fn reselect_star(&self) -> Result<(), GuiderError>;
}

/// Concrete reqwest-backed implementation of [`GuiderClient`].
#[derive(Debug, Clone)]
pub struct GuiderServiceClient {
    client: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

impl GuiderServiceClient {
    /// Build a client against the given base URL. `timeout` is the
    /// per-request HTTP deadline for the quick endpoints (stop,
    /// pause, resume, stats); the settle-blocking endpoints (start,
    /// dither) stretch it to the request's settle `timeout` plus
    /// [`SETTLE_BACKSTOP_MARGIN`] when that is longer, so a
    /// legitimate service-side `settle_timeout` error always arrives
    /// instead of a client-side cut.
    ///
    /// `auth` sends HTTP Basic Auth credentials on every request, for
    /// an auth-enabled guider service (issue #620).
    ///
    /// `ca_cert_path` is the observatory CA (rp.md §Configuration):
    /// without it, an `https://` `base_url` signed by that CA fails
    /// certificate verification (issue #609).
    ///
    /// # Errors
    ///
    /// Returns an error if the CA certificate cannot be read or parsed,
    /// or if the HTTP client fails to build.
    pub fn new(
        base_url: String,
        timeout: Duration,
        auth: Option<&ClientAuthConfig>,
        ca_cert_path: Option<&std::path::Path>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // No client-level timeout: each request carries its own (see
        // `settle_request_timeout`).
        let mut builder = rusty_photon_tls::client::client_builder(ca_cert_path)?;
        if let Some(a) = auth {
            let encoded = BASE64.encode(format!("{}:{}", a.username, a.password));
            // `set_sensitive` keeps the credential out of `Client`'s `Debug`
            // impl, which prints `default_headers` unconditionally — without
            // it, an incidental `{:?}`/`debug!()` of this client leaks the
            // password (base64 is trivially reversible).
            let mut header_value: reqwest::header::HeaderValue =
                format!("Basic {encoded}").parse()?;
            header_value.set_sensitive(true);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("authorization", header_value);
            builder = builder.default_headers(headers);
        }
        let client = builder.build()?;
        Ok(Self {
            client,
            base_url: trim_trailing_slash(base_url),
            timeout,
        })
    }

    /// The base URL the client was constructed with (no trailing
    /// slash). Useful for tests that want to log or assert.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// HTTP deadline for a settle-blocking request: the configured
    /// quick-call timeout, stretched to cover the settle deadline
    /// plus margin when the request pins one.
    fn settle_request_timeout(&self, settle: Option<&SettleOverride>) -> Duration {
        settle.and_then(|s| s.timeout).map_or(self.timeout, |t| {
            self.timeout.max(t.saturating_add(SETTLE_BACKSTOP_MARGIN))
        })
    }

    /// Send a request and parse the success body as `T`, mapping
    /// transport failures and the service's structured error envelope
    /// onto [`GuiderError`].
    async fn execute<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, GuiderError> {
        let response = request
            .send()
            .await
            .map_err(|e| GuiderError::ServiceUnreachable(e.to_string()))?;

        if response.status().is_success() {
            response
                .json::<T>()
                .await
                .map_err(|e| GuiderError::Internal(format!("failed to parse success body: {e}")))
        } else {
            let status = response.status();
            // Try the structured envelope first; fall back to
            // Internal so a service bug returning malformed JSON on
            // a 5xx isn't lost.
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<unreadable response body: {e}>"));
            match serde_json::from_str::<ErrorResponseBody>(&body) {
                Ok(env) => Err(GuiderError::Service {
                    code: env.error,
                    message: env.message,
                    details: env.details,
                }),
                Err(parse_err) => Err(GuiderError::Internal(format!(
                    "service returned status {status} with non-envelope body: {parse_err}; raw: {body}"
                ))),
            }
        }
    }
}

fn trim_trailing_slash(mut s: String) -> String {
    if s.ends_with('/') {
        s.pop();
    }
    s
}

#[async_trait]
impl GuiderClient for GuiderServiceClient {
    async fn start_guiding(
        &self,
        request: StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError> {
        let url = format!("{}/api/v1/guiding/start", self.base_url);
        let timeout = self.settle_request_timeout(request.settle.as_ref());
        self.execute(self.client.post(&url).timeout(timeout).json(&request))
            .await
    }

    async fn stop_guiding(&self) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/guiding/stop", self.base_url);
        self.execute::<StateOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|_| ())
    }

    async fn pause_guiding(&self, full: bool) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/guiding/pause", self.base_url);
        self.execute::<StateOutcome>(
            self.client
                .post(&url)
                .timeout(self.timeout)
                .json(&serde_json::json!({ "full": full })),
        )
        .await
        .map(|_| ())
    }

    async fn resume_guiding(&self) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/guiding/resume", self.base_url);
        self.execute::<StateOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|_| ())
    }

    async fn dither(&self, request: DitherRequest) -> Result<SettledOutcome, GuiderError> {
        let url = format!("{}/api/v1/dither", self.base_url);
        let timeout = self.settle_request_timeout(request.settle.as_ref());
        self.execute(self.client.post(&url).timeout(timeout).json(&request))
            .await
    }

    async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError> {
        let url = format!("{}/api/v1/guiding/stats", self.base_url);
        self.execute(self.client.get(&url).timeout(self.timeout))
            .await
    }

    async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError> {
        let url = format!("{}/api/v1/guiding/metrics", self.base_url);
        self.execute(self.client.get(&url).timeout(self.timeout))
            .await
    }

    async fn current_equipment(&self) -> Result<PhdEquipment, GuiderError> {
        let url = format!("{}/api/v1/equipment", self.base_url);
        self.execute(self.client.get(&url).timeout(self.timeout))
            .await
    }

    async fn clear_calibration(&self) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/calibration/clear", self.base_url);
        self.execute::<StateOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|_| ())
    }

    async fn reselect_star(&self) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/star/reselect", self.base_url);
        self.execute::<StateOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::Value;
    use tokio::sync::RwLock;

    /// Test-side stub: spawn axum on `127.0.0.1:0`, configurable
    /// success / error behavior, captures every request as
    /// `(path, body)`.
    struct TestStub {
        url: String,
        requests: Arc<RwLock<Vec<(String, Value)>>>,
        _shutdown_tx: tokio::sync::oneshot::Sender<()>,
    }

    enum StubBehavior {
        Success,
        Error { status: StatusCode, body: Value },
        SuccessMalformed,
        ErrorMalformed,
    }

    #[derive(Clone)]
    struct StubState {
        behavior: Arc<StubBehavior>,
        requests: Arc<RwLock<Vec<(String, Value)>>>,
    }

    async fn spawn_stub(behavior: StubBehavior) -> TestStub {
        let requests: Arc<RwLock<Vec<(String, Value)>>> = Arc::new(RwLock::new(Vec::new()));
        let state = StubState {
            behavior: Arc::new(behavior),
            requests: requests.clone(),
        };

        let app = Router::new()
            .route("/api/v1/guiding/start", post(settled_handler))
            .route("/api/v1/guiding/stop", post(state_handler))
            .route("/api/v1/guiding/pause", post(state_handler))
            .route("/api/v1/guiding/resume", post(state_handler))
            .route("/api/v1/dither", post(settled_handler))
            .route("/api/v1/guiding/stats", get(stats_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        TestStub {
            url,
            requests,
            _shutdown_tx: shutdown_tx,
        }
    }

    async fn record(state: &StubState, uri: &axum::http::Uri, body: &axum::body::Bytes) {
        let parsed = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        state
            .requests
            .write()
            .await
            .push((uri.path().to_string(), parsed));
    }

    fn behavior_response(behavior: &StubBehavior, success: Value) -> axum::response::Response {
        match behavior {
            StubBehavior::Success => Json(success).into_response(),
            StubBehavior::Error { status, body } => (*status, Json(body.clone())).into_response(),
            StubBehavior::SuccessMalformed => (StatusCode::OK, "not json").into_response(),
            StubBehavior::ErrorMalformed => {
                (StatusCode::INTERNAL_SERVER_ERROR, "not json").into_response()
            }
        }
    }

    async fn settled_handler(
        State(state): State<StubState>,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        record(&state, &uri, &body).await;
        behavior_response(
            &state.behavior,
            serde_json::json!({
                "state": "guiding",
                "rms_ra_px": 0.3,
                "rms_dec_px": 0.4,
                "total_rms_px": 0.5,
                "sample_count": 2,
            }),
        )
    }

    async fn state_handler(
        State(state): State<StubState>,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        record(&state, &uri, &body).await;
        behavior_response(&state.behavior, serde_json::json!({ "state": "stopped" }))
    }

    async fn stats_handler(
        State(state): State<StubState>,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        record(&state, &uri, &body).await;
        behavior_response(
            &state.behavior,
            serde_json::json!({
                "app_state": "Guiding",
                "guiding": true,
                "rms_ra_px": 0.3,
                "rms_dec_px": 0.4,
                "total_rms_px": 0.5,
                "snr": 25.0,
                "star_mass": 5432.0,
                "sample_count": 2,
            }),
        )
    }

    fn client_for(stub: &TestStub) -> GuiderServiceClient {
        GuiderServiceClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap()
    }

    #[tokio::test]
    async fn start_guiding_happy_path_returns_outcome() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let outcome = client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap();

        assert_eq!(outcome.state, "guiding");
        assert_eq!(outcome.rms_ra_px, Some(0.3));
        assert_eq!(outcome.rms_dec_px, Some(0.4));
        assert_eq!(outcome.total_rms_px, Some(0.5));
        assert_eq!(outcome.sample_count, 2);
    }

    #[tokio::test]
    async fn start_guiding_forwards_recalibrate_and_settle_verbatim() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        client
            .start_guiding(StartGuidingRequest {
                recalibrate: true,
                settle: Some(SettleOverride {
                    pixels: Some(0.8),
                    time: Some(Duration::from_secs(8)),
                    timeout: Some(Duration::from_secs(40)),
                }),
            })
            .await
            .unwrap();

        let (path, body) = stub.requests.read().await[0].clone();
        assert_eq!(path, "/api/v1/guiding/start");
        assert_eq!(body["recalibrate"], true);
        assert_eq!(body["settle"]["pixels"], 0.8);
        assert_eq!(body["settle"]["time"], "8s");
        assert_eq!(body["settle"]["timeout"], "40s");
    }

    #[tokio::test]
    async fn start_guiding_omits_unset_settle() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap();

        let (_, body) = stub.requests.read().await[0].clone();
        assert_eq!(body["recalibrate"], false);
        assert!(
            body.get("settle").is_none(),
            "expected 'settle' to be absent, got: {body}"
        );
    }

    #[tokio::test]
    async fn settle_override_omits_unset_subfields() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        client
            .start_guiding(StartGuidingRequest {
                recalibrate: false,
                settle: Some(SettleOverride {
                    pixels: Some(1.2),
                    time: None,
                    timeout: None,
                }),
            })
            .await
            .unwrap();

        let (_, body) = stub.requests.read().await[0].clone();
        assert_eq!(body["settle"]["pixels"], 1.2);
        assert!(body["settle"].get("time").is_none());
        assert!(body["settle"].get("timeout").is_none());
    }

    #[tokio::test]
    async fn stop_pause_resume_hit_their_endpoints() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        client.stop_guiding().await.unwrap();
        client.pause_guiding(true).await.unwrap();
        client.resume_guiding().await.unwrap();

        let requests = stub.requests.read().await.clone();
        assert_eq!(requests[0].0, "/api/v1/guiding/stop");
        assert_eq!(requests[1].0, "/api/v1/guiding/pause");
        assert_eq!(requests[1].1["full"], true);
        assert_eq!(requests[2].0, "/api/v1/guiding/resume");
    }

    #[tokio::test]
    async fn dither_forwards_amount_ra_only_and_settle() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let outcome = client
            .dither(DitherRequest {
                amount_px: 5.0,
                ra_only: true,
                settle: Some(SettleOverride {
                    pixels: None,
                    time: None,
                    timeout: Some(Duration::from_secs(30)),
                }),
            })
            .await
            .unwrap();

        assert_eq!(outcome.total_rms_px, Some(0.5));
        let (path, body) = stub.requests.read().await[0].clone();
        assert_eq!(path, "/api/v1/dither");
        assert_eq!(body["amount_px"], 5.0);
        assert_eq!(body["ra_only"], true);
        assert_eq!(body["settle"]["timeout"], "30s");
    }

    #[tokio::test]
    async fn guiding_stats_parses_the_full_body() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let stats = client.guiding_stats().await.unwrap();

        assert_eq!(stats.app_state, "Guiding");
        assert!(stats.guiding);
        assert_eq!(stats.rms_ra_px, Some(0.3));
        assert_eq!(stats.snr, Some(25.0));
        assert_eq!(stats.star_mass, Some(5432.0));
        assert_eq!(stats.sample_count, 2);
    }

    #[tokio::test]
    async fn null_telemetry_fields_parse_as_none() {
        // The Error arm doubles as "return this exact body with this
        // status" — with status 200 it exercises success-path parsing
        // of a stats body whose telemetry fields are all null.
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::OK,
            body: serde_json::json!({
                "app_state": "Stopped",
                "guiding": false,
                "rms_ra_px": null,
                "rms_dec_px": null,
                "total_rms_px": null,
                "snr": null,
                "star_mass": null,
                "sample_count": 0,
            }),
        })
        .await;
        let client = client_for(&stub);

        let stats = client.guiding_stats().await.unwrap();
        assert_eq!(stats.app_state, "Stopped");
        assert!(!stats.guiding);
        assert!(stats.rms_ra_px.is_none());
        assert!(stats.snr.is_none());
        assert_eq!(stats.sample_count, 0);
    }

    #[tokio::test]
    async fn service_error_envelope_propagates() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::CONFLICT,
            body: serde_json::json!({
                "error": "not_guiding",
                "message": "PHD2 is not guiding (state: Stopped)",
            }),
        })
        .await;
        let client = client_for(&stub);

        let err = client
            .dither(DitherRequest {
                amount_px: 3.0,
                ra_only: false,
                settle: None,
            })
            .await
            .unwrap_err();

        match err {
            GuiderError::Service { code, message, .. } => {
                assert_eq!(code, "not_guiding");
                assert!(message.contains("not guiding"));
            }
            other => panic!("expected Service error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn settle_timeout_envelope_propagates() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::GATEWAY_TIMEOUT,
            body: serde_json::json!({
                "error": "settle_timeout",
                "message": "settle did not complete in time",
            }),
        })
        .await;
        let client = client_for(&stub);

        let err = client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, GuiderError::Service { code, .. } if code == "settle_timeout"));
    }

    #[tokio::test]
    async fn malformed_success_body_is_internal() {
        let stub = spawn_stub(StubBehavior::SuccessMalformed).await;
        let client = client_for(&stub);

        let err = client.stop_guiding().await.unwrap_err();
        assert!(matches!(err, GuiderError::Internal(_)));
    }

    #[tokio::test]
    async fn non_envelope_error_body_is_internal() {
        let stub = spawn_stub(StubBehavior::ErrorMalformed).await;
        let client = client_for(&stub);

        let err = client.resume_guiding().await.unwrap_err();
        match err {
            GuiderError::Internal(msg) => {
                assert!(msg.contains("non-envelope body"), "got: {msg}");
            }
            other => panic!("expected Internal error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_is_service_unreachable() {
        // Port 1 is reserved and reliably refuses connections on
        // dev hosts and CI runners (same pattern rp-plate-solver's
        // unreachable test uses).
        let client = GuiderServiceClient::new(
            "http://127.0.0.1:1".to_string(),
            Duration::from_secs(2),
            None,
            None,
        )
        .unwrap();
        let err = client.stop_guiding().await.unwrap_err();
        assert!(matches!(err, GuiderError::ServiceUnreachable(_)));
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let client = GuiderServiceClient::new(
            "http://localhost:11130/".to_string(),
            Duration::from_secs(5),
            None,
            None,
        )
        .unwrap();
        assert_eq!(client.base_url(), "http://localhost:11130");
    }

    #[test]
    fn settle_request_timeout_stretches_past_the_settle_deadline() {
        let client = GuiderServiceClient::new(
            "http://localhost:11130".to_string(),
            Duration::from_secs(90),
            None,
            None,
        )
        .unwrap();

        // No settle timeout on the request → the configured timeout.
        assert_eq!(client.settle_request_timeout(None), Duration::from_secs(90));

        // A settle timeout shorter than the configured timeout (minus
        // margin) leaves the configured timeout in charge.
        let short = SettleOverride {
            pixels: None,
            time: None,
            timeout: Some(Duration::from_secs(30)),
        };
        assert_eq!(
            client.settle_request_timeout(Some(&short)),
            Duration::from_secs(90)
        );

        // A long settle timeout stretches the HTTP deadline past the
        // service's backstop (timeout + 10 s grace) with margin.
        let long = SettleOverride {
            pixels: None,
            time: None,
            timeout: Some(Duration::from_mins(2)),
        };
        assert_eq!(
            client.settle_request_timeout(Some(&long)),
            Duration::from_secs(135)
        );
    }

    #[test]
    fn settle_request_timeout_saturates_instead_of_overflowing_on_an_extreme_settle_timeout() {
        let client = GuiderServiceClient::new(
            "http://localhost:11130".to_string(),
            Duration::from_secs(90),
            None,
            None,
        )
        .unwrap();
        let extreme = SettleOverride {
            pixels: None,
            time: None,
            timeout: Some(Duration::MAX),
        };
        assert_eq!(client.settle_request_timeout(Some(&extreme)), Duration::MAX);
    }

    #[test]
    fn new_with_missing_ca_cert_fails() {
        // Proves `ca_cert_path` is actually wired to the CA-trust
        // builder, not silently dropped (issue #609).
        let result = GuiderServiceClient::new(
            "http://localhost:11130".to_string(),
            Duration::from_secs(5),
            None,
            Some(std::path::Path::new("/nonexistent/ca.pem")),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn new_with_auth_sends_basic_auth_header() {
        let captured: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let captured_for_handler = captured.clone();
        let app = Router::new().route(
            "/api/v1/guiding/stop",
            post(move |headers: axum::http::HeaderMap| {
                let captured = captured_for_handler.clone();
                async move {
                    let auth_header = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    *captured.write().await = auth_header;
                    Json(serde_json::json!({ "state": "stopped" })).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let auth = ClientAuthConfig {
            username: "observatory".to_string(),
            password: "secret".to_string(),
        };
        let client = GuiderServiceClient::new(
            format!("http://127.0.0.1:{port}"),
            Duration::from_secs(5),
            Some(&auth),
            None,
        )
        .unwrap();
        client.stop_guiding().await.unwrap();

        let header = captured.read().await.clone();
        let expected = format!("Basic {}", BASE64.encode("observatory:secret"));
        assert_eq!(header, Some(expected));
        shutdown_tx.send(()).ok();
    }

    // ----- credential redaction in Debug output --------------------------
    //
    // `GuiderServiceClient` derives `Debug`, and reqwest's `Client` Debug
    // impl prints `default_headers` unconditionally — without
    // `set_sensitive`, an incidental `{:?}`/`debug!()` of this client would
    // leak the password (the header's base64 encoding is trivially
    // reversible).
    #[test]
    fn debug_format_never_leaks_the_auth_header() {
        let auth = ClientAuthConfig {
            username: "observatory".to_string(),
            password: "s3cret-pw".to_string(),
        };
        let client = GuiderServiceClient::new(
            "http://localhost:11130".to_string(),
            Duration::from_secs(5),
            Some(&auth),
            None,
        )
        .unwrap();
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains(&BASE64.encode("observatory:s3cret-pw")),
            "credential leaked into Debug output: {rendered}"
        );
        assert!(
            !rendered.contains("s3cret-pw"),
            "plaintext password leaked into Debug output: {rendered}"
        );
    }

    #[test]
    fn settle_override_is_empty_only_when_all_fields_unset() {
        assert!(SettleOverride::default().is_empty());
        assert!(!SettleOverride {
            pixels: Some(0.5),
            time: None,
            timeout: None,
        }
        .is_empty());
        assert!(!SettleOverride {
            pixels: None,
            time: Some(Duration::from_secs(5)),
            timeout: None,
        }
        .is_empty());
        assert!(!SettleOverride {
            pixels: None,
            time: None,
            timeout: Some(Duration::from_secs(5)),
        }
        .is_empty());
    }
}

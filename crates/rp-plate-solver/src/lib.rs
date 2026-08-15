#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! HTTP client for the `plate-solver` rp-managed service.
//!
//! Wraps the wrapper's frozen HTTP contract (`POST /api/v1/solve`)
//! behind a small, mockable Rust API. Used by `rp`'s `plate_solve`
//! MCP tool today; future callers (workflow plugins, ad-hoc CLI
//! tools, the `center_on_target` compound tool) consume the same
//! `PlateSolveClient` trait.
//!
//! Wire types are defined here, not pulled from the `plate-solver`
//! crate — the inter-service contract is HTTP, not in-process Rust
//! types (Tenet 4: remote interfaces only). The contract lives in
//! `docs/services/plate-solver.md`; this module is a thin
//! transport.

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

/// Solve request payload sent to the wrapper. Matches the
/// `SolveRequestBody` struct in `services/plate-solver/src/api.rs`
/// field-for-field. All hint fields are optional; omitted fields
/// produce no flag and the wrapper falls through to ASTAP's
/// blind-solve behavior.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SolveRequest {
    /// Absolute path to the FITS file. The wrapper rejects relative
    /// paths with `invalid_request`.
    pub fits_path: String,
    /// Pointing-hint right ascension in decimal **degrees** (the
    /// wrapper converts to ASTAP's hours internally). `None` ⇒
    /// omit field entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ra_hint: Option<f64>,
    /// Pointing-hint declination in decimal degrees. `None` ⇒ omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dec_hint: Option<f64>,
    /// Field-of-view hint in decimal degrees. `None` ⇒ omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fov_hint_deg: Option<f64>,
    /// Search-radius hint in decimal degrees. `None` ⇒ omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_radius_deg: Option<f64>,
    /// Wall-clock deadline applied by the wrapper. `None` ⇒ wrapper
    /// applies its own `default_solve_timeout`.
    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

/// Solve response body. Matches the wrapper's `SolveResponseBody`
/// in `services/plate-solver/src/api.rs`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SolveOutcome {
    /// Image-center right ascension in decimal degrees.
    pub ra_center: f64,
    /// Image-center declination in decimal degrees.
    pub dec_center: f64,
    /// Pixel scale from `|CDELT1|` in arcseconds per pixel.
    pub pixel_scale_arcsec: f64,
    /// Field rotation from `CROTA2` in degrees.
    pub rotation_deg: f64,
    /// Solver banner returned by the wrapper (e.g.
    /// `"astap-2026.05.03"`).
    pub solver: String,
    /// Full CRPIX + CD-matrix mapping. `None` when the wrapper's
    /// `.wcs` sidecar lacked a complete six-key set (the wrapper
    /// never synthesizes one from CDELT/CROTA2). `#[serde(default)]`
    /// tolerates wrappers that predate the field.
    #[serde(default)]
    pub wcs_matrix: Option<WcsMatrix>,
}

/// Full WCS linear mapping. Matches the wrapper's `WcsMatrix` in
/// `services/plate-solver/src/api.rs` (nested under `wcs_matrix` in
/// `SolveResponseBody`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct WcsMatrix {
    /// Reference pixel x in the FITS 1-based pixel convention.
    pub crpix1: f64,
    /// Reference pixel y in the FITS 1-based pixel convention.
    pub crpix2: f64,
    /// CD matrix element (1,1) in degrees per pixel.
    pub cd1_1: f64,
    /// CD matrix element (1,2) in degrees per pixel.
    pub cd1_2: f64,
    /// CD matrix element (2,1) in degrees per pixel.
    pub cd2_1: f64,
    /// CD matrix element (2,2) in degrees per pixel.
    pub cd2_2: f64,
}

/// Structured error envelope returned by the wrapper (matches
/// `ErrorResponse` in `services/plate-solver/src/error.rs`).
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
/// - `Wrapper { code, message, details }` — the wrapper returned a
///   structured error envelope (`invalid_request`, `fits_not_found`,
///   `solve_failed`, `solve_timeout`, `internal`). Propagate
///   verbatim.
/// - `Internal` — a wrapper-shaped HTTP error that didn't parse as
///   the structured envelope, or a Rust-side bug (rare).
#[derive(Debug, Error)]
pub enum SolveError {
    #[error("service unreachable: {0}")]
    ServiceUnreachable(String),

    #[error("{code}: {message}")]
    Wrapper {
        code: String,
        message: String,
        details: serde_json::Value,
    },

    #[error("internal: {0}")]
    Internal(String),
}

/// HTTP client to the `plate-solver` service.
#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
#[async_trait]
pub trait PlateSolveClient: Send + Sync {
    /// Issue `POST /api/v1/solve` with the given request and return
    /// the parsed solution.
    async fn solve(&self, request: SolveRequest) -> Result<SolveOutcome, SolveError>;
}

/// Concrete reqwest-backed implementation of [`PlateSolveClient`].
#[derive(Debug, Clone)]
pub struct PlateSolverClient {
    client: reqwest::Client,
    base_url: String,
}

impl PlateSolverClient {
    /// Build a client against the given base URL with an HTTP-client
    /// outer timeout. The outer timeout is the connection-side
    /// backstop for Tenet 1; per-request `SolveRequest::timeout`
    /// bounds the *solver* side independently.
    ///
    /// `auth` sends HTTP Basic Auth credentials on every request, for
    /// an auth-enabled plate-solver service (issue #620).
    ///
    /// `ca_cert_path` is the observatory CA (rp.md §Configuration):
    /// without it, an `https://` `base_url` signed by that CA fails
    /// certificate verification (issue #609).
    pub fn new(
        base_url: String,
        http_timeout: Duration,
        auth: Option<&ClientAuthConfig>,
        ca_cert_path: Option<&std::path::Path>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut builder =
            rusty_photon_tls::client::client_builder(ca_cert_path)?.timeout(http_timeout);
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
        })
    }

    /// The base URL the client was constructed with (no trailing
    /// slash). Useful for tests that want to log or assert.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

fn trim_trailing_slash(s: String) -> String {
    if let Some(stripped) = s.strip_suffix('/') {
        stripped.to_string()
    } else {
        s
    }
}

#[async_trait]
impl PlateSolveClient for PlateSolverClient {
    async fn solve(&self, request: SolveRequest) -> Result<SolveOutcome, SolveError> {
        let url = format!("{}/api/v1/solve", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| SolveError::ServiceUnreachable(e.to_string()))?;

        if response.status().is_success() {
            response
                .json::<SolveOutcome>()
                .await
                .map_err(|e| SolveError::Internal(format!("failed to parse success body: {e}")))
        } else {
            let status = response.status();
            // Try the structured envelope first; fall back to
            // Internal so a wrapper bug returning malformed JSON on
            // a 5xx isn't lost.
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<unreadable response body: {e}>"));
            match serde_json::from_str::<ErrorResponseBody>(&body) {
                Ok(env) => Err(SolveError::Wrapper {
                    code: env.error,
                    message: env.message,
                    details: env.details,
                }),
                Err(parse_err) => Err(SolveError::Internal(format!(
                    "wrapper returned status {status} with non-envelope body: {parse_err}; raw: {body}"
                ))),
            }
        }
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
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;
    use tokio::sync::RwLock;

    /// Test-side stub: spawn axum on `127.0.0.1:0`, configurable
    /// success / error behavior, captures every request body.
    struct TestStub {
        url: String,
        requests: Arc<RwLock<Vec<Value>>>,
        _shutdown_tx: tokio::sync::oneshot::Sender<()>,
    }

    enum StubBehavior {
        Success(SolveOutcome),
        Error { status: StatusCode, body: Value },
        SuccessMalformed,
        ErrorMalformed,
    }

    #[derive(Clone)]
    struct StubState {
        behavior: Arc<StubBehavior>,
        requests: Arc<RwLock<Vec<Value>>>,
    }

    async fn spawn_stub(behavior: StubBehavior) -> TestStub {
        let requests: Arc<RwLock<Vec<Value>>> = Arc::new(RwLock::new(Vec::new()));
        let state = StubState {
            behavior: Arc::new(behavior),
            requests: requests.clone(),
        };

        let app = Router::new()
            .route("/api/v1/solve", post(handler))
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

    async fn handler(
        State(state): State<StubState>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        state.requests.write().await.push(body);
        match &*state.behavior {
            StubBehavior::Success(outcome) => Json(outcome.clone()).into_response(),
            StubBehavior::Error { status, body } => (*status, Json(body.clone())).into_response(),
            StubBehavior::SuccessMalformed => (StatusCode::OK, "not json").into_response(),
            StubBehavior::ErrorMalformed => {
                (StatusCode::INTERNAL_SERVER_ERROR, "not json").into_response()
            }
        }
    }

    fn ok_outcome() -> SolveOutcome {
        SolveOutcome {
            ra_center: 10.6848,
            dec_center: 41.269,
            pixel_scale_arcsec: 1.05,
            rotation_deg: 12.3,
            solver: "stub-astap".to_string(),
            wcs_matrix: None,
        }
    }

    #[tokio::test]
    async fn solve_happy_path_returns_outcome() {
        let stub = spawn_stub(StubBehavior::Success(ok_outcome())).await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();

        let outcome = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(outcome.ra_center, 10.6848);
        assert_eq!(outcome.dec_center, 41.269);
        assert_eq!(outcome.pixel_scale_arcsec, 1.05);
        assert_eq!(outcome.rotation_deg, 12.3);
        assert_eq!(outcome.solver, "stub-astap");
        assert_eq!(outcome.wcs_matrix, None);
    }

    #[tokio::test]
    async fn solve_parses_populated_wcs_matrix() {
        let stub = spawn_stub(StubBehavior::Success(SolveOutcome {
            wcs_matrix: Some(WcsMatrix {
                crpix1: 512.0,
                crpix2: 384.0,
                cd1_1: -2.91e-4,
                cd1_2: 1.2e-6,
                cd2_1: 1.1e-6,
                cd2_2: 2.91e-4,
            }),
            ..ok_outcome()
        }))
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();

        let outcome = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let matrix = outcome.wcs_matrix.unwrap();
        assert_eq!(matrix.crpix1, 512.0);
        assert_eq!(matrix.crpix2, 384.0);
        assert_eq!(matrix.cd1_1, -2.91e-4);
        assert_eq!(matrix.cd1_2, 1.2e-6);
        assert_eq!(matrix.cd2_1, 1.1e-6);
        assert_eq!(matrix.cd2_2, 2.91e-4);
    }

    #[test]
    fn outcome_without_wcs_matrix_key_deserializes_as_none() {
        // A wrapper predating the field omits the key entirely; the
        // `#[serde(default)]` keeps the client compatible.
        let outcome: SolveOutcome = serde_json::from_str(
            r#"{
                "ra_center": 10.6848,
                "dec_center": 41.269,
                "pixel_scale_arcsec": 1.05,
                "rotation_deg": 12.3,
                "solver": "astap-cli"
            }"#,
        )
        .unwrap();
        assert_eq!(outcome.wcs_matrix, None);
    }

    #[tokio::test]
    async fn solve_forwards_request_fields_verbatim() {
        let stub = spawn_stub(StubBehavior::Success(ok_outcome())).await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();

        client
            .solve(SolveRequest {
                fits_path: "/tmp/m31.fits".to_string(),
                ra_hint: Some(160.272),
                dec_hint: Some(41.269),
                fov_hint_deg: Some(1.5),
                search_radius_deg: Some(2.0),
                timeout: Some(Duration::from_secs(45)),
            })
            .await
            .unwrap();

        let req = stub.requests.read().await[0].clone();
        assert_eq!(req["fits_path"], "/tmp/m31.fits");
        assert_eq!(req["ra_hint"], 160.272);
        assert_eq!(req["dec_hint"], 41.269);
        assert_eq!(req["fov_hint_deg"], 1.5);
        assert_eq!(req["search_radius_deg"], 2.0);
        assert_eq!(req["timeout"], "45s");
    }

    #[tokio::test]
    async fn solve_omits_unset_optional_fields() {
        let stub = spawn_stub(StubBehavior::Success(ok_outcome())).await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();

        client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let req = stub.requests.read().await[0].clone();
        for field in [
            "ra_hint",
            "dec_hint",
            "fov_hint_deg",
            "search_radius_deg",
            "timeout",
        ] {
            assert!(
                req.get(field).is_none(),
                "expected '{field}' to be absent on a blind request, got: {:?}",
                req.get(field)
            );
        }
    }

    fn err_envelope(code: &str, message: &str) -> Value {
        serde_json::json!({
            "error": code,
            "message": message,
        })
    }

    #[tokio::test]
    async fn solve_propagates_invalid_request_error() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::BAD_REQUEST,
            body: err_envelope("invalid_request", "malformed body"),
        })
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        match err {
            SolveError::Wrapper { code, message, .. } => {
                assert_eq!(code, "invalid_request");
                assert_eq!(message, "malformed body");
            }
            other => panic!("expected Wrapper error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn solve_propagates_fits_not_found_error() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::NOT_FOUND,
            body: err_envelope("fits_not_found", "/tmp/missing.fits: No such file"),
        })
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/missing.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SolveError::Wrapper { code, .. } if code == "fits_not_found"));
    }

    #[tokio::test]
    async fn solve_propagates_solve_failed_error() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: err_envelope("solve_failed", "ASTAP exited with code 1"),
        })
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SolveError::Wrapper { code, .. } if code == "solve_failed"));
    }

    #[tokio::test]
    async fn solve_propagates_solve_timeout_error() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::GATEWAY_TIMEOUT,
            body: err_envelope("solve_timeout", "wall-clock deadline expired"),
        })
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SolveError::Wrapper { code, .. } if code == "solve_timeout"));
    }

    #[tokio::test]
    async fn solve_propagates_internal_error_envelope() {
        let stub = spawn_stub(StubBehavior::Error {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: err_envelope("internal", "broken pipe"),
        })
        .await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        match err {
            SolveError::Wrapper { code, message, .. } => {
                assert_eq!(code, "internal");
                assert_eq!(message, "broken pipe");
            }
            other => panic!("expected Wrapper error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn solve_returns_internal_when_success_body_malformed() {
        let stub = spawn_stub(StubBehavior::SuccessMalformed).await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SolveError::Internal(_)));
    }

    #[tokio::test]
    async fn solve_returns_internal_when_error_body_not_envelope() {
        let stub = spawn_stub(StubBehavior::ErrorMalformed).await;
        let client =
            PlateSolverClient::new(stub.url.clone(), Duration::from_secs(5), None, None).unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        match err {
            SolveError::Internal(msg) => {
                assert!(msg.contains("non-envelope body"), "got: {msg}");
            }
            other => panic!("expected Internal error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn solve_surfaces_connection_refused_as_service_unreachable() {
        // Port 1 is reserved and reliably refuses connections on
        // dev hosts and CI runners (same pattern auto_focus uses
        // for the unreachable-focuser scenario).
        let client = PlateSolverClient::new(
            "http://127.0.0.1:1".to_string(),
            Duration::from_secs(2),
            None,
            None,
        )
        .unwrap();
        let err = client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SolveError::ServiceUnreachable(_)));
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let client = PlateSolverClient::new(
            "http://localhost:11131/".to_string(),
            Duration::from_secs(5),
            None,
            None,
        )
        .unwrap();
        assert_eq!(client.base_url(), "http://localhost:11131");
    }

    #[test]
    fn new_with_missing_ca_cert_fails() {
        // Proves `ca_cert_path` is actually wired to the CA-trust
        // builder, not silently dropped (issue #609).
        let result = PlateSolverClient::new(
            "http://localhost:11131".to_string(),
            Duration::from_secs(5),
            None,
            Some(std::path::Path::new("/nonexistent/ca.pem")),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn new_with_auth_sends_basic_auth_header() {
        use axum::http::HeaderMap;

        let captured: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let captured_for_handler = captured.clone();
        let app = Router::new().route(
            "/api/v1/solve",
            post(move |headers: HeaderMap, Json(_): Json<Value>| {
                let captured = captured_for_handler.clone();
                async move {
                    let auth_header = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    *captured.write().await = auth_header;
                    Json(ok_outcome()).into_response()
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
        let client = PlateSolverClient::new(
            format!("http://127.0.0.1:{port}"),
            Duration::from_secs(5),
            Some(&auth),
            None,
        )
        .unwrap();
        client
            .solve(SolveRequest {
                fits_path: "/tmp/x.fits".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let header = captured.read().await.clone();
        let expected = format!("Basic {}", BASE64.encode("observatory:secret"));
        assert_eq!(header, Some(expected));
        shutdown_tx.send(()).ok();
    }

    // ----- credential redaction in Debug output --------------------------
    //
    // `PlateSolverClient` derives `Debug`, and reqwest's `Client` Debug impl
    // prints `default_headers` unconditionally — without `set_sensitive`,
    // an incidental `{:?}`/`debug!()` of this client would leak the
    // password (the header's base64 encoding is trivially reversible).
    #[test]
    fn debug_format_never_leaks_the_auth_header() {
        let auth = ClientAuthConfig {
            username: "observatory".to_string(),
            password: "s3cret-pw".to_string(),
        };
        let client = PlateSolverClient::new(
            "http://localhost:11131".to_string(),
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
}

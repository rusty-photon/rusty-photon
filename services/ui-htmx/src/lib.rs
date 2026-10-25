#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `ui-htmx` — the server-rendered web UI (BFF) over rusty-photon's running
//! system.
//!
//! A client of rp (not part of `rp`): the activity stream, rp's event stream
//! rendered server-side and pushed over SSE, and the targets inbox over rp's
//! MCP-only target tools, with axum + Maud + HTMX. Configuration is edited on
//! each service's own setup page: the navigation links rp's, and the BFF's
//! own is [`setup`]. See
//! [`docs/services/ui-htmx.md`](../../../docs/services/ui-htmx.md).

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
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

pub mod assets;
pub mod config;
pub mod doctor;
/// Test-only `/fixtures/*` routes (UI-testing plan §9 Tier 1) — compiled ONLY
/// under the `test-fixtures` cargo feature, so they ship nothing in the real
/// binary.
///
/// `#[coverage(off)]` keeps this test-only code out of the coverage numbers
/// even when the feature is on (e.g. the `--all-features` coverage build).
#[cfg(feature = "test-fixtures")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub mod fixtures;
pub mod io;
pub mod pages;
pub mod rp_client;
pub mod setup;
/// Test-only Server-Sent-Events fixture routes (UI-testing plan §9 Tier 2) —
/// compiled ONLY under the `test-sse` cargo feature, so they ship nothing in the
/// real binary.
///
/// `#[coverage(off)]` keeps this test-only code (and the streaming
/// `async-stream` machinery it uses) out of the coverage numbers even when the
/// feature is on.
#[cfg(feature = "test-sse")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub mod sse_fixtures;
pub mod sse_proxy;
pub mod targets_client;

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::Router;
use rusty_photon_setup::origin::{browser_host, link_for};

pub use config::{load_config, Config};
pub use io::{HttpClient, ReqwestHttpClient};

/// State for the rp-backed surfaces (`/stream`, `/targets`, and the
/// navigation's link to rp's setup page).
pub struct RpState {
    /// rp's MCP-only target tools (the targets inbox) — per-request
    /// sessions through `rp-mcp-client`, same `rp` block credentials.
    pub(crate) targets: targets_client::SharedTargetsClient,
    /// rp's REST surface (equipment status).
    pub(crate) api: Arc<dyn rp_client::RpApi>,
    /// rp's base URL, without a trailing slash: the SSE proxy's subscribe
    /// URL, error banners, and the link to rp's setup page.
    pub(crate) base_url: String,
    /// A raw `reqwest` client for the SSE proxy's long-lived streaming GET —
    /// the buffered [`HttpClient`] seam can't stream. CA-trusting; `stream_auth`
    /// carries the Basic credentials to present.
    pub(crate) stream_client: reqwest::Client,
    pub(crate) stream_auth: Option<(String, String)>,
}

impl RpState {
    /// The link to rp's setup page for a browser that sent `headers`: rp's
    /// own address, its host replaced by the one the browser reached the BFF
    /// by when rp's is a loopback address (the single-box rig's
    /// `http://127.0.0.1:11115`, which a browser elsewhere would follow to
    /// itself).
    pub(crate) fn setup_link(&self, headers: &HeaderMap) -> String {
        link_for(
            &format!("{}/setup", self.base_url),
            browser_host(headers).as_deref(),
        )
    }

    fn with_targets(&self, targets: targets_client::SharedTargetsClient) -> Self {
        Self {
            targets,
            api: Arc::clone(&self.api),
            base_url: self.base_url.clone(),
            stream_client: self.stream_client.clone(),
            stream_auth: self.stream_auth.clone(),
        }
    }
}

/// Shared handler state: the rp-backed surface state, the BFF's own setup
/// page when `main` attached one, and the token that ends open SSE streams.
#[derive(Clone)]
pub struct AppState {
    rp: Arc<RpState>,
    setup: Option<setup::Pages>,
    /// Ends open SSE proxy streams on service shutdown — axum's graceful
    /// shutdown does not close them on its own (axum #2673); `main` links this
    /// to the `ServiceRunner` shutdown (the same pattern as rp's `sse_shutdown`).
    sse_shutdown: tokio_util::sync::CancellationToken,
}

impl AppState {
    /// The rp-backed surface state.
    pub(crate) const fn rp(&self) -> &Arc<RpState> {
        &self.rp
    }

    /// The token that ends open SSE streams on shutdown.
    pub(crate) const fn sse_shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.sse_shutdown
    }

    /// Link the SSE shutdown token to the service lifecycle (called by `main`;
    /// the constructors default to a fresh token so tests need no wiring).
    #[must_use]
    pub fn with_sse_shutdown(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.sse_shutdown = token;
        self
    }

    /// Serve the BFF's own setup page at `/setup` (called by `main`, which
    /// knows the configuration file's path).
    #[must_use]
    pub fn with_setup_pages(mut self, pages: setup::Pages) -> Self {
        self.setup = Some(pages);
        self
    }
}

/// Validate a configured base URL and build the `reqwest`-backed `HttpClient`
/// for it (CA trust + optional Basic auth). Rejects credentials embedded in the
/// URL (`http://user:pass@host`) — they would otherwise leak into error messages
/// (rendered in the page) and debug logs that echo the request URL; credentials
/// belong in the `auth` field, sent as a redactable `Authorization` header.
fn build_http_client(
    what: &str,
    base_url: &str,
    auth: Option<&config::DriverAuth>,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn HttpClient>, Box<dyn std::error::Error + Send + Sync>> {
    // Deliberately no `{base_url}` echo: a malformed URL can carry embedded
    // credentials, and this message reaches logs. `what` names the target;
    // the parse error says why; the operator has the URL in their own config.
    let parsed =
        reqwest::Url::parse(base_url).map_err(|e| format!("invalid base_url for {what}: {e}"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "{what} base_url must not contain credentials \
             (user:pass@…); put them in the `auth` field instead"
        )
        .into());
    }
    let reqwest_client = match auth {
        Some(auth) => ReqwestHttpClient::with_auth(
            ca_cert_path,
            auth.username.clone(),
            auth.password.clone(),
        )?,
        None => ReqwestHttpClient::new(ca_cert_path)?,
    };
    Ok(Arc::new(reqwest_client))
}

impl AppState {
    /// Build the production state from the BFF's `rp` block.
    ///
    /// # Errors
    ///
    /// Returns an error if rp's HTTP client cannot be built (credentials in
    /// its URL, a bad CA path or PEM).
    pub fn from_config(config: &Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let rp = &config.rp;
        let http = build_http_client(
            "rp",
            &rp.base_url,
            rp.auth.as_ref(),
            rp.ca_cert_path.as_deref(),
        )?;
        let base_url = rp.base_url.trim_end_matches('/').to_string();
        let rp_state = RpState {
            api: Arc::new(rp_client::RestRpApi::new(http, &base_url)),
            targets: Arc::new(targets_client::McpTargetsClient::new(
                &base_url,
                rp.auth.as_ref(),
                rp.ca_cert_path.clone(),
            )),
            base_url,
            stream_client: rusty_photon_tls::client::build_reqwest_client(
                rp.ca_cert_path.as_deref(),
            )
            .map_err(|e| format!("rp target: failed to build stream client: {e}"))?,
            stream_auth: rp
                .auth
                .as_ref()
                .map(|a| (a.username.clone(), a.password.clone())),
        };
        Ok(Self {
            rp: Arc::new(rp_state),
            setup: None,
            sse_shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }

    /// Build state over an explicit rp REST surface (tests inject a stub for
    /// the stream handlers). The targets client defaults to an
    /// always-unavailable stub; inject one with
    /// [`with_targets_client`](Self::with_targets_client).
    pub fn with_rp_api(api: Arc<dyn rp_client::RpApi>) -> Self {
        Self {
            rp: Arc::new(RpState {
                targets: Arc::new(targets_client::UnwiredTargets),
                api,
                base_url: "http://rp.test".to_string(),
                stream_client: reqwest::Client::new(),
                stream_auth: None,
            }),
            setup: None,
            sse_shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Swap the targets-tools client on the test state (targets-page unit
    /// tests inject a stub for states the end-to-end suite can't produce —
    /// the gated/down card, malformed responses).
    #[must_use]
    pub fn with_targets_client(mut self, targets: Arc<dyn targets_client::TargetsClient>) -> Self {
        self.rp = Arc::new(self.rp.with_targets(targets));
        self
    }

    /// Point the test state's SSE proxy at an explicit rp base URL (unit tests
    /// stub rp's subscribe endpoint with an in-test axum server — ADR-004).
    #[must_use]
    pub fn with_rp_base_url(mut self, base_url: &str) -> Self {
        let mut rp = self.rp.with_targets(Arc::clone(&self.rp.targets));
        rp.base_url = base_url.trim_end_matches('/').to_string();
        self.rp = Arc::new(rp);
        self
    }
}

/// Build the BFF axum router.
pub fn build_router(state: AppState) -> Router {
    let setup = state.setup.clone();
    let router = Router::new()
        .route("/", get(index))
        .route("/targets", get(pages::targets::page))
        .route("/targets/goal-row", get(pages::targets::goal_row_fragment))
        .route(
            "/targets/{slug}",
            get(pages::targets::review).post(pages::targets::save),
        )
        .route("/targets/{slug}/active", post(pages::targets::set_active))
        .route("/targets/{slug}/delete", post(pages::targets::delete))
        .route("/stream", get(pages::stream::page))
        .route("/stream/events", get(sse_proxy::events))
        .route("/stream/equipment", get(pages::stream::equipment_fragment))
        .route("/health", get(health))
        .route("/assets/app.css", get(assets::app_css))
        .route("/assets/htmx.min.js", get(assets::htmx_js))
        .route("/assets/htmx-ext-sse.js", get(assets::htmx_sse_js));
    // Test-only `/fixtures/*` routes, present only when the `test-fixtures`
    // feature is on (the BDD suite's binary). This `let` shadow is the standard
    // cfg-gated router-extend; the merge runs at startup so it stays covered.
    #[cfg(feature = "test-fixtures")]
    let router = router.merge(fixtures::routes());
    // Test-only SSE fixture routes (plan §9 Tier 2), present only with `test-sse`.
    #[cfg(feature = "test-sse")]
    let router = router.merge(sse_fixtures::routes());
    let router = router.with_state(state);
    match setup {
        Some(pages) => router.merge(setup::router(pages)),
        None => router,
    }
}

/// `/` — the activity stream is the BFF's front page.
async fn index() -> Redirect {
    Redirect::to("/stream")
}

async fn health() -> &'static str {
    "OK"
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn from_config_trims_rps_address() {
        let json = r#"{ "rp": { "base_url": "http://127.0.0.1:11115/" } }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let state = AppState::from_config(&config).unwrap();
        assert_eq!(state.rp().base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn from_config_rejects_rp_url_credentials() {
        let json = r#"{ "rp": { "base_url": "http://obs:secret@127.0.0.1:11115" } }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        match AppState::from_config(&config) {
            Ok(_) => panic!("expected from_config to reject credentials in the rp base_url"),
            Err(e) => assert!(
                e.to_string().contains("must not contain credentials"),
                "{e}"
            ),
        }
    }

    fn rp_at(base_url: &str) -> AppState {
        AppState::with_rp_api(Arc::new(rp_client::MockRpApi::new())).with_rp_base_url(base_url)
    }

    fn reached_as(host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        headers
    }

    #[test]
    fn rps_setup_link_on_loopback_follows_the_host_the_browser_used() {
        let state = rp_at("https://127.0.0.1:11115");
        assert_eq!(
            state.rp().setup_link(&reached_as("rig.local:11120")),
            "https://rig.local:11115/setup"
        );
    }

    #[test]
    fn rps_setup_link_elsewhere_is_used_as_written() {
        let state = rp_at("http://pier1.example:11115");
        assert_eq!(
            state.rp().setup_link(&reached_as("laptop.local:11120")),
            "http://pier1.example:11115/setup"
        );
    }

    #[tokio::test]
    async fn the_front_page_is_the_activity_stream() {
        let response = build_router(rp_at("http://127.0.0.1:11115"))
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/stream");
    }

    #[tokio::test]
    async fn setup_is_served_only_once_the_pages_are_attached() {
        let bare = build_router(rp_at("http://127.0.0.1:11115"))
            .oneshot(Request::get("/setup").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(bare.status(), StatusCode::NOT_FOUND);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-htmx.json");
        let config = Config::default();
        std::fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
        let pages = setup::pages(&config, path, config::CliOverrides::default());
        let served = build_router(rp_at("http://127.0.0.1:11115").with_setup_pages(pages))
            .oneshot(
                Request::get("/setup")
                    .header(header::HOST, "127.0.0.1:11120")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
    }
}

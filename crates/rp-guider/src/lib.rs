#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! HTTP client for the guider rp-managed service.
//!
//! Wraps the guider's HTTP contract (`docs/services/guider.md` § HTTP
//! API) behind a small, mockable Rust API: the guide run
//! (`guiding/start`, `guiding/stop`, `guiding/pause`,
//! `guiding/resume`, `dither`, `dither/abort`), the reads
//! (`guiding/stats`, `guiding/metrics`, `devices`), the calibration
//! store's clear, the star re-selection and the camera lease. `rp`
//! uses it for its guiding MCP tools, for the safety enforcer's
//! stop-guiding-on-unsafe step, and for what it consumes of the guider
//! internally (rp.md § Guider Service).
//!
//! Wire types are defined here, not pulled from the guider's crate:
//! the inter-service contract is HTTP, not in-process Rust types
//! (Tenet 4: remote interfaces only). This module is a thin transport
//! and holds the part of the contract `rp` uses. Positional quantities
//! are **guide-camera pixels** (`*_px`, `settle.pixels`) or arcseconds
//! (`*_arcsec`), as their names say.

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

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rp_auth::config::ClientAuthConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Head-room on a settle `timeout` in a settle-waiting request's deadline.
///
/// The guider answers `settle_timeout` when its own settle deadline
/// passes, and the margin lets that answer arrive before the transport
/// deadline cuts the connection (rp.md § Guider Service, Deadlines).
pub const SETTLE_MARGIN: Duration = Duration::from_secs(15);

/// The deadline of a `dither`: the settle timeout the request carries
/// plus [`SETTLE_MARGIN`]. A request with no settle timeout runs under
/// the guider's own, which the client cannot see, and gets `timeout`.
#[must_use]
pub fn dither_deadline(timeout: Duration, settle_timeout: Option<Duration>) -> Duration {
    settle_timeout.map_or(timeout, |t| t.saturating_add(SETTLE_MARGIN))
}

/// The deadline of a `guiding/start`: `calibration_timeout`, since a
/// start calibrates when the guider holds no calibration it can use,
/// plus what a [`dither_deadline`] allows for the settle.
#[must_use]
pub fn start_deadline(
    timeout: Duration,
    calibration_timeout: Duration,
    settle_timeout: Option<Duration>,
) -> Duration {
    calibration_timeout.saturating_add(dither_deadline(timeout, settle_timeout))
}

/// The guider's state (guider.md § States).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuiderState {
    Idle,
    Looping,
    Calibrating,
    Guiding,
    Paused,
    StarLost,
}

impl GuiderState {
    /// The state's name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Looping => "looping",
            Self::Calibrating => "calibrating",
            Self::Guiding => "guiding",
            Self::Paused => "paused",
            Self::StarLost => "star_lost",
        }
    }

    /// `true` in the run states: `calibrating`, `guiding`, `paused`
    /// and `star_lost`.
    #[must_use]
    pub const fn is_run(self) -> bool {
        !matches!(self, Self::Idle | Self::Looping)
    }
}

impl fmt::Display for GuiderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Partial settle override forwarded to the guider. Omitted fields
/// fall back to the *guider's* configured `settling` block, field by
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
    /// The guider's deadline for the settle to complete.
    #[serde(default, with = "humantime_serde::option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

impl SettleOverride {
    /// `true` when every field is unset — the caller should send no
    /// `settle` object at all so the guider's defaults apply.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pixels.is_none() && self.time.is_none() && self.timeout.is_none()
    }
}

/// Request body for `POST /api/v1/guiding/start`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StartGuidingRequest {
    /// Calibrate before guiding starts, whatever the calibration store
    /// holds.
    pub recalibrate: bool,
    /// Per-call settle override; `None` ⇒ the guider's configured
    /// `settling` defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle: Option<SettleOverride>,
}

/// The size of a dither. The guider takes exactly one of the two
/// fields, and converts arcseconds with its own pixel scale.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum DitherAmount {
    /// Guide-camera pixels, sent as `amount_px`.
    #[serde(rename = "amount_px")]
    Px(f64),
    /// Arcseconds on the sky, sent as `amount_arcsec`.
    #[serde(rename = "amount_arcsec")]
    Arcsec(f64),
}

/// Request body for `POST /api/v1/dither`.
#[derive(Debug, Clone, Serialize)]
pub struct DitherRequest {
    #[serde(flatten)]
    pub amount: DitherAmount,
    /// Restrict the dither to the RA axis.
    pub ra_only: bool,
    /// Per-call settle override; `None` ⇒ the guider's configured
    /// `settling` defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle: Option<SettleOverride>,
}

/// Success body of `guiding/start` and `dither`: guiding once settled.
///
/// The RMS fields are `None` while `sample_count` is 0, and the Dec
/// and total ones with the guider's Dec axis off (guider.md
/// § Statistics).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettledOutcome {
    pub state: GuiderState,
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    pub rms_ra_arcsec: Option<f64>,
    pub rms_dec_arcsec: Option<f64>,
    pub total_rms_arcsec: Option<f64>,
    /// Guide steps in the guider's RMS window.
    pub sample_count: u32,
}

/// The latest stop the guider reports in its stats, a requested one
/// included.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LastStop {
    /// `requested`, or one of the reasons the guider stops for by
    /// itself (guider.md § Stopping on its own).
    pub reason: String,
    /// When it stopped, as an RFC 3339 timestamp.
    pub at: String,
    pub message: String,
}

/// Success body of `GET /api/v1/guiding/stats`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuidingStats {
    pub state: GuiderState,
    /// `true` only in the state `guiding`.
    pub guiding: bool,
    /// `true` while a start or a dither waits for its settle.
    pub settling: bool,
    pub pixel_scale_arcsec: Option<f64>,
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    pub rms_ra_arcsec: Option<f64>,
    pub rms_dec_arcsec: Option<f64>,
    pub total_rms_arcsec: Option<f64>,
    /// The latest measured frame's; `None` when no frame has been
    /// measured since the loop started. The same holds for
    /// `star_mass` and `star_count`.
    pub snr: Option<f64>,
    pub star_mass: Option<f64>,
    pub star_count: Option<u32>,
    pub sample_count: u32,
    /// `None` until something has stopped since the guider started.
    pub last_stop: Option<LastStop>,
    pub totals: GuidingTotals,
}

#[cfg(any(test, feature = "mock"))]
impl GuidingStats {
    /// A reading in `state` with nothing measured, nothing stopped and
    /// nothing counted, for a test to fill in.
    #[must_use]
    pub fn in_state(state: GuiderState) -> Self {
        Self {
            state,
            guiding: state == GuiderState::Guiding,
            settling: false,
            pixel_scale_arcsec: None,
            rms_ra_px: None,
            rms_dec_px: None,
            total_rms_px: None,
            rms_ra_arcsec: None,
            rms_dec_arcsec: None,
            total_rms_arcsec: None,
            snr: None,
            star_mass: None,
            star_count: None,
            sample_count: 0,
            last_stop: None,
            totals: GuidingTotals {
                epoch: "test-epoch".to_string(),
                guide_steps: 0,
                ra_samples: 0,
                ra_sum_sq_px2: 0.0,
                dec_samples: 0,
                dec_sum_sq_px2: 0.0,
                star_lost: 0,
                interruptions: 0,
            },
        }
    }
}

/// The guider's running totals (guider.md § Statistics): what it has
/// counted since it started, never reset. Two readings under the same
/// `epoch` subtract to the guiding between them.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GuidingTotals {
    /// Names the run of the service; a restart draws a new one.
    pub epoch: String,
    pub guide_steps: u64,
    /// Steps with an RA sample, and the sum of the square of the RA
    /// error in px².
    pub ra_samples: u64,
    pub ra_sum_sq_px2: f64,
    /// The same for Dec.
    pub dec_samples: u64,
    pub dec_sum_sq_px2: f64,
    /// Star-lost frames of runs.
    pub star_lost: u64,
    /// Each time a run stops, is paused, calibrates again or selects
    /// its stars again.
    pub interruptions: u64,
}

/// One entry of `GET /api/v1/guiding/metrics`: a measured frame's star
/// metrics, or a star-lost marker (`star_lost: true`, no HFD).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FrameMetrics {
    /// Counts the frames measured since the guider started. Under one
    /// `epoch` it only grows, so a reader keeps the entries above the
    /// highest number it has seen.
    pub frame: u64,
    /// Half-flux diameter in guide-camera pixels.
    pub hfd: Option<f64>,
    pub snr: Option<f64>,
    pub star_mass: Option<f64>,
    pub star_count: u32,
    pub star_lost: bool,
}

/// Success body of `GET /api/v1/guiding/metrics`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuidingMetrics {
    pub state: GuiderState,
    /// `true` only in the state `guiding`.
    pub guiding: bool,
    /// Names the run of the service, as in [`GuidingTotals`]. Under a
    /// new one the frame count has started again.
    pub epoch: String,
    /// The last 50 measured frames, oldest first.
    pub frames: Vec<FrameMetrics>,
}

/// What `GET /api/v1/devices` reports of one device, as far as `rp`
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GuiderDevice {
    /// The `UniqueID` the device's server listed for it; `None` until
    /// a device poll of the guider's has read the list.
    pub unique_id: Option<String>,
}

/// Success body of `GET /api/v1/devices`. A device the guider is not
/// configured with is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GuiderDevices {
    pub camera: Option<GuiderDevice>,
    pub mount: Option<GuiderDevice>,
}

/// Success body of the endpoints `rp` reads only the state of (`stop`,
/// `pause`, `resume`, `calibration/clear`, `star/reselect`).
#[derive(Debug, Deserialize)]
struct StateOutcome {
    state: GuiderState,
}

/// Success body of `POST /api/v1/dither/abort`.
#[derive(Debug, Deserialize)]
struct AbortOutcome {
    aborted: bool,
}

/// Success body of `DELETE /api/v1/camera/lease/{token}`.
#[derive(Debug, Deserialize)]
struct ReleaseOutcome {
    released: bool,
}

/// The error envelope the guider answers with (guider.md § Error
/// envelope).
#[derive(Debug, Clone, Deserialize)]
struct ErrorResponseBody {
    error: String,
    message: String,
    #[serde(default)]
    details: serde_json::Value,
}

/// Errors surfaced by the client. Variants:
/// - `ServiceUnreachable` — connection refused, DNS failure, or no
///   answer within the request's deadline. The caller cannot
///   distinguish further.
/// - `Service { code, message, details }` — the guider answered with
///   its error envelope; `code` is one of guider.md § Error envelope.
///   Propagate verbatim.
/// - `Internal` — an answer that is neither the expected body nor the
///   envelope.
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

impl GuiderError {
    /// The envelope's code when the guider answered with one.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Service { code, .. } => Some(code),
            Self::ServiceUnreachable(_) | Self::Internal(_) => None,
        }
    }
}

/// HTTP client to the guider service.
#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
#[async_trait]
pub trait GuiderClient: Send + Sync {
    /// `POST /api/v1/guiding/start` — guide, calibrating first when
    /// needed, and wait until guiding has settled.
    async fn start_guiding(
        &self,
        request: StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError>;

    /// `POST /api/v1/guiding/stop` — stop the loop, a calibration or
    /// guiding, and wait for the loop's end. Idempotent.
    async fn stop_guiding(&self) -> Result<GuiderState, GuiderError>;

    /// `POST /api/v1/guiding/pause` — hold the corrections. `full`
    /// stops the exposures too.
    async fn pause_guiding(&self, full: bool) -> Result<GuiderState, GuiderError>;

    /// `POST /api/v1/guiding/resume` — release the corrections. The
    /// state it answers is `guiding`, or `star_lost` for a run that is
    /// in it.
    async fn resume_guiding(&self) -> Result<GuiderState, GuiderError>;

    /// `POST /api/v1/dither` — move the lock and wait until guiding
    /// has settled there.
    async fn dither(&self, request: DitherRequest) -> Result<SettledOutcome, GuiderError>;

    /// `POST /api/v1/dither/abort` — end a dither that waits to
    /// settle. `false` when none was in flight.
    async fn abort_dither(&self) -> Result<bool, GuiderError>;

    /// `GET /api/v1/guiding/stats` — state, RMS window and running
    /// totals.
    async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError>;

    /// `GET /api/v1/guiding/metrics` — the per-frame window.
    async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError>;

    /// `GET /api/v1/devices` — what the guider knows of its camera and
    /// its mount.
    async fn devices(&self) -> Result<GuiderDevices, GuiderError>;

    /// `POST /api/v1/calibration/clear` — empty the calibration store.
    /// The guider refuses it in a run state.
    async fn clear_calibration(&self) -> Result<GuiderState, GuiderError>;

    /// `POST /api/v1/star/reselect` — choose the guide stars anew on
    /// the next frame.
    async fn reselect_star(&self) -> Result<GuiderState, GuiderError>;

    /// `POST /api/v1/camera/lease` — take the camera lease under
    /// `token` for `ttl`.
    async fn acquire_camera_lease(&self, token: &str, ttl: Duration) -> Result<(), GuiderError>;

    /// `PUT /api/v1/camera/lease/{token}` — renew the lease for `ttl`.
    /// A token that holds no lease answers the code `lease_not_found`.
    async fn renew_camera_lease(&self, token: &str, ttl: Duration) -> Result<(), GuiderError>;

    /// `DELETE /api/v1/camera/lease/{token}` — release the lease.
    /// `false` when the token held none.
    async fn release_camera_lease(&self, token: &str) -> Result<bool, GuiderError>;
}

/// Concrete reqwest-backed implementation of [`GuiderClient`].
#[derive(Debug, Clone)]
pub struct GuiderServiceClient {
    client: reqwest::Client,
    base_url: String,
    timeout: Duration,
    calibration_timeout: Duration,
}

impl GuiderServiceClient {
    /// Build a client against the given base URL. `timeout` is the
    /// deadline of a request the guider answers at once (stop, pause,
    /// resume, a read, a lease call). The two that wait for a settle
    /// run under [`dither_deadline`] and [`start_deadline`], the
    /// latter with `calibration_timeout`.
    ///
    /// `auth` sends HTTP Basic Auth credentials on every request, for
    /// a guider that has its `server.auth` on.
    ///
    /// `ca_cert_path` is the observatory CA (rp.md § Configuration):
    /// without it, an `https://` `base_url` signed by that CA fails
    /// certificate verification.
    ///
    /// # Errors
    ///
    /// Returns an error if the CA certificate cannot be read or parsed,
    /// or if the HTTP client fails to build.
    pub fn new(
        base_url: String,
        timeout: Duration,
        calibration_timeout: Duration,
        auth: Option<&ClientAuthConfig>,
        ca_cert_path: Option<&std::path::Path>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // No client-level timeout: each request carries its own.
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
            calibration_timeout,
        })
    }

    /// The base URL the client was constructed with (no trailing
    /// slash). Useful for tests that want to log or assert.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// A `POST` with no body under the quick-call deadline, answered
    /// with the guider's state.
    async fn post_for_state(&self, path: &str) -> Result<GuiderState, GuiderError> {
        let url = format!("{}{path}", self.base_url);
        self.execute::<StateOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|outcome| outcome.state)
    }

    /// Send a request and parse the success body as `T`, mapping
    /// transport failures and the guider's error envelope onto
    /// [`GuiderError`].
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

/// A `ttl` as the guider reads a duration: humantime text.
fn ttl_text(ttl: Duration) -> String {
    humantime_serde::re::humantime::format_duration(ttl).to_string()
}

#[async_trait]
impl GuiderClient for GuiderServiceClient {
    async fn start_guiding(
        &self,
        request: StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError> {
        let url = format!("{}/api/v1/guiding/start", self.base_url);
        let deadline = start_deadline(
            self.timeout,
            self.calibration_timeout,
            request.settle.as_ref().and_then(|s| s.timeout),
        );
        self.execute(self.client.post(&url).timeout(deadline).json(&request))
            .await
    }

    async fn stop_guiding(&self) -> Result<GuiderState, GuiderError> {
        self.post_for_state("/api/v1/guiding/stop").await
    }

    async fn pause_guiding(&self, full: bool) -> Result<GuiderState, GuiderError> {
        let url = format!("{}/api/v1/guiding/pause", self.base_url);
        self.execute::<StateOutcome>(
            self.client
                .post(&url)
                .timeout(self.timeout)
                .json(&serde_json::json!({ "full": full })),
        )
        .await
        .map(|outcome| outcome.state)
    }

    async fn resume_guiding(&self) -> Result<GuiderState, GuiderError> {
        self.post_for_state("/api/v1/guiding/resume").await
    }

    async fn dither(&self, request: DitherRequest) -> Result<SettledOutcome, GuiderError> {
        let url = format!("{}/api/v1/dither", self.base_url);
        let deadline = dither_deadline(
            self.timeout,
            request.settle.as_ref().and_then(|s| s.timeout),
        );
        self.execute(self.client.post(&url).timeout(deadline).json(&request))
            .await
    }

    async fn abort_dither(&self) -> Result<bool, GuiderError> {
        let url = format!("{}/api/v1/dither/abort", self.base_url);
        self.execute::<AbortOutcome>(self.client.post(&url).timeout(self.timeout))
            .await
            .map(|outcome| outcome.aborted)
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

    async fn devices(&self) -> Result<GuiderDevices, GuiderError> {
        let url = format!("{}/api/v1/devices", self.base_url);
        self.execute(self.client.get(&url).timeout(self.timeout))
            .await
    }

    async fn clear_calibration(&self) -> Result<GuiderState, GuiderError> {
        self.post_for_state("/api/v1/calibration/clear").await
    }

    async fn reselect_star(&self) -> Result<GuiderState, GuiderError> {
        self.post_for_state("/api/v1/star/reselect").await
    }

    async fn acquire_camera_lease(&self, token: &str, ttl: Duration) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/camera/lease", self.base_url);
        self.execute::<serde::de::IgnoredAny>(
            self.client
                .post(&url)
                .timeout(self.timeout)
                .json(&serde_json::json!({ "token": token, "ttl": ttl_text(ttl) })),
        )
        .await
        .map(|_| ())
    }

    async fn renew_camera_lease(&self, token: &str, ttl: Duration) -> Result<(), GuiderError> {
        let url = format!("{}/api/v1/camera/lease/{token}", self.base_url);
        self.execute::<serde::de::IgnoredAny>(
            self.client
                .put(&url)
                .timeout(self.timeout)
                .json(&serde_json::json!({ "ttl": ttl_text(ttl) })),
        )
        .await
        .map(|_| ())
    }

    async fn release_camera_lease(&self, token: &str) -> Result<bool, GuiderError> {
        let url = format!("{}/api/v1/camera/lease/{token}", self.base_url);
        self.execute::<ReleaseOutcome>(self.client.delete(&url).timeout(self.timeout))
            .await
            .map(|outcome| outcome.released)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{Method, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};
    use tokio::sync::RwLock;

    const TOKEN: &str = "5f2b8c1e9a4d47c3b0e6a1d2c3f4e5a6";
    const EPOCH: &str = "0b9f6a0e-3c1d-4f6e-9a57-5d1c2e8f7a10";

    type Requests = Arc<RwLock<Vec<(Method, String, Value)>>>;

    /// Test-side stub: spawn axum on `127.0.0.1:0`, configurable
    /// success / error behavior, captures every request as
    /// `(method, path, body)`.
    struct TestStub {
        url: String,
        requests: Requests,
        _shutdown_tx: tokio::sync::oneshot::Sender<()>,
    }

    enum StubBehavior {
        /// The contract's example answer for the path asked.
        Success,
        /// This status and body, whatever the path. With status 200 it
        /// serves a success body of the test's own.
        Answer {
            status: StatusCode,
            body: Value,
        },
        SuccessMalformed,
        ErrorMalformed,
    }

    #[derive(Clone)]
    struct StubState {
        behavior: Arc<StubBehavior>,
        requests: Requests,
    }

    async fn spawn_stub(behavior: StubBehavior) -> TestStub {
        let requests: Requests = Arc::new(RwLock::new(Vec::new()));
        let state = StubState {
            behavior: Arc::new(behavior),
            requests: requests.clone(),
        };

        let app = Router::new().fallback(handler).with_state(state);

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

    fn settled_body() -> Value {
        json!({
            "state": "guiding",
            "rms_ra_px": 0.21,
            "rms_dec_px": 0.18,
            "total_rms_px": 0.28,
            "rms_ra_arcsec": 0.63,
            "rms_dec_arcsec": 0.54,
            "total_rms_arcsec": 0.83,
            "sample_count": 12,
        })
    }

    fn stats_body() -> Value {
        json!({
            "state": "guiding",
            "guiding": true,
            "settling": false,
            "pixel_scale_arcsec": 2.99,
            "rms_ra_px": 0.21,
            "rms_dec_px": 0.18,
            "total_rms_px": 0.28,
            "rms_ra_arcsec": 0.63,
            "rms_dec_arcsec": 0.54,
            "total_rms_arcsec": 0.83,
            "snr": 24.8,
            "star_mass": 5120.0,
            "star_count": 5,
            "sample_count": 50,
            "last_stop": null,
            "totals": {
                "epoch": EPOCH,
                "guide_steps": 1530,
                "ra_samples": 1530,
                "ra_sum_sq_px2": 67.5,
                "dec_samples": 1528,
                "dec_sum_sq_px2": 49.6,
                "star_lost": 2,
                "interruptions": 1,
            },
        })
    }

    /// The contract's example answer for a request (guider.md § HTTP
    /// API).
    fn success_body(method: &Method, path: &str) -> Value {
        match (method.as_str(), path) {
            ("POST", "/api/v1/guiding/start" | "/api/v1/dither") => settled_body(),
            ("POST", "/api/v1/guiding/stop" | "/api/v1/calibration/clear") => {
                json!({ "state": "idle" })
            }
            ("POST", "/api/v1/guiding/pause") => json!({ "state": "paused" }),
            ("POST", "/api/v1/guiding/resume") => json!({ "state": "guiding" }),
            ("POST", "/api/v1/dither/abort") => json!({ "state": "guiding", "aborted": true }),
            ("POST", "/api/v1/star/reselect") => json!({ "state": "guiding", "star_count": 5 }),
            ("GET", "/api/v1/guiding/stats") => stats_body(),
            ("GET", "/api/v1/guiding/metrics") => json!({
                "state": "guiding",
                "guiding": true,
                "epoch": EPOCH,
                "frames": [
                    { "frame": 41, "hfd": 2.31, "snr": 24.8, "star_mass": 5120.0, "star_count": 5, "star_lost": false },
                    { "frame": 42, "hfd": null, "snr": null, "star_mass": null, "star_count": 0, "star_lost": true },
                ],
            }),
            ("GET", "/api/v1/devices") => json!({
                "camera": {
                    "reachable": true,
                    "connected": true,
                    "unique_id": "5b0f6c1e-93a4-4f7d-8a2e-1c7d40e9b6f3",
                    "leased": false,
                    "sensor_type": "rggb",
                    "pixel_size_um": 1.45,
                    "width": 3840,
                    "height": 2192,
                    "binning": 2,
                    "can_abort_exposure": true,
                },
                "mount": {
                    "reachable": true,
                    "connected": true,
                    "unique_id": "c2d94a70-6e15-4b8c-b3f1-9a05e7d2816c",
                    "can_pulse_guide": true,
                },
                "rotator": null,
            }),
            ("POST", "/api/v1/camera/lease") | ("PUT", _) => {
                json!({ "token": TOKEN, "expires_in": "60s" })
            }
            ("DELETE", _) => json!({ "released": true }),
            other => panic!("the stub has no answer for {other:?}"),
        }
    }

    async fn handler(
        State(state): State<StubState>,
        method: Method,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let parsed = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        };
        state
            .requests
            .write()
            .await
            .push((method.clone(), uri.path().to_string(), parsed));
        match &*state.behavior {
            StubBehavior::Success => Json(success_body(&method, uri.path())).into_response(),
            StubBehavior::Answer { status, body } => (*status, Json(body.clone())).into_response(),
            StubBehavior::SuccessMalformed => (StatusCode::OK, "not json").into_response(),
            StubBehavior::ErrorMalformed => {
                (StatusCode::INTERNAL_SERVER_ERROR, "not json").into_response()
            }
        }
    }

    fn client_at(url: &str) -> GuiderServiceClient {
        GuiderServiceClient::new(
            url.to_string(),
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
            None,
        )
        .unwrap()
    }

    fn client_for(stub: &TestStub) -> GuiderServiceClient {
        client_at(&stub.url)
    }

    fn pixels(amount: f64) -> DitherRequest {
        DitherRequest {
            amount: DitherAmount::Px(amount),
            ra_only: false,
            settle: None,
        }
    }

    #[tokio::test]
    async fn start_guiding_happy_path_returns_outcome() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let outcome = client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap();

        assert_eq!(outcome.state, GuiderState::Guiding);
        assert_eq!(outcome.rms_ra_px, Some(0.21));
        assert_eq!(outcome.rms_dec_px, Some(0.18));
        assert_eq!(outcome.total_rms_px, Some(0.28));
        assert_eq!(outcome.rms_ra_arcsec, Some(0.63));
        assert_eq!(outcome.rms_dec_arcsec, Some(0.54));
        assert_eq!(outcome.total_rms_arcsec, Some(0.83));
        assert_eq!(outcome.sample_count, 12);
    }

    #[tokio::test]
    async fn a_settled_outcome_with_no_sample_parses_its_rms_as_none() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({
                "state": "guiding",
                "rms_ra_px": null,
                "rms_dec_px": null,
                "total_rms_px": null,
                "rms_ra_arcsec": null,
                "rms_dec_arcsec": null,
                "total_rms_arcsec": null,
                "sample_count": 0,
            }),
        })
        .await;
        let client = client_for(&stub);

        let outcome = client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap();

        assert_eq!(outcome.rms_ra_px, None);
        assert_eq!(outcome.total_rms_arcsec, None);
        assert_eq!(outcome.sample_count, 0);
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

        let (method, path, body) = stub.requests.read().await[0].clone();
        assert_eq!(method, Method::POST);
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

        let (_, _, body) = stub.requests.read().await[0].clone();
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

        let (_, _, body) = stub.requests.read().await[0].clone();
        assert_eq!(body["settle"]["pixels"], 1.2);
        assert!(body["settle"].get("time").is_none());
        assert!(body["settle"].get("timeout").is_none());
    }

    #[tokio::test]
    async fn stop_pause_and_resume_answer_the_guiders_state() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        assert_eq!(client.stop_guiding().await.unwrap(), GuiderState::Idle);
        assert_eq!(
            client.pause_guiding(true).await.unwrap(),
            GuiderState::Paused
        );
        assert_eq!(client.resume_guiding().await.unwrap(), GuiderState::Guiding);

        let requests = stub.requests.read().await.clone();
        assert_eq!(requests[0].1, "/api/v1/guiding/stop");
        assert_eq!(requests[1].1, "/api/v1/guiding/pause");
        assert_eq!(requests[1].2["full"], true);
        assert_eq!(requests[2].1, "/api/v1/guiding/resume");
    }

    #[tokio::test]
    async fn resume_guiding_hands_on_star_lost() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({ "state": "star_lost" }),
        })
        .await;
        let client = client_for(&stub);

        assert_eq!(
            client.resume_guiding().await.unwrap(),
            GuiderState::StarLost
        );
    }

    #[tokio::test]
    async fn dither_forwards_pixels_ra_only_and_settle() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let outcome = client
            .dither(DitherRequest {
                amount: DitherAmount::Px(5.0),
                ra_only: true,
                settle: Some(SettleOverride {
                    pixels: None,
                    time: None,
                    timeout: Some(Duration::from_secs(30)),
                }),
            })
            .await
            .unwrap();

        assert_eq!(outcome.total_rms_px, Some(0.28));
        let (_, path, body) = stub.requests.read().await[0].clone();
        assert_eq!(path, "/api/v1/dither");
        assert_eq!(body["amount_px"], 5.0);
        assert!(body.get("amount_arcsec").is_none(), "got: {body}");
        assert_eq!(body["ra_only"], true);
        assert_eq!(body["settle"]["timeout"], "30s");
    }

    #[tokio::test]
    async fn dither_forwards_arcseconds_in_their_own_field() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        client
            .dither(DitherRequest {
                amount: DitherAmount::Arcsec(4.5),
                ra_only: false,
                settle: None,
            })
            .await
            .unwrap();

        let (_, _, body) = stub.requests.read().await[0].clone();
        assert_eq!(body["amount_arcsec"], 4.5);
        assert!(body.get("amount_px").is_none(), "got: {body}");
        assert_eq!(body["ra_only"], false);
        assert!(body.get("settle").is_none(), "got: {body}");
    }

    #[tokio::test]
    async fn abort_dither_answers_whether_a_dither_was_in_flight() {
        let stub = spawn_stub(StubBehavior::Success).await;
        assert!(client_for(&stub).abort_dither().await.unwrap());
        let (method, path, _) = stub.requests.read().await[0].clone();
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/api/v1/dither/abort");

        let none = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({ "state": "idle", "aborted": false }),
        })
        .await;
        assert!(!client_for(&none).abort_dither().await.unwrap());
    }

    #[tokio::test]
    async fn guiding_stats_parses_the_full_body() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let stats = client.guiding_stats().await.unwrap();

        assert_eq!(stats.state, GuiderState::Guiding);
        assert!(stats.guiding);
        assert!(!stats.settling);
        assert_eq!(stats.pixel_scale_arcsec, Some(2.99));
        assert_eq!(stats.rms_ra_px, Some(0.21));
        assert_eq!(stats.total_rms_arcsec, Some(0.83));
        assert_eq!(stats.snr, Some(24.8));
        assert_eq!(stats.star_mass, Some(5120.0));
        assert_eq!(stats.star_count, Some(5));
        assert_eq!(stats.sample_count, 50);
        assert_eq!(stats.last_stop, None);
        assert_eq!(
            stats.totals,
            GuidingTotals {
                epoch: EPOCH.to_string(),
                guide_steps: 1530,
                ra_samples: 1530,
                ra_sum_sq_px2: 67.5,
                dec_samples: 1528,
                dec_sum_sq_px2: 49.6,
                star_lost: 2,
                interruptions: 1,
            }
        );
    }

    #[tokio::test]
    async fn stats_of_a_stopped_guider_carry_the_last_stop_and_null_telemetry() {
        let mut body = stats_body();
        for key in [
            "rms_ra_px",
            "rms_dec_px",
            "total_rms_px",
            "rms_ra_arcsec",
            "rms_dec_arcsec",
            "total_rms_arcsec",
            "snr",
            "star_mass",
            "star_count",
            "pixel_scale_arcsec",
        ] {
            body[key] = Value::Null;
        }
        body["state"] = json!("idle");
        body["guiding"] = json!(false);
        body["sample_count"] = json!(0);
        body["last_stop"] = json!({
            "reason": "mount_slewing",
            "at": "2026-11-12T03:20:01Z",
            "message": "the mount is slewing",
        });
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body,
        })
        .await;
        let client = client_for(&stub);

        let stats = client.guiding_stats().await.unwrap();

        assert_eq!(stats.state, GuiderState::Idle);
        assert!(!stats.guiding);
        assert_eq!(stats.rms_ra_px, None);
        assert_eq!(stats.snr, None);
        assert_eq!(stats.star_count, None);
        assert_eq!(stats.pixel_scale_arcsec, None);
        assert_eq!(
            stats.last_stop,
            Some(LastStop {
                reason: "mount_slewing".to_string(),
                at: "2026-11-12T03:20:01Z".to_string(),
                message: "the mount is slewing".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn guiding_metrics_parses_frames_and_the_epoch() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let metrics = client.guiding_metrics().await.unwrap();

        assert_eq!(metrics.state, GuiderState::Guiding);
        assert!(metrics.guiding);
        assert_eq!(metrics.epoch, EPOCH);
        assert_eq!(metrics.frames.len(), 2);
        assert_eq!(metrics.frames[0].frame, 41);
        assert_eq!(metrics.frames[0].hfd, Some(2.31));
        assert_eq!(metrics.frames[0].star_count, 5);
        assert!(!metrics.frames[0].star_lost);
        assert_eq!(metrics.frames[1].hfd, None);
        assert!(metrics.frames[1].star_lost);
    }

    #[tokio::test]
    async fn devices_parses_the_unique_ids() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        let devices = client.devices().await.unwrap();

        assert_eq!(
            devices.camera.unwrap().unique_id.as_deref(),
            Some("5b0f6c1e-93a4-4f7d-8a2e-1c7d40e9b6f3")
        );
        assert_eq!(
            devices.mount.unwrap().unique_id.as_deref(),
            Some("c2d94a70-6e15-4b8c-b3f1-9a05e7d2816c")
        );
        let (method, path, _) = stub.requests.read().await[0].clone();
        assert_eq!(method, Method::GET);
        assert_eq!(path, "/api/v1/devices");
    }

    #[tokio::test]
    async fn devices_parses_a_missing_device_and_an_unread_id() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({
                "camera": null,
                "mount": { "reachable": false, "connected": null, "unique_id": null, "can_pulse_guide": null },
                "rotator": null,
            }),
        })
        .await;
        let client = client_for(&stub);

        let devices = client.devices().await.unwrap();

        assert_eq!(devices.camera, None);
        assert_eq!(devices.mount, Some(GuiderDevice { unique_id: None }));
    }

    #[tokio::test]
    async fn clear_calibration_and_reselect_star_hit_their_endpoints() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);

        assert_eq!(client.clear_calibration().await.unwrap(), GuiderState::Idle);
        assert_eq!(client.reselect_star().await.unwrap(), GuiderState::Guiding);

        let requests = stub.requests.read().await.clone();
        assert_eq!(requests[0].1, "/api/v1/calibration/clear");
        assert_eq!(requests[1].1, "/api/v1/star/reselect");
    }

    #[tokio::test]
    async fn the_camera_lease_is_acquired_renewed_and_released_by_its_token() {
        let stub = spawn_stub(StubBehavior::Success).await;
        let client = client_for(&stub);
        let ttl = Duration::from_secs(45);

        client.acquire_camera_lease(TOKEN, ttl).await.unwrap();
        client.renew_camera_lease(TOKEN, ttl).await.unwrap();
        assert!(client.release_camera_lease(TOKEN).await.unwrap());

        let requests = stub.requests.read().await.clone();
        let lease_path = format!("/api/v1/camera/lease/{TOKEN}");
        assert_eq!(
            requests[0],
            (
                Method::POST,
                "/api/v1/camera/lease".to_string(),
                json!({ "token": TOKEN, "ttl": "45s" })
            )
        );
        assert_eq!(
            requests[1],
            (Method::PUT, lease_path.clone(), json!({ "ttl": "45s" }))
        );
        assert_eq!(requests[2], (Method::DELETE, lease_path, Value::Null));
    }

    #[tokio::test]
    async fn releasing_a_lease_the_token_does_not_hold_answers_false() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({ "released": false }),
        })
        .await;

        assert!(!client_for(&stub).release_camera_lease(TOKEN).await.unwrap());
    }

    #[tokio::test]
    async fn renewing_a_lease_the_guider_no_longer_holds_carries_its_code() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::NOT_FOUND,
            body: json!({
                "error": "lease_not_found",
                "message": "the token holds no lease",
            }),
        })
        .await;

        let err = client_for(&stub)
            .renew_camera_lease(TOKEN, Duration::from_mins(1))
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some("lease_not_found"));
    }

    #[tokio::test]
    async fn service_error_envelope_propagates() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: json!({
                "error": "guide_failed",
                "message": "the run stopped: the mount is slewing",
                "details": { "stop_reason": "mount_slewing" },
            }),
        })
        .await;
        let client = client_for(&stub);

        let err = client.dither(pixels(3.0)).await.unwrap_err();

        assert_eq!(
            err.to_string(),
            "guide_failed: the run stopped: the mount is slewing"
        );
        match err {
            GuiderError::Service { code, details, .. } => {
                assert_eq!(code, "guide_failed");
                assert_eq!(details["stop_reason"], "mount_slewing");
            }
            other => panic!("expected Service error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn settle_timeout_envelope_propagates() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::GATEWAY_TIMEOUT,
            body: json!({
                "error": "settle_timeout",
                "message": "guiding did not settle in time",
            }),
        })
        .await;
        let client = client_for(&stub);

        let err = client
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("settle_timeout"));
    }

    #[tokio::test]
    async fn malformed_success_body_is_internal() {
        let stub = spawn_stub(StubBehavior::SuccessMalformed).await;
        let client = client_for(&stub);

        let err = client.stop_guiding().await.unwrap_err();
        assert!(matches!(err, GuiderError::Internal(_)));
        assert_eq!(err.code(), None);
    }

    #[tokio::test]
    async fn a_state_outside_the_contract_is_internal() {
        let stub = spawn_stub(StubBehavior::Answer {
            status: StatusCode::OK,
            body: json!({ "state": "Stopped" }),
        })
        .await;

        let err = client_for(&stub).stop_guiding().await.unwrap_err();
        assert!(matches!(err, GuiderError::Internal(_)), "got: {err:?}");
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
        let client = client_at("http://127.0.0.1:1");
        let err = client.stop_guiding().await.unwrap_err();
        assert!(matches!(err, GuiderError::ServiceUnreachable(_)));
        assert_eq!(err.code(), None);
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let client = client_at("http://localhost:11129/");
        assert_eq!(client.base_url(), "http://localhost:11129");
    }

    #[test]
    fn a_dither_runs_under_its_settle_timeout_plus_the_margin() {
        let timeout = Duration::from_secs(90);

        // No settle timeout on the request → the configured timeout.
        assert_eq!(dither_deadline(timeout, None), timeout);
        // A settle timeout decides the deadline, shorter than the
        // configured timeout or longer.
        assert_eq!(
            dither_deadline(timeout, Some(Duration::from_secs(40))),
            Duration::from_secs(55)
        );
        assert_eq!(
            dither_deadline(timeout, Some(Duration::from_mins(2))),
            Duration::from_secs(135)
        );
    }

    #[test]
    fn a_start_runs_under_the_calibration_timeout_on_top_of_the_settle() {
        let timeout = Duration::from_secs(90);
        let calibration = Duration::from_mins(10);

        assert_eq!(
            start_deadline(timeout, calibration, None),
            Duration::from_secs(690)
        );
        assert_eq!(
            start_deadline(timeout, calibration, Some(Duration::from_secs(40))),
            Duration::from_secs(655)
        );
    }

    #[test]
    fn the_deadlines_saturate_instead_of_overflowing() {
        let timeout = Duration::from_secs(90);

        assert_eq!(dither_deadline(timeout, Some(Duration::MAX)), Duration::MAX);
        assert_eq!(
            start_deadline(timeout, Duration::MAX, Some(Duration::from_secs(40))),
            Duration::MAX
        );
    }

    #[test]
    fn the_run_states_are_the_four_past_looping() {
        for state in [GuiderState::Idle, GuiderState::Looping] {
            assert!(!state.is_run(), "{state}");
        }
        for state in [
            GuiderState::Calibrating,
            GuiderState::Guiding,
            GuiderState::Paused,
            GuiderState::StarLost,
        ] {
            assert!(state.is_run(), "{state}");
        }
    }

    #[test]
    fn a_state_displays_as_its_wire_name() {
        for state in [
            GuiderState::Idle,
            GuiderState::Looping,
            GuiderState::Calibrating,
            GuiderState::Guiding,
            GuiderState::Paused,
            GuiderState::StarLost,
        ] {
            assert_eq!(json!(state), json!(state.to_string()));
            assert_eq!(state.to_string(), state.as_str());
        }
        assert_eq!(GuiderState::StarLost.as_str(), "star_lost");
    }

    #[test]
    fn new_with_missing_ca_cert_fails() {
        // Proves `ca_cert_path` is wired to the CA-trust builder, not
        // silently dropped.
        let result = GuiderServiceClient::new(
            "http://localhost:11129".to_string(),
            Duration::from_secs(5),
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
                    Json(json!({ "state": "idle" })).into_response()
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
            "http://localhost:11129".to_string(),
            Duration::from_secs(5),
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

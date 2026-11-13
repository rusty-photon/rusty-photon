//! Axum router and HTTP handlers for the guider service endpoints.
//!
//! The behavior contract is `docs/services/phd2-guider.md` § "HTTP
//! Service Mode"; the canonical executable contract is
//! `tests/features/http_api.feature`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{rejection::JsonRejection, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::error::ServiceError;
use super::guider::{GuiderOps, StatsSnapshot};

pub fn build_router(ops: Arc<GuiderOps>) -> Router {
    Router::new()
        .route("/api/v1/guiding/start", post(start_guiding))
        .route("/api/v1/guiding/stop", post(stop_guiding))
        .route("/api/v1/guiding/pause", post(pause_guiding))
        .route("/api/v1/guiding/resume", post(resume_guiding))
        .route("/api/v1/dither", post(dither))
        .route("/api/v1/guiding/stats", get(stats))
        .route("/api/v1/guiding/metrics", get(metrics))
        .route("/api/v1/equipment", get(equipment))
        .route("/api/v1/calibration/clear", post(clear_calibration))
        .route("/api/v1/star/reselect", post(reselect_star))
        .route("/api/v1/phd2/restart", post(restart_phd2))
        .route("/health", get(health))
        .with_state(ops)
}

/// Partial settle override; omitted fields fall back to the config
/// `settling` block, field by field.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettleBody {
    #[serde(default)]
    pixels: Option<f64>,
    #[serde(default, with = "humantime_serde::option")]
    time: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    timeout: Option<Duration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartBody {
    #[serde(default)]
    recalibrate: bool,
    #[serde(default)]
    settle: Option<SettleBody>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DitherBody {
    amount_px: f64,
    #[serde(default)]
    ra_only: bool,
    #[serde(default)]
    settle: Option<SettleBody>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseBody {
    #[serde(default)]
    full: bool,
}

/// Response shape shared by `guiding/start` and `dither`.
#[derive(Debug, Serialize)]
struct SettledResponse {
    state: &'static str,
    rms_ra_px: Option<f64>,
    rms_dec_px: Option<f64>,
    total_rms_px: Option<f64>,
    sample_count: usize,
}

impl SettledResponse {
    const fn from_snapshot(snapshot: StatsSnapshot) -> Self {
        Self {
            state: "guiding",
            rms_ra_px: snapshot.rms_ra_px,
            rms_dec_px: snapshot.rms_dec_px,
            total_rms_px: snapshot.total_rms_px,
            sample_count: snapshot.sample_count,
        }
    }
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    app_state: String,
    guiding: bool,
    rms_ra_px: Option<f64>,
    rms_dec_px: Option<f64>,
    total_rms_px: Option<f64>,
    snr: Option<f64>,
    star_mass: Option<f64>,
    sample_count: usize,
    totals: TotalsResponse,
}

/// The running totals on the wire (`docs/services/phd2-guider.md`
/// § "Running totals").
#[derive(Debug, Serialize)]
struct TotalsResponse {
    epoch: String,
    guide_steps: u64,
    ra_samples: u64,
    ra_sum_sq_px2: f64,
    dec_samples: u64,
    dec_sum_sq_px2: f64,
    star_lost: u64,
    interruptions: u64,
}

#[derive(Debug, Serialize)]
struct StateResponse {
    state: &'static str,
}

/// Parse a request body whose fields are all optional: an empty (or
/// whitespace-only) body is `{}`; a malformed one is
/// `invalid_request`. Explicit `Bytes` parsing (rather than the `Json`
/// extractor) keeps the error inside the structured envelope and makes
/// the empty body valid.
fn parse_optional_body<T: Default + serde::de::DeserializeOwned>(
    bytes: &axum::body::Bytes,
) -> Result<T, ServiceError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes)
        .map_err(|e| ServiceError::InvalidRequest(format!("malformed body: {e}")))
}

fn parse_required_body<T: serde::de::DeserializeOwned>(
    bytes: &axum::body::Bytes,
) -> Result<T, ServiceError> {
    serde_json::from_slice(bytes)
        .map_err(|e| ServiceError::InvalidRequest(format!("malformed body: {e}")))
}

/// Merge a per-request settle override onto the config defaults,
/// rejecting a non-positive or non-finite threshold before anything
/// reaches the PHD2 RPC layer (same bar as `amount_px`).
fn resolve_settle(
    ops: &GuiderOps,
    settle: Option<SettleBody>,
) -> Result<crate::config::SettleParams, ServiceError> {
    let settle = settle.unwrap_or_default();
    if let Some(pixels) = settle.pixels {
        if !pixels.is_finite() || pixels <= 0.0 {
            return Err(ServiceError::InvalidRequest(format!(
                "settle.pixels must be a positive number of pixels, got {pixels}"
            )));
        }
    }
    Ok(ops.resolve_settle(settle.pixels, settle.time, settle.timeout))
}

async fn start_guiding(
    State(ops): State<Arc<GuiderOps>>,
    bytes: axum::body::Bytes,
) -> Result<Json<SettledResponse>, ServiceError> {
    let body: StartBody = parse_optional_body(&bytes)?;
    let settle = resolve_settle(&ops, body.settle)?;
    let snapshot = ops.start_guiding(settle, body.recalibrate).await?;
    Ok(Json(SettledResponse::from_snapshot(snapshot)))
}

async fn stop_guiding(
    State(ops): State<Arc<GuiderOps>>,
) -> Result<Json<StateResponse>, ServiceError> {
    ops.stop().await?;
    Ok(Json(StateResponse { state: "stopped" }))
}

async fn pause_guiding(
    State(ops): State<Arc<GuiderOps>>,
    bytes: axum::body::Bytes,
) -> Result<Json<StateResponse>, ServiceError> {
    let body: PauseBody = parse_optional_body(&bytes)?;
    ops.pause(body.full).await?;
    Ok(Json(StateResponse { state: "paused" }))
}

async fn resume_guiding(
    State(ops): State<Arc<GuiderOps>>,
) -> Result<Json<StateResponse>, ServiceError> {
    ops.resume().await?;
    Ok(Json(StateResponse { state: "resumed" }))
}

async fn dither(
    State(ops): State<Arc<GuiderOps>>,
    bytes: axum::body::Bytes,
) -> Result<Json<SettledResponse>, ServiceError> {
    let body: DitherBody = parse_required_body(&bytes)?;
    if !body.amount_px.is_finite() || body.amount_px <= 0.0 {
        return Err(ServiceError::InvalidRequest(format!(
            "amount_px must be a positive number of pixels, got {}",
            body.amount_px
        )));
    }
    let settle = resolve_settle(&ops, body.settle)?;
    let snapshot = ops.dither(body.amount_px, body.ra_only, settle).await?;
    Ok(Json(SettledResponse::from_snapshot(snapshot)))
}

async fn stats(State(ops): State<Arc<GuiderOps>>) -> Result<Json<StatsResponse>, ServiceError> {
    let stats = ops.stats().await?;
    Ok(Json(StatsResponse {
        app_state: stats.app_state.to_string(),
        guiding: stats.app_state == crate::events::AppState::Guiding,
        rms_ra_px: stats.snapshot.rms_ra_px,
        rms_dec_px: stats.snapshot.rms_dec_px,
        total_rms_px: stats.snapshot.total_rms_px,
        snr: stats.snapshot.snr,
        star_mass: stats.snapshot.star_mass,
        sample_count: stats.snapshot.sample_count,
        totals: TotalsResponse {
            epoch: stats.epoch.to_string(),
            guide_steps: stats.totals.guide_steps,
            ra_samples: stats.totals.ra.count(),
            ra_sum_sq_px2: stats.totals.ra.sum_of_squares(),
            dec_samples: stats.totals.dec.count(),
            dec_sum_sq_px2: stats.totals.dec.sum_of_squares(),
            star_lost: stats.totals.star_lost,
            interruptions: stats.totals.interruptions,
        },
    }))
}

/// Which calibration to clear; the serde names are the wire contract
/// (`"mount"` default, `"ao"`, `"both"`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClearCalibrationBody {
    #[serde(default)]
    which: ClearTarget,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ClearTarget {
    #[default]
    Mount,
    Ao,
    Both,
}

impl From<ClearTarget> for crate::types::CalibrationTarget {
    fn from(t: ClearTarget) -> Self {
        match t {
            ClearTarget::Mount => Self::Mount,
            ClearTarget::Ao => Self::AO,
            ClearTarget::Both => Self::Both,
        }
    }
}

async fn metrics(
    State(ops): State<Arc<GuiderOps>>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let metrics = ops.metrics().await?;
    Ok(Json(serde_json::json!({
        "guiding": metrics.guiding,
        "frames": metrics.frames,
    })))
}

/// Serialized by hand rather than via `types::Equipment`'s derive so
/// the wire keeps the documented lowercase slot names (`ao`, not the
/// RPC's `AO`) and every slot is present (`null` when unconfigured).
async fn equipment(
    State(ops): State<Arc<GuiderOps>>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let equipment = ops.equipment().await?;
    let slot = |d: &Option<crate::types::EquipmentDevice>| {
        d.as_ref().map_or(
            serde_json::Value::Null,
            |d| serde_json::json!({ "name": d.name, "connected": d.connected }),
        )
    };
    Ok(Json(serde_json::json!({
        "camera": slot(&equipment.camera),
        "mount": slot(&equipment.mount),
        "aux_mount": slot(&equipment.aux_mount),
        "ao": slot(&equipment.ao),
        "rotator": slot(&equipment.rotator),
    })))
}

async fn clear_calibration(
    State(ops): State<Arc<GuiderOps>>,
    bytes: axum::body::Bytes,
) -> Result<Json<StateResponse>, ServiceError> {
    let body: ClearCalibrationBody = parse_optional_body(&bytes)?;
    ops.clear_calibration(body.which.into()).await?;
    Ok(Json(StateResponse { state: "cleared" }))
}

async fn reselect_star(
    State(ops): State<Arc<GuiderOps>>,
) -> Result<Json<StateResponse>, ServiceError> {
    ops.reselect_star().await?;
    Ok(Json(StateResponse { state: "selected" }))
}

/// The body a restart request carries: an empty JSON object, so the request
/// has to be JSON and a page on another site cannot submit it as a form.
///
/// It converts from a JSON map rather than deriving a field-less struct with
/// `deny_unknown_fields`, whose derive expands to an empty enum that
/// `clippy::empty_enums` denies.
#[derive(Debug, Deserialize)]
#[serde(try_from = "serde_json::Map<String, serde_json::Value>")]
struct RestartBody;

impl TryFrom<serde_json::Map<String, serde_json::Value>> for RestartBody {
    type Error = String;

    fn try_from(fields: serde_json::Map<String, serde_json::Value>) -> Result<Self, Self::Error> {
        fields.keys().next().map_or(Ok(Self), |field| {
            Err(format!(
                "a restart request carries no fields, got {field:?}"
            ))
        })
    }
}

#[derive(Debug, Serialize)]
struct RestartResponse {
    phd2: &'static str,
    equipment: &'static str,
    elapsed_ms: u64,
}

/// A body that is not JSON keeps axum's own `415`; a JSON body that is not
/// an empty object is `invalid_request`.
async fn restart_phd2(
    State(ops): State<Arc<GuiderOps>>,
    body: Result<Json<RestartBody>, JsonRejection>,
) -> Response {
    match body {
        Ok(Json(RestartBody)) => {}
        Err(rejection @ JsonRejection::MissingJsonContentType(_)) => {
            return rejection.into_response();
        }
        Err(rejection) => {
            return ServiceError::InvalidRequest(rejection.body_text()).into_response();
        }
    }
    // The restart runs on a task of its own, so a client that goes away
    // does not cut it off between ending PHD2 and connecting its equipment.
    let restart = tokio::spawn(async move { ops.restart_phd2().await });
    match restart.await {
        Ok(Ok(done)) => Json(RestartResponse {
            phd2: "restarted",
            equipment: "connected",
            elapsed_ms: u64::try_from(done.elapsed.as_millis()).unwrap_or(u64::MAX),
        })
        .into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => {
            ServiceError::Internal(format!("the restart did not finish: {e}")).into_response()
        }
    }
}

/// The 503 means alive-but-degraded (PHD2 off is the normal daytime
/// state); `message` is the opaque explanation sentinel displays on its
/// dashboard without interpreting (docs/services/sentinel.md §Service
/// Health Supervision).
async fn health(State(ops): State<Arc<GuiderOps>>) -> impl IntoResponse {
    if ops.is_connected().await {
        (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "message": format!(
                    "no connection to PHD2 at {}; reconnecting automatically — \
                     start PHD2 with its event server enabled (Tools → Enable Server)",
                    ops.phd2_addr()
                ),
            })),
        )
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn test_ops() -> GuiderOps {
        GuiderOps::new(
            Arc::new(crate::client::Phd2Client::new(
                crate::config::Phd2Config::default(),
            )),
            crate::config::SettleParams::default(),
            Duration::from_secs(10),
        )
    }

    #[test]
    fn a_start_body_defaults_to_no_recalibrate_and_no_settle_override() {
        let body: StartBody = serde_json::from_str("{}").unwrap();
        assert!(!body.recalibrate);
        assert!(body.settle.is_none());
    }

    #[test]
    fn a_whitespace_only_body_parses_as_the_default() {
        let bytes = axum::body::Bytes::from_static(b" \n\t ");
        let body: StartBody = parse_optional_body(&bytes).unwrap();
        assert!(!body.recalibrate);
        assert!(body.settle.is_none());
    }

    #[test]
    fn a_non_positive_settle_threshold_is_rejected_before_any_rpc() {
        let ops = test_ops();
        for pixels in [0.0, -1.5, f64::NAN, f64::INFINITY] {
            let settle = Some(SettleBody {
                pixels: Some(pixels),
                time: None,
                timeout: None,
            });
            let err = resolve_settle(&ops, settle).unwrap_err();
            assert_eq!(err.code(), super::super::error::ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn settle_durations_parse_as_humantime_strings() {
        let body: StartBody =
            serde_json::from_str(r#"{"settle": {"pixels": 2.0, "time": "5s", "timeout": "30s"}}"#)
                .unwrap();
        let settle = body.settle.unwrap();
        assert_eq!(settle.pixels, Some(2.0));
        assert_eq!(settle.time, Some(Duration::from_secs(5)));
        assert_eq!(settle.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_clear_calibration_body_maps_every_target() {
        for (body, expected) in [
            ("{}", crate::types::CalibrationTarget::Mount),
            (
                r#"{"which": "mount"}"#,
                crate::types::CalibrationTarget::Mount,
            ),
            (r#"{"which": "ao"}"#, crate::types::CalibrationTarget::AO),
            (
                r#"{"which": "both"}"#,
                crate::types::CalibrationTarget::Both,
            ),
        ] {
            let parsed: ClearCalibrationBody = serde_json::from_str(body).unwrap();
            assert_eq!(
                crate::types::CalibrationTarget::from(parsed.which),
                expected,
                "body {body}"
            );
        }
        assert!(serde_json::from_str::<ClearCalibrationBody>(r#"{"which": "camera"}"#).is_err());
    }

    #[test]
    fn a_dither_body_requires_amount_px() {
        let err = serde_json::from_str::<DitherBody>(r#"{"ra_only": true}"#).unwrap_err();
        assert!(err.to_string().contains("amount_px"));
    }

    #[test]
    fn unknown_body_fields_are_rejected() {
        let err = serde_json::from_str::<StartBody>(r#"{"recalibrate": false, "pixels": 1.0}"#)
            .unwrap_err();
        assert!(err.to_string().contains("pixels"));
    }

    #[test]
    fn a_restart_body_is_an_empty_object() {
        serde_json::from_str::<RestartBody>("{}").unwrap();
        let err = serde_json::from_str::<RestartBody>(r#"{"force": true}"#).unwrap_err();
        assert!(err.to_string().contains("\"force\""), "{err}");
        let err = serde_json::from_str::<RestartBody>("[]").unwrap_err();
        assert!(err.to_string().contains("map"), "{err}");
    }

    /// `POST /api/v1/phd2/restart` with `body` of `content_type` on a
    /// guider that is not switched over: the status and the body.
    async fn post_restart(content_type: &str, body: &'static str) -> (StatusCode, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/v1/phd2/restart",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, build_router(Arc::new(test_ops())))
                .await
                .unwrap();
        });
        let response = reqwest::Client::new()
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let text = response.text().await.unwrap();
        server.abort();
        (status, text)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // reqwest's client builds a rustls config, whose aws-lc C code Miri cannot call
    async fn a_restart_posted_as_a_form_is_refused_with_415() {
        let (status, body) = post_restart("application/x-www-form-urlencoded", "force=true").await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // reqwest's client builds a rustls config, whose aws-lc C code Miri cannot call
    async fn a_restart_with_a_json_body_that_is_not_an_empty_object_is_invalid() {
        for body in ["[]", r#"{"force": true}"#, "{"] {
            let (status, text) = post_restart("application/json", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
            let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(envelope["error"], "invalid_request", "{body}: {text}");
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // reqwest's client builds a rustls config, whose aws-lc C code Miri cannot call
    async fn a_restart_on_a_host_not_switched_over_is_unsupported() {
        let (status, text) = post_restart("application/json", "{}").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{text}");
        let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope["error"], "restart_unsupported", "{text}");
    }

    #[test]
    fn the_settled_response_serializes_null_rms_when_unsampled() {
        let response = SettledResponse::from_snapshot(StatsSnapshot {
            rms_ra_px: None,
            rms_dec_px: None,
            total_rms_px: None,
            snr: None,
            star_mass: None,
            sample_count: 0,
        });
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["state"], "guiding");
        assert_eq!(json["rms_ra_px"], serde_json::Value::Null);
        assert_eq!(json["sample_count"], 0);
    }
}

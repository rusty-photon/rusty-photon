//! Axum router and HTTP handlers for the guider service endpoints.
//!
//! The behavior contract is `docs/services/phd2-guider.md` § "HTTP
//! Service Mode"; the canonical executable contract is
//! `tests/features/http_api.feature`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
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
    let slot = |d: &Option<crate::types::EquipmentDevice>| match d {
        Some(d) => serde_json::json!({ "name": d.name, "connected": d.connected }),
        None => serde_json::Value::Null,
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

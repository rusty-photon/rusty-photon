use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::camera::DeviceState;
use crate::pointing::{validate_pointing, PointingSource};

/// Wire shape of `/sky-survey/position`: all angles in degrees.
#[derive(Debug, Serialize)]
struct PositionDegreesResponse {
    ra: f64,
    dec: f64,
    rotation: f64,
}

/// Wire shape of a `/sky-survey/position` write: all angles in
/// degrees; a missing `rotation` keeps the current value.
#[derive(Debug, Deserialize)]
struct PositionDegreesRequest {
    ra: f64,
    dec: f64,
    #[serde(default)]
    rotation: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ConflictBody {
    error: &'static str,
    reason: &'static str,
}

pub fn build_router(state: Arc<DeviceState>) -> Router {
    Router::new()
        .route(
            "/sky-survey/position",
            get(get_position).post(post_position),
        )
        .with_state(state)
}

async fn get_position(State(state): State<Arc<DeviceState>>) -> impl IntoResponse {
    // F6: returns the most recently snapshotted pointing in either
    // mode. In static mode this is the value updated by POST; in
    // follow mode it's the last successful mount read (written
    // before the SkyView fetch, so it reflects mount state even if
    // a later exposure step fails).
    let snap = state.last_snapshot.snapshot().await;
    Json(PositionDegreesResponse {
        ra: snap.ra_deg,
        dec: snap.dec_deg,
        rotation: snap.rotation_deg,
    })
}

async fn post_position(
    State(state): State<Arc<DeviceState>>,
    body: Result<Json<PositionDegreesRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let Ok(Json(req)) = body else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if !state.connected.load(Ordering::Acquire) {
        // P6: 409 when disconnected.
        return (
            StatusCode::CONFLICT,
            Json(ConflictBody {
                error: "not_connected",
                reason: "device must be connected to accept position writes",
            }),
        )
            .into_response();
    }

    // F7: in follow mode, POST arms a one-shot override that the next
    // light `StartExposure` consumes — letting test harnesses inject
    // "the camera saw something different from where the mount thinks
    // it is" without requiring the mount itself to lie about its
    // position. The static-mode write path is unchanged.
    match &state.pointing_source {
        PointingSource::Static(p) => match p.update(req.ra, req.dec, req.rotation).await {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::BAD_REQUEST.into_response(),
        },
        PointingSource::Telescope(_) => {
            // Reuse the shared validator so the static-mode write
            // path and the follow-mode one-shot override path stay
            // in lockstep on input validation. We don't actually
            // mutate `last_snapshot` here — the next exposure will
            // overwrite it from either the override or a fresh mount
            // read — but identical 400-on-bad-input semantics with
            // static mode (P4/P5) matters for clients.
            let current_rotation = state.last_snapshot.snapshot().await.rotation_deg;
            let Ok(validated) = validate_pointing(req.ra, req.dec, req.rotation, current_rotation)
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let mut guard = state.next_pointing_override.lock();
            *guard = Some(validated);
            drop(guard);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

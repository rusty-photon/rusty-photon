//! HTTP routes (docs/services/guider.md § HTTP API).
//!
//! The camera loop's requests and reads, `GET /api/v1/devices` and
//! `GET /health`. The devices answer and `/health` answer from the
//! configuration and the device poll, so neither costs a device call.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;

use crate::api::{read_body, ApiError, ErrorCode, NoFields};
use crate::config::Config;
use crate::devices::{CameraState, DevicesState, MountState, Reach};
use crate::guider::{Guider, MetricsBody, SelectionAnswer, StateAnswer, StatsBody};
use crate::preview;
use crate::sensor::{effective_binning, sensor_type_name};

/// What the routes answer from.
#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    devices: watch::Receiver<DevicesState>,
    guider: Arc<Guider>,
}

impl AppState {
    #[must_use]
    pub const fn new(
        config: Arc<Config>,
        devices: watch::Receiver<DevicesState>,
        guider: Arc<Guider>,
    ) -> Self {
        Self {
            config,
            devices,
            guider,
        }
    }
}

/// Build the router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/loop", post(start_loop))
        .route("/api/v1/star/reselect", post(reselect))
        .route("/api/v1/guiding/stop", post(stop))
        .route("/api/v1/guiding/stats", get(stats))
        .route("/api/v1/guiding/metrics", get(metrics))
        .route("/api/v1/frame/latest", get(frame_latest))
        .route("/api/v1/devices", get(devices))
        .route("/health", get(health))
        .with_state(state)
}

/// `POST /api/v1/loop`.
async fn start_loop(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<SelectionAnswer>, ApiError> {
    read_body::<NoFields>(&body)?;
    state.guider.start_loop().await.map(Json)
}

/// `POST /api/v1/star/reselect`.
async fn reselect(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<SelectionAnswer>, ApiError> {
    read_body::<NoFields>(&body)?;
    state.guider.reselect().await.map(Json)
}

/// `POST /api/v1/guiding/stop`.
async fn stop(State(state): State<AppState>, body: Bytes) -> Result<Json<StateAnswer>, ApiError> {
    read_body::<NoFields>(&body)?;
    state.guider.stop().await.map(Json)
}

/// `GET /api/v1/guiding/stats`.
async fn stats(State(state): State<AppState>) -> Json<StatsBody> {
    Json(state.guider.stats())
}

/// `GET /api/v1/guiding/metrics`.
async fn metrics(State(state): State<AppState>) -> Json<MetricsBody> {
    Json(state.guider.metrics())
}

/// `GET /api/v1/frame/latest`: the PNG is drawn off the async workers.
async fn frame_latest(State(state): State<AppState>) -> Result<Response, ApiError> {
    let latest = state.guider.latest_frame()?;
    let radius = state.config.stars.search_region_px.get();
    let png = tokio::task::spawn_blocking(move || {
        preview::render(&latest.frame, &latest.centres, radius)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            ErrorCode::Internal,
            format!("drawing the frame failed: {e}"),
        )
    })?
    .map_err(|e| ApiError::new(ErrorCode::Internal, e))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// `GET /api/v1/devices`.
#[derive(Debug, Serialize, PartialEq)]
struct DevicesBody {
    camera: Option<CameraBody>,
    mount: Option<MountBody>,
    /// Always `null`: the guider is a client of no rotator.
    rotator: Option<()>,
}

#[derive(Debug, Serialize, PartialEq)]
struct CameraBody {
    reachable: bool,
    connected: bool,
    unique_id: Option<String>,
    sensor_type: Option<&'static str>,
    pixel_size_um: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    /// The effective binning (guider.md § Sensor handling).
    binning: Option<u8>,
    can_abort_exposure: Option<bool>,
}

#[derive(Debug, Serialize, PartialEq)]
struct MountBody {
    reachable: bool,
    connected: bool,
    unique_id: Option<String>,
    can_pulse_guide: Option<bool>,
}

async fn devices(State(state): State<AppState>) -> Json<DevicesBody> {
    let devices = state.devices.borrow().clone();
    Json(devices_body(&state.config, &devices))
}

fn devices_body(config: &Config, devices: &DevicesState) -> DevicesBody {
    DevicesBody {
        camera: devices
            .camera
            .as_ref()
            .map(|camera| camera_body(config, camera)),
        mount: devices.mount.as_ref().map(mount_body),
        rotator: None,
    }
}

fn camera_body(config: &Config, camera: &CameraState) -> CameraBody {
    let sensor = camera.statics.as_ref();
    CameraBody {
        reachable: camera.reach.reachable(),
        connected: camera.reach.connected(),
        unique_id: camera.unique_id.clone(),
        sensor_type: sensor.map(|sensor| sensor_type_name(sensor.sensor_type)),
        pixel_size_um: sensor.map(|sensor| sensor.pixel_size_um),
        width: sensor.map(|sensor| sensor.width),
        height: sensor.map(|sensor| sensor.height),
        binning: sensor
            .and_then(|sensor| effective_binning(sensor.sensor_type, config.camera.binning.get())),
        can_abort_exposure: sensor.map(|sensor| sensor.can_abort_exposure),
    }
}

fn mount_body(mount: &MountState) -> MountBody {
    MountBody {
        reachable: mount.reach.reachable(),
        connected: mount.reach.connected(),
        unique_id: mount.unique_id.clone(),
        can_pulse_guide: mount.statics.map(|statics| statics.can_pulse_guide),
    }
}

/// `GET /health`.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct HealthBody {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthBody>) {
    let devices = state.devices.borrow().clone();
    let problems = problems(&state.config, &devices);
    if problems.is_empty() {
        (
            StatusCode::OK,
            Json(HealthBody {
                status: "ok",
                message: None,
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthBody {
                status: "unavailable",
                message: Some(problems.join("; ")),
            }),
        )
    }
}

/// Everything that keeps the guider from guiding, configuration first:
/// what the configuration does not name, then each device that does not
/// answer, is not listed or is not connected.
fn problems(config: &Config, devices: &DevicesState) -> Vec<String> {
    let mut problems = Vec::new();
    if config.devices.camera.is_none() {
        problems.push("the configuration names no camera".to_owned());
    }
    if config.devices.mount.is_none() {
        problems.push("the configuration names no mount".to_owned());
    }
    if config.optics.focal_length_mm.is_none() {
        problems.push("the configuration names no focal length".to_owned());
    }
    if let (Some(camera), Some(address)) = (&devices.camera, &config.devices.camera) {
        problems.extend(reach_problem(
            "camera",
            "camera",
            address.device_number,
            camera.reach,
        ));
    }
    if let (Some(mount), Some(address)) = (&devices.mount, &config.devices.mount) {
        problems.extend(reach_problem(
            "mount",
            "telescope",
            address.device_number,
            mount.reach,
        ));
    }
    problems
}

/// What keeps a device of `kind` (the Alpaca device type its server lists
/// it as) from being used, if anything.
fn reach_problem(device: &str, kind: &str, device_number: u32, reach: Reach) -> Option<String> {
    match reach {
        Reach::NoAnswer => Some(format!("the {device} does not answer")),
        Reach::NotListed => Some(format!(
            "the {device}'s server lists no {kind} at device number {device_number}"
        )),
        Reach::NotConnected => Some(format!("the {device} is not connected")),
        Reach::Connected => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use ascom_alpaca::api::camera::SensorType;

    use super::*;
    use crate::devices::{CameraSensor, MountCapabilities};

    fn config(value: serde_json::Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    fn rig() -> Config {
        config(json!({
            "devices": {
                "camera": { "alpaca_url": "http://127.0.0.1:1" },
                "mount": { "alpaca_url": "http://127.0.0.1:2", "device_number": 3 }
            },
            "optics": { "focal_length_mm": 200.0 },
            "camera": { "binning": 2 }
        }))
    }

    fn sensor(sensor_type: SensorType) -> CameraSensor {
        CameraSensor {
            sensor_type,
            pixel_size_um: 3.76,
            width: 1936,
            height: 1096,
            max_adu: 4095,
            max_bin: 4,
            can_abort_exposure: true,
        }
    }

    fn connected() -> DevicesState {
        DevicesState {
            camera: Some(CameraState {
                reach: Reach::Connected,
                unique_id: Some("cam".to_owned()),
                statics: Some(sensor(SensorType::Monochrome)),
                statics_current: true,
            }),
            mount: Some(MountState {
                reach: Reach::Connected,
                unique_id: Some("mount".to_owned()),
                statics: Some(MountCapabilities {
                    can_pulse_guide: true,
                }),
                statics_current: true,
            }),
        }
    }

    #[test]
    fn a_connected_rig_has_no_problem() {
        assert_eq!(problems(&rig(), &connected()), Vec::<String>::new());
    }

    #[test]
    fn each_reach_names_its_problem() {
        assert_eq!(
            reach_problem("camera", "camera", 0, Reach::NoAnswer).as_deref(),
            Some("the camera does not answer")
        );
        assert_eq!(
            reach_problem("mount", "telescope", 3, Reach::NotListed).as_deref(),
            Some("the mount's server lists no telescope at device number 3")
        );
        assert_eq!(
            reach_problem("camera", "camera", 0, Reach::NotConnected).as_deref(),
            Some("the camera is not connected")
        );
        assert_eq!(reach_problem("camera", "camera", 0, Reach::Connected), None);
    }

    #[test]
    fn the_configuration_gaps_come_before_the_devices() {
        let config = config(json!({
            "devices": { "camera": { "alpaca_url": "http://127.0.0.1:1" } }
        }));
        let devices = DevicesState {
            camera: Some(CameraState::default()),
            mount: None,
        };
        assert_eq!(
            problems(&config, &devices),
            [
                "the configuration names no mount",
                "the configuration names no focal length",
                "the camera does not answer"
            ]
        );
    }

    #[test]
    fn the_mount_follows_the_camera_and_names_its_device_number() {
        let mut devices = connected();
        if let Some(camera) = devices.camera.as_mut() {
            camera.reach = Reach::NotConnected;
        }
        if let Some(mount) = devices.mount.as_mut() {
            mount.reach = Reach::NotListed;
        }
        assert_eq!(
            problems(&rig(), &devices),
            [
                "the camera is not connected",
                "the mount's server lists no telescope at device number 3"
            ]
        );
    }

    #[test]
    fn a_colour_camera_is_reported_with_its_type_and_no_binning() {
        let mut devices = connected();
        if let Some(camera) = devices.camera.as_mut() {
            camera.statics = Some(sensor(SensorType::Color));
        }
        let body = serde_json::to_value(devices_body(&rig(), &devices)).unwrap();
        assert_eq!(body["camera"]["sensor_type"], json!("color"));
        assert_eq!(body["camera"]["binning"], json!(null));
    }

    #[test]
    fn the_devices_answer_carries_every_field_of_the_contract() {
        let body = serde_json::to_value(devices_body(&rig(), &connected())).unwrap();
        assert_eq!(
            body,
            json!({
                "camera": {
                    "reachable": true,
                    "connected": true,
                    "unique_id": "cam",
                    "sensor_type": "monochrome",
                    "pixel_size_um": 3.76,
                    "width": 1936,
                    "height": 1096,
                    "binning": 2,
                    "can_abort_exposure": true
                },
                "mount": {
                    "reachable": true,
                    "connected": true,
                    "unique_id": "mount",
                    "can_pulse_guide": true
                },
                "rotator": null
            })
        );
    }

    #[test]
    fn a_device_not_configured_is_null() {
        let body = serde_json::to_value(devices_body(&Config::default(), &DevicesState::default()))
            .unwrap();
        assert_eq!(
            body,
            json!({ "camera": null, "mount": null, "rotator": null })
        );
    }

    #[test]
    fn the_healthy_body_has_no_message() {
        let body = serde_json::to_value(HealthBody {
            status: "ok",
            message: None,
        })
        .unwrap();
        assert_eq!(body, json!({ "status": "ok" }));
    }
}

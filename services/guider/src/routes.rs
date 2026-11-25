//! HTTP routes (docs/services/guider.md § HTTP API): `GET /api/v1/devices`
//! and `GET /health`, both answered from the configuration and the device
//! poll, so neither costs a device call.

use std::sync::Arc;

use ascom_alpaca::api::camera::SensorType;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;

use crate::config::Config;
use crate::devices::{CameraState, DevicesState, MountState, Reach};

/// What the routes answer from.
#[derive(Debug, Clone)]
pub struct AppState {
    config: Arc<Config>,
    devices: watch::Receiver<DevicesState>,
}

impl AppState {
    #[must_use]
    pub const fn new(config: Arc<Config>, devices: watch::Receiver<DevicesState>) -> Self {
        Self { config, devices }
    }
}

/// Build the router.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/devices", get(devices))
        .route("/health", get(health))
        .with_state(state)
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

/// `SensorType` as the API names it.
const fn sensor_type_name(sensor_type: SensorType) -> &'static str {
    match sensor_type {
        SensorType::Monochrome => "monochrome",
        SensorType::Color => "color",
        SensorType::RGGB => "rggb",
        SensorType::CMYG => "cmyg",
        SensorType::CMYG2 => "cmyg2",
        SensorType::LRGB => "lrgb",
    }
}

/// The binning of the measured frame: `camera.binning` for a
/// `Monochrome` camera, 2 for an `RGGB` one, whose 2 × 2 cells are summed,
/// and none for a sensor the guider cannot measure.
const fn effective_binning(sensor_type: SensorType, binning: u8) -> Option<u8> {
    match sensor_type {
        SensorType::Monochrome => Some(binning),
        SensorType::RGGB => Some(2),
        SensorType::Color | SensorType::CMYG | SensorType::CMYG2 | SensorType::LRGB => None,
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
            }),
            mount: Some(MountState {
                reach: Reach::Connected,
                unique_id: Some("mount".to_owned()),
                statics: Some(MountCapabilities {
                    can_pulse_guide: true,
                }),
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
    fn a_monochrome_camera_is_binned_as_configured() {
        assert_eq!(effective_binning(SensorType::Monochrome, 2), Some(2));
    }

    #[test]
    fn an_rggb_camera_is_summed_two_by_two() {
        assert_eq!(effective_binning(SensorType::RGGB, 1), Some(2));
    }

    #[test]
    fn a_sensor_the_guider_cannot_measure_has_no_binning() {
        for sensor_type in [
            SensorType::Color,
            SensorType::CMYG,
            SensorType::CMYG2,
            SensorType::LRGB,
        ] {
            assert_eq!(effective_binning(sensor_type, 1), None, "{sensor_type:?}");
        }
    }

    #[test]
    fn every_sensor_type_has_its_api_name() {
        let names = [
            SensorType::Monochrome,
            SensorType::Color,
            SensorType::RGGB,
            SensorType::CMYG,
            SensorType::CMYG2,
            SensorType::LRGB,
        ]
        .map(sensor_type_name);
        assert_eq!(
            names,
            ["monochrome", "color", "rggb", "cmyg", "cmyg2", "lrgb"]
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

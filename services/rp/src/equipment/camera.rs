use std::sync::Arc;

use ascom_alpaca::api::{Camera, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

/// Per-camera runtime state held by the equipment registry.
///
/// The `max_adu` / `pixel_size_*` / `sensor_*_px` fields are populated
/// once by `connect_camera` after the underlying ASCOM Alpaca device
/// reports `Connected = true`. They describe invariant physical-sensor
/// properties (full-well depth, pixel pitch, array dimensions) that
/// cannot change for the life of the connection, so `do_capture`
/// consumes them from this cache instead of paying one Alpaca round-trip
/// per property per exposure — per Tenet 1 ("don't re-fetch invariant
/// data"). Each field is `Option<...>` so a transient connect-time read
/// failure simply drops that piece of metadata (downstream the
/// `max_adu`-driven cache-variant choice and the document's `optics`
/// block degrade gracefully) rather than refusing to register the
/// camera.
pub struct CameraEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::CameraConfig,
    pub device: Option<Arc<dyn Camera>>,
    /// Camera's `MaxADU` capability, cached at connect time. Drives the
    /// FITS bit-depth (`u16` vs `i32`) and cache-variant selection in
    /// `do_capture`, and is persisted to the sidecar's `max_adu` field.
    pub max_adu: Option<u32>,
    /// `PixelSizeX` in microns, cached at connect time. Fed into the
    /// sidecar's `optics` block alongside `pixel_size_y_um`.
    pub pixel_size_x_um: Option<f64>,
    /// `PixelSizeY` in microns, cached at connect time.
    pub pixel_size_y_um: Option<f64>,
    /// `CameraXSize` in pixels, cached at connect time. Feeds the
    /// `optics` block; combined with `pixel_size_x_um` and the operator-
    /// supplied `focal_length_mm` to derive pixel-scale and FOV.
    pub sensor_width_px: Option<u32>,
    /// `CameraYSize` in pixels, cached at connect time.
    pub sensor_height_px: Option<u32>,
}

pub(super) async fn connect_camera(
    config: &config::CameraConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> CameraEntry {
    debug!(camera_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to camera");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(camera_id = %config.id, error = %e, "failed to create Alpaca client for camera");
            return CameraEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
                max_adu: None,
                pixel_size_x_um: None,
                pixel_size_y_um: None,
                sensor_width_px: None,
                sensor_height_px: None,
            };
        }
    };

    let label = format!("camera {}", config.id);
    let outcome = retry_connect_attempt(&label, |_attempt| async {
        let devices = match tokio::time::timeout(GET_DEVICES_TIMEOUT, client.get_devices()).await {
            Ok(Ok(devices)) => devices,
            Ok(Err(e)) => return AttemptOutcome::Transient(format!("get_devices: {e}")),
            Err(_) => {
                return AttemptOutcome::Transient(format!(
                    "get_devices: timeout after {GET_DEVICES_TIMEOUT:?}"
                ));
            }
        };

        let mut camera_index = 0u32;
        let mut found_camera: Option<Arc<dyn Camera>> = None;
        for device in devices {
            if let TypedDevice::Camera(cam) = device {
                if camera_index == config.device_number {
                    found_camera = Some(cam);
                    break;
                }
                camera_index = camera_index.saturating_add(1);
            }
        }

        let Some(cam) = found_camera else {
            return AttemptOutcome::Permanent(format!(
                "camera at index {} not found on Alpaca server",
                config.device_number
            ));
        };

        match cam.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(cam),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(cam) => {
            // The Alpaca device is now Connected — the five physical-
            // sensor properties below are invariant for the life of the
            // connection, so we read them exactly once here and hand them
            // to every subsequent `do_capture` via the cache. Each read
            // is independent: a transient failure on one property only
            // drops *that* field (and the downstream metadata it would
            // have populated), not the whole CameraEntry — the camera
            // still gets registered and exposures still work. The five
            // properties intentionally happen after `set_connected(true)`
            // succeeds because some Alpaca drivers reject property reads
            // on disconnected devices.
            // Field order matters: the metadata reads borrow `cam`
            // before `device` takes ownership of it.
            let entry = CameraEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                max_adu: match cam.max_adu().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!(camera_id = %config.id, error = %e, "max_adu unavailable at connect time; downstream captures will persist max_adu: None and write FITS as i32");
                        None
                    }
                },
                pixel_size_x_um: match cam.pixel_size_x().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!(camera_id = %config.id, error = %e, "pixel_size_x unavailable at connect time; downstream captures will omit the optics block");
                        None
                    }
                },
                pixel_size_y_um: match cam.pixel_size_y().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!(camera_id = %config.id, error = %e, "pixel_size_y unavailable at connect time; downstream captures will omit the optics block");
                        None
                    }
                },
                sensor_width_px: match cam.camera_x_size().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!(camera_id = %config.id, error = %e, "camera_x_size unavailable at connect time; downstream captures will omit the optics block");
                        None
                    }
                },
                sensor_height_px: match cam.camera_y_size().await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!(camera_id = %config.id, error = %e, "camera_y_size unavailable at connect time; downstream captures will omit the optics block");
                        None
                    }
                },
                device: Some(cam),
            };
            debug!(
                camera_id = %config.id,
                max_adu = ?entry.max_adu,
                pixel_size_x_um = ?entry.pixel_size_x_um,
                pixel_size_y_um = ?entry.pixel_size_y_um,
                sensor_width_px = ?entry.sensor_width_px,
                sensor_height_px = ?entry.sensor_height_px,
                "camera connected successfully; cached invariant sensor metadata"
            );
            entry
        }
        Err(msg) => {
            error!(camera_id = %config.id, error = %msg, "failed to connect camera");
            CameraEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
                max_adu: None,
                pixel_size_x_um: None,
                pixel_size_y_um: None,
                sensor_width_px: None,
                sensor_height_px: None,
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config;
    use crate::equipment::test_support::spawn_stub;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    fn camera_config_for(url: &str) -> config::CameraConfig {
        config::CameraConfig {
            id: "main-camera".to_string(),
            name: "test".to_string(),
            alpaca_url: url.to_string(),
            device_type: String::new(),
            device_number: 0,
            cooler_targets_c: Vec::new(),
            gain: None,
            offset: None,
            readout_time_estimate: None,
            auth: None,
        }
    }

    /// Stub server that advertises one Camera at index 0, accepts
    /// `set_connected(true)`, and replies to all five invariant property
    /// reads. Verifies `connect_camera` populates the cache fields when
    /// every Alpaca call succeeds.
    #[tokio::test]
    async fn connect_camera_success_populates_invariant_metadata_cache() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Camera 0",
                            "DeviceType": "Camera",
                            "DeviceNumber": 0,
                            "UniqueID": "test-camera-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/maxadu",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 65535,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/pixelsizex",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 3.76,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/pixelsizey",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 3.76,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/cameraxsize",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 1024,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/cameraysize",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 1024,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            );

        let stub = spawn_stub(app).await;
        let entry = connect_camera(&camera_config_for(&stub.url()), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(entry.max_adu, Some(65535));
        assert_eq!(entry.pixel_size_x_um, Some(3.76));
        assert_eq!(entry.pixel_size_y_um, Some(3.76));
        assert_eq!(entry.sensor_width_px, Some(1024));
        assert_eq!(entry.sensor_height_px, Some(1024));
    }

    /// Connect-time read failures (here: `maxadu` returns an ASCOM error)
    /// must scope to the single field. The entry is still connected; the
    /// other four properties populate normally; only `max_adu` is `None`.
    /// Pins the "best-effort metadata" posture documented on `CameraEntry`.
    #[tokio::test]
    async fn connect_camera_property_read_failure_scopes_to_single_field() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Camera 0",
                            "DeviceType": "Camera",
                            "DeviceNumber": 0,
                            "UniqueID": "test-camera-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/maxadu",
                get(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1024,
                        "ErrorMessage": "simulated maxadu failure"
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/pixelsizex",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 3.76,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/pixelsizey",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 3.76,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/cameraxsize",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 1024,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/cameraysize",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": 1024,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            );

        let stub = spawn_stub(app).await;
        let entry = connect_camera(&camera_config_for(&stub.url()), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(
            entry.max_adu, None,
            "max_adu must be None when the connect-time read fails"
        );
        assert_eq!(
            entry.pixel_size_x_um,
            Some(3.76),
            "pixel_size_x must still cache when its own read succeeds"
        );
        assert_eq!(entry.pixel_size_y_um, Some(3.76));
        assert_eq!(entry.sensor_width_px, Some(1024));
        assert_eq!(entry.sensor_height_px, Some(1024));
    }

    /// A malformed `alpaca_url` makes `build_alpaca_client` fail before
    /// the retry loop is entered, so `connect_camera` returns the
    /// disconnected entry from the early-return arm: no device, every
    /// cached metadata field `None`. No stub server is involved.
    #[tokio::test]
    async fn connect_camera_client_build_failure_returns_disconnected_entry() {
        let entry = connect_camera(&camera_config_for("not-a-url"), None).await;
        assert!(
            !entry.connected,
            "entry must be disconnected when the client cannot be built"
        );
        assert!(entry.device.is_none(), "no device should be held");
        assert_eq!(entry.max_adu, None);
        assert_eq!(entry.pixel_size_x_um, None);
        assert_eq!(entry.pixel_size_y_um, None);
        assert_eq!(entry.sensor_width_px, None);
        assert_eq!(entry.sensor_height_px, None);
    }

    /// When the Alpaca server advertises no camera at the requested
    /// `device_number`, the connect closure returns `Permanent` (so the
    /// retry loop exits immediately, no backoff), `retry_connect_attempt`
    /// surfaces `Err`, and `connect_camera` takes the `Err(msg)` arm: a
    /// disconnected entry with no device and no cached metadata. Covers
    /// the device-not-found branch and the whole failure-return block.
    #[tokio::test]
    async fn connect_camera_device_not_found_returns_disconnected_entry() {
        // One camera advertised at index 0; the config asks for index 1.
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async {
                Json(serde_json::json!({
                    "Value": [{
                        "DeviceName": "Camera 0",
                        "DeviceType": "Camera",
                        "DeviceNumber": 0,
                        "UniqueID": "test-camera-uid"
                    }],
                    "ErrorNumber": 0,
                    "ErrorMessage": ""
                }))
            }),
        );

        let stub = spawn_stub(app).await;
        let mut config = camera_config_for(&stub.url());
        config.device_number = 1;
        let entry = connect_camera(&config, None).await;

        assert!(
            !entry.connected,
            "entry must be disconnected when no camera is found at the index"
        );
        assert!(entry.device.is_none(), "no device should be held");
        assert_eq!(entry.max_adu, None);
        assert_eq!(entry.pixel_size_x_um, None);
        assert_eq!(entry.pixel_size_y_um, None);
        assert_eq!(entry.sensor_width_px, None);
        assert_eq!(entry.sensor_height_px, None);
    }

    /// A failed `get_devices` maps to `Transient`, so the connect loop
    /// exhausts all `CONNECT_ATTEMPTS` and `connect_camera` returns a
    /// disconnected entry. `start_paused` advances the 1 s + 2 s backoff
    /// in virtual time, so the retries don't slow the suite.
    #[tokio::test(start_paused = true)]
    async fn connect_camera_get_devices_error_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "simulated get_devices failure",
                )
            }),
        );

        let stub = spawn_stub(app).await;
        let entry = connect_camera(&camera_config_for(&stub.url()), None).await;
        assert!(
            !entry.connected,
            "entry must be disconnected when get_devices keeps failing"
        );
        assert!(entry.device.is_none(), "no device should be held");
        assert_eq!(entry.max_adu, None);
        assert_eq!(entry.sensor_height_px, None);
    }

    /// A `set_connected` error maps to `Transient`: the camera is found,
    /// but turning it on fails on every attempt, so the loop gives up and
    /// `connect_camera` returns a disconnected entry. `start_paused`
    /// collapses the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_camera_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Camera 0",
                            "DeviceType": "Camera",
                            "DeviceNumber": 0,
                            "UniqueID": "test-camera-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );

        let stub = spawn_stub(app).await;
        let entry = connect_camera(&camera_config_for(&stub.url()), None).await;
        assert!(
            !entry.connected,
            "entry must be disconnected when set_connected keeps failing"
        );
        assert!(entry.device.is_none(), "no device should be held");
        assert_eq!(entry.max_adu, None);
        assert_eq!(entry.sensor_height_px, None);
    }
}

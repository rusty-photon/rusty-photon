use std::sync::Arc;

use ascom_alpaca::api::camera::SensorType;
use ascom_alpaca::api::Camera;
use tracing::{debug, error};

use super::binding::{establish_listed, EstablishError};
use super::session::DeviceSession;
use crate::config;

/// Invariant physical-sensor properties, cached per established session.
///
/// Read once, right after the device reports `Connected = true` (pixel
/// pitch, array dimensions, the camera model's facts). They cannot
/// change for the life of the connection, so `do_capture` consumes them
/// from this cache instead of paying one Alpaca round-trip per property
/// per exposure — per Tenet 1 ("don't re-fetch invariant data").
/// `MaxADU` is not among them: a driver may tie it to the readout mode,
/// which any client can change, so a capture reads it live. Each field
/// is `Option<...>` so a transient read failure at session-establish
/// time simply drops that piece of metadata (downstream the document's
/// `optics` block degrades gracefully) rather than refusing to register
/// the camera. The reconnect supervisor re-reads the whole set on every
/// re-established session — a service restart may have put a different
/// device behind the same config entry — and installs the fresh reads
/// with the fresh handle in one step, so the two cannot be paired
/// across sessions.
#[derive(Clone, Debug, Default)]
pub struct CameraInvariants {
    /// The driver's ASCOM `Name` (e.g. `QHY600M`), persisted as the
    /// sidecar's `camera_name` and the FITS header's `INSTRUME`. Not a
    /// sensor property, but just as fixed for the life of a session.
    pub name: Option<String>,
    /// `PixelSizeX` in microns. Fed into the sidecar's `optics` block
    /// alongside `pixel_size_y_um`.
    pub pixel_size_x_um: Option<f64>,
    /// `PixelSizeY` in microns.
    pub pixel_size_y_um: Option<f64>,
    /// `CameraXSize` in pixels. Feeds the `optics` block; combined with
    /// `pixel_size_x_um` and the operator-supplied `focal_length_mm` to
    /// derive pixel-scale and FOV.
    pub sensor_width_px: Option<u32>,
    /// `CameraYSize` in pixels.
    pub sensor_height_px: Option<u32>,
    /// `MaxBinX` — the largest horizontal binning factor the camera
    /// accepts. Bounds the `binning` a capture may ask for; `None`
    /// (the connect-time read failed) skips the check and leaves the
    /// driver as the backstop.
    pub max_bin_x: Option<u8>,
    /// `MaxBinY`, the vertical counterpart of `max_bin_x`.
    pub max_bin_y: Option<u8>,
    /// `CanAsymmetricBin` — whether the camera accepts `x != y`.
    /// `None` under the same rule as `max_bin_x`.
    pub can_asymmetric_bin: Option<bool>,
    /// `SensorType` — mono, multi-plane colour, or which colour mosaic.
    /// Persisted as the sidecar's `sensor_type`.
    pub sensor_type: Option<SensorType>,
    /// `BayerOffsetX`, the column offset of the mosaic `sensor_type`
    /// names. Not read from a camera that reports itself monochrome,
    /// whose driver refuses it.
    pub bayer_offset_x: Option<u8>,
    /// `BayerOffsetY`, under the same rule as `bayer_offset_x`.
    pub bayer_offset_y: Option<u8>,
    /// `ReadoutModes` — the names a capture's `ReadoutMode` index is
    /// looked up in for the sidecar's `readout_mode_name`.
    pub readout_modes: Option<Vec<String>>,
    /// `CanGetCoolerPower` — whether a capture reads `CoolerPower`. A
    /// driver that cannot report power may still answer that read, so
    /// `None` (the connect-time read failed) skips it as `false` does.
    pub can_get_cooler_power: Option<bool>,
}

/// Per-camera runtime state held by the equipment registry.
pub struct CameraEntry {
    pub id: String,
    pub config: config::CameraConfig,
    /// The handle and the session's cached metadata, in one slot:
    /// [`DeviceSession`] installs them together and [`Self::snapshot`]
    /// serves them together, so a capture cannot pair one session's
    /// handle with another's sensor geometry.
    pub session: DeviceSession<dyn Camera, CameraInvariants>,
    /// Held by a capture from its first frame-geometry write until its
    /// image is downloaded (rp.md § Capture Tool Details, "Binning" →
    /// Concurrency), so captures through one camera run one at a time
    /// and cannot interleave their geometry writes; held on by a
    /// cancelled capture until its camera is idle, and by a train lease
    /// that holds the camera (§ Train Lease).
    capture_slot: super::DeviceSlot,
    /// Held by a capture of a frame that needs no sky (a dark, a bias, a
    /// panel flat) from the moment it holds the camera until the
    /// capture ends, and by the unsafe transition while it aborts this
    /// camera's exposure (rp.md § `SafetyMonitor` Polling, step 3). The
    /// transition skips a camera whose indoor slot is held, so such a
    /// frame runs on, and a capture of one that starts during the abort
    /// waits for the abort rather than starting into it.
    indoor_slot: super::DeviceSlot,
}

impl CameraEntry {
    #[must_use]
    pub fn new(
        id: String,
        config: config::CameraConfig,
        session: DeviceSession<dyn Camera, CameraInvariants>,
    ) -> Self {
        Self {
            id,
            config,
            session,
            capture_slot: super::DeviceSlot::default(),
            indoor_slot: super::DeviceSlot::default(),
        }
    }

    /// Wait for this camera's capture slot. The caller races it against
    /// its own cancellation; dropping the guard hands the slot to the
    /// next waiter.
    pub async fn capture_slot(&self) -> super::SlotGuard {
        self.capture_slot.take("camera", &self.id).await
    }

    /// Wait for this camera's indoor slot, for an exposure of a frame
    /// that needs no sky. Only the unsafe transition's abort of this
    /// camera holds it against such a capture, and only for one device
    /// call.
    pub async fn indoor_slot(&self) -> super::SlotGuard {
        self.indoor_slot.take("camera indoor slot", &self.id).await
    }

    /// The indoor slot if no frame that needs no sky is exposing on this
    /// camera; `None` while one is.
    #[must_use]
    pub fn try_indoor_slot(&self) -> Option<super::SlotGuard> {
        self.indoor_slot.try_take()
    }

    /// The registered-but-unconnected entry every failed connect path
    /// returns: the camera stays addressable (and recoverable by the
    /// reconnect supervisor) with no device handle and no cached
    /// metadata.
    fn disconnected(config: &config::CameraConfig) -> Self {
        Self::new(
            config.id.clone(),
            config.clone(),
            DeviceSession::disconnected(),
        )
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn Camera>> {
        self.session.device()
    }

    /// The cached invariant sensor metadata, for a caller that needs
    /// no handle to go with it.
    #[must_use]
    pub fn invariants(&self) -> CameraInvariants {
        self.session.metadata()
    }

    /// The device handle and the metadata read from the same session.
    /// `None` when no session has ever been established.
    #[must_use]
    pub fn snapshot(&self) -> Option<(Arc<dyn Camera>, CameraInvariants)> {
        self.session.snapshot()
    }
}

/// One auxiliary device read — a session-establish invariant, or a
/// capture's per-frame metadata: a failure only drops *that* field
/// (logged with its downstream consequence), never the whole session or
/// capture.
pub(crate) fn optional_read<T, E: std::fmt::Display>(
    result: Result<T, E>,
    camera_id: &str,
    consequence: &str,
) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            debug!(camera_id = %camera_id, error = %e, "{}", consequence);
            None
        }
    }
}

/// Locate the configured camera on its Alpaca server, switch it on, and
/// read the invariant sensor metadata — the shared routine behind the
/// startup connect and the reconnect supervisor's re-establish (rp.md
/// § Device Session Recovery).
pub(super) async fn establish_camera(
    config: &config::CameraConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<(Arc<dyn Camera>, CameraInvariants), EstablishError> {
    let cam = establish_listed(config, ca_cert_path).await?;

    // The Alpaca device is now Connected — the driver name and the
    // camera-model properties below are invariant for the life of the
    // session, so
    // they are read exactly once here and served from the cache to
    // every subsequent `do_capture`. Each read is independent: a
    // failure on one property only drops *that* field, not the whole
    // session. The reads intentionally happen after `set_connected`
    // succeeds because some Alpaca drivers reject property reads on
    // disconnected devices.
    let sensor_type = optional_read(cam.sensor_type().await, &config.id, "sensor_type unavailable at session-establish time; downstream captures will omit sensor_type");
    let (bayer_offset_x, bayer_offset_y) = if sensor_type == Some(SensorType::Monochrome) {
        (None, None)
    } else {
        (
            optional_read(cam.bayer_offset_x().await, &config.id, "bayer_offset_x unavailable at session-establish time; downstream captures will omit bayer_offset_x"),
            optional_read(cam.bayer_offset_y().await, &config.id, "bayer_offset_y unavailable at session-establish time; downstream captures will omit bayer_offset_y"),
        )
    };
    let invariants = CameraInvariants {
        name: optional_read(cam.name().await, &config.id, "name unavailable at session-establish time; downstream captures will omit camera_name and INSTRUME"),
        pixel_size_x_um: optional_read(cam.pixel_size_x().await, &config.id, "pixel_size_x unavailable at session-establish time; downstream captures will record no x pixel size, pixel scale or FOV width, nor XPIXSZ"),
        pixel_size_y_um: optional_read(cam.pixel_size_y().await, &config.id, "pixel_size_y unavailable at session-establish time; downstream captures will record no y pixel size, pixel scale or FOV height, nor YPIXSZ"),
        sensor_width_px: optional_read(cam.camera_x_size().await, &config.id, "camera_x_size unavailable at session-establish time; downstream captures will record no sensor width or FOV width"),
        sensor_height_px: optional_read(cam.camera_y_size().await, &config.id, "camera_y_size unavailable at session-establish time; downstream captures will record no sensor height or FOV height"),
        max_bin_x: optional_read(cam.max_bin_x().await, &config.id, "max_bin_x unavailable at session-establish time; a capture's binning will not be range-checked before it reaches the driver"),
        max_bin_y: optional_read(cam.max_bin_y().await, &config.id, "max_bin_y unavailable at session-establish time; a capture's binning will not be range-checked before it reaches the driver"),
        can_asymmetric_bin: optional_read(cam.can_asymmetric_bin().await, &config.id, "can_asymmetric_bin unavailable at session-establish time; an asymmetric binning will not be rejected before it reaches the driver"),
        sensor_type,
        bayer_offset_x,
        bayer_offset_y,
        readout_modes: optional_read(cam.readout_modes().await, &config.id, "readout_modes unavailable at session-establish time; downstream captures will omit readout_mode_name"),
        can_get_cooler_power: optional_read(cam.can_get_cooler_power().await, &config.id, "can_get_cooler_power unavailable at session-establish time; downstream captures will not read cooler_power_pct"),
    };
    Ok((cam, invariants))
}

pub(super) async fn connect_camera(
    config: &config::CameraConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> CameraEntry {
    debug!(camera_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to camera");

    match establish_camera(config, ca_cert_path).await {
        Ok((cam, invariants)) => {
            debug!(
                camera_id = %config.id,
                name = ?invariants.name,
                pixel_size_x_um = ?invariants.pixel_size_x_um,
                pixel_size_y_um = ?invariants.pixel_size_y_um,
                sensor_width_px = ?invariants.sensor_width_px,
                sensor_height_px = ?invariants.sensor_height_px,
                max_bin_x = ?invariants.max_bin_x,
                max_bin_y = ?invariants.max_bin_y,
                can_asymmetric_bin = ?invariants.can_asymmetric_bin,
                sensor_type = ?invariants.sensor_type,
                bayer_offset_x = ?invariants.bayer_offset_x,
                bayer_offset_y = ?invariants.bayer_offset_y,
                readout_modes = ?invariants.readout_modes,
                can_get_cooler_power = ?invariants.can_get_cooler_power,
                "camera connected successfully; cached invariant sensor metadata"
            );
            CameraEntry::new(
                config.id.clone(),
                config.clone(),
                DeviceSession::connected_with(cam, invariants),
            )
        }
        Err(msg) => {
            error!(camera_id = %config.id, error = %msg, "failed to connect camera");
            CameraEntry::disconnected(config)
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
            unique_id: None,
            cooler_targets_c: Vec::new(),
            gain: None,
            offset: None,
            readout_time_estimate: None,
            auth: None,
        }
    }

    /// Stub server that advertises one Camera at index 0, accepts
    /// `set_connected(true)`, and replies to the four sensor-geometry
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
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        let inv = entry.invariants();
        assert_eq!(inv.pixel_size_x_um, Some(3.76));
        assert_eq!(inv.pixel_size_y_um, Some(3.76));
        assert_eq!(inv.sensor_width_px, Some(1024));
        assert_eq!(inv.sensor_height_px, Some(1024));
    }

    /// Connect-time read failures (here: `pixelsizex` returns an ASCOM
    /// error) must scope to the single field. The entry is still
    /// connected; the other three properties populate normally; only
    /// `pixel_size_x_um` is `None`.
    /// Pins the "best-effort metadata" posture documented on
    /// `CameraInvariants`.
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
                "/api/v1/camera/0/pixelsizex",
                get(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1024,
                        "ErrorMessage": "simulated pixelsizex failure"
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
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        let inv = entry.invariants();
        assert_eq!(
            inv.pixel_size_x_um, None,
            "pixel_size_x must be None when the connect-time read fails"
        );
        assert_eq!(
            inv.pixel_size_y_um,
            Some(3.76),
            "pixel_size_y must still cache when its own read succeeds"
        );
        assert_eq!(inv.sensor_width_px, Some(1024));
        assert_eq!(inv.sensor_height_px, Some(1024));
    }

    /// Stub server that advertises one Camera at index 0, accepts
    /// `set_connected(true)`, and answers each listed property read with
    /// its value. Any other read gets a 404, which drops its field.
    fn camera_answering(properties: &[(&'static str, serde_json::Value)]) -> Router {
        let mut app = Router::new()
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
            );
        for (property, value) in properties {
            let value = value.clone();
            app = app.route(
                &format!("/api/v1/camera/0/{property}"),
                get(move || {
                    let value = value.clone();
                    async move {
                        Json(serde_json::json!({
                            "Value": value,
                            "ErrorNumber": 0,
                            "ErrorMessage": ""
                        }))
                    }
                }),
            );
        }
        app
    }

    #[tokio::test]
    async fn connect_camera_caches_a_colour_cameras_sensor_type_and_bayer_offsets() {
        let stub = spawn_stub(camera_answering(&[
            ("sensortype", serde_json::json!(2)),
            ("bayeroffsetx", serde_json::json!(1)),
            ("bayeroffsety", serde_json::json!(0)),
        ]))
        .await;
        let inv = connect_camera(&camera_config_for(&stub.url()), None)
            .await
            .invariants();
        assert_eq!(inv.sensor_type, Some(SensorType::RGGB));
        assert_eq!(inv.bayer_offset_x, Some(1));
        assert_eq!(inv.bayer_offset_y, Some(0));
    }

    /// A mono camera's driver refuses the Bayer offsets, so they are not
    /// read. The stub answers them anyway, so a read would show.
    #[tokio::test]
    async fn connect_camera_reads_no_bayer_offsets_from_a_monochrome_camera() {
        let stub = spawn_stub(camera_answering(&[
            ("sensortype", serde_json::json!(0)),
            ("bayeroffsetx", serde_json::json!(1)),
            ("bayeroffsety", serde_json::json!(1)),
        ]))
        .await;
        let inv = connect_camera(&camera_config_for(&stub.url()), None)
            .await
            .invariants();
        assert_eq!(inv.sensor_type, Some(SensorType::Monochrome));
        assert_eq!(inv.bayer_offset_x, None);
        assert_eq!(inv.bayer_offset_y, None);
    }

    /// A camera whose sensor type is unknown may still have a mosaic, so
    /// its offsets are read.
    #[tokio::test]
    async fn connect_camera_reads_the_bayer_offsets_when_the_sensor_type_is_unknown() {
        let stub = spawn_stub(camera_answering(&[
            ("bayeroffsetx", serde_json::json!(0)),
            ("bayeroffsety", serde_json::json!(1)),
        ]))
        .await;
        let inv = connect_camera(&camera_config_for(&stub.url()), None)
            .await
            .invariants();
        assert_eq!(inv.sensor_type, None);
        assert_eq!(inv.bayer_offset_x, Some(0));
        assert_eq!(inv.bayer_offset_y, Some(1));
    }

    #[tokio::test]
    async fn connect_camera_caches_the_readout_modes_and_whether_cooler_power_is_readable() {
        let stub = spawn_stub(camera_answering(&[
            ("readoutmodes", serde_json::json!(["Normal", "High Gain"])),
            ("cangetcoolerpower", serde_json::json!(true)),
        ]))
        .await;
        let inv = connect_camera(&camera_config_for(&stub.url()), None)
            .await
            .invariants();
        assert_eq!(
            inv.readout_modes,
            Some(vec!["Normal".to_string(), "High Gain".to_string()])
        );
        assert_eq!(inv.can_get_cooler_power, Some(true));
    }

    /// A malformed `alpaca_url` makes `build_alpaca_client` fail before
    /// the retry loop is entered, so `connect_camera` returns the
    /// disconnected entry: no device, every cached metadata field `None`.
    /// No stub server is involved.
    #[tokio::test]
    async fn connect_camera_client_build_failure_returns_disconnected_entry() {
        let entry = connect_camera(&camera_config_for("not-a-url"), None).await;
        assert!(
            !entry.is_connected(),
            "entry must be disconnected when the client cannot be built"
        );
        assert!(entry.device().is_none(), "no device should be held");
        let inv = entry.invariants();
        assert_eq!(inv.pixel_size_x_um, None);
        assert_eq!(inv.pixel_size_y_um, None);
        assert_eq!(inv.sensor_width_px, None);
        assert_eq!(inv.sensor_height_px, None);
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
            !entry.is_connected(),
            "entry must be disconnected when no camera is found at the index"
        );
        assert!(entry.device().is_none(), "no device should be held");
        let inv = entry.invariants();
        assert_eq!(inv.pixel_size_x_um, None);
        assert_eq!(inv.pixel_size_y_um, None);
        assert_eq!(inv.sensor_width_px, None);
        assert_eq!(inv.sensor_height_px, None);
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
            !entry.is_connected(),
            "entry must be disconnected when get_devices keeps failing"
        );
        assert!(entry.device().is_none(), "no device should be held");
        let inv = entry.invariants();
        assert_eq!(inv.sensor_height_px, None);
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
            !entry.is_connected(),
            "entry must be disconnected when set_connected keeps failing"
        );
        assert!(entry.device().is_none(), "no device should be held");
        let inv = entry.invariants();
        assert_eq!(inv.sensor_height_px, None);
    }

    /// A stub camera whose `Connected = true` answers `error_number`,
    /// counting the attempts.
    fn refusing_camera(
        error_number: u16,
        attempts: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "SVBony camera on p9 (placeholder)",
                            "DeviceType": "Camera",
                            "DeviceNumber": 0,
                            "UniqueID": "placeholder:svbony-camera:p9"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(move || {
                    let attempts = attempts.clone();
                    async move {
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Json(serde_json::json!({
                            "ErrorNumber": error_number,
                            "ErrorMessage": "no working camera is enumerated on p9"
                        }))
                    }
                }),
            )
    }

    /// A placeholder's refusal is permanent for the pass: one attempt, no
    /// backoff, and the driver's reason in the error. Real time, not paused:
    /// a paused clock auto-advances while the stub's HTTP is in flight and
    /// times `get_devices` out before the stub answers.
    #[tokio::test]
    async fn a_placeholder_is_tried_once_per_pass() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let stub = spawn_stub(refusing_camera(
            rusty_photon_doctor_checks::claims::PLACEHOLDER_ERROR_CODE,
            attempts.clone(),
        ))
        .await;
        let Err(error) = establish_camera(&camera_config_for(&stub.url()), None).await else {
            panic!("a placeholder connected");
        };
        assert!(!error.is_identity_refusal(), "{error}");
        let error = error.to_string();
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{error}"
        );
        assert!(error.contains("placeholder"), "{error}");
        assert!(
            error.contains("no working camera is enumerated on p9"),
            "{error}"
        );
    }
}

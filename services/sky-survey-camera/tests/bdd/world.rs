// Phase-3 is shipping in slices: fields populated by later slices'
// step bodies (e.g. last_image_dimensions, last_error) are read only
// by step files that haven't landed yet. Silence dead-code so the
// husky precommit hook (`-D warnings`) stays green between slices.
#![allow(dead_code)]

use ascom_alpaca::api::{Camera, TypedDevice};
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tempfile::TempDir;

/// Behaviour the `SkyView` stub is currently configured with. Cloned
/// out of the `RwLock` in the request handler, so each variant must
/// own its data.
#[derive(Debug, Clone)]
pub enum StubBehavior {
    /// HEAD and GET both 200 with empty body.
    Ok,
    /// HEAD 200; GET 200 with the given FITS payload.
    ServingFits(Vec<u8>),
    /// HEAD 200; GET 500.
    Status500,
    /// HEAD 200; GET hangs forever (used to keep an exposure
    /// in-flight for E2 / A1 / A2).
    Hold,
    /// HEAD 200; GET 200 with non-FITS bytes (S6).
    Malformed,
}

#[derive(Debug)]
pub struct StubState {
    pub behavior: Arc<RwLock<StubBehavior>>,
    pub get_count: Arc<AtomicU32>,
}

/// Behaviour the in-test ASCOM Telescope stub serves on each
/// `right_ascension` / `declination` read. Drives the F1/F2/F5
/// follow-mode scenarios.
#[derive(Debug, Clone)]
pub enum MountStubBehavior {
    /// Serve `Value=<ra_hours>` / `Value=<dec_deg>` with `ErrorNumber=0`.
    Ok { ra_hours: f64, dec_deg: f64 },
    /// Serve `ErrorNumber=1024` ("driver-specific") on every read —
    /// drives F2.
    AscomError,
    /// `/management/v1/configureddevices` returns an empty list, so
    /// device resolution fails — drives the `DeviceNotFound` branch.
    NoTelescope,
}

#[derive(Debug)]
pub struct MountStubState {
    pub behavior: Arc<RwLock<MountStubBehavior>>,
    pub read_count: Arc<AtomicU32>,
}

/// Behaviour the in-test ASCOM Rotator stub serves on each `position`
/// read. Drives the F8 follow-mode scenario. Mirrors
/// [`MountStubBehavior`].
#[derive(Debug, Clone)]
pub enum RotatorStubBehavior {
    /// Serve `Value=<position_angle>` with `ErrorNumber=0`.
    Ok { position_angle: f64 },
    /// Serve `ErrorNumber=1024` ("driver-specific") on every read.
    AscomError,
}

#[derive(Debug)]
pub struct RotatorStubState {
    pub behavior: Arc<RwLock<RotatorStubBehavior>>,
    pub read_count: Arc<AtomicU32>,
}

/// Build a minimal valid FITS payload of the given dimensions filled
/// with zero pixels (BITPIX = 32). Suitable for both happy-path tests
/// and cache-hit pre-seeding.
pub fn make_zero_fits(width: u32, height: u32) -> Vec<u8> {
    fn push(header: &mut String, line: String) {
        let mut padded = format!("{line:<80}");
        padded.truncate(80);
        header.push_str(&padded);
    }
    let mut header = String::new();
    push(&mut header, "SIMPLE  =                    T".to_string());
    push(&mut header, "BITPIX  =                   32".to_string());
    push(&mut header, "NAXIS   =                    2".to_string());
    push(&mut header, format!("NAXIS1  = {width:>20}"));
    push(&mut header, format!("NAXIS2  = {height:>20}"));
    push(&mut header, "END".to_string());
    while !header.len().is_multiple_of(2880) {
        header.push(' ');
    }
    let mut bytes = header.into_bytes();
    let data_len = (width as usize) * (height as usize) * 4;
    bytes.extend(vec![0u8; data_len]);
    while !bytes.len().is_multiple_of(2880) {
        bytes.push(0);
    }
    bytes
}

/// Cucumber World for sky-survey-camera BDD scenarios.
#[derive(Debug, Default, World)]
pub struct SkySurveyCameraWorld {
    /// Spawned binary handle (set when the service is started).
    pub service: Option<ServiceHandle>,

    /// Temp dir holding config.json + cache dir for the running scenario.
    pub temp_dir: Option<TempDir>,

    /// Path to the config.json the service was started with.
    pub config_path: Option<PathBuf>,

    /// Parsed JSON body of the last config.get / config.apply / config.schema action.
    pub last_response: Option<Value>,
    /// Result of the last `supported_actions` query.
    pub last_supported_actions: Option<Vec<String>>,
    /// ASCOM error code (raw) from the last config action that failed.
    pub last_action_error_code: Option<u16>,

    /// Optics config under construction by Given steps.
    pub focal_length_mm: Option<f64>,
    pub pixel_size_x_um: Option<f64>,
    pub pixel_size_y_um: Option<f64>,
    pub sensor_width_px: Option<u32>,
    pub sensor_height_px: Option<u32>,

    /// Initial pointing baked into the config (overridden at runtime by
    /// POST /sky-survey/position).
    pub initial_ra_deg: f64,
    pub initial_dec_deg: f64,
    pub initial_rotation_deg: f64,

    /// Override for `cache_dir`; if set it's substituted into the config
    /// instead of the default `<temp_dir>/cache`. Used to feed
    /// connection-lifecycle scenarios a deliberately non-writable path.
    pub cache_dir_override: Option<PathBuf>,

    /// Override for the survey endpoint URL injected into config.
    pub survey_endpoint_override: Option<String>,

    /// Shared state of the `SkyView` stub server (None until spawned).
    pub stub_state: Option<Arc<StubState>>,

    /// Shared state of the ASCOM Telescope stub (None until spawned).
    pub mount_stub_state: Option<Arc<MountStubState>>,

    /// Shared state of the ASCOM Rotator stub (None until spawned).
    pub rotator_stub_state: Option<Arc<RotatorStubState>>,

    /// When set, the camera is configured in telescope-follow mode
    /// pointing at this URL. Built by `spawn_mount_stub`.
    pub telescope_endpoint_override: Option<String>,

    /// When set, `pointing.rotator` is emitted pointing at this URL so
    /// follow mode sources rotation from the rotator. Built by
    /// `spawn_rotator_stub`.
    pub rotator_endpoint_override: Option<String>,

    /// Configured RA offset for follow mode (arcsec, signed).
    pub telescope_offset_ra_arcsec: f64,

    /// Configured Dec offset for follow mode (arcsec, signed).
    pub telescope_offset_dec_arcsec: f64,

    /// Survey backend choice.
    pub survey_name: Option<String>,

    /// HTTP client reused across step calls for performance.
    pub http: Option<reqwest::Client>,

    /// Captured outcomes for Then assertions.
    pub last_http_status: Option<u16>,
    pub last_http_body: Option<String>,
    pub last_ascom_error: Option<u32>,
    pub last_image_dimensions: Option<(u32, u32)>,
    pub last_error: Option<String>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for SkySurveyCameraWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The tls-auth smoke's base config plus the two fields
        // `start_with_tls_auth` normally fills at spawn time: the
        // mandatory `survey.cache_dir` (doctor only parses the config,
        // so the directory need not exist) and a plain `server` block.
        let mut config = TlsAuthSmokeWorld::base_test_config(self);
        config["survey"]["cache_dir"] = Value::String(
            std::env::temp_dir()
                .join("sky-survey-camera-doctor-smoke-cache")
                .to_string_lossy()
                .to_string(),
        );
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl TlsAuthSmokeWorld for SkySurveyCameraWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    /// The `build_config_json` defaults, minus the two runtime-dependent
    /// `survey` fields (`endpoint`, `cache_dir`) that
    /// [`Self::start_with_tls_auth`] fills in once the `SkyView` stub and the
    /// temp dir exist.
    fn base_test_config(&self) -> serde_json::Value {
        serde_json::json!({
            "device": {
                "name": "Test Sky Survey Camera",
                "unique_id": "sky-survey-camera-test-001",
                "description": "BDD test instance",
            },
            "optics": {
                "focal_length_mm": 1000.0,
                "pixel_size_x_um": 3.76,
                "pixel_size_y_um": 3.76,
                "sensor_width_px": 640,
                "sensor_height_px": 480,
            },
            "pointing": {
                "initial_ra_deg": 0.0,
                "initial_dec_deg": 0.0,
                "initial_rotation_deg": 0.0,
            },
            "survey": {
                "name": "DSS2 Red",
                "request_timeout": "5s",
            },
        })
    }

    async fn start_with_tls_auth(&mut self, mut config: serde_json::Value) {
        // Point the survey endpoint at a local stub so the config never
        // references the real SkyView URL (the scenario never fetches).
        self.spawn_skyview_stub().await;
        config["survey"]["endpoint"] = Value::String(
            self.survey_endpoint_override
                .clone()
                .expect("stub endpoint set by spawn_skyview_stub"),
        );
        config["survey"]["cache_dir"] =
            Value::String(self.cache_dir().to_string_lossy().to_string());
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.service = Some(handle);
    }
}

impl SkySurveyCameraWorld {
    pub fn http(&mut self) -> reqwest::Client {
        self.http
            .get_or_insert_with(|| {
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .expect("failed to build reqwest client")
            })
            .clone()
    }

    pub fn temp_dir(&mut self) -> &TempDir {
        self.temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"))
    }

    pub fn cache_dir(&mut self) -> PathBuf {
        if let Some(override_path) = self.cache_dir_override.clone() {
            return override_path;
        }
        self.temp_dir().path().join("cache")
    }

    pub fn build_config_json(&mut self) -> Value {
        let cache_dir = self.cache_dir().to_string_lossy().to_string();
        let mut survey = serde_json::json!({
            "name": self.survey_name.clone().unwrap_or_else(|| "DSS2 Red".to_string()),
            "request_timeout": "5s",
            "cache_dir": cache_dir,
        });
        if let Some(endpoint) = &self.survey_endpoint_override {
            survey["endpoint"] = Value::String(endpoint.clone());
        }
        let mut pointing = serde_json::json!({
            "initial_ra_deg": self.initial_ra_deg,
            "initial_dec_deg": self.initial_dec_deg,
            "initial_rotation_deg": self.initial_rotation_deg,
        });
        if let Some(url) = &self.telescope_endpoint_override {
            pointing["telescope"] = serde_json::json!({
                "alpaca_url": url,
                "device_number": 0,
                "offset_ra_arcsec": self.telescope_offset_ra_arcsec,
                "offset_dec_arcsec": self.telescope_offset_dec_arcsec,
                "request_timeout": "2s",
            });
        }
        if let Some(url) = &self.rotator_endpoint_override {
            pointing["rotator"] = serde_json::json!({
                "alpaca_url": url,
                "device_number": 0,
                "request_timeout": "2s",
            });
        }
        serde_json::json!({
            "device": {
                "name": "Test Sky Survey Camera",
                "unique_id": "sky-survey-camera-test-001",
                "description": "BDD test instance",
            },
            "optics": {
                "focal_length_mm": self.focal_length_mm.unwrap_or(1000.0),
                "pixel_size_x_um": self.pixel_size_x_um.unwrap_or(3.76),
                "pixel_size_y_um": self.pixel_size_y_um.unwrap_or(3.76),
                "sensor_width_px": self.sensor_width_px.unwrap_or(640),
                "sensor_height_px": self.sensor_height_px.unwrap_or(480),
            },
            "pointing": pointing,
            "survey": survey,
            "server": {
                "port": 0,
            },
        })
    }

    /// Spawn a stub `SkyView` server on `127.0.0.1:0` whose behaviour is
    /// stored in shared state and can be mutated mid-scenario. Points
    /// the survey endpoint at the stub and seeds the world's
    /// `stub_state` so step bodies can switch behaviours and inspect
    /// the GET counter.
    pub async fn spawn_skyview_stub(&mut self) {
        let state = Arc::new(StubState {
            behavior: Arc::new(RwLock::new(StubBehavior::Ok)),
            get_count: Arc::new(AtomicU32::new(0)),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind stub listener");
        let addr = listener.local_addr().expect("local_addr");
        let handler_state = Arc::clone(&state);
        let app = axum::Router::new()
            .fallback(axum::routing::any(handle_stub))
            .with_state(handler_state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.survey_endpoint_override = Some(format!("http://{addr}/"));
        self.stub_state = Some(state);
    }

    /// Backwards-compat alias used by step bodies that only care about
    /// the connection-time HEAD reachability check.
    pub async fn spawn_skyview_stub_ok(&mut self) {
        self.spawn_skyview_stub().await;
    }

    /// Spawn a tiny ASCOM Alpaca Telescope stub on `127.0.0.1:0`.
    /// Serves only the three endpoints `AlpacaMountReader` exercises:
    /// `GET /management/v1/configureddevices`,
    /// `GET /api/v1/telescope/0/rightascension`,
    /// `GET /api/v1/telescope/0/declination`.
    /// Records the initial behaviour and points
    /// `telescope_endpoint_override` at the bound address so
    /// `build_config_json` emits `pointing.telescope`.
    pub async fn spawn_mount_stub(&mut self, behavior: MountStubBehavior) {
        let state = Arc::new(MountStubState {
            behavior: Arc::new(RwLock::new(behavior)),
            read_count: Arc::new(AtomicU32::new(0)),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind mount stub listener");
        let addr = listener.local_addr().expect("local_addr");
        let app = axum::Router::new()
            .route(
                "/management/v1/configureddevices",
                axum::routing::get(handle_configured_devices),
            )
            .route(
                "/api/v1/telescope/0/rightascension",
                axum::routing::get(handle_right_ascension),
            )
            .route(
                "/api/v1/telescope/0/declination",
                axum::routing::get(handle_declination),
            )
            .with_state(Arc::clone(&state));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.telescope_endpoint_override = Some(format!("http://{addr}/"));
        self.mount_stub_state = Some(state);
    }

    pub fn set_mount_stub_behavior(&mut self, behavior: MountStubBehavior) {
        let state = self
            .mount_stub_state
            .as_ref()
            .expect("mount stub not spawned — call spawn_mount_stub first");
        *state.behavior.write().expect("mount stub rwlock") = behavior;
    }

    /// Spawn a tiny ASCOM Alpaca Rotator stub on `127.0.0.1:0`. Serves
    /// only the two endpoints `AlpacaRotatorReader` exercises:
    /// `GET /management/v1/configureddevices` and
    /// `GET /api/v1/rotator/0/position`. Points
    /// `rotator_endpoint_override` at the bound address so
    /// `build_config_json` emits `pointing.rotator`. Mirrors
    /// `spawn_mount_stub`.
    pub async fn spawn_rotator_stub(&mut self, behavior: RotatorStubBehavior) {
        let state = Arc::new(RotatorStubState {
            behavior: Arc::new(RwLock::new(behavior)),
            read_count: Arc::new(AtomicU32::new(0)),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind rotator stub listener");
        let addr = listener.local_addr().expect("local_addr");
        let app = axum::Router::new()
            .route(
                "/management/v1/configureddevices",
                axum::routing::get(handle_rotator_configured_devices),
            )
            .route(
                "/api/v1/rotator/0/position",
                axum::routing::get(handle_rotator_position),
            )
            .with_state(Arc::clone(&state));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.rotator_endpoint_override = Some(format!("http://{addr}/"));
        self.rotator_stub_state = Some(state);
    }

    pub fn set_rotator_stub_behavior(&mut self, behavior: RotatorStubBehavior) {
        let state = self
            .rotator_stub_state
            .as_ref()
            .expect("rotator stub not spawned — call spawn_rotator_stub first");
        *state.behavior.write().expect("rotator stub rwlock") = behavior;
    }

    pub fn set_stub_behavior(&mut self, behavior: StubBehavior) {
        let state = self
            .stub_state
            .as_ref()
            .expect("stub not spawned — call spawn_skyview_stub first");
        *state.behavior.write().expect("stub behavior rwlock") = behavior;
    }

    pub fn stub_get_count(&self) -> u32 {
        self.stub_state
            .as_ref()
            .map_or(0, |s| s.get_count.load(Ordering::Relaxed))
    }

    /// Point the survey endpoint at `127.0.0.1:1`, a low-numbered
    /// privileged loopback port that is essentially never bound in
    /// CI / dev environments — a connection attempt is refused
    /// immediately. (Privileged-but-unused, not "reserved" in the
    /// IANA sense.) If a future test environment binds port 1, swap
    /// this for an explicit `bind → drop` to guarantee refusal.
    pub fn set_unreachable_survey_endpoint(&mut self) {
        self.survey_endpoint_override = Some("http://127.0.0.1:1/".to_string());
    }

    /// Build a `cache_dir` whose parent path is a regular file rather
    /// than a directory, so that `mkdir -p` (a.k.a.
    /// `std::fs::create_dir_all`) reliably fails on every supported
    /// platform.
    pub fn set_unwritable_cache_dir(&mut self) {
        let blocker = self.temp_dir().path().join("blocker");
        std::fs::write(&blocker, b"").expect("failed to write blocker file");
        self.cache_dir_override = Some(blocker.join("cache"));
    }

    /// Write the accumulated config to `<temp_dir>/config.json` and
    /// spawn the service binary. Stores the handle on the world.
    pub async fn start_service(&mut self) {
        let config = self.build_config_json();
        let config_path = {
            let dir = self.temp_dir();
            let path = dir.path().join("config.json");
            std::fs::write(&path, config.to_string()).expect("failed to write config.json");
            path
        };
        let config_path_str = config_path
            .to_str()
            .expect("temp config path must be valid UTF-8 for ServiceHandle::start");
        let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path_str).await;
        self.config_path = Some(config_path);
        self.service = Some(handle);
    }

    pub fn base_url(&self) -> String {
        let handle = self.service.as_ref().expect("service not started");
        format!("http://127.0.0.1:{}", handle.port)
    }

    /// The OS-assigned port the spawned service bound.
    pub const fn bound_port(&self) -> u16 {
        self.service.as_ref().expect("service not started").port
    }

    /// Acquire a typed ASCOM Camera client, polling until the device is
    /// advertised.
    pub async fn acquire_camera(&self) -> Arc<dyn Camera> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.bound_port()));
        let client = AlpacaClient::new_from_addr(addr);
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Ok(mut devices) = client.get_devices().await {
                if let Some(TypedDevice::Camera(c)) = devices.next() {
                    return c;
                }
            }
        }
        panic!("sky-survey-camera did not advertise a Camera within 15s");
    }

    /// Call `config.get`, stash the parsed response, and return the `config`
    /// object so a When step can edit it and re-apply.
    pub async fn current_config(&mut self) -> Value {
        let camera = self.acquire_camera().await;
        let body = camera
            .action("config.get".to_string(), String::new())
            .await
            .expect("config.get failed");
        let parsed: Value = serde_json::from_str(&body).expect("config.get returned invalid JSON");
        let config = parsed
            .get("config")
            .cloned()
            .expect("config.get response missing `config`");
        self.last_response = Some(parsed);
        config
    }

    /// Call `config.apply` with `params`, stashing the parsed response.
    pub async fn call_config_apply(&mut self, params: Value) {
        let camera = self.acquire_camera().await;
        let body = camera
            .action("config.apply".to_string(), params.to_string())
            .await
            .expect("config.apply failed");
        self.last_response =
            Some(serde_json::from_str(&body).expect("config.apply returned invalid JSON"));
    }

    /// Poll `config.get` via a fresh client until `device.description` equals
    /// `expected`, tolerating the brief blip while the server rebinds.
    pub async fn wait_for_config_description(&self, expected: &str) {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.bound_port()));
        for _ in 0..80 {
            if try_get_description(addr).await.as_deref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("reloaded service did not report description {expected} within 20s");
    }

    /// PUT /api/v1/camera/0/connected — toggle ASCOM Connected.
    pub async fn set_camera_connected(&mut self, connected: bool) {
        let extra = [("Connected", connected.to_string())];
        self.put_camera("connected", &extra).await;
    }

    /// GET /api/v1/camera/0/{method} with the standard ASCOM envelope,
    /// capturing the response and any ASCOM `ErrorNumber` the way
    /// [`Self::put_camera`] does — so a scenario can assert what a *read*
    /// answered with, not only a command.
    ///
    /// Returns the parsed `ErrorNumber` (0 = success).
    pub async fn get_camera(&mut self, method: &str) -> u32 {
        let url = format!("{}/api/v1/camera/0/{method}", self.base_url());
        let client = self.http();
        let response = client
            .get(&url)
            .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET /{method} failed: {e}"));
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        self.last_http_status = Some(status.as_u16());
        self.last_http_body = Some(body.clone());
        let mut err_num = 0;
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            err_num = value
                .get("ErrorNumber")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            if err_num != 0 {
                self.last_ascom_error = Some(err_num);
            }
        }
        err_num
    }

    /// PUT /api/v1/camera/0/{method} with the given form parameters
    /// plus the standard ASCOM `ClientID` / `ClientTransactionID`
    /// envelope. Captures the response body, HTTP status, and any
    /// ASCOM `ErrorNumber` into the world for assertions.
    ///
    /// Returns the parsed `ErrorNumber` (0 = success).
    pub async fn put_camera(&mut self, method: &str, params: &[(&str, String)]) -> u32 {
        let url = format!("{}/api/v1/camera/0/{method}", self.base_url());
        let client = self.http();
        let mut form: Vec<(&str, String)> = Vec::with_capacity(params.len() + 2);
        form.push(("ClientID", "1".to_string()));
        form.push(("ClientTransactionID", "1".to_string()));
        for (k, v) in params {
            form.push((k, v.clone()));
        }
        let response = client
            .put(&url)
            .form(&form)
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT /{method} failed: {e}"));
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        self.last_http_status = Some(status.as_u16());
        self.last_http_body = Some(body.clone());
        let mut err_num = 0;
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            err_num = value
                .get("ErrorNumber")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            if err_num != 0 {
                self.last_ascom_error = Some(err_num);
            }
        }
        // ASCOM convention is HTTP 200 + ErrorNumber for "logical"
        // failures, but parameter parsing rejections (e.g. a negative
        // Duration that doesn't fit `std::time::Duration`) come back
        // as HTTP 4xx with no ASCOM envelope. Map those to
        // INVALID_VALUE so test assertions still see a captured
        // error.
        if err_num == 0 && !status.is_success() {
            err_num = 0x401;
            self.last_ascom_error = Some(err_num);
        }
        err_num
    }

    /// Set BinX/BinY/NumX/NumY/StartX/StartY then call `StartExposure`.
    /// Stops at the first ASCOM error so the captured `last_ascom_error`
    /// matches what the test scenario expects.
    #[allow(clippy::too_many_arguments)]
    pub async fn drive_start_exposure(
        &mut self,
        bin_x: i32,
        bin_y: i32,
        num_x: i32,
        num_y: i32,
        start_x: i32,
        start_y: i32,
        duration_s: f64,
    ) {
        // Reset captured error so a failure on (e.g.) set_bin_x in
        // scenario N+1 isn't masked by a leftover from scenario N.
        self.last_ascom_error = None;

        let steps: &[(&str, &str, String)] = &[
            ("binx", "BinX", bin_x.to_string()),
            ("biny", "BinY", bin_y.to_string()),
            ("numx", "NumX", num_x.to_string()),
            ("numy", "NumY", num_y.to_string()),
            ("startx", "StartX", start_x.to_string()),
            ("starty", "StartY", start_y.to_string()),
        ];
        for (method, key, value) in steps {
            let err = self.put_camera(method, &[(key, value.clone())]).await;
            if err != 0 {
                return;
            }
        }
        // Duration is a JSON-friendly seconds value per ASCOM Camera spec.
        let extra = [
            ("Duration", duration_s.to_string()),
            ("Light", "true".to_string()),
        ];
        self.put_camera("startexposure", &extra).await;
    }

    /// Drive a `StartExposure` with `Light = false` and the default
    /// 640×480 sub-frame used by the survey scenarios.
    pub async fn drive_start_exposure_dark(&mut self) {
        self.last_ascom_error = None;
        let extra = [
            ("Duration", "1.0".to_string()),
            ("Light", "false".to_string()),
        ];
        self.put_camera("startexposure", &extra).await;
    }

    /// Drive a `StartExposure` with `Light = true` and the default
    /// sub-frame (full sensor).
    pub async fn drive_start_exposure_default(&mut self) {
        self.last_ascom_error = None;
        let extra = [
            ("Duration", "1.0".to_string()),
            ("Light", "true".to_string()),
        ];
        self.put_camera("startexposure", &extra).await;
    }

    /// Poll `image_ready` until true or the deadline expires. Returns
    /// `true` on success, `false` on timeout. Used by survey-fetch
    /// scenarios that need to wait for the spawned exposure task.
    pub async fn wait_for_image_ready(&mut self, deadline: Duration) -> bool {
        let start = std::time::Instant::now();
        let url = format!("{}/api/v1/camera/0/imageready", self.base_url());
        let client = self.http();
        while start.elapsed() < deadline {
            let response = client
                .get(&url)
                .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
                .send()
                .await;
            if let Ok(resp) = response {
                if let Ok(value) = resp.json::<Value>().await {
                    if value["Value"].as_bool().unwrap_or(false) {
                        return true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// GET /api/v1/camera/0/imagearray and return its dimensions.
    /// The ASCOM `ImageArray` response is `{ ..., "Value": { "Type",
    /// "Rank", "Value": [[...]] } }` — a nested envelope. The pixel
    /// array is indexed `[X][Y]` per the ASCOM spec, so the outer
    /// length is `NumX` and the inner length is `NumY`.
    pub async fn get_image_dimensions(&mut self) -> (u32, u32) {
        let body = self.fetch_image_array_json().await;
        let pixels = &body["Value"]["Value"];
        let outer = pixels.as_array().expect("ImageArray pixels not an array");
        let width = outer.len() as u32;
        let height = outer
            .first()
            .and_then(|row| row.as_array().map(|r| r.len() as u32))
            .unwrap_or(0);
        (width, height)
    }

    /// GET /api/v1/camera/0/imagearray and assert every value is zero.
    pub async fn assert_image_all_zero(&mut self) {
        let body = self.fetch_image_array_json().await;
        let pixels = body["Value"]["Value"]
            .as_array()
            .expect("ImageArray pixels not an array");
        for row in pixels {
            for cell in row.as_array().expect("row not array") {
                let v = cell.as_i64().expect("cell not int");
                assert_eq!(v, 0, "expected all pixels zero, found {v}");
            }
        }
    }

    async fn fetch_image_array_json(&mut self) -> Value {
        let url = format!("{}/api/v1/camera/0/imagearray", self.base_url());
        let client = self.http();
        let response = client
            .get(&url)
            .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
            .header("accept", "application/json")
            .send()
            .await
            .expect("GET /imagearray failed");
        response.json().await.expect("response not JSON")
    }

    /// Pre-seed the `cache_dir` with a FITS file matching the cache key
    /// the next exposure will compute. Slice 4 derives the same key
    /// formula in `survey::SurveyRequest::cache_key` so we re-use it
    /// here (via a tiny dependency-free reimplementation in the
    /// helper, to avoid pulling library types into tests).
    pub fn preseed_cache(&mut self, cache_key: &str, bytes: &[u8]) {
        let cache_dir = self.cache_dir();
        std::fs::create_dir_all(&cache_dir).expect("create cache_dir");
        let path = cache_dir.join(format!("{cache_key}.fits"));
        std::fs::write(&path, bytes).expect("write cache fits");
    }
}

async fn handle_stub(
    axum::extract::State(state): axum::extract::State<Arc<StubState>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{Method, StatusCode};
    use axum::response::IntoResponse;

    let method = request.method().clone();
    if method == Method::GET {
        state.get_count.fetch_add(1, Ordering::Relaxed);
    }
    if method == Method::HEAD {
        return StatusCode::OK.into_response();
    }
    let behavior = state.behavior.read().expect("stub rwlock").clone();
    match behavior {
        StubBehavior::Ok => StatusCode::OK.into_response(),
        StubBehavior::ServingFits(bytes) => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/fits")
            .body(Body::from(bytes))
            .expect("response build"),
        StubBehavior::Status500 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        StubBehavior::Hold => {
            std::future::pending::<()>().await;
            unreachable!("std::future::pending never resolves")
        }
        StubBehavior::Malformed => axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/fits")
            .body(Body::from(b"this is definitely not a fits file".to_vec()))
            .expect("response build"),
    }
}

async fn handle_configured_devices(
    axum::extract::State(state): axum::extract::State<Arc<MountStubState>>,
) -> axum::Json<Value> {
    let behavior = state.behavior.read().expect("mount stub rwlock").clone();
    let devices = match behavior {
        MountStubBehavior::NoTelescope => Vec::new(),
        _ => vec![serde_json::json!({
            "DeviceName": "Telescope 0",
            "DeviceType": "Telescope",
            "DeviceNumber": 0,
            "UniqueID": "test-mount-uid"
        })],
    };
    axum::Json(serde_json::json!({
        "Value": devices,
        "ErrorNumber": 0,
        "ErrorMessage": ""
    }))
}

async fn handle_right_ascension(
    axum::extract::State(state): axum::extract::State<Arc<MountStubState>>,
) -> axum::Json<Value> {
    state.read_count.fetch_add(1, Ordering::Relaxed);
    let behavior = state.behavior.read().expect("mount stub rwlock").clone();
    match behavior {
        MountStubBehavior::Ok { ra_hours, .. } => axum::Json(serde_json::json!({
            "Value": ra_hours,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })),
        MountStubBehavior::AscomError => axum::Json(serde_json::json!({
            "Value": 0.0,
            "ErrorNumber": 1024,
            "ErrorMessage": "simulated mount read failure"
        })),
        MountStubBehavior::NoTelescope => axum::Json(serde_json::json!({
            "Value": 0.0,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })),
    }
}

async fn handle_declination(
    axum::extract::State(state): axum::extract::State<Arc<MountStubState>>,
) -> axum::Json<Value> {
    state.read_count.fetch_add(1, Ordering::Relaxed);
    let behavior = state.behavior.read().expect("mount stub rwlock").clone();
    match behavior {
        MountStubBehavior::Ok { dec_deg, .. } => axum::Json(serde_json::json!({
            "Value": dec_deg,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })),
        MountStubBehavior::AscomError => axum::Json(serde_json::json!({
            "Value": 0.0,
            "ErrorNumber": 1024,
            "ErrorMessage": "simulated mount read failure"
        })),
        MountStubBehavior::NoTelescope => axum::Json(serde_json::json!({
            "Value": 0.0,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })),
    }
}

async fn handle_rotator_configured_devices(
    axum::extract::State(_state): axum::extract::State<Arc<RotatorStubState>>,
) -> axum::Json<Value> {
    axum::Json(serde_json::json!({
        "Value": [{
            "DeviceName": "Rotator 0",
            "DeviceType": "Rotator",
            "DeviceNumber": 0,
            "UniqueID": "test-rotator-uid"
        }],
        "ErrorNumber": 0,
        "ErrorMessage": ""
    }))
}

async fn handle_rotator_position(
    axum::extract::State(state): axum::extract::State<Arc<RotatorStubState>>,
) -> axum::Json<Value> {
    state.read_count.fetch_add(1, Ordering::Relaxed);
    let behavior = state.behavior.read().expect("rotator stub rwlock").clone();
    match behavior {
        RotatorStubBehavior::Ok { position_angle } => axum::Json(serde_json::json!({
            "Value": position_angle,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })),
        RotatorStubBehavior::AscomError => axum::Json(serde_json::json!({
            "Value": 0.0,
            "ErrorNumber": 1024,
            "ErrorMessage": "simulated rotator read failure"
        })),
    }
}

/// Read `device.description` from `config.get` via a fresh client, returning
/// `None` on any transport/parse failure (e.g. mid-reload).
async fn try_get_description(addr: std::net::SocketAddr) -> Option<String> {
    let client = AlpacaClient::new_from_addr(addr);
    let mut devices = client.get_devices().await.ok()?;
    if let Some(TypedDevice::Camera(c)) = devices.next() {
        let body = c
            .action("config.get".to_string(), String::new())
            .await
            .ok()?;
        let parsed: Value = serde_json::from_str(&body).ok()?;
        return parsed["config"]["device"]["description"]
            .as_str()
            .map(str::to_string);
    }
    None
}

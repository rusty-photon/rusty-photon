//! Cucumber `World` for the qhy-camera BDD suite.
//!
//! Each scenario spawns the qhy-camera binary (built with the `simulation`
//! backend so `Sdk::new()` yields a QHY178M-Simulated camera + 7-position CFW)
//! and drives it through the typed `ascom-alpaca` Camera / `FilterWheel` clients
//! over real HTTP — mirroring the qhy-focuser / dsd-fp2 pattern.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::{Camera, FilterWheel, TypedDevice};
use ascom_alpaca::ASCOMErrorCode;
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use tempfile::TempDir;

/// The SDK serial of the `qhyccd-rs` simulated camera / CFW. Fixed and known, so
/// BDD scenarios can key a per-serial config override (`filter_names`) on it.
pub const SIM_SERIAL: &str = "SIM-QHY178M";

/// How long [`CameraWorld::wait_image_ready`] waits for a frame to land.
///
/// Sized for the slowest runner in the fleet, not for a developer box.
/// `ImageReady` only flips once the driver's `u16`->`i32` widen+transpose has
/// finished — CPU-heavy work, running unoptimised in CI, while several other
/// BDD suites share the same 3-core macOS runner. The equivalent zwo-camera
/// wait was measured there at 7.5 s and still unfinished.
///
/// Matching `star-adventurer-gti`'s `DEBUG_RETRY_WINDOW`, the other in-repo
/// budget written for a starved runner.
const IMAGE_READY_BUDGET: Duration = Duration::from_secs(20);

#[derive(Debug, Default, World)]
pub struct CameraWorld {
    pub handle: Option<ServiceHandle>,
    pub camera: Option<Arc<dyn Camera>>,
    pub filter_wheel: Option<Arc<dyn FilterWheel>>,
    pub temp_dir: Option<TempDir>,

    // Config knobs set by Given steps before the service starts.
    pub filter_names: Option<Vec<String>>,
    pub empty_backend: bool,

    // Result stashes ("When does, Then asserts").
    pub last_error_code: Option<u16>,
    pub last_response: Option<serde_json::Value>,
    pub last_actions: Option<Vec<String>>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for CameraWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        serde_json::json!({ "devices": {}, "server": { "port": 0 } })
    }
}

impl TlsAuthSmokeWorld for CameraWorld {
    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        serde_json::json!({
            "devices": {}
        })
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.handle = Some(handle);
    }
}

impl CameraWorld {
    fn write_config(&mut self) -> String {
        let mut devices = serde_json::Map::new();
        if let Some(names) = &self.filter_names {
            devices.insert(
                SIM_SERIAL.to_string(),
                serde_json::json!({ "filter_names": names }),
            );
        }
        let config = serde_json::json!({
            "devices": devices,
            // Port 0 → OS-assigned; the real port is read from the `bound_addr=`
            // line on stdout by ServiceHandle.
            "server": { "port": 0 },
        });
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("temp dir"));
        let path = dir.path().join("qhy-camera.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize config"),
        )
        .expect("write config");
        path.to_str().expect("utf8 config path").to_string()
    }

    /// Spawn the service binary and acquire the typed device clients.
    pub async fn start(&mut self) {
        let config_path = self.write_config();
        let handle = if self.empty_backend {
            ServiceHandle::start_with_args(
                env!("CARGO_PKG_NAME"),
                &["--config", &config_path, "--simulation-empty"],
            )
            .await
        } else {
            ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await
        };
        self.handle = Some(handle);
        self.acquire().await;
    }

    async fn acquire(&mut self) {
        let port = self.handle.as_ref().expect("service handle").port;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        for _ in 0..80 {
            let client = AlpacaClient::new_from_addr(addr);
            if let Ok(devices) = client.get_devices().await {
                let mut camera = None;
                let mut filter_wheel = None;
                for device in devices {
                    match device {
                        TypedDevice::Camera(c) => camera = Some(c),
                        TypedDevice::FilterWheel(f) => filter_wheel = Some(f),
                        #[allow(unreachable_patterns)]
                        _ => {}
                    }
                }
                if self.empty_backend {
                    // Zero cameras is the expected, healthy state here (C0).
                    self.camera = camera;
                    self.filter_wheel = filter_wheel;
                    return;
                }
                if camera.is_some() {
                    self.camera = camera;
                    self.filter_wheel = filter_wheel;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // Reaching here means the loop never took its success `return`: either the
        // management API never responded (both modes) or no Camera registered
        // (non-empty backend). Fail loudly in both cases so a scenario stops with
        // an actionable error instead of proceeding against an unhealthy or
        // never-started service. (An empty backend's *healthy* state is zero
        // cameras AFTER a successful get_devices() — which returns inside the loop.)
        assert!(
            !self.empty_backend,
            "qhy-camera management API did not respond within 20s (empty backend)"
        );
        panic!("qhy-camera did not register a Camera device within 20s");
    }

    pub fn camera(&self) -> Arc<dyn Camera> {
        Arc::clone(self.camera.as_ref().expect("camera not acquired"))
    }

    pub fn filter_wheel(&self) -> Arc<dyn FilterWheel> {
        Arc::clone(
            self.filter_wheel
                .as_ref()
                .expect("filter wheel not acquired"),
        )
    }

    pub fn base_url(&self) -> String {
        self.handle
            .as_ref()
            .expect("service handle")
            .base_url
            .clone()
    }

    /// The management API answers a `get_devices` request (server is healthy).
    pub async fn management_responds(&self) -> bool {
        let port = self.handle.as_ref().expect("service handle").port;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        AlpacaClient::new_from_addr(addr)
            .get_devices()
            .await
            .is_ok()
    }

    /// Start a long-running exposure and leave it in flight — i.e. parked in the
    /// driver's cancellable wait, well short of the readout.
    pub async fn start_in_flight(&mut self) {
        self.camera()
            .start_exposure(Duration::from_secs(30), true)
            .await
            .expect("start in-flight exposure");
        // Let the detached capture task reach its wait.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    /// Poll `ImageReady` until the frame lands or [`IMAGE_READY_BUDGET`] runs
    /// out.
    ///
    /// The budget is wall-clock, not a poll count. Each iteration is a full
    /// Alpaca round trip, so a fixed count of naps promises a budget it cannot
    /// keep: the loop this replaced advertised 6 s as 240 x 25 ms and, in the
    /// zwo-camera copy of it, actually fired at ~7.5 s on a loaded runner,
    /// because the round trips are not free. A deadline says what it means on
    /// every machine.
    pub async fn wait_image_ready(&self) {
        let deadline = Instant::now() + IMAGE_READY_BUDGET;
        loop {
            if self.camera().image_ready().await.unwrap_or(false) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "exposure did not complete within {IMAGE_READY_BUDGET:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drive a `StartExposure` and stash the ASCOM error code (`None` on success).
    /// Sets bin/ROI via the typed client first; a negative duration (which a
    /// `std::time::Duration` cannot hold) goes via raw HTTP.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_start_exposure(
        &mut self,
        bin_x: u8,
        bin_y: u8,
        num_x: u32,
        num_y: u32,
        start_x: u32,
        start_y: u32,
        duration: f64,
        light: bool,
    ) {
        let camera = self.camera();
        let _ = camera.set_bin_x(bin_x).await;
        let _ = camera.set_bin_y(bin_y).await;
        let _ = camera.set_num_x(num_x).await;
        let _ = camera.set_num_y(num_y).await;
        let _ = camera.set_start_x(start_x).await;
        let _ = camera.set_start_y(start_y).await;

        if duration < 0.0 {
            let code = raw_start_exposure(&self.base_url(), 0, duration, light).await;
            self.last_error_code = (code != 0).then_some(code);
        } else {
            match camera
                .start_exposure(Duration::from_secs_f64(duration), light)
                .await
            {
                Ok(()) => self.last_error_code = None,
                Err(e) => self.last_error_code = Some(e.code.raw()),
            }
        }
    }

    /// Call a vendor config action; stash the parsed JSON (`last_response`) on
    /// success, or the ASCOM error code (`last_error_code`) on failure.
    pub async fn call_action(&mut self, action: &str, params: &str) {
        match self
            .camera()
            .action(action.to_string(), params.to_string())
            .await
        {
            Ok(body) => {
                self.last_error_code = None;
                self.last_response =
                    Some(serde_json::from_str(&body).expect("action returned invalid JSON"));
            }
            Err(e) => {
                self.last_error_code = Some(e.code.raw());
                self.last_response = None;
            }
        }
    }

    /// The `config` object from a `config.get` response.
    pub async fn config_get(&mut self) -> serde_json::Value {
        self.call_action("config.get", "").await;
        self.last_response
            .as_ref()
            .and_then(|r| r.get("config").cloned())
            .expect("config.get response missing `config`")
    }
}

/// Map an ASCOM error-code *name* (as written in the feature files) to its raw
/// `u16`, so Then steps can assert "rejected with ASCOM <NAME>".
pub fn ascom_code(name: &str) -> u16 {
    match name {
        "INVALID_VALUE" => ASCOMErrorCode::INVALID_VALUE.raw(),
        "NOT_CONNECTED" => ASCOMErrorCode::NOT_CONNECTED.raw(),
        "NOT_IMPLEMENTED" => ASCOMErrorCode::NOT_IMPLEMENTED.raw(),
        "INVALID_OPERATION" => ASCOMErrorCode::INVALID_OPERATION.raw(),
        other => panic!("unknown ASCOM error code name: {other}"),
    }
}

/// Drive `StartExposure` over raw HTTP — the only way to submit a negative
/// `Duration` (the typed client takes a `std::time::Duration`). Returns the
/// response `ErrorNumber` (0 = success).
async fn raw_start_exposure(base_url: &str, device: u32, duration_secs: f64, light: bool) -> u16 {
    let url = format!("{base_url}/api/v1/camera/{device}/startexposure");
    let form = [
        ("Duration", duration_secs.to_string()),
        ("Light", if light { "True" } else { "False" }.to_string()),
        ("ClientID", "1".to_string()),
        ("ClientTransactionID", "1".to_string()),
    ];
    match reqwest::Client::new().put(&url).form(&form).send().await {
        Ok(resp) => {
            // Fail loudly on a non-Alpaca response (500/HTML body, proxy error,
            // schema change) instead of silently reporting success (ErrorNumber 0)
            // — otherwise the BDD assertions become unreliable.
            let status = resp.status();
            let body = resp.text().await.expect("read startexposure response body");
            let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
                panic!("startexposure response was not valid JSON (status {status}): {e}; body: {body}")
            });
            let error_number = json["ErrorNumber"].as_u64().unwrap_or_else(|| {
                panic!("startexposure response missing ErrorNumber (status {status}): {json}")
            });
            // Fail loudly on an out-of-range code rather than silently truncating
            // with `as u16` — this helper exists to make BDD failures actionable.
            u16::try_from(error_number).unwrap_or_else(|_| {
                panic!("startexposure ErrorNumber {error_number} exceeds u16 (status {status}): {json}")
            })
        }
        Err(e) => panic!("raw startexposure request failed: {e}"),
    }
}

//! Cucumber `World` for the qhy-camera BDD suite.
//!
//! Each scenario spawns the qhy-camera binary (built with the `simulation`
//! backend so `Sdk::new()` yields a QHY178M-Simulated camera, whose 7-position
//! CFW the service's startup probe finds, or a `usb_devices` entry declares)
//! and drives it through the typed `ascom-alpaca` Camera / `FilterWheel`
//! clients over real HTTP — mirroring the qhy-focuser / dsd-fp2 pattern.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ascom_alpaca::api::{Camera, FilterWheel, TypedDevice};
use ascom_alpaca::ASCOMErrorCode;
use ascom_alpaca::Client as AlpacaClient;
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
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
    /// Camera device 0, the one most scenarios address.
    pub camera: Option<Arc<dyn Camera>>,
    /// Every registered Camera device, indexed by its device number.
    pub cameras: Vec<Arc<dyn Camera>>,
    /// Every registered `FilterWheel` device, indexed by its device number.
    pub filter_wheels: Vec<Arc<dyn FilterWheel>>,
    pub temp_dir: Option<TempDir>,

    // Config knobs set by Given steps before the service starts.
    pub filter_names: Option<Vec<String>>,
    pub empty_backend: bool,
    /// The file whose existence takes the simulated camera off the bus (C9);
    /// `None` starts a camera that never leaves.
    pub departure_file: Option<std::path::PathBuf>,
    /// The service is expected to register no camera although the backend has
    /// one (a camera its CFW probe cannot open, C0): startup then waits for the
    /// device list rather than for a camera.
    pub expect_no_camera: bool,
    /// The configuration the next start writes, minus its `server` block;
    /// `None` writes the default (no `usb_devices` list), with
    /// [`Self::filter_names`] as the simulated camera's override.
    pub config_json: Option<serde_json::Value>,
    /// The staged USB inventory handed over with `--usb-inventory` (U7);
    /// `None` leaves the simulation build its default inventory.
    pub usb_inventory: Option<PathBuf>,

    // Result stashes ("When does, Then asserts").
    pub last_error_code: Option<u16>,
    pub last_error_message: Option<String>,
    pub last_actions: Option<Vec<String>>,
    /// How the last start that was expected to fail went: `Err` with the
    /// harness's account of the refusal, or `Ok` when the service came up.
    pub start_refusal: Option<Result<(), String>>,
    /// The port a list configuration is written with, reserved once per
    /// scenario so every rewrite of the file names the same one.
    pub config_port: Option<u16>,
    /// The last `doctor` run's output (`doctor --json` or `doctor --devices`).
    pub doctor_output: Option<std::process::Output>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,

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

impl SetupPagesWorld for CameraWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
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
    /// Write the scenario's configuration and return its path.
    ///
    /// Port 0 (OS-assigned) by default; the real port is read from the
    /// `bound_addr=` line on stdout by `ServiceHandle`. A configuration with a
    /// `usb_devices` list, or a staged USB scan, takes a reserved port instead,
    /// because only those can rebuild the service on their own (U6, and the
    /// no-list default's failed scan): a rebuild rebinds
    /// the configured port, and port 0 would move the service to a port the
    /// harness never learns. The port is reserved once per scenario, so a
    /// later rewrite keeps the port a rebuild of the running service rebinds.
    pub async fn write_config(&mut self) -> String {
        let mut config = self.config_json.clone().unwrap_or_else(|| {
            let mut devices = serde_json::Map::new();
            if let Some(names) = &self.filter_names {
                devices.insert(
                    SIM_SERIAL.to_string(),
                    serde_json::json!({ "filter_names": names }),
                );
            }
            serde_json::json!({ "devices": devices })
        });
        let port = if config.get("usb_devices").is_some() || self.usb_inventory.is_some() {
            bdd_infra::reserved_port_once(&mut self.config_port).await
        } else {
            0
        };
        config
            .as_object_mut()
            .expect("the scenario's configuration is a JSON object")
            .insert("server".to_string(), serde_json::json!({ "port": port }));
        let path = self.scratch_dir().join("qhy-camera.json");
        tokio::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize config"),
        )
        .await
        .expect("write config");
        path.to_str().expect("utf8 config path").to_string()
    }

    /// The scenario's scratch directory, created on first use under Bazel's
    /// per-action `TEST_TMPDIR` when there is one (testing.md §5.1): the config
    /// and the departure file live here, read by the service under test.
    pub fn scratch_dir(&mut self) -> std::path::PathBuf {
        self.temp_dir
            .get_or_insert_with(|| {
                bdd_infra::scratch::new_dir("qhy-camera-bdd-").expect("scratch dir")
            })
            .path()
            .to_path_buf()
    }

    /// The service's command line for this scenario.
    pub async fn start_args(&mut self) -> Vec<String> {
        let config_path = self.write_config().await;
        let mut args = vec!["--config".to_string(), config_path];
        if self.empty_backend {
            args.push("--simulation-empty".to_string());
        } else if let Some(departure) = &self.departure_file {
            args.push("--simulation-departure-file".to_string());
            args.push(departure.to_str().expect("utf8 departure path").to_string());
        }
        if let Some(inventory) = &self.usb_inventory {
            args.push("--usb-inventory".to_string());
            args.push(inventory.to_str().expect("utf8 inventory path").to_string());
        }
        args
    }

    /// Spawn the service binary, hand its address and config file to the
    /// setup-page steps, and acquire the typed device clients.
    pub async fn start(&mut self) {
        let args = self.start_args().await;
        let config_path = PathBuf::from(args.get(1).expect("--config's path follows it"));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let handle = ServiceHandle::start_with_args(env!("CARGO_PKG_NAME"), &args).await;
        setup_pages::adopt_plain(&mut self.setup_pages, handle.port, config_path);
        self.handle = Some(handle);
        self.acquire().await;
    }

    /// Spawn the binary through the shared TLS + auth fixture, so the setup
    /// pages sit behind the credential.
    pub async fn start_with_tls_and_auth_pages(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = serde_json::json!({ "devices": {} });
        config["server"] = self.tls_auth.pki().server_block(0);
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        let config_path = bdd_infra::tls_auth::staged_config_path(&self.tls_auth)
            .expect("the TLS fixture staged a config file");
        setup_pages::adopt_tls_auth(&mut self.setup_pages, &self.tls_auth, config_path);
        self.handle = Some(handle);
    }

    /// Start the service when the scenario expects it to refuse, recording
    /// how it went in [`Self::start_refusal`].
    pub async fn try_start(&mut self) {
        let args = self.start_args().await;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match ServiceHandle::try_start_with_args(env!("CARGO_PKG_NAME"), &args).await {
            Ok(handle) => {
                self.handle = Some(handle);
                self.start_refusal = Some(Ok(()));
            }
            Err(why) => self.start_refusal = Some(Err(why)),
        }
    }

    /// Whether this scenario's service should register at least one Camera:
    /// a list registers exactly its entries, whatever the SDK sees (U4), and
    /// no list registers what the scan places and the CFW probe can open
    /// (U11) — the simulated camera on its default record, and on a staged
    /// scan whatever that places, which may be nothing.
    fn expects_a_camera(&self) -> bool {
        match self.config_json.as_ref().and_then(|c| c.get("usb_devices")) {
            Some(list) => list.as_array().is_some_and(|l| !l.is_empty()),
            None => !(self.empty_backend || self.expect_no_camera) && self.usb_inventory.is_none(),
        }
    }

    async fn acquire(&mut self) {
        let port = self.port();
        let expects_a_camera = self.expects_a_camera();
        for _ in 0..80 {
            if let Some((cameras, filter_wheels)) = registered_devices(port).await {
                // Zero cameras is the expected, healthy state for an empty
                // backend, an empty list, or a camera the probe cannot open
                // (C0).
                if !cameras.is_empty() || !expects_a_camera {
                    self.camera = cameras.first().cloned();
                    self.cameras = cameras;
                    self.filter_wheels = filter_wheels;
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // Reaching here means the loop never took its success `return`: either
        // the management API never responded, or no Camera registered where
        // one was expected. Fail loudly in both cases so a scenario stops with
        // an actionable error instead of proceeding against an unhealthy or
        // never-started service.
        assert!(
            expects_a_camera,
            "qhy-camera management API did not respond within 20s (no camera expected)"
        );
        panic!("qhy-camera did not register a Camera device within 20s");
    }

    pub fn camera(&self) -> Arc<dyn Camera> {
        Arc::clone(self.camera.as_ref().expect("camera not acquired"))
    }

    /// The Camera registered at `device_number`.
    pub fn device(&self, device_number: u32) -> Arc<dyn Camera> {
        let index = usize::try_from(device_number).expect("device number fits usize");
        Arc::clone(self.cameras.get(index).unwrap_or_else(|| {
            panic!(
                "no camera device {device_number}; {} registered",
                self.cameras.len()
            )
        }))
    }

    /// The `FilterWheel` registered at `device_number`.
    pub fn wheel(&self, device_number: u32) -> Arc<dyn FilterWheel> {
        let index = usize::try_from(device_number).expect("device number fits usize");
        Arc::clone(self.filter_wheels.get(index).unwrap_or_else(|| {
            panic!(
                "no filterwheel device {device_number}; {} registered",
                self.filter_wheels.len()
            )
        }))
    }

    /// The port the running service listens on.
    pub const fn port(&self) -> u16 {
        self.handle.as_ref().expect("service handle").port
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
    ///
    /// The budget is checked after each poll, so a wait can overshoot it by
    /// the round trip that was in flight when it expired, and a frame landing
    /// inside that round trip still passes. Both are deliberate: this is a
    /// generosity floor, not a stopwatch. Failing a frame that arrived a few
    /// milliseconds late would reintroduce the flake the budget exists to
    /// remove, and bounding the final poll by the remaining time would
    /// surface a transport error in place of this message — see
    /// `docs/skills/testing.md` §5.9 rule 2. The panic reports the wait it
    /// measured, so nobody has to reconstruct it from CI timestamps.
    pub async fn wait_image_ready(&self) {
        let start = Instant::now();
        loop {
            if self.camera().image_ready().await.unwrap_or(false) {
                return;
            }
            let waited = start.elapsed();
            assert!(
                waited < IMAGE_READY_BUDGET,
                "exposure did not complete within {IMAGE_READY_BUDGET:?} (waited {waited:?})"
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

    /// Start an exposure of whatever sub-frame the camera currently holds —
    /// the frame a client that only ever sets `BinX`/`BinY` exposes, which is
    /// the one the driver chose rather than one the test dictated.
    pub async fn try_start_current_frame(&mut self, duration: f64, light: bool) {
        match self
            .camera()
            .start_exposure(Duration::from_secs_f64(duration), light)
            .await
        {
            Ok(()) => self.last_error_code = None,
            Err(e) => self.last_error_code = Some(e.code.raw()),
        }
    }
}

/// The Camera and `FilterWheel` devices a server at `port` registers, each in
/// device-number order, or `None` when its management API does not answer.
pub async fn registered_devices(
    port: u16,
) -> Option<(Vec<Arc<dyn Camera>>, Vec<Arc<dyn FilterWheel>>)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let devices = AlpacaClient::new_from_addr(addr).get_devices().await.ok()?;
    let mut cameras = Vec::new();
    let mut filter_wheels = Vec::new();
    for device in devices {
        match device {
            TypedDevice::Camera(camera) => cameras.push(camera),
            TypedDevice::FilterWheel(wheel) => filter_wheels.push(wheel),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    Some((cameras, filter_wheels))
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

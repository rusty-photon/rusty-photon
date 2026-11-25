//! The guider suite's world: the simulated devices a scenario scripts
//! (`guide-simulator-alpaca`), the configuration it builds, the guider it
//! starts, and what a refused start printed.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ascom_alpaca::api::{Camera, TypedDevice};
use ascom_alpaca::Client;
use bdd_infra::doctor_smoke::{DoctorSmokeState, DoctorSmokeWorld};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use guide_simulator::{CameraConfig, Seeing, SimConfig, SimTime, SkyConfig};
use guide_simulator_alpaca::{Device, DevicesConfig, FrameFault, SimDevices};
use serde_json::{json, Value};
use tempfile::TempDir;

/// How long a step waits for the guider to show what the scenario
/// expects. It bounds a hang and asserts no timing (docs/skills/testing.md
/// §5.9): the guider polls every 100 ms, and the slowest CI leg starts a
/// herd of services at once.
pub const WAIT_BUDGET: Duration = Duration::from_secs(60);

/// How often the guider polls its devices in every scenario.
pub const POLL_INTERVAL: &str = "100ms";

/// The deadline of each request a step sends, well inside
/// [`WAIT_BUDGET`] so that a wait gets several attempts.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One client for every request the suite sends the guider.
pub fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build the suite's HTTP client")
    })
}

/// The simulated sky's seeing, as a full width at half maximum.
const SEEING_ARCSEC: f64 = 6.0;

/// The exposure of every frame in every scenario, unless the scenario
/// sets its own.
pub const EXPOSURE: &str = "200ms";

/// What a start the guider refused printed.
#[derive(Debug)]
pub struct Refusal {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// An answer of the guider's: its status, its content type, its body, and
/// the body read as JSON when it is.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub json: Option<Value>,
}

impl Answer {
    /// The body as JSON; panics when it is not.
    pub fn json(&self) -> &Value {
        self.json.as_ref().unwrap_or_else(|| {
            panic!(
                "the answer {} is not JSON: {}",
                self.status,
                String::from_utf8_lossy(&self.bytes)
            )
        })
    }
}

#[derive(Debug, World)]
#[world(init = Self::new)]
pub struct GuiderWorld {
    /// Everything the scenario writes: the config file, the state
    /// directory, a CA file.
    pub scratch: TempDir,
    /// The simulated devices' configuration, when the scenario has them.
    pub sim_config: Option<DevicesConfig>,
    /// Whether each simulated device starts connected.
    pub camera_connected: bool,
    pub mount_connected: bool,
    pub sim: Option<SimDevices>,
    /// A fault every exposure, or a count of them, fails with from the
    /// simulated devices' start.
    pub frame_faults: Option<(FrameFault, Option<usize>)>,
    /// Which simulated devices the guider's configuration names.
    pub name_camera: bool,
    pub name_mount: bool,
    /// The configuration being built, without the device addresses that
    /// the simulated devices' servers give at the start.
    pub config: Value,
    /// Fields set on the configuration after the device addresses, as
    /// JSON pointers and values.
    pub overrides: Vec<(String, Value)>,
    /// The file's text in place of `config`, for a file that is not JSON.
    pub config_text: Option<String>,
    /// The `--config` path in place of the file the suite writes.
    pub config_path: Option<PathBuf>,
    pub guider: Option<ServiceHandle>,
    pub refusal: Option<Refusal>,
    /// The answer to the scenario's latest request, and the simulated time
    /// it arrived at.
    pub answer: Option<Answer>,
    pub answered_at: Option<SimTime>,
    /// A request sent in the background, still to be answered.
    pub background: Option<tokio::task::JoinHandle<Result<Answer, String>>>,
    /// The frame numbers the metrics window held at a step that noted
    /// them.
    pub noted_frames: Vec<u64>,
    pub tls_auth: TlsAuthState,
    pub doctor_smoke: DoctorSmokeState,
}

impl GuiderWorld {
    fn new() -> Self {
        let scratch = bdd_infra::scratch::new_dir("guider-bdd-").expect("create a scratch dir");
        let config = base_config(&scratch);
        Self {
            scratch,
            sim_config: None,
            camera_connected: false,
            mount_connected: false,
            sim: None,
            frame_faults: None,
            name_camera: false,
            name_mount: false,
            config,
            overrides: Vec::new(),
            config_text: None,
            config_path: None,
            guider: None,
            refusal: None,
            answer: None,
            answered_at: None,
            background: None,
            noted_frames: Vec::new(),
            tls_auth: TlsAuthState::default(),
            doctor_smoke: DoctorSmokeState::default(),
        }
    }

    /// The scratch directory's path.
    pub fn scratch_path(&self) -> PathBuf {
        self.scratch.path().to_path_buf()
    }

    /// The simulated devices' configuration, created on first use: a
    /// 320 × 240 sensor of 2 µm pixels, in seeing of 6″, three pixels at
    /// bin 1: a frame binned 2 × 2 still resolves a star, where it would
    /// put the light of one in a single pixel in finer seeing and pass it
    /// over as a hot pixel.
    pub fn sim_config(&mut self) -> &mut DevicesConfig {
        self.sim_config.get_or_insert_with(|| DevicesConfig {
            sim: SimConfig {
                camera: CameraConfig {
                    width: 320,
                    height: 240,
                    ..CameraConfig::default()
                },
                sky: SkyConfig {
                    seeing: Seeing {
                        fwhm_arcsec: SEEING_ARCSEC,
                        ..SkyConfig::default().seeing
                    },
                    ..SkyConfig::default()
                },
                ..SimConfig::default()
            },
            ..DevicesConfig::default()
        })
    }

    /// The simulated devices, once started.
    pub const fn sim(&self) -> &SimDevices {
        self.sim
            .as_ref()
            .expect("the scenario has no simulated devices")
    }

    /// Set `pointer` in the configuration after the device addresses go
    /// in.
    pub fn set_field(&mut self, pointer: &str, value: Value) {
        self.overrides.push((pointer.to_owned(), value));
    }

    /// Start the simulated devices, if the scenario has them, write the
    /// configuration, and start the guider. A start the guider refuses is
    /// run again to its exit, so its output can be read.
    pub async fn start_guider(&mut self) {
        if let Some(sim_config) = self.sim_config.clone() {
            let sim = SimDevices::start(sim_config)
                .await
                .expect("start the simulated devices");
            sim.set_connected(Device::Camera, self.camera_connected);
            sim.set_connected(Device::Mount, self.mount_connected);
            if let Some((fault, count)) = self.frame_faults {
                sim.fail_frames(fault, count);
            }
            if self.name_camera {
                self.config["devices"]["camera"] = json!({ "alpaca_url": sim.camera_url() });
            }
            if self.name_mount {
                self.config["devices"]["mount"] = json!({ "alpaca_url": sim.mount_url() });
            }
            self.sim = Some(sim);
        }
        for (pointer, value) in std::mem::take(&mut self.overrides) {
            set_pointer(&mut self.config, &pointer, value);
        }

        let path = if let Some(path) = &self.config_path {
            path.clone()
        } else {
            let path = self.scratch.path().join("guider.json");
            let text = self
                .config_text
                .clone()
                .unwrap_or_else(|| self.config.to_string());
            std::fs::write(&path, text).expect("write the guider's config");
            path
        };
        let path = path.to_string_lossy().into_owned();
        match ServiceHandle::try_start(env!("CARGO_PKG_NAME"), &path).await {
            Ok(handle) => self.guider = Some(handle),
            // A guider that exited is run again for its output. One that
            // neither bound nor exited is not: run again, it could serve
            // and never exit.
            Err(error) if error.contains("exited without binding") => {
                let output =
                    bdd_infra::run_once_async(env!("CARGO_PKG_NAME"), &["--config", &path], None)
                        .await;
                self.refusal = Some(Refusal {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Err(error) => panic!("the guider neither bound nor exited: {error}"),
        }
    }

    /// The running guider's base URL.
    pub fn base_url(&self) -> String {
        self.guider
            .as_ref()
            .expect("the guider is not running")
            .base_url()
    }

    /// `method path` on the guider, with `body` when there is one.
    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&str>,
    ) -> Result<Answer, String> {
        send(
            http().request(method, format!("{}{path}", self.base_url())),
            body,
        )
        .await
        .map_err(|e| format!("{path}: {e}"))
    }

    /// `POST path` with `body`, its answer kept as the scenario's latest
    /// with the simulated time it arrived at.
    pub async fn post(&mut self, path: &str, body: Option<&str>) {
        let answer = self
            .request(reqwest::Method::POST, path, body)
            .await
            .unwrap_or_else(|e| panic!("POST {e}"));
        self.answered_at = self.sim.as_ref().map(SimDevices::now);
        self.answer = Some(answer);
    }

    /// `POST path` with `body` in the background.
    pub fn post_in_background(&mut self, path: &str, body: Option<&str>) {
        let request = http().post(format!("{}{path}", self.base_url()));
        let body = body.map(str::to_owned);
        self.background = Some(tokio::spawn(async move {
            send(request, body.as_deref())
                .await
                .map_err(|e| e.to_string())
        }));
    }

    /// `GET path` on the guider: the status and the JSON body.
    pub async fn get(&self, path: &str) -> Result<(u16, Value), String> {
        let response = http()
            .get(format!("{}{path}", self.base_url()))
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = response.status().as_u16();
        let body = response
            .json::<Value>()
            .await
            .map_err(|e| format!("GET {path}: the body is not JSON: {e}"))?;
        Ok((status, body))
    }

    /// Wait until `GET path` answers what `accept` takes, and return that
    /// answer. Panics with the last answer, or the last failure, when
    /// [`WAIT_BUDGET`] runs out.
    pub async fn wait_for(
        &self,
        path: &str,
        what: &str,
        accept: impl Fn(u16, &Value) -> bool,
    ) -> (u16, Value) {
        let start = Instant::now();
        let mut last = String::from("no answer yet");
        while start.elapsed() < WAIT_BUDGET {
            match self.get(path).await {
                Ok((status, body)) if accept(status, &body) => return (status, body),
                Ok((status, body)) => last = format!("{status} {body}"),
                Err(error) => last = error,
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "GET {path} did not show {what} within {WAIT_BUDGET:?} (waited {:?}); last answer: {last}",
            start.elapsed()
        );
    }

    /// Wait until `done` holds. Panics naming `what` when [`WAIT_BUDGET`]
    /// runs out.
    pub async fn wait_until(&self, what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < WAIT_BUDGET {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{what} did not come within {WAIT_BUDGET:?}");
    }

    /// A client of the simulated camera of the suite's own, as another
    /// client of the rig would hold.
    pub async fn sim_camera(&self) -> Arc<dyn Camera> {
        let client = Client::new(self.sim().camera_url().as_str())
            .expect("a client of the simulated camera");
        let listed = tokio::time::timeout(REQUEST_TIMEOUT, client.get_devices())
            .await
            .expect("the simulated camera's server answers")
            .expect("the device list");
        for device in listed {
            if let TypedDevice::Camera(camera) = device {
                return camera;
            }
        }
        panic!("the simulated camera is not listed");
    }

    /// The `Connected` the simulated `device` reports to a client of its
    /// own.
    pub async fn sim_connected(&self, device: Device) -> bool {
        let url = match device {
            Device::Camera => self.sim().camera_url(),
            Device::Mount => self.sim().mount_url(),
        };
        let read = async {
            let client = Client::new(url.as_str()).expect("a client of the simulated device");
            let listed = client.get_devices().await.expect("the device list");
            for listed in listed {
                match (device, listed) {
                    (Device::Camera, TypedDevice::Camera(camera)) => {
                        return camera.connected().await.expect("Connected");
                    }
                    (Device::Mount, TypedDevice::Telescope(mount)) => {
                        return mount.connected().await.expect("Connected");
                    }
                    _ => {}
                }
            }
            panic!("the simulated {device:?} is not listed");
        };
        tokio::time::timeout(REQUEST_TIMEOUT, read)
            .await
            .expect("the simulated device answers")
    }

    /// Stop the guider, then the simulated devices.
    pub async fn stop(&mut self) {
        if let Some(guider) = self.guider.as_mut() {
            guider.stop().await;
        }
        if let Some(sim) = self.sim.take() {
            sim.stop().await;
        }
    }
}

/// Send `request` with `body`, and read the answer.
async fn send(request: reqwest::RequestBuilder, body: Option<&str>) -> reqwest::Result<Answer> {
    let request = match body {
        Some(body) => request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned()),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await?.to_vec();
    let json = serde_json::from_slice(&bytes).ok();
    Ok(Answer {
        status,
        content_type,
        bytes,
        json,
    })
}

/// The configuration every scenario starts from: the listener on port 0
/// of the loopback address, the state directory in the scratch directory,
/// a poll every [`POLL_INTERVAL`] and exposures of [`EXPOSURE`].
fn base_config(scratch: &TempDir) -> Value {
    json!({
        "server": { "port": 0, "bind_address": "127.0.0.1" },
        "state_dir": scratch.path().join("state"),
        "devices": { "poll_interval": POLL_INTERVAL },
        "camera": { "exposure": EXPOSURE }
    })
}

/// Set the JSON `pointer` in `target` to `value`, creating the objects on
/// the way.
pub fn set_pointer(target: &mut Value, pointer: &str, value: Value) {
    let mut node = target;
    let segments: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    let (last, parents) = segments.split_last().expect("a pointer with a segment");
    for segment in parents {
        if !node.get(*segment).is_some_and(Value::is_object) {
            node[*segment] = json!({});
        }
        node = &mut node[*segment];
    }
    node[*last] = value;
}

impl DoctorSmokeWorld for GuiderWorld {
    fn doctor_smoke(&mut self) -> &mut DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> Value {
        base_config(&self.scratch)
    }
}

impl TlsAuthSmokeWorld for GuiderWorld {
    /// Answers 200 whatever the devices, which `/health` does not.
    const PROBE_PATH: &'static str = "/api/v1/devices";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> Value {
        let mut config = base_config(&self.scratch);
        if let Some(object) = config.as_object_mut() {
            object.remove("server");
        }
        config
    }

    async fn start_with_tls_auth(&mut self, config: Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.guider = Some(handle);
    }
}

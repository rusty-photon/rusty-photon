//! BDD World struct and helpers for the guider HTTP service suite.
//!
//! Each scenario spawns `mock_phd2` (with per-scenario env selecting
//! the settle/stop behavior), writes a config pointing at it, and
//! starts `phd2-guider serve` via `bdd_infra::ServiceHandle`.

use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

#[derive(Debug, Default, World)]
pub struct GuiderWorld {
    /// Handle to the spawned `phd2-guider serve` process. Stopped in
    /// the cucumber `after` hook in `tests/bdd.rs`.
    pub service_handle: Option<ServiceHandle>,

    /// The mock PHD2 child. Killed on drop.
    pub mock: Option<MockPhd2Handle>,

    /// Per-scenario temp dir holding the config file and the RPC log.
    pub temp_dir: Option<TempDir>,

    /// JSON-lines file `mock_phd2` appends each received RPC to.
    pub rpc_log_path: Option<PathBuf>,

    /// Result of the most recent HTTP request (status + body).
    pub last_response: Option<HttpResponse>,

    /// State for the shared TLS + auth smoke steps (`auth.feature`).
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,

    /// The config file the running service was started on.
    pub config_path: Option<PathBuf>,

    /// The camera facade's port, reserved before the service starts.
    pub camera_port: Option<u16>,

    /// The facade camera, as an Alpaca client sees it.
    pub camera: Option<std::sync::Arc<dyn ascom_alpaca::api::Camera>>,

    /// The ASCOM error of the most recent facade call that failed:
    /// `(error number, message)`.
    pub last_ascom_error: Option<(u16, String)>,

    /// The frame a staged capture marker names.
    pub marker_frame: Option<PathBuf>,

    /// The most recent `config.schema` / `config.apply` answer.
    pub config_action_reply: Option<serde_json::Value>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for GuiderWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // Not `base_test_config()`: that shape reads the spawned mock
        // PHD2's port, and the doctor smoke never spawns one (doctor
        // only parses the config, it never dials PHD2). Same shape,
        // with the stock PHD2 port and a plain `server` block.
        serde_json::json!({
            "server": { "port": 0 },
            "stop_timeout": "10s",
            "phd2": {
                "host": "127.0.0.1",
                "port": 4400,
                "connection_timeout": "2s",
                "command_timeout": "5s",
                "reconnect": { "enabled": true, "interval": "200ms" }
            },
            "settling": { "pixels": 0.5, "time": "10s", "timeout": "60s" }
        })
    }
}

impl TlsAuthSmokeWorld for GuiderWorld {
    /// The guider is not an Alpaca device; its credential gate is probed on
    /// the health route (which also requires the PHD2 connection to be up).
    const PROBE_PATH: &'static str = "/health";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // Same shape as `start_service`'s config, pointing at the mock PHD2
        // the scenario's first Given spawned.
        let phd2_port = self.mock.as_ref().expect("mock PHD2 not started").port;
        serde_json::json!({
            "stop_timeout": "10s",
            "phd2": {
                "host": "127.0.0.1",
                "port": phd2_port,
                "connection_timeout": "2s",
                "command_timeout": "5s",
                "reconnect": { "enabled": true, "interval": "200ms" }
            },
            "settling": { "pixels": 0.5, "time": "10s", "timeout": "60s" }
        })
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let path = bdd_infra::tls_auth::stage_config_file(&mut self.tls_auth, &config);
        let handle = bdd_infra::ServiceHandle::start_with_args(
            env!("CARGO_PKG_NAME"),
            &["--config", path.to_str().unwrap(), "serve"],
        )
        .await;
        self.tls_auth.port = Some(handle.port);
        // The cucumber `after` hook in `tests/bdd.rs` stops the handle here.
        self.service_handle = Some(handle);
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

#[derive(Debug)]
pub struct MockPhd2Handle {
    pub port: u16,
    /// Taken out once, to be killed and reaped.
    child: Option<Child>,
}

impl Drop for MockPhd2Handle {
    /// Kill the mock and reap it on a thread of its own: the world is
    /// dropped on a runtime worker, and a blocking wait there would stall
    /// every other scenario in flight.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Spawn `cmd` (`mock_phd2`) and wait for the port it announces. A
/// wedged child fails the scenario within 10 s (and is reaped) rather
/// than hanging the suite on an endless stdout read. Blocking: run it on
/// the blocking pool.
fn spawn_mock(mut cmd: Command) -> (u16, Child) {
    let mut child = cmd.spawn().expect("spawn mock_phd2");
    let stdout = child.stdout.take().expect("mock_phd2 stdout piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let port = BufReader::new(stdout).lines().find_map(|line| {
            line.ok().and_then(|l| {
                l.strip_prefix("MOCK_PHD2_PORT:")
                    .and_then(|p| p.parse::<u16>().ok())
            })
        });
        let _ = tx.send(port);
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Some(port)) => (port, child),
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mock_phd2 announced no port within 10s (read outcome: {outcome:?})");
        }
    }
}

impl GuiderWorld {
    /// Locate the in-tree `mock_phd2` binary the way
    /// `tests/test_integration.rs` does: explicit `MOCK_PHD2_BINARY`
    /// (set by the Bazel test target), then `CARGO_BIN_EXE_mock_phd2`
    /// (set by Cargo for `[[test]]` crates).
    pub fn mock_phd2_path() -> PathBuf {
        if let Ok(p) = std::env::var("MOCK_PHD2_BINARY") {
            let path = PathBuf::from(p);
            if path.exists() {
                return path;
            }
        }
        if let Some(p) = option_env!("CARGO_BIN_EXE_mock_phd2") {
            let path = PathBuf::from(p);
            if path.exists() {
                return path;
            }
        }
        panic!(
            "mock_phd2 binary not found. Tried MOCK_PHD2_BINARY env var, then \
             CARGO_BIN_EXE_mock_phd2. Run `cargo build --tests -p phd2-guider`."
        )
    }

    /// Lazily create the per-scenario temp dir.
    pub fn temp_dir_path(&mut self) -> PathBuf {
        if self.temp_dir.is_none() {
            self.temp_dir = Some(TempDir::new().expect("create temp dir"));
        }
        self.temp_dir.as_ref().unwrap().path().to_path_buf()
    }

    /// The camera facade's `image_dir`, created on first use inside the
    /// scenario's temp dir.
    pub fn image_dir(&mut self) -> PathBuf {
        let dir = self.temp_dir_path().join("images");
        std::fs::create_dir_all(&dir).expect("create image_dir");
        dir
    }

    /// Service base URL (e.g., `http://127.0.0.1:37113`).
    pub fn service_url(&self) -> String {
        let handle = self
            .service_handle
            .as_ref()
            .expect("guider service not started — Given step missing?");
        format!("http://127.0.0.1:{}", handle.port)
    }

    /// Spawn `mock_phd2` with an auto-assigned port and the given
    /// settle/stop behavior, parsing the bound port from its stdout.
    pub async fn start_mock(&mut self, settle_mode: &str, stop_mode: &str) {
        self.start_mock_env(settle_mode, stop_mode, &[]).await;
    }

    /// [`Self::start_mock`] with additional mock environment knobs
    /// (e.g. `MOCK_PHD2_ROTATOR=connected` for the equipment
    /// passthrough scenarios).
    pub async fn start_mock_env(
        &mut self,
        settle_mode: &str,
        stop_mode: &str,
        extra: &[(&str, &str)],
    ) {
        let dir = self.temp_dir_path();
        let rpc_log = dir.join("rpc_log.jsonl");
        self.rpc_log_path = Some(rpc_log.clone());

        let mut envs: Vec<(OsString, OsString)> = vec![
            ("MOCK_PHD2_PORT".into(), "0".into()),
            ("MOCK_PHD2_SETTLE_MODE".into(), settle_mode.into()),
            ("MOCK_PHD2_STOP_MODE".into(), stop_mode.into()),
            ("MOCK_PHD2_RPC_LOG".into(), rpc_log.into_os_string()),
        ];
        envs.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into())),
        );
        // The binary lookup (it stats files), the spawn and the wait for
        // the port run on the blocking pool: spawning is slow on a loaded
        // Windows runner, and a step that blocks stalls the one poll loop
        // every scenario shares (see `bdd_infra::off_loop`). A panic in
        // here fails the step with its own message.
        let (port, child) = bdd_infra::off_loop(move || {
            let mut cmd = Command::new(Self::mock_phd2_path());
            cmd.envs(envs)
                .stdout(Stdio::piped())
                // mock_phd2 logs verbosely to stderr; discard it so a full
                // pipe buffer can never deadlock the mock under parallel
                // scenarios (same policy as tests/test_integration.rs).
                .stderr(Stdio::null());
            apply_child_coverage_profile(&mut cmd);
            spawn_mock(cmd)
        })
        .await;
        self.mock = Some(MockPhd2Handle {
            port,
            child: Some(child),
        });
    }

    /// Write a config pointing at the given PHD2 port and start
    /// `phd2-guider serve`. When `wait_connected` is set, poll
    /// `/health` until the service reports the PHD2 connection is up,
    /// so the first scenario request can never race the initial
    /// connect.
    pub async fn start_service(
        &mut self,
        phd2_port: u16,
        stop_timeout: &str,
        wait_connected: bool,
    ) {
        let config = Self::service_config(phd2_port, stop_timeout);
        self.start_service_with_config(config, wait_connected).await;
    }

    /// The suite's config: the service on an OS-picked port, pointed at
    /// the PHD2 on `phd2_port`.
    pub fn service_config(phd2_port: u16, stop_timeout: &str) -> serde_json::Value {
        serde_json::json!({
            "server": {
                "bind_address": "127.0.0.1",
                "port": 0,  // OS picks a free port; ServiceHandle parses it from stdout
            },
            "stop_timeout": stop_timeout,
            "phd2": {
                "host": "127.0.0.1",
                "port": phd2_port,
                "connection_timeout": "2s",
                "command_timeout": "5s",
                "reconnect": { "enabled": true, "interval": "200ms" }
            },
            "settling": { "pixels": 0.5, "time": "10s", "timeout": "60s" }
        })
    }

    /// Write `config` and start `phd2-guider serve` on it. See
    /// [`Self::start_service`] for `wait_connected`.
    pub async fn start_service_with_config(
        &mut self,
        config: serde_json::Value,
        wait_connected: bool,
    ) {
        let dir = self.temp_dir_path();
        let config_path = dir.join("config.json");
        self.config_path = Some(config_path.clone());
        tokio::fs::write(&config_path, config.to_string())
            .await
            .expect("write config");
        let config_str = config_path.to_string_lossy().into_owned();

        let handle =
            ServiceHandle::start_with_args("phd2-guider", &["--config", &config_str, "serve"])
                .await;
        self.service_handle = Some(handle);

        if wait_connected {
            let url = format!("{}/health", self.service_url());
            let client = reqwest::Client::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                // Per-request timeout keeps a hung request from blowing
                // past the loop's overall deadline.
                let probe = client.get(&url).timeout(Duration::from_secs(2)).send();
                if let Ok(response) = probe.await {
                    if response.status().as_u16() == 200 {
                        break;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "guider service never reported a PHD2 connection on /health"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    /// Record an HTTP response into the world.
    pub async fn record_response(&mut self, response: reqwest::Response) {
        let status = response.status().as_u16();
        let text = response.text().await.expect("read response body");
        let body = if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("non-JSON response body ({e}): {text}"))
        };
        self.last_response = Some(HttpResponse { status, body });
    }

    pub const fn last_response(&self) -> &HttpResponse {
        self.last_response
            .as_ref()
            .expect("no HTTP request made yet — When step missing?")
    }

    /// Read the mock's RPC log: one `{method, params}` JSON object per
    /// line. A missing file means no RPC was received yet.
    pub fn logged_rpcs(&self) -> Vec<serde_json::Value> {
        let Some(path) = &self.rpc_log_path else {
            return Vec::new();
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("RPC log line is JSON"))
            .collect()
    }

    /// The logged RPCs with the given method name.
    pub fn logged_rpcs_named(&self, method: &str) -> Vec<serde_json::Value> {
        self.logged_rpcs()
            .into_iter()
            .filter(|rpc| rpc["method"] == method)
            .collect()
    }

    /// Shared suite-wide HTTP client. The timeout exceeds the
    /// worst-case settle backstop (default settle.timeout 60 s + the
    /// 10 s grace), so a legitimate settle-timeout response is never
    /// preempted client-side.
    pub fn http_client() -> reqwest::Client {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(90))
                    .build()
                    .expect("build reqwest client")
            })
            .clone()
    }
}

/// Point a directly spawned child at its own coverage profile pool
/// under `bazel coverage`. No-op unless `COVERAGE_DIR` is set. Same
/// helper as `tests/test_integration.rs` (see PR #342).
fn apply_child_coverage_profile(cmd: &mut Command) {
    if let Some(dir) = std::env::var_os("COVERAGE_DIR") {
        let mut path = PathBuf::from(&dir);
        if path.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                path = cwd.join(path);
            }
        }
        path.push("phd2-guider-%8m.profraw");
        cmd.env("LLVM_PROFILE_FILE", path);
    }
}

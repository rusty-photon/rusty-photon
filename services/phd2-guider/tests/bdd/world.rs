//! BDD World struct and helpers for the guider HTTP service suite.
//!
//! Each scenario spawns `mock_phd2` (with per-scenario env selecting
//! the settle/stop behavior), writes a config pointing at it, and
//! starts `phd2-guider serve` via `bdd_infra::ServiceHandle`.

use crate::phd2_unit::Phd2Unit;
use bdd_infra::setup_pages::{self, SetupPagesState, SetupPagesWorld};
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

    /// State for the shared setup-page steps (`setup_pages.feature`).
    pub setup_pages: SetupPagesState,

    /// The facade's answer to a setup path, unfollowed: its status and
    /// its `Location`.
    pub facade_setup_answer: Option<(u16, Option<String>)>,

    /// The most recent sweep-lease Action's answer.
    pub lease_reply: Option<serde_json::Value>,

    /// The facade image as the `ImageBytes` body the client last fetched.
    pub image_bytes: Option<Vec<u8>>,

    /// The stand-in for the `rusty-photon-phd2` unit, in the restart
    /// scenarios.
    pub unit: Option<Phd2Unit>,

    /// Environment the guider service starts with, beyond the suite's own.
    pub service_env: Vec<(String, String)>,
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

impl SetupPagesWorld for GuiderWorld {
    fn setup_pages(&mut self) -> &mut SetupPagesState {
        &mut self.setup_pages
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
        self.tls_auth.port = Some(handle.bound_port());
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

/// A `mock_phd2` command with `envs`, its stdout piped for the port it
/// announces.
fn mock_command(envs: &[(OsString, OsString)]) -> Command {
    let mut cmd = Command::new(GuiderWorld::mock_phd2_path());
    cmd.envs(envs.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        // mock_phd2 logs verbosely to stderr; discard it so a full
        // pipe buffer can never deadlock the mock under parallel
        // scenarios (same policy as tests/test_integration.rs).
        .stderr(Stdio::null());
    apply_child_coverage_profile(&mut cmd);
    cmd
}

/// Spawn `cmd` (`mock_phd2`) and wait for the port it announces. A
/// wedged child fails the scenario within 10 s (and is reaped) rather
/// than hanging the suite on an endless stdout read. Blocking: run it on
/// the blocking pool.
fn spawn_mock(mut cmd: Command) -> (u16, Child) {
    let mut child = bdd_infra::with_spawn_lock(|| cmd.spawn()).expect("spawn mock_phd2");
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
    /// Locate the in-tree `mock_phd2` binary: explicit `MOCK_PHD2_BINARY`
    /// (set by the Bazel test target), then `CARGO_BIN_EXE_mock_phd2` (set
    /// by Cargo for `[[test]]` crates). A set override is used as given, as
    /// `bdd_infra`'s binary lookup uses one: a stale path fails the spawn
    /// loudly, naming it, rather than quietly testing another build.
    pub fn mock_phd2_path() -> PathBuf {
        if let Some(path) = std::env::var_os("MOCK_PHD2_BINARY") {
            return PathBuf::from(path);
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
        format!("http://127.0.0.1:{}", handle.port())
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

        // The unit starts PHD2 again on the port it had, so a mock the unit
        // keeps takes a port the test knows first (docs/skills/testing.md
        // § 5.1); any other binds port 0 and announces it.
        let keeps = self.unit.as_ref().is_some_and(Phd2Unit::keeps);
        let port = if keeps {
            bdd_infra::reserved_test_port_async().await
        } else {
            0
        };
        let mut envs: Vec<(OsString, OsString)> = vec![
            ("MOCK_PHD2_PORT".into(), port.to_string().into()),
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
        let first = envs.clone();
        let (port, child) = bdd_infra::off_loop(move || spawn_mock(mock_command(&first))).await;
        let child = match self.unit.as_ref().filter(|unit| unit.keeps()) {
            Some(unit) => {
                // The PHD2 the unit starts again is the same PHD2, without
                // the scenario's fault: it starts Stopped and answers.
                envs.retain(|(key, _)| key != "MOCK_PHD2_MODE" && key != "MOCK_PHD2_APP_STATE");
                unit.adopt(child, Box::new(move || spawn_mock(mock_command(&envs))));
                None
            }
            None => Some(child),
        };
        self.mock = Some(MockPhd2Handle { port, child });
    }

    /// Stand in for the `rusty-photon-phd2` unit: keeping PHD2 running
    /// (`keeps`), or inactive. A mock PHD2 started after this is the unit's
    /// main process when the unit keeps PHD2 running.
    pub async fn stand_in_for_the_unit(&mut self, keeps: bool) {
        let dir = self.temp_dir_path().join("unit");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create the unit's directory");
        let unit = Phd2Unit::create(dir, keeps).await;
        self.service_env = vec![("PATH".to_string(), unit.path_env())];
        self.unit = Some(unit);
    }

    /// The suite's config, switched over to PHD2 under the service manager.
    pub fn managed_config(phd2_port: u16) -> serde_json::Value {
        let mut config = Self::service_config(phd2_port, "10s");
        config["phd2"]["managed"] = true.into();
        // Below the suite's HTTP timeout, so a restart that fails answers
        // before the client gives up on it.
        config["phd2"]["restart_timeout"] = "60s".into();
        config
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

        self.spawn_on_config_file().await;

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

    /// Start `phd2-guider serve` on the scenario's config file and point the
    /// setup-page steps at it.
    async fn spawn_on_config_file(&mut self) {
        let config_path = self
            .config_path
            .clone()
            .expect("the scenario wrote no config file");
        let config_str = config_path.to_string_lossy().into_owned();
        let envs: Vec<(&str, &str)> = self
            .service_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let handle = ServiceHandle::start_with_env(
            "phd2-guider",
            &["--config", &config_str, "serve"],
            &envs,
        )
        .await;
        setup_pages::adopt_plain(&mut self.setup_pages, handle.bound_port(), config_path);
        self.service_handle = Some(handle);
    }

    /// Stop the service and start it again on the same config file: what a
    /// saved change waits for.
    pub async fn restart_service(&mut self) {
        let mut handle = self
            .service_handle
            .take()
            .expect("the guider service was not started");
        handle.stop().await;
        self.camera = None;
        self.spawn_on_config_file().await;
    }

    /// Start the service behind TLS and auth and point the setup-page steps
    /// at it.
    pub async fn start_with_tls_and_auth_pages(&mut self) {
        let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
        self.tls_auth.pki = Some(pki);
        let mut config = self.base_test_config();
        config["server"] = self.tls_auth.pki().server_block(0);
        self.start_with_tls_auth(config).await;
        let config_path = bdd_infra::tls_auth::staged_config_path(&self.tls_auth)
            .expect("the TLS fixture staged a config file");
        setup_pages::adopt_tls_auth(&mut self.setup_pages, &self.tls_auth, config_path);
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

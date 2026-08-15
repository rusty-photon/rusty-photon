//! BDD test world for sentinel service (binary-spawning pattern)

use std::path::PathBuf;
use std::time::Duration;

use bdd_infra::ServiceHandle;
use cucumber::World;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default, World)]
pub struct SentinelWorld {
    // Service handles
    pub filemonitor: Option<ServiceHandle>,
    pub sentinel: Option<ServiceHandle>,

    // Temp file management
    pub temp_dir: Option<TempDir>,
    pub temp_file_path: Option<PathBuf>,

    // Filemonitor config accumulation
    pub fm_rules: Vec<serde_json::Value>,
    pub fm_polling_interval: u64,

    // Sentinel config accumulation
    pub sentinel_monitor_name: String,
    pub sentinel_polling_interval: u64,
    pub sentinel_transitions: Vec<serde_json::Value>,
    pub sentinel_has_notifiers: bool,
    pub sentinel_monitors: Vec<serde_json::Value>,
    pub sentinel_probe_domain: Option<String>,

    // Result capture
    pub last_response_body: Option<String>,
    pub last_status_code: Option<u16>,
    pub last_error: Option<String>,

    // TLS + auth test state (shared PKI fixture: CA, service cert, credentials)
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,

    // Local Pushover API stub so notification scenarios never hit the real
    // api.pushover.net (slow, non-hermetic, rejects test credentials).
    pub pushover_stub: Option<tokio::task::JoinHandle<()>>,
    pub pushover_stub_url: Option<String>,

    // Operation watchdog: a controllable stub standing in for rp's SSE
    // stream, plus the URL sentinel's watchdog should subscribe to.
    pub rp_event_stub: Option<RpEventStub>,
    pub watchdog_rp_url: Option<String>,

    // Corrective ladder: a stub Alpaca service the watchdog can health-check
    // and abort, discovered as the "mount" service. Present only for the
    // abort scenario.
    pub mount_stub: Option<MountServiceStub>,

    // Whether the mount stub has been wired into discovery (unit + sibling
    // config), so the watchdog's `slew` family runs the ladder against it.
    pub mount_discovered: bool,

    // Service discovery: the stub service-manager directory handed to the
    // spawned sentinel via SENTINEL_SERVICE_MANAGER_DIR.
    pub service_manager_dir: Option<PathBuf>,

    // Service health supervision: a stub HTTP service whose /health answer
    // (an arbitrary status code) is flippable at runtime.
    pub health_stub: Option<FlippableHealthStub>,

    // Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for SentinelWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // A fresh world's accumulated config: no monitors, notifiers, or
        // transitions — the same minimal shape the monitoring scenarios
        // stage, proven against sentinel's deny_unknown_fields load.
        self.build_sentinel_config()
    }
}

impl SentinelWorld {
    /// Create a temp file with the given content and store its path.
    pub fn create_temp_file(&mut self, content: &str) -> PathBuf {
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let file_path = dir.path().join("monitored.txt");
        std::fs::write(&file_path, content).expect("failed to write temp file");
        self.temp_file_path = Some(file_path.clone());
        file_path
    }

    /// The shared PKI fixture (panics if the cert-generation Given hasn't run).
    pub fn pki(&self) -> &bdd_infra::tls_auth::PkiFixture {
        self.pki.as_deref().expect("TLS certs not generated")
    }

    /// `server.tls` fragment for the filemonitor certificate pair signed by
    /// the fixture's CA — the cross-service scenarios spawn a TLS-enabled
    /// filemonitor that must chain to the same CA sentinel trusts.
    pub fn fm_tls_block(&self) -> serde_json::Value {
        let certs_dir = self
            .pki()
            .cert_path()
            .parent()
            .expect("cert path has no parent")
            .to_path_buf();
        serde_json::json!({
            "cert": certs_dir.join("filemonitor.pem").to_string_lossy(),
            "key": certs_dir.join("filemonitor-key.pem").to_string_lossy(),
        })
    }

    /// Build filemonitor JSON config from accumulated state.
    pub fn build_filemonitor_config(&self) -> serde_json::Value {
        let file_path = self.temp_file_path.as_ref().map_or_else(
            || "nonexistent.txt".to_string(),
            |p| p.to_string_lossy().to_string(),
        );

        let polling = if self.fm_polling_interval > 0 {
            self.fm_polling_interval
        } else {
            60
        };

        serde_json::json!({
            "device": {
                "name": "Test",
                "unique_id": "sentinel-bdd-test",
                "description": "BDD test device"
            },
            "file": {
                "path": file_path,
                "polling_interval": format!("{polling}s")
            },
            "parsing": {
                "rules": self.fm_rules,
                "case_sensitive": false
            },
            "server": {
                "port": 0,
                "discovery_port": null
            }
        })
    }

    /// Build sentinel JSON config pointing at the given filemonitor port.
    pub fn build_sentinel_config(&self) -> serde_json::Value {
        let polling = if self.sentinel_polling_interval > 0 {
            self.sentinel_polling_interval
        } else {
            1
        };

        let mut config = serde_json::json!({
            "monitors": self.sentinel_monitors,
            "notifiers": [],
            "transitions": self.sentinel_transitions,
            "dashboard": {
                "enabled": true,
                "history_size": 100
            },
            "server": {
                "port": 0
            }
        });

        if let Some(domain) = &self.sentinel_probe_domain {
            config["probe_domain"] = serde_json::json!(domain);
        }

        if self.sentinel_has_notifiers {
            let mut pushover = serde_json::json!({
                "type": "pushover",
                "api_token": "test-token",
                "user_key": "test-user"
            });
            // Point the notifier at the local stub instead of api.pushover.net.
            if let Some(url) = &self.pushover_stub_url {
                pushover["api_url"] = serde_json::json!(url);
            }
            config["notifiers"] = serde_json::json!([pushover]);
        }

        // Set polling interval on all monitors
        if let Some(monitors) = config["monitors"].as_array_mut() {
            for m in monitors.iter_mut() {
                m["polling_interval"] = serde_json::json!(format!("{polling}s"));
            }
        }

        // Wire the operation watchdog when a watched rp URL is set. Buffers are
        // zeroed so the tracked deadline equals the operation's
        // `max_duration_ms` exactly, keeping the BDD fast and deterministic;
        // reconnect is tight so the "unresponsive" path resolves quickly.
        if let Some(rp_url) = &self.watchdog_rp_url {
            let mut watchdog = serde_json::json!({
                "rp_url": rp_url,
                "reconnect_max_attempts": 2,
                "reconnect_backoff": "1s",
                "default_buffer": "0s",
                "notifiers": ["pushover"],
                "operations": { "slew": { "buffer": "0s" } }
            });
            // When the corrective service stub is discovered, make `slew` run
            // the abort ladder against it (responsive service + abort verb =>
            // the ladder stops at a clean abort, so nothing is restarted). The
            // stub is wired into discovery — a `rusty-photon-mount` unit plus
            // a sibling mount.json carrying the stub's port — by
            // `start_mount_service_stub`.
            if self.mount_discovered {
                watchdog["operations"]["slew"]["on_expiry"] =
                    serde_json::json!("abort_then_restart");
                watchdog["operations"]["slew"]["service"] = serde_json::json!("mount");
            }
            config["operation_watchdog"] = watchdog;
        }

        config
    }

    /// Start filemonitor binary with the accumulated config.
    pub async fn start_filemonitor(&mut self) {
        let config_json = self.build_filemonitor_config();
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let config_path = dir.path().join("filemonitor_config.json");
        std::fs::write(&config_path, config_json.to_string())
            .expect("failed to write filemonitor config");

        let handle = ServiceHandle::start("filemonitor", config_path.to_str().unwrap()).await;

        self.filemonitor = Some(handle);
    }

    /// Add a monitor config entry pointing at the running filemonitor.
    pub fn add_filemonitor_monitor(&mut self, name: &str) {
        let fm = self.filemonitor.as_ref().expect("filemonitor not started");
        self.sentinel_monitor_name = name.to_string();
        self.sentinel_monitors.push(serde_json::json!({
            "type": "alpaca_safety_monitor",
            "name": name,
            "host": "127.0.0.1",
            "port": fm.port,
            "device_number": 0,
            "polling_interval": "1s"
        }));
    }

    /// The stub service-manager directory (created on first use, with fast
    /// policy timings and an empty unit listing). Handed to the spawned
    /// sentinel via `SENTINEL_SERVICE_MANAGER_DIR`, so no scenario ever
    /// enumerates the host's real service manager.
    pub fn service_manager_dir(&mut self) -> PathBuf {
        if let Some(dir) = &self.service_manager_dir {
            return dir.clone();
        }
        let root = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let dir = root.path().join("svcmgr");
        std::fs::create_dir_all(&dir).expect("failed to create service manager dir");
        std::fs::write(dir.join("units.txt"), "").expect("failed to seed units.txt");
        // Tight timings so threshold/backoff scenarios resolve in seconds:
        // discovery every 250ms, probe every 200ms, threshold 4, backoff
        // 1s..2s, restart budget 1s. Threshold is 4 (not the minimum 2) so
        // the negative "records no restarts" assertions survive a loaded CI
        // runner: a single slow/timed-out probe is normal jitter, and even
        // two in a row must not read as a dead service — see issue #677.
        std::fs::write(
            dir.join("policy.json"),
            serde_json::json!({
                "discovery_interval": "250ms",
                "poll_interval": "200ms",
                "failure_threshold": 4,
                "restart_backoff": "1s",
                "restart_backoff_max": "2s",
                "restart_budget": "1s"
            })
            .to_string(),
        )
        .expect("failed to write policy.json");
        self.service_manager_dir = Some(dir.clone());
        dir
    }

    fn units_txt_path(&mut self) -> PathBuf {
        self.service_manager_dir().join("units.txt")
    }

    /// Add (or replace) a unit line in the stub's `units.txt`.
    pub fn add_discovered_unit(&mut self, unit: &str, state: &str) {
        use std::fmt::Write as _;
        self.remove_discovered_unit(unit);
        let path = self.units_txt_path();
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        writeln!(content, "{unit} {state}").unwrap();
        std::fs::write(&path, content).expect("failed to write units.txt");
    }

    /// Drop a unit line from the stub's `units.txt`.
    pub fn remove_discovered_unit(&mut self, unit: &str) {
        let path = self.units_txt_path();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let kept: String = content
            .lines()
            .filter(|line| line.split_whitespace().next() != Some(unit))
            .fold(String::new(), |mut acc, line| {
                acc.push_str(line);
                acc.push('\n');
                acc
            });
        std::fs::write(&path, kept).expect("failed to write units.txt");
    }

    /// Make the stub fail every restart of `unit`.
    pub fn fail_restarts_of(&mut self, unit: &str) {
        let path = self
            .service_manager_dir()
            .join(format!("restart-fail-{unit}"));
        std::fs::write(path, "").expect("failed to write restart-fail marker");
    }

    /// Keep restarted units in their prior state (models a restart that does
    /// not bring the service back), for every unit currently listed.
    pub fn leave_restarted_units_stuck(&mut self) {
        let units: Vec<String> = std::fs::read_to_string(self.units_txt_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect();
        let dir = self.service_manager_dir();
        for unit in units {
            std::fs::write(dir.join(format!("stuck-{unit}")), "")
                .expect("failed to write stuck marker");
        }
    }

    /// Restart-log lines (one unit name per recorded restart).
    pub fn restart_log(&mut self) -> Vec<String> {
        std::fs::read_to_string(self.service_manager_dir().join("restarts.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Poll the stub's restart log until it records at least `min` restarts
    /// of `unit` or `ceiling` elapses. Returns the final count.
    pub async fn wait_for_restarts(&mut self, unit: &str, min: usize, ceiling: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + ceiling;
        loop {
            let count = self.restart_log().iter().filter(|l| *l == unit).count();
            if count >= min || tokio::time::Instant::now() >= deadline {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Write a supervised service's own `<service>.json` next to sentinel's
    /// config file, so discovery can derive its probe URL from the shared
    /// `server` block. `bind_address` is pinned to the loopback literal the
    /// stub actually listens on (matching every other service's BDD world)
    /// so sentinel's probe never pays for a `localhost` hostname resolution
    /// — a source of intermittent latency on loaded Windows CI runners that
    /// contributed to issue #677.
    pub fn write_service_config(&mut self, service: &str, port: u16) {
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        std::fs::write(
            dir.path().join(format!("{service}.json")),
            serde_json::json!({ "server": { "port": port, "bind_address": "127.0.0.1" } })
                .to_string(),
        )
        .expect("failed to write sibling service config");
    }

    /// POST a dashboard endpoint (empty body) and capture status + body.
    pub async fn http_post(&mut self, path: &str) {
        let client = reqwest::Client::new();
        let url = self.dashboard_url(path);
        match client.post(&url).send().await {
            Ok(resp) => {
                self.last_status_code = Some(resp.status().as_u16());
                self.last_response_body = Some(resp.text().await.unwrap_or_default());
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
        }
    }

    /// Start a local stub that mimics the Pushover API, replying 200 to any
    /// request. Lets notification scenarios exercise the dispatch path without
    /// the real api.pushover.net round-trip (slow, network-dependent, and the
    /// source of the flaky "history is empty" race the fixed sleep used to mask).
    pub async fn start_pushover_stub(&mut self) {
        use axum::Router;

        let app = Router::new().fallback(|| async {
            axum::Json(serde_json::json!({ "status": 1, "request": "stub" }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind pushover stub listener");
        let addr = listener
            .local_addr()
            .expect("pushover stub listener has no local addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.pushover_stub = Some(handle);
        self.pushover_stub_url = Some(format!("http://{addr}/1/messages.json"));
    }

    /// Start sentinel binary with the accumulated config. The stub service
    /// manager directory is always created and passed via
    /// `SENTINEL_SERVICE_MANAGER_DIR` — a spawned sentinel must never
    /// enumerate the host's real service manager, even in scenarios that
    /// discover nothing.
    pub async fn start_sentinel(&mut self) {
        // Stand up the Pushover stub before sentinel so its URL can be baked
        // into the config the child process loads.
        if self.sentinel_has_notifiers && self.pushover_stub_url.is_none() {
            self.start_pushover_stub().await;
        }
        let manager_dir = self.service_manager_dir();
        let config_json = self.build_sentinel_config();
        let dir = self
            .temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"));
        let config_path = dir.path().join("sentinel_config.json");
        std::fs::write(&config_path, config_json.to_string())
            .expect("failed to write sentinel config");

        let handle = ServiceHandle::start_with_env(
            env!("CARGO_PKG_NAME"),
            &["--config", config_path.to_str().unwrap()],
            &[(
                "SENTINEL_SERVICE_MANAGER_DIR",
                manager_dir.to_str().unwrap(),
            )],
        )
        .await;

        self.sentinel = Some(handle);
    }

    /// Try to start sentinel, capturing errors instead of panicking.
    pub async fn try_start_sentinel(&mut self, config_path: &str) {
        match ServiceHandle::try_start(env!("CARGO_PKG_NAME"), config_path).await {
            Ok(handle) => {
                self.sentinel = Some(handle);
                self.last_error = None;
            }
            Err(e) => {
                self.last_error = Some(e);
            }
        }
    }

    /// Build the dashboard URL for a given path.
    pub fn dashboard_url(&self, path: &str) -> String {
        let sentinel = self.sentinel.as_ref().expect("sentinel not started");
        format!("{}{}", sentinel.base_url, path)
    }

    /// Wait until sentinel has polled at least once (`last_poll_epoch_ms` > 0).
    pub async fn wait_for_poll(&self) {
        let client = reqwest::Client::new();
        let url = self.dashboard_url("/api/status");

        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(json) = resp.json::<Vec<serde_json::Value>>().await {
                    if json.iter().any(|m| {
                        m.get("last_poll_epoch_ms")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0)
                            > 0
                    }) {
                        return;
                    }
                }
            }
        }
        panic!("sentinel did not poll within 30 seconds");
    }

    /// Poll `/api/status` until `name` reports `expected_state`, or ~15s elapses.
    /// Returns the last observed state for that monitor (so a timeout produces a
    /// useful assertion message). Replaces a blind fixed sleep, removing the race
    /// against filemonitor + sentinel polling latency.
    pub async fn wait_for_status(&self, name: &str, expected_state: &str) -> Option<String> {
        let mut last = None;
        for _ in 0..60 {
            for monitor in self.get_status().await {
                if monitor["name"].as_str() == Some(name) {
                    let state = monitor["state"].as_str().map(str::to_string);
                    if state.as_deref() == Some(expected_state) {
                        return state;
                    }
                    last = state;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        last
    }

    /// Poll `/api/history` until a record matches `predicate`, or ~15s elapses.
    /// Returns the final history snapshot regardless, so the caller can assert and
    /// print it on failure. Waits for the notification record to actually land
    /// rather than assuming a fixed delay is enough.
    pub async fn wait_for_history<F>(&self, predicate: F) -> Vec<serde_json::Value>
    where
        F: Fn(&serde_json::Value) -> bool,
    {
        let mut history = self.get_history().await;
        for _ in 0..60 {
            if history.iter().any(&predicate) {
                return history;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            history = self.get_history().await;
        }
        history
    }

    /// GET a dashboard endpoint and return the response body as string.
    pub async fn http_get(&mut self, path: &str) {
        let client = reqwest::Client::new();
        let url = self.dashboard_url(path);
        match client.get(&url).send().await {
            Ok(resp) => {
                self.last_status_code = Some(resp.status().as_u16());
                self.last_response_body = Some(resp.text().await.unwrap_or_default());
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
            }
        }
    }

    /// GET /api/status and parse as JSON array.
    pub async fn get_status(&self) -> Vec<serde_json::Value> {
        let client = reqwest::Client::new();
        let url = self.dashboard_url("/api/status");
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("failed to GET /api/status");
        resp.json().await.expect("failed to parse status JSON")
    }

    /// GET /api/history and parse as JSON array.
    pub async fn get_history(&self) -> Vec<serde_json::Value> {
        let client = reqwest::Client::new();
        let url = self.dashboard_url("/api/history");
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("failed to GET /api/history");
        resp.json().await.expect("failed to parse history JSON")
    }

    /// Start the controllable rp SSE stub with the given pre-formatted SSE
    /// frames and point the watchdog at it.
    pub async fn start_rp_event_stub(&mut self, frames: Vec<String>) {
        let stub = RpEventStub::start(frames).await;
        self.watchdog_rp_url = Some(stub.base_url().to_string());
        self.rp_event_stub = Some(stub);
    }

    /// Start a stub Alpaca telescope service the corrective ladder can probe
    /// (reports connected) and abort (records the call), and wire it into
    /// discovery as the running `mount` service: a `rusty-photon-mount` unit
    /// in the stub manager plus a sibling `mount.json` carrying the stub's
    /// port, from which sentinel derives its Alpaca base URL.
    pub async fn start_mount_service_stub(&mut self) {
        let stub = MountServiceStub::start().await;
        self.add_discovered_unit("rusty-photon-mount", "running");
        self.write_service_config("mount", stub.port());
        self.mount_discovered = true;
        self.mount_stub = Some(stub);
    }

    /// Start the flippable health stub answering the given HTTP status, and
    /// wire it into discovery as `service` in the given run state: a
    /// `rusty-photon-<service>` unit plus a sibling `<service>.json` carrying
    /// the stub's port, from which sentinel derives the probe URL.
    pub fn discover_health_stub_as(&mut self, service: &str, state: &str) {
        let port = self
            .health_stub
            .as_ref()
            .expect("health stub not started")
            .port();
        self.add_discovered_unit(&format!("rusty-photon-{service}"), state);
        self.write_service_config(service, port);
    }

    /// Start the flippable health stub answering the given HTTP status.
    pub async fn start_health_stub(&mut self, status: u16) {
        self.health_stub = Some(FlippableHealthStub::start(status).await);
    }

    /// Start the flippable health stub answering the given HTTP status with
    /// a fixed raw body (served for every status the stub is flipped to).
    pub async fn start_health_stub_with_body(&mut self, status: u16, body: String) {
        self.health_stub = Some(FlippableHealthStub::start_with_body(status, Some(body)).await);
    }

    /// GET /api/services and parse as JSON array.
    pub async fn get_services(&self) -> Vec<serde_json::Value> {
        let client = reqwest::Client::new();
        let url = self.dashboard_url("/api/services");
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("failed to GET /api/services");
        resp.json().await.expect("failed to parse services JSON")
    }

    /// Poll `/api/services` until `name` reports `expected` health, or ~15s
    /// elapses. Returns the last observed health for the assertion message.
    pub async fn wait_for_service_health(&self, name: &str, expected: &str) -> Option<String> {
        let mut last = None;
        for _ in 0..60 {
            for service in self.get_services().await {
                if service["name"].as_str() == Some(name) {
                    let health = service["health"].as_str().map(str::to_string);
                    if health.as_deref() == Some(expected) {
                        return health;
                    }
                    last = health;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        last
    }
}

/// A stub HTTP service whose `GET /health` answer (an arbitrary HTTP status,
/// e.g. 200, 401, 503) is flippable at runtime — the supervised target for
/// the health-supervision BDD.
#[derive(Debug)]
pub struct FlippableHealthStub {
    port: u16,
    status: std::sync::Arc<std::sync::atomic::AtomicU16>,
    handle: tokio::task::JoinHandle<()>,
}

impl FlippableHealthStub {
    pub async fn start(initial_status: u16) -> Self {
        Self::start_with_body(initial_status, None).await
    }

    /// `body`: a fixed raw JSON body to serve instead of the default
    /// `{"status":"stub"}` — how the degraded-message scenarios put a
    /// `message` field on the wire.
    pub async fn start_with_body(initial_status: u16, body: Option<String>) -> Self {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::sync::atomic::Ordering;

        let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(initial_status));
        let answer = std::sync::Arc::clone(&status);
        let app = Router::new().route(
            "/health",
            get(move || {
                let answer = std::sync::Arc::clone(&answer);
                let body = body.clone();
                async move {
                    let code = StatusCode::from_u16(answer.load(Ordering::SeqCst))
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    match body {
                        Some(raw) => (code, [(header::CONTENT_TYPE, "application/json")], raw)
                            .into_response(),
                        None => {
                            (code, Json(serde_json::json!({ "status": "stub" }))).into_response()
                        }
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind health stub");
        let addr = listener.local_addr().expect("health stub has no addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            port: addr.port(),
            status,
            handle,
        }
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub fn set_status(&self, status: u16) {
        self.status
            .store(status, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for FlippableHealthStub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A stub Alpaca telescope service for the corrective-ladder BDD: it answers
/// `GET .../telescope/0/connected` as responsive, records every
/// `PUT .../telescope/0/abortslew` so the test can assert the watchdog aborted
/// the right device, and answers the management API so health supervision
/// (which probes every discovered running service) sees it as up.
#[derive(Debug)]
pub struct MountServiceStub {
    port: u16,
    abort_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
    handle: tokio::task::JoinHandle<()>,
}

impl MountServiceStub {
    pub async fn start() -> Self {
        use axum::routing::{get, put};
        use axum::{Json, Router};
        use std::sync::atomic::Ordering;

        let abort_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count = std::sync::Arc::clone(&abort_count);
        let app = Router::new()
            .route(
                "/api/v1/telescope/0/connected",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": true, "ErrorNumber": 0, "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [], "ErrorNumber": 0, "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/abortslew",
                put(move || {
                    let count = std::sync::Arc::clone(&count);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind mount service stub");
        let addr = listener
            .local_addr()
            .expect("mount service stub has no addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            port: addr.port(),
            abort_count,
            handle,
        }
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Number of `abortslew` calls received so far.
    pub fn abort_count(&self) -> u32 {
        self.abort_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for MountServiceStub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A minimal, controllable Server-Sent-Events server standing in for rp's
/// `GET /api/events/subscribe`. Every accepted connection receives the
/// pre-scripted frames (as one chunked-transfer body) and is then held open —
/// no disconnect — so the sentinel watchdog tracks exactly the operations the
/// script describes. Built on raw tokio TCP so it pulls in no new dependency.
#[derive(Debug)]
pub struct RpEventStub {
    base_url: String,
    cancel: CancellationToken,
}

impl RpEventStub {
    /// Bind on an ephemeral loopback port and serve `frames` (each an SSE
    /// block without its trailing blank line) to every connection.
    pub async fn start(frames: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind rp event stub");
        let addr = listener.local_addr().expect("rp event stub has no addr");
        let cancel = CancellationToken::new();
        let body: String = frames.iter().fold(String::new(), |mut acc, f| {
            acc.push_str(f);
            acc.push_str("\n\n");
            acc
        });
        let server_cancel = cancel.clone();

        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = server_cancel.cancelled() => break,
                    res = listener.accept() => res,
                };
                let Ok((mut sock, _)) = accepted else { break };
                let body = body.clone();
                let conn_cancel = server_cancel.clone();
                tokio::spawn(async move {
                    // Drain the request so the client's write completes.
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let chunk = format!("{:x}\r\n{}\r\n", body.len(), body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: text/event-stream\r\n\
                         Cache-Control: no-cache\r\n\
                         Transfer-Encoding: chunked\r\n\r\n{chunk}"
                    );
                    if sock.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                    // Hold the connection open until the stub is shut down.
                    conn_cancel.cancelled().await;
                });
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            cancel,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for RpEventStub {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

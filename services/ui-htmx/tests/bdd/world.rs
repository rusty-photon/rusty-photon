//! Cucumber `World` for the `ui-htmx` config-page BDD suite.
//!
//! Mirrors the binary-spawning pattern the other services use (see
//! [`sentinel`](../../../../sentinel/tests/bdd/world.rs)): every scenario
//! spawns the *real* `ui-htmx` binary **plus** a real `rp` orchestrator and a
//! real `dsd-fp2` driver (mock hardware) rostered in rp's config — every
//! config target is roster-derived (#569) — and drives the BFF over HTTP.
//! There is no in-process router and no stubbed client: the production
//! `ReqwestHttpClient` → `AlpacaConfigClient` path and the driver's real
//! `config.get` / `config.apply` / in-process reload are exercised end to end.
//!
//! Requires both binaries pre-built with `--all-features` (the `dsd-fp2` mock
//! transport is feature-gated): `cargo build --all-features --all-targets`.

use std::path::PathBuf;
use std::time::Duration;

use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::browser::BrowserSession;
use crate::dom;

/// The dsd-fp2 `CoverCalibrator` action endpoint the BFF (and these helpers) call.
const DRIVER_ACTION_PATH: &str = "/api/v1/covercalibrator/0/action";

/// A reserved, low-numbered loopback port that reliably refuses connections —
/// the repo's convention for an unreachable Alpaca target (see ui-htmx's
/// `io.rs` test and rp's BDD steps). Deterministic, unlike a released free port.
const UNREACHABLE_PORT: u16 = 1;

/// The roster id the dsd-fp2 driver is registered under in rp's config — the
/// config-page suites reach it at [`UiWorld::device_config_path`].
const DEVICE_ID: &str = "dsd-fp2";

#[derive(Debug, Default, World)]
pub struct UiWorld {
    /// The dsd-fp2 driver the BFF configures (absent in the "unreachable" case).
    pub driver: Option<ServiceHandle>,
    /// The BFF under test.
    pub ui: Option<ServiceHandle>,
    /// A real sentinel the BFF's restart affordance calls (restart scenarios only).
    pub sentinel: Option<ServiceHandle>,
    /// The stub service-manager directory handed to the spawned sentinel via
    /// `SENTINEL_SERVICE_MANAGER_DIR` (restart scenarios only): sentinel
    /// discovers the units its `units.txt` lists and records every restart in
    /// its `restarts.log`.
    service_manager_dir: Option<PathBuf>,
    temp_dir: Option<TempDir>,
    /// The port the driver bound — OS-assigned (the driver binds `:0`),
    /// discovered from its stdout. The reload-reconnect scenario pins this into
    /// the driver's config (see [`UiWorld::pin_driver_port`]) so an in-process
    /// reload rebinds the *same* port and the BFF can reconnect.
    driver_port: u16,
    /// The rendered HTML of the last BFF response. Kept as a `String` (never a
    /// parsed DOM): the `!Send` `scraper::Html` is built, queried, and dropped
    /// inside the synchronous [`crate::dom`] helpers, so it never crosses an
    /// `.await` (see [`crate::dom`] and the UI-testing plan §4).
    pub last_body: String,
    /// The URL of the last top-level page navigation, reported as `HX-Current-URL`
    /// on subsequent htmx requests so the captured fragments match what a browser
    /// would send.
    current_url: String,
    /// The real headless-browser session, lazily started by the first
    /// `@browser` step (Layer C / P3). Absent for the default, browser-free suite.
    pub browser: Option<BrowserSession>,
    /// How long `ServiceHandle::stop()` took to bring the BFF down after the
    /// browser was quit. The coverage-invariant scenario (plan §9 Tier 0 step 3)
    /// asserts this is well under the 5s SIGKILL grace — a graceful exit ran the
    /// BFF's `atexit`, so its `.profraw` coverage was flushed (testing.md §5.4).
    pub bff_stop_elapsed: Option<Duration>,
    /// Live PIDs in the browser process group recorded just before the simulated
    /// geckodriver crash (≥ geckodriver + Firefox) — non-empty proves the reaper
    /// had real work to do (plan §9 Tier 0 step 4).
    pub session_pids_before: Vec<u32>,
    /// Live PIDs left in the browser process group after the reaper ran — must be
    /// empty (no orphaned geckodriver/Firefox/content processes survive).
    pub orphan_survivors: Vec<u32>,
    /// The absolute screenshot + page-source paths captured before the reap.
    pub artifacts: Option<(PathBuf, PathBuf)>,
    /// The rp orchestrator the Phase-5 suites spawn (rp config page, equipment
    /// roster, activity stream). Absent for the driver-only scenarios.
    pub rp: Option<ServiceHandle>,
    /// rp's OS-assigned port (rp binds `:0`).
    pub rp_port: u16,
    /// rp's temp config file — the roster mutations persist here, and the
    /// on-disk assertions read it back.
    pub rp_config_path: Option<PathBuf>,
    /// The stub Alpaca safety monitor rp polls in the activity-stream
    /// scenarios: flipping it is how the harness provokes rp events on an
    /// empty roster (rp keeps no session to start or stop).
    pub safety_stub: Option<bdd_infra::rp_harness::AlpacaDeviceStub>,
    /// A live reader of the BFF's `/stream/events` SSE proxy. Dropped in the
    /// `after` hook **before** the BFF stops (testing.md §5.4 — an open stream
    /// blocks graceful shutdown and silently loses subprocess coverage).
    pub sse: Option<crate::sse_client::StreamEventsClient>,
    /// The SSE `id` of the first feed frame, captured for the replay-cursor
    /// scenario.
    pub sse_cursor: Option<u64>,
    /// Shared PKI + credentials fixture for the TLS + auth suite
    /// (`auth.feature`, `tls.feature`).
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,
    /// Config JSON staged by a Given step for a custom-config BFF start.
    pub pending_config: Option<Value>,
    /// Doctor-subcommand smoke state (staged config file + run output).
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
    /// rp's pinned `session.data_directory` for scenarios that restart rp
    /// over the same target store (the stale-goal roster flag). `None`
    /// until a targets-suite Given pins it.
    rp_data_dir: Option<PathBuf>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for UiWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The same shape `start_bff_with_rp_at` stages: the shared core
        // `server` block plus the required rp target (doctor only parses —
        // nothing is contacted).
        json!({
            "server": { "bind_address": "127.0.0.1", "port": 0 },
            "rp": {}
        })
    }
}

impl UiWorld {
    fn temp_path(&mut self, file: &str) -> PathBuf {
        self.temp_dir
            .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"))
            .path()
            .join(file)
    }

    // --- spawning real services -------------------------------------------

    /// The roster-derived config-page path of the dsd-fp2 device the suites
    /// exercise (rp's roster is the only device source — #569).
    pub fn device_config_path() -> String {
        format!("/config/rp:cover_calibrators:{DEVICE_ID}")
    }

    /// Spawn a real dsd-fp2 driver (mock hardware) reporting the given
    /// effective `serial.port` and `max_brightness`, roster it in a fresh rp,
    /// and point a fresh BFF at that rp.
    pub async fn start_driver_and_bff(&mut self, serial_port: &str, max_brightness: u32) {
        // The driver binds port 0, so the OS assigns a free port atomically —
        // no racy preselection. The reload-reconnect scenario pins the resulting
        // port (`pin_driver_port`) so a reload keeps the same address.
        self.start_driver_and_rp_with_cover_calibrator_config(
            DEVICE_ID,
            serial_port,
            max_brightness,
        )
        .await;
        self.start_bff_with_rp().await;
    }

    /// Spawn a real dsd-fp2 driver whose `serial.port` is pinned by a `--port`
    /// command-line override (so `config.get` reports it in `overrides[]`),
    /// roster it in a fresh rp, and point a fresh BFF at that rp.
    pub async fn start_driver_with_serial_override_and_bff(&mut self, serial_port: &str) {
        // The file carries its own serial.port; the override pins a different
        // effective value, which is what the page must render read-only. Binds
        // port 0 (OS-assigned); this scenario never reloads, so the port needn't
        // be pinned.
        let config_path = self.write_driver_config("/dev/ttyACM0", 4096);
        let handle = ServiceHandle::start_with_args(
            "dsd-fp2",
            &["--config", &config_path, "--port", serial_port],
        )
        .await;
        self.driver_port = handle.port;
        self.driver = Some(handle);
        self.wait_for_driver_ready().await;
        self.roster_driver_and_start_rp(DEVICE_ID).await;
        self.start_bff_with_rp().await;
    }

    /// Spawn rp with a roster entry pointing at a driver that is not running,
    /// and a BFF at that rp — so the device page's `config.get` is refused.
    pub async fn start_bff_with_unreachable_driver(&mut self) {
        self.driver_port = UNREACHABLE_PORT;
        self.roster_driver_and_start_rp(DEVICE_ID).await;
        self.start_bff_with_rp().await;
    }

    /// Spawn just the BFF — no driver, no rp process — for the `/fixtures/*`
    /// scenarios (plan §9 Tier 1). The fixtures never talk to rp, so the
    /// configured (unreachable) rp target is never contacted; this is the
    /// lightest setup that brings up a fixtures-capable BFF. The spawned
    /// binary carries the `test-fixtures` feature (cargo `--all-features`;
    /// Bazel `:ui-htmx_fixtures`).
    pub async fn start_bff_only(&mut self) {
        self.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    }

    /// Spawn the BFF from the config JSON a Given step staged in
    /// `pending_config` (the TLS + auth suite: `auth.feature`, `tls.feature`).
    pub async fn start_bff_from_pending_config(&mut self) {
        let config = self.pending_config.take().expect("config not staged");
        let path = self.temp_path("ui-htmx-tls-auth.json");
        std::fs::write(&path, config.to_string()).expect("failed to write BFF config");
        let handle = ServiceHandle::start("ui-htmx", path.to_str().unwrap()).await;
        self.ui = Some(handle);
    }

    // --- the sentinel the restart affordance calls --------------------------

    /// The stub service-manager directory (created on first use, with an empty
    /// unit listing). Handed to the spawned sentinel via
    /// `SENTINEL_SERVICE_MANAGER_DIR`, so the test sentinel never enumerates
    /// the host's real service manager.
    pub fn service_manager_dir(&mut self) -> PathBuf {
        if let Some(dir) = &self.service_manager_dir {
            return dir.clone();
        }
        let dir = self.temp_path("svcmgr");
        std::fs::create_dir_all(&dir).expect("failed to create service manager dir");
        std::fs::write(dir.join("units.txt"), "").expect("failed to seed units.txt");
        self.service_manager_dir = Some(dir.clone());
        dir
    }

    /// List a unit in the stub's `units.txt`, so sentinel discovers it.
    pub fn add_discovered_unit(&mut self, unit: &str, state: &str) {
        use std::fmt::Write as _;
        // Idempotent: rewrite without any prior line for `unit` first, so a
        // repeated Given can never leave duplicate enumeration entries.
        let path = self.service_manager_dir().join("units.txt");
        let mut content: String = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.split_whitespace().next() != Some(unit))
            .fold(String::new(), |mut acc, line| {
                acc.push_str(line);
                acc.push('\n');
                acc
            });
        writeln!(content, "{unit} {state}").unwrap();
        std::fs::write(&path, content).expect("failed to write units.txt");
    }

    /// Make the stub service manager fail every restart of `unit`.
    pub fn fail_restarts_of(&mut self, unit: &str) {
        let path = self
            .service_manager_dir()
            .join(format!("restart-fail-{unit}"));
        std::fs::write(path, "").expect("failed to write restart-fail marker");
    }

    /// The stub's restart log (one unit name per recorded restart).
    pub fn restart_log(&mut self) -> Vec<String> {
        std::fs::read_to_string(self.service_manager_dir().join("restarts.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Spawn a real sentinel discovering units from the stub service-manager
    /// directory (list them first via
    /// [`add_discovered_unit`](Self::add_discovered_unit)), then **replace**
    /// the already-running BFF with one whose config carries a `sentinel`
    /// block — the sentinel URL is only known once it has bound, and the
    /// driver Given has already started the first BFF. Sentinel runs its
    /// first discovery pass before binding, so the discovered set is complete
    /// as soon as the handle returns. Its config lives in its own
    /// subdirectory, seeded with a sibling `dsd-fp2.json` carrying the
    /// driver's real bound port: discovery derives each service's probe port
    /// from such files, and the BFF's roster-derived restart match needs
    /// `probe_port` to equal the rostered device's `alpaca_url` port. (The
    /// driver's own config — port 0 — must not be mistaken for it.)
    pub async fn start_sentinel_and_rewire_bff(&mut self) {
        let manager_dir = self.service_manager_dir();
        let config = json!({
            "dashboard": { "enabled": true, "history_size": 100 },
            "server": { "port": 0, "bind_address": "127.0.0.1" },
        });
        let config_dir = self.temp_path("sentinel-home");
        std::fs::create_dir_all(&config_dir).expect("failed to create sentinel config dir");
        std::fs::write(
            config_dir.join("dsd-fp2.json"),
            json!({ "server": { "port": self.driver_port, "discovery_port": null } }).to_string(),
        )
        .expect("failed to write the driver probe config");
        let path = config_dir.join("sentinel.json");
        std::fs::write(&path, config.to_string()).expect("failed to write sentinel config");
        let handle = ServiceHandle::start_with_env(
            "sentinel",
            &["--config", path.to_str().unwrap()],
            &[(
                "SENTINEL_SERVICE_MANAGER_DIR",
                manager_dir.to_str().unwrap(),
            )],
        )
        .await;
        self.sentinel = Some(handle);
        if let Some(mut ui) = self.ui.take() {
            ui.stop().await;
        }
        self.start_bff_with_rp().await;
    }

    /// Follow a page's own "Restart via Sentinel" affordance the way htmx
    /// would: render the page at `path`, read the button's rendered `hx-post`
    /// URL, and POST it with htmx's headers (the affordance carries no form
    /// fields).
    pub async fn request_restart_at(&mut self, path: &str) {
        self.get(path).await;
        let url = dom::attr(&self.last_body, "button.restart-sentinel", "hx-post")
            .unwrap_or_else(|| panic!("no restart affordance in:\n{}", self.last_body));
        let resp = reqwest::Client::new()
            .post(self.ui_url(&url))
            .headers(self.hx_headers())
            .send()
            .await
            .expect("BFF restart POST failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// [`request_restart_at`](Self::request_restart_at) for the rostered
    /// dsd-fp2 device page.
    pub async fn request_restart(&mut self) {
        let path = Self::device_config_path();
        self.request_restart_at(&path).await;
    }

    fn write_driver_config(&mut self, serial_port: &str, max_brightness: u32) -> String {
        let config = json!({
            "serial": {
                "port": serial_port,
                "baud_rate": 115_200,
                "polling_interval": "100ms",
                "timeout": "2s"
            },
            // Port 0: the OS assigns a free port atomically (no preselect race).
            "server": { "port": 0, "discovery_port": null },
            "cover_calibrator": {
                "name": "Deep Sky Dad FP2",
                "unique_id": "dsd-fp2-ui-bdd",
                "description": "BDD test instance",
                "enabled": true,
                "max_brightness": max_brightness
            }
        });
        let path = self.temp_path("dsd-fp2.json");
        std::fs::write(&path, config.to_string()).expect("failed to write driver config");
        path.to_str().unwrap().to_string()
    }

    // --- talking to the driver directly (to build realistic submissions) ---

    /// Call one of the driver's config actions directly, returning the parsed
    /// inner body, or `None` on any transport / ASCOM / decode failure (e.g.
    /// mid-reload). The driver wraps the body as a JSON string in `Value`.
    async fn driver_action(&self, action: &str, parameters: &str) -> Option<Value> {
        let url = format!(
            "http://127.0.0.1:{}{}",
            self.driver_port, DRIVER_ACTION_PATH
        );
        let resp = reqwest::Client::new()
            .put(&url)
            .form(&[
                ("Action", action),
                ("Parameters", parameters),
                ("ClientID", "1"),
                ("ClientTransactionID", "1"),
            ])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let envelope: Value = resp.json().await.ok()?;
        if envelope.get("ErrorNumber").and_then(Value::as_i64) != Some(0) {
            return None;
        }
        let inner = envelope.get("Value")?.as_str()?;
        serde_json::from_str(inner).ok()
    }

    /// Wait until the freshly-spawned driver answers `config.get`.
    async fn wait_for_driver_ready(&self) {
        for _ in 0..100 {
            if self.driver_action("config.get", "").await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("dsd-fp2 driver did not answer config.get within 10s");
    }

    /// The driver's current `config.get` `(config, overrides)` — the exact blob
    /// the BFF embeds in the page's hidden field (`serde_json` is
    /// order-deterministic), so re-submitting it round-trips unchanged.
    async fn driver_config(&self) -> (Value, Vec<String>) {
        let body = self
            .driver_action("config.get", "")
            .await
            .expect("driver config.get failed");
        let config = body
            .get("config")
            .cloned()
            .expect("config.get response missing `config`");
        let overrides = body
            .get("overrides")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        (config, overrides)
    }

    /// Pin the driver's OS-assigned bound port into its config via a direct
    /// `config.apply`, so a later in-process reload rebinds the *same* port and
    /// the BFF can reconnect. The driver starts on port 0 (race-free); this
    /// fixes the port afterwards without ever preselecting one. Required only by
    /// the reload-reconnect scenario; other scenarios don't reload.
    pub async fn pin_driver_port(&self) {
        let (mut config, _) = self.driver_config().await;
        let target = u64::from(self.driver_port);
        config["server"]["port"] = json!(self.driver_port);
        let params = serde_json::to_string(&config).expect("serialize pinned config");
        // `config.apply` returns before its fire-after-response reload, so poll
        // `config.get` until the driver has reloaded and reports the pinned port.
        self.driver_action("config.apply", &params).await;
        for _ in 0..100 {
            if let Some(body) = self.driver_action("config.get", "").await {
                if body.pointer("/config/server/port").and_then(Value::as_u64) == Some(target) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "driver did not rebind the pinned port {} within 10s",
            self.driver_port
        );
    }

    // --- the rp orchestrator (Phase-5 suites) --------------------------------

    /// Spawn a real rp from the given harness builder, remembering its config
    /// path for on-disk assertions. rp binds `:0`; readiness is `/health`.
    pub async fn start_rp(&mut self, builder: &bdd_infra::rp_harness::RpConfigBuilder) {
        let config = builder.build();
        let path = self.temp_path("rp.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serialize rp config"),
        )
        .expect("failed to write rp config");
        let handle = ServiceHandle::start("rp", path.to_str().unwrap()).await;
        assert!(
            bdd_infra::rp_harness::wait_for_rp_healthy(&handle.base_url).await,
            "rp did not become healthy"
        );
        self.rp_port = handle.port;
        self.rp = Some(handle);
        self.rp_config_path = Some(path);
    }

    /// Spawn rp with an empty roster (site configured so site-field validation
    /// scenarios have something to reject).
    pub async fn start_rp_with_empty_roster(&mut self) {
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        self.start_rp(&builder).await;
    }

    /// Spawn rp with an otherwise empty roster and one stub safety monitor,
    /// polled fast (250 ms) so a flip lands in test time. The stub starts
    /// safe.
    pub async fn start_rp_with_safety_monitor(&mut self) {
        let stub = bdd_infra::rp_harness::AlpacaDeviceStub::start(
            bdd_infra::rp_harness::StubDevice::SafetyMonitor,
        );
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.add_safety_monitor(bdd_infra::rp_harness::SafetyMonitorConfig {
            id: "weather-watcher".to_string(),
            alpaca_url: stub.url(),
            device_number: 0,
        });
        builder.with_safety_poll_interval(Duration::from_millis(250));
        self.safety_stub = Some(stub);
        self.start_rp(&builder).await;
    }

    /// Flip the stub safety monitor and wait until rp's `get_safety_status`
    /// reports the transition — the poll has landed and the
    /// `safety_changed` event is on the bus — so two flips in a row are
    /// never collapsed into none by the poll interval.
    pub async fn set_safety(&self, safe: bool) {
        self.safety_stub
            .as_ref()
            .expect("no stub safety monitor — start rp with one")
            .set_is_safe(safe);
        let expected = if safe { "safe" } else { "unsafe" };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = self
                .rp_tool("get_safety_status", json!({}))
                .await
                .expect("get_safety_status is ungated and must answer");
            if status.get("overall").and_then(Value::as_str) == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rp never reported the safety monitor as {expected}: {status}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Spawn a real dsd-fp2 (mock hardware), then rp with that driver in its
    /// roster as cover calibrator `id` — the all-first-party managed-device
    /// stack (no `OmniSim`).
    pub async fn start_driver_and_rp_with_cover_calibrator(&mut self, id: &str) {
        self.start_driver_and_rp_with_cover_calibrator_config(id, "/dev/ttyACM0", 4096)
            .await;
    }

    /// [`start_driver_and_rp_with_cover_calibrator`] with an explicit driver
    /// config (the config-page suite renders these exact values).
    pub async fn start_driver_and_rp_with_cover_calibrator_config(
        &mut self,
        id: &str,
        serial_port: &str,
        max_brightness: u32,
    ) {
        let config_path = self.write_driver_config(serial_port, max_brightness);
        let handle = ServiceHandle::start("dsd-fp2", &config_path).await;
        self.driver_port = handle.port;
        self.driver = Some(handle);
        self.wait_for_driver_ready().await;
        self.roster_driver_and_start_rp(id).await;
    }

    /// Spawn rp with the (already known) `self.driver_port` rostered as cover
    /// calibrator `id`.
    async fn roster_driver_and_start_rp(&mut self, id: &str) {
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.add_cover_calibrator(bdd_infra::rp_harness::CoverCalibratorConfig {
            id: id.to_string(),
            alpaca_url: format!("http://127.0.0.1:{}", self.driver_port),
            device_number: 0,
            poll_interval: Some(Duration::from_millis(100)),
        });
        self.start_rp(&builder).await;
    }

    /// Spawn a BFF whose config carries an `rp` target at the given port.
    /// When a sentinel is running (restart scenarios), its URL is carried in
    /// the BFF's `sentinel` block so the restart affordances render.
    pub async fn start_bff_with_rp_at(&mut self, rp_port: u16) {
        let mut config = json!({
            "server": { "bind_address": "127.0.0.1", "port": 0 },
            "rp": { "base_url": format!("http://127.0.0.1:{rp_port}") }
        });
        if let Some(sentinel) = &self.sentinel {
            config["sentinel"] = json!({
                "base_url": format!("http://127.0.0.1:{}", sentinel.port)
            });
        }
        let path = self.temp_path("ui-htmx.json");
        std::fs::write(&path, config.to_string()).expect("failed to write BFF config");
        let handle = ServiceHandle::start("ui-htmx", path.to_str().unwrap()).await;
        self.ui = Some(handle);
    }

    /// Spawn a BFF pointed at the running rp.
    pub async fn start_bff_with_rp(&mut self) {
        assert!(self.rp.is_some(), "start rp before the BFF");
        self.start_bff_with_rp_at(self.rp_port).await;
    }

    /// Spawn a BFF pointed at an rp that is not running.
    pub async fn start_bff_with_unreachable_rp(&mut self) {
        self.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    }

    /// Spawn rp with one imaging train (an unconnectable camera entry —
    /// target CRUD and the config joins need no live device) carrying the
    /// given `default_position_angle_degrees` — the targets suite's
    /// inherit-hint fixture.
    pub async fn start_rp_with_imaging_train(&mut self, default_angle: f64) {
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.add_camera(bdd_infra::rp_harness::CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: format!("http://127.0.0.1:{UNREACHABLE_PORT}"),
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
        builder.add_optical_train(bdd_infra::rp_harness::OpticalTrainConfig {
            id: "main".to_string(),
            purpose: Some("imaging".to_string()),
            focal_length_mm: None,
            default_position_angle_degrees: Some(default_angle),
            devices: vec!["main-cam".to_string()],
            auto_focus: None,
        });
        self.start_rp(&builder).await;
    }

    /// Spawn rp with one filter wheel (unconnectable — the roster union is
    /// config-level) listing `filters`, over a **pinned** data directory so
    /// [`restart_rp_with_filters`](Self::restart_rp_with_filters) reopens
    /// the same target store.
    pub async fn start_rp_with_filters(&mut self, filters: &[String]) {
        let builder = self.filters_builder(filters);
        self.start_rp(&builder).await;
    }

    /// Stop the running rp and start a fresh one whose filter wheel lists
    /// `filters`, over the same pinned data directory — the only way a
    /// stored goal can come to reference a filter outside the roster,
    /// since rp validates goals at write time.
    pub async fn restart_rp_with_filters(&mut self, filters: &[String]) {
        let mut rp = self.rp.take().expect("rp not started");
        rp.stop().await;
        let builder = self.filters_builder(filters);
        self.start_rp(&builder).await;
    }

    fn filters_builder(&mut self, filters: &[String]) -> bdd_infra::rp_harness::RpConfigBuilder {
        let data_dir = self
            .rp_data_dir
            .get_or_insert_with(|| {
                let dir = self
                    .temp_dir
                    .get_or_insert_with(|| TempDir::new().expect("failed to create temp dir"))
                    .path()
                    .join("rp-data");
                std::fs::create_dir_all(&dir).expect("failed to create rp data dir");
                dir
            })
            .clone();
        let mut builder = bdd_infra::rp_harness::RpConfigBuilder::new();
        builder.with_site(47.6, -122.3);
        builder.with_data_directory(data_dir.to_str().unwrap());
        builder.add_filter_wheel(bdd_infra::rp_harness::FilterWheelConfig {
            id: "wheel".to_string(),
            alpaca_url: format!("http://127.0.0.1:{UNREACHABLE_PORT}"),
            device_number: 0,
            filters: filters.to_vec(),
        });
        builder
    }

    // --- seeding and inspecting rp's target store over MCP -----------------

    /// Call one of rp's target MCP tools on a fresh session (connect, call,
    /// drop — the BFF's own per-request pattern), returning the parsed
    /// result or the tool-error message.
    pub async fn rp_tool(&self, tool: &str, args: Value) -> Result<Value, String> {
        let rp = self.rp.as_ref().expect("rp not started");
        let client = bdd_infra::rp_harness::McpTestClient::connect(&format!("{}/mcp", rp.base_url))
            .await
            .expect("MCP connect to rp failed");
        client.call_tool(tool, args).await
    }

    /// Seed a target through rp's own `add_target` (the operator form).
    pub async fn add_target_via_mcp(&self, params: Value) {
        self.rp_tool("add_target", params)
            .await
            .expect("add_target failed");
    }

    /// Fetch one target's row through `get_target`.
    pub async fn target_via_mcp(&self, slug: &str) -> Result<Value, String> {
        self.rp_tool("get_target", json!({ "slug": slug }))
            .await
            .map(|v| v.get("target").cloned().expect("get_target without target"))
    }

    /// The rp config file as currently persisted on disk.
    pub fn rp_config_on_disk(&self) -> Value {
        let path = self
            .rp_config_path
            .as_ref()
            .expect("rp config path unknown");
        let raw = std::fs::read_to_string(path).expect("failed to read rp config");
        serde_json::from_str(&raw).expect("rp config on disk is not JSON")
    }

    /// Connect a reader to the BFF's `/stream/events` SSE proxy.
    pub async fn connect_stream_events(&mut self, last_event_id: Option<u64>) {
        let url = self.ui_url("/stream/events");
        self.sse = Some(crate::sse_client::StreamEventsClient::connect(&url, last_event_id).await);
    }

    // --- driving the BFF over HTTP ----------------------------------------

    fn ui_url(&self, path: &str) -> String {
        let ui = self.ui.as_ref().expect("BFF not started");
        format!("{}{}", ui.base_url, path)
    }

    /// The `HX-*` request headers htmx attaches to a swap request, so a captured
    /// fragment is what a browser would actually receive. The unlock/lock/retry
    /// affordances and the reconnect poller all declare `hx-target="#config-card"`;
    /// htmx sends the resolved element id (no leading `#`).
    fn hx_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("HX-Request", HeaderValue::from_static("true"));
        headers.insert("HX-Target", HeaderValue::from_static("config-card"));
        if let Ok(value) = HeaderValue::from_str(&self.current_url) {
            headers.insert("HX-Current-URL", value);
        }
        headers
    }

    /// GET a BFF page as a top-level browser navigation: no `HX-*` headers, so
    /// the server returns the full styled page. Captures the rendered HTML and
    /// records the URL for `HX-Current-URL` on any follow-up htmx request.
    pub async fn get(&mut self, path: &str) {
        let url = self.ui_url(path);
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("BFF GET failed");
        self.current_url = url;
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Fetch a `/fixtures/*` swap endpoint as an htmx request and return its
    /// `(HX-* response header value, body)` for the named header — the §A
    /// "header-presence tripwire" (plan §9 Tier 1). It proves the divergence-
    /// carrying signal (e.g. `HX-Retarget`) is observable in the *response* even
    /// though the body bytes are a plain fragment a P2 snapshot couldn't tell apart
    /// from a normal swap; the browser then proves it actually retargets.
    pub async fn fixture_response_header_and_body(
        &self,
        path: &str,
        header: &str,
    ) -> (Option<String>, String) {
        let resp = reqwest::Client::new()
            .get(self.ui_url(path))
            .header("HX-Request", "true")
            .send()
            .await
            .expect("fixture GET failed");
        let header_value = resp
            .headers()
            .get(header)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body = resp.text().await.unwrap_or_default();
        (header_value, body)
    }

    /// Issue the GET that htmx would for an `hx-get` affordance: the full `HX-*`
    /// header set a browser's htmx sends, so the server returns the `#config-card`
    /// fragment. `path` is the affordance's own rendered URL.
    async fn hx_get(&mut self, path: &str) {
        let resp = reqwest::Client::new()
            .get(self.ui_url(path))
            .headers(self.hx_headers())
            .send()
            .await
            .expect("BFF htmx GET failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Submit the config form the way a browser+htmx would: render the page, read
    /// the hidden blobs and enabled controls **straight from the rendered HTML**,
    /// apply the operator's edits, and POST to the form's own `hx-post` URL with
    /// htmx's headers. Disabled fields are omitted exactly as a browser omits
    /// them, so read-only/locked fields round-trip from the hidden blob — no
    /// side-channel `config.get` is consulted.
    pub async fn submit_form(&mut self, changes: &[(&str, &str)]) {
        let path = Self::device_config_path();
        self.submit_form_at(&path, changes).await;
    }

    /// Render the page at `path` and submit its form the way htmx would: the
    /// rendered hidden blobs + enabled controls, with the operator's edits
    /// replacing (or adding) the named values. Works for any single-form page
    /// (config pages, the equipment entry forms).
    pub async fn submit_form_at(&mut self, path: &str, changes: &[(&str, &str)]) {
        // Render the form first so the submission is built from real page output.
        self.get(path).await;
        self.submit_rendered_form(changes).await;
    }

    /// POST an htmx affordance URL directly (e.g. the equipment Remove button
    /// after the operator confirms) and capture the swapped fragment.
    pub async fn post_htmx(&mut self, path: &str) {
        let resp = reqwest::Client::new()
            .post(self.ui_url(path))
            .headers(self.hx_headers())
            .send()
            .await
            .expect("BFF POST failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Submit the form in the **already-rendered** `last_body` (used when a
    /// prior step navigated to the form, e.g. the add-equipment flow).
    pub async fn submit_rendered_form(&mut self, changes: &[(&str, &str)]) {
        self.submit_rendered_form_multi(changes, ("", &[])).await;
    }

    /// Like [`submit_rendered_form`](Self::submit_rendered_form), plus a
    /// checkbox group: `multi` is `(field name, checked values)`, submitted as
    /// one pair per value under the shared name — the shape a browser posts
    /// for a checkbox group. The rendered group's own pairs are replaced.
    pub async fn submit_rendered_form_multi(
        &mut self,
        changes: &[(&str, &str)],
        multi: (&str, &[&str]),
    ) {
        let action =
            dom::form_post_url(&self.last_body).expect("rendered page has no <form hx-post>");
        let mut pairs = dom::successful_controls(&self.last_body);
        // The operator edits one or more enabled fields before submitting:
        // replace the rendered value with the typed one (or add it if absent).
        for &(name, value) in changes {
            pairs.retain(|(n, _)| n.as_str() != name);
            pairs.push((name.to_string(), value.to_string()));
        }
        let (multi_name, multi_values) = multi;
        if !multi_name.is_empty() {
            pairs.retain(|(n, _)| n.as_str() != multi_name);
            for value in multi_values {
                pairs.push((multi_name.to_string(), (*value).to_string()));
            }
        }
        let resp = reqwest::Client::new()
            .post(self.ui_url(&action))
            .headers(self.hx_headers())
            .form(&pairs)
            .send()
            .await
            .expect("BFF POST failed");
        self.last_body = resp.text().await.unwrap_or_default();
    }

    /// Follow the page's "Unlock to edit" affordance for `field` the way htmx
    /// would: render the page, read the affordance's own `hx-get` URL from the
    /// rendered HTML, then issue that htmx GET — instead of fabricating the
    /// `?unlock=` URL out of band.
    pub async fn open_with_unlock(&mut self, field: &str) {
        let path = Self::device_config_path();
        self.get(&path).await;
        let url = dom::unlock_url(&self.last_body, field).unwrap_or_else(|| {
            panic!("no unlock affordance for {field:?} in:\n{}", self.last_body)
        });
        self.hx_get(&url).await;
    }

    /// Poll the htmx reconnect endpoint the way the `hx-trigger="every 1s"`
    /// poller would, until the refreshed form serves `expected` as `field`'s
    /// input value — matched against the rendered input's `value` attribute (DOM,
    /// not substring).
    ///
    /// Waiting only for the form to reappear is not enough: until the old server
    /// tears down it keeps answering `config.get` with the pre-reload config, so
    /// the page would briefly re-render with the stale value. The new value
    /// appears only once the rebuilt server is serving. The poll is bounded by a
    /// wall-clock budget because a *real* driver subprocess takes time to tear
    /// down and rebind during its in-process reload — the literal no-sleep poll
    /// in the plan's §9 is the browser layer's job, where htmx's own poller drives
    /// the live DOM.
    pub async fn poll_status_until_value(&mut self, field: &str, expected: &str) {
        const MAX_POLLS: usize = 80;
        const POLL_INTERVAL: Duration = Duration::from_millis(250);
        let status_path = format!("{}/status", Self::device_config_path());
        for _ in 0..MAX_POLLS {
            self.hx_get(&status_path).await;
            if dom::input(&self.last_body, field)
                .map(|i| i.value)
                .as_deref()
                == Some(expected)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        panic!(
            "driver did not serve {field}={expected:?} within {}s; last body:\n{}",
            MAX_POLLS * POLL_INTERVAL.as_millis() as usize / 1000,
            self.last_body
        );
    }

    // --- driving a real browser (@browser scenarios, Layer C) -------------

    /// Lazily start the headless browser session (geckodriver + Firefox) the
    /// first time an `@browser` step needs it.
    pub async fn ensure_browser(&mut self) {
        if self.browser.is_none() {
            self.browser = Some(BrowserSession::start().await);
        }
    }

    /// The live browser session (panics if no `@browser` step started one).
    pub const fn browser(&self) -> &BrowserSession {
        self.browser
            .as_ref()
            .expect("browser session not started — is this an @browser scenario?")
    }

    /// Navigate the real browser to a BFF page as a top-level navigation, so
    /// `htmx.min.js` is fetched and executed.
    pub async fn browser_goto(&mut self, path: &str) {
        self.ensure_browser().await;
        let url = self.ui_url(path);
        self.browser().goto(&url).await;
    }

    /// Tear down in the **correct** order — browser first, then the BFF — and time
    /// the BFF stop (plan §9 Tier 0 step 3). Quitting the browser first closes its
    /// `WebDriver` session (and so Firefox's held connections to the BFF), so the
    /// BFF's graceful shutdown completes promptly instead of blocking until the
    /// 5s SIGKILL grace and losing its `.profraw` coverage flush (testing.md
    /// §5.4). Both handles are taken, so the `after`-hook teardown skips them.
    pub async fn quit_browser_then_stop_bff(&mut self) {
        if let Some(browser) = self.browser.take() {
            browser.quit().await;
        }
        let mut ui = self.ui.take().expect("BFF not started");
        let start = std::time::Instant::now();
        ui.stop().await;
        self.bff_stop_elapsed = Some(start.elapsed());
    }

    /// Worst-case browser teardown (plan §9 Tier 0 step 4): capture failure
    /// artifacts while the session is live, simulate a geckodriver crash that
    /// orphans Firefox, then reap the whole process group — recording the group's
    /// membership before the crash and the survivors after the reap. The browser
    /// handle is taken (reaped, not gracefully quit), so the `after`-hook skips it.
    pub async fn crash_and_reap_browser(&mut self) {
        let mut session = self
            .browser
            .take()
            .expect("no browser session — is this an @browser scenario?");
        let pgid = session.geckodriver_pid();
        // Artifact-before-quit: capture the screenshot + page source while the
        // session is still live, at an absolute (chdir-safe) path. Keep the Result
        // — do NOT unwrap here, so a capture failure can't panic *before* the reap
        // and orphan the tree; the Then-step asserts the artifacts landed.
        let dir = crate::browser::artifact_dir();
        let artifacts = session
            .save_failure_artifacts(&dir, "ui-htmx-panic-recovery")
            .await;
        // Record our whole tree (geckodriver + Firefox + content) while it is live
        // and still in the group.
        let before = crate::browser::live_pids_in_group(pgid);
        session.simulate_geckodriver_crash(); // SIGKILL geckodriver → Firefox orphaned
                                              // Kill the tree: the group (killpg) plus every captured pid (covers a child
                                              // that escaped the group). geckodriver is left a zombie holding the pgid, so
                                              // the drain scan below can't race a recycled pgid; it is reaped by
                                              // kill_on_drop when `session` drops at the end of this method.
        session.reap_tree(&before);
        // ~10s bounded: SIGKILLed processes take a moment to become reaped zombies,
        // and a loaded CI host is slower (the scan skips zombies, so a drained
        // group reads empty). Recycle-safe: geckodriver's zombie still holds pgid.
        let survivors = crate::browser::wait_until_group_drains(pgid, 100).await;
        self.artifacts = artifacts.ok();
        self.session_pids_before = before;
        self.orphan_survivors = survivors;
    }
}

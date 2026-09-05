#![allow(dead_code)]
//! BDD test world for the calibrator-flats tool provider.
//!
//! Holds the three external processes (`OmniSim`, calibrator-flats, rp)
//! plus the harness MCP client that calls the flats tools *through rp's
//! proxy*. The shared harness types come from `bdd_infra::rp_harness`;
//! everything below is the per-scenario accumulator state for this
//! service's tests.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bdd_infra::rp_harness::{
    build_calibrator_flats_config, CameraConfig, CoverCalibratorConfig, FilterWheelConfig,
    McpTestClient, OpticalTrainConfig, ReceivedEvent, RpConfigBuilder, SafetyMonitorConfig,
    WebhookReceiver,
};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::{Map, Value};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

#[derive(World, derive_more::Debug)]
#[debug("CalibratorFlatsWorld {{ .. }}")]
pub struct CalibratorFlatsWorld {
    // --- Infrastructure handles ---
    pub omnisim: Option<bdd_infra::rp_harness::OmniSimHandle>,
    pub rp: Option<ServiceHandle>,
    pub calibrator_flats: Option<ServiceHandle>,
    pub webhook_receiver: Option<WebhookReceiver>,
    /// The scenario's MCP client to rp. Dropped in the `after` hook
    /// *before* rp stops (testing.md §5.4).
    pub mcp_client: Option<McpTestClient>,

    // --- rp config building ---
    pub cameras: Vec<CameraConfig>,
    pub filter_wheels: Vec<FilterWheelConfig>,
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    pub safety_monitors: Vec<SafetyMonitorConfig>,
    pub optical_trains: Vec<OpticalTrainConfig>,
    pub plugin_configs: Vec<Value>,
    /// rp's port, picked before the provider starts: the provider's
    /// config names rp's MCP URL, and rp dials the provider at startup,
    /// so rp cannot be started first on an ephemeral port.
    pub rp_port: Option<u16>,

    // --- calibrator-flats config building ---
    /// Per-scenario overrides layered onto
    /// [`build_calibrator_flats_config`] (`tolerance`, `max_iterations`,
    /// `min_exposure`, `flat_warn_tolerance`, ...).
    pub provider_overrides: Map<String, Value>,
    /// Holds the scenario's redb store; seeded before the provider
    /// starts, since redb admits one process at a time.
    pub store_dir: tempfile::TempDir,

    // --- Webhook state ---
    pub received_events: Arc<RwLock<Vec<ReceivedEvent>>>,

    // --- Tool-call state ---
    pub last_tool_result: Option<Result<Value, String>>,
    pub last_tool_list: Option<Vec<String>>,
    /// Background tool calls by a second client, `(tool, task)`.
    pub background_calls: Vec<(String, JoinHandle<Result<Value, String>>)>,

    // --- TLS + auth smoke test (`auth.feature`) ---
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,
}

impl Default for CalibratorFlatsWorld {
    fn default() -> Self {
        Self {
            omnisim: None,
            rp: None,
            calibrator_flats: None,
            webhook_receiver: None,
            mcp_client: None,
            cameras: Vec::new(),
            filter_wheels: Vec::new(),
            cover_calibrators: Vec::new(),
            safety_monitors: Vec::new(),
            optical_trains: Vec::new(),
            plugin_configs: Vec::new(),
            rp_port: None,
            provider_overrides: Map::new(),
            store_dir: tempfile::tempdir().expect("create the scenario's store directory"),
            received_events: Arc::new(RwLock::new(Vec::new())),
            last_tool_result: None,
            last_tool_list: None,
            background_calls: Vec::new(),
            tls_auth: TlsAuthState::default(),
            doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState::default(),
        }
    }
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for CalibratorFlatsWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The tls-auth smoke's base config plus a plain `server` block.
        let mut config = TlsAuthSmokeWorld::base_test_config(self);
        config["server"] = serde_json::json!({ "port": 0 });
        config
    }
}

impl TlsAuthSmokeWorld for CalibratorFlatsWorld {
    const PROBE_PATH: &'static str = "/health";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // No rp runs in the smoke scenarios: the URL is never dialed,
        // and the store opens in the scenario's own directory.
        build_calibrator_flats_config("http://127.0.0.1:1/mcp", &self.store_path_string())
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.calibrator_flats = Some(handle);
    }
}

impl CalibratorFlatsWorld {
    pub fn omnisim_url(&self) -> String {
        self.omnisim
            .as_ref()
            .expect("OmniSim must be started before accessing its URL")
            .base_url
            .clone()
    }

    pub fn rp_url(&self) -> String {
        self.rp
            .as_ref()
            .map(|h| h.base_url.clone())
            .expect("rp must be started before accessing its URL")
    }

    pub fn rp_mcp_url(&self) -> String {
        format!("{}/mcp", self.rp_url())
    }

    pub fn calibrator_flats_url(&self) -> String {
        self.calibrator_flats
            .as_ref()
            .map(|h| h.base_url.clone())
            .expect("calibrator-flats must be started before accessing its URL")
    }

    /// The scenario's redb store file.
    pub fn store_path(&self) -> PathBuf {
        self.store_dir.path().join("calibrator-flats.redb")
    }

    pub fn store_path_string(&self) -> String {
        self.store_path().to_string_lossy().into_owned()
    }

    /// The scenario's MCP client.
    pub const fn mcp(&self) -> &McpTestClient {
        self.mcp_client
            .as_ref()
            .expect("no MCP client — add an 'And an MCP client connected to rp' step")
    }

    /// The last tool result, as `Ok(value)` or `Err(message)`.
    pub const fn last_result(&self) -> &Result<Value, String> {
        self.last_tool_result
            .as_ref()
            .expect("no tool call was made in this scenario")
    }

    /// Build the rp config JSON by feeding accumulated equipment and plugin
    /// entries through [`RpConfigBuilder`].
    pub fn build_rp_config(&self) -> Value {
        let mut builder = RpConfigBuilder::new();
        for camera in &self.cameras {
            builder.add_camera(camera.clone());
        }
        for fw in &self.filter_wheels {
            builder.add_filter_wheel(fw.clone());
        }
        for cc in &self.cover_calibrators {
            builder.add_cover_calibrator(cc.clone());
        }
        for sm in &self.safety_monitors {
            builder.add_safety_monitor(sm.clone());
        }
        for train in &self.optical_trains {
            builder.add_optical_train(train.clone());
        }
        for plugin in &self.plugin_configs {
            builder.add_plugin(plugin.clone());
        }
        if !self.safety_monitors.is_empty() {
            builder.with_safety_poll_interval(Duration::from_millis(250));
        }
        // `take_flats` captures as `frame_type: "Flat"`, which rp files
        // under its `directory_pattern` — whose default renders
        // `{night_date}` from the site's local night, so rp needs a
        // site. No mount is rostered, so any site will do.
        builder.with_site(47.6062, -122.3321);
        if let Some(port) = self.rp_port {
            builder.with_port(port);
        }
        builder.build()
    }

    /// Wait for rp's `/health` endpoint to return 200.
    pub async fn wait_for_rp_healthy(&self) -> bool {
        bdd_infra::rp_harness::wait_for_rp_healthy(&self.rp_url()).await
    }

    /// Wait for at least `count` events of the given type. 40 × 250ms = 10s.
    pub async fn wait_for_events(&self, event_type: &str, count: usize) -> bool {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let events = self.received_events.read().await;
            let matching = events.iter().filter(|e| e.event_type == event_type).count();
            if matching >= count {
                return true;
            }
        }
        false
    }
}

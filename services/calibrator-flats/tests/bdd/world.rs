#![allow(dead_code)]
//! BDD test world for the calibrator-flats service.
//!
//! Holds the three external processes (`OmniSim`, rp, calibrator-flats) plus
//! an in-process webhook receiver. The shared harness types come from
//! `bdd_infra::rp_harness`; everything below is just the per-scenario
//! accumulator state for this service's tests.

use std::sync::Arc;
use std::time::Duration;

use bdd_infra::rp_harness::{
    build_calibrator_flats_config, CameraConfig, CoverCalibratorConfig, FilterWheelConfig,
    ReceivedEvent, RpConfigBuilder, WebhookReceiver,
};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Default, World, derive_more::Debug)]
#[debug("CalibratorFlatsWorld {{ .. }}")]
pub struct CalibratorFlatsWorld {
    // --- Infrastructure handles ---
    pub omnisim: Option<bdd_infra::rp_harness::OmniSimHandle>,
    pub rp: Option<ServiceHandle>,
    pub calibrator_flats: Option<ServiceHandle>,
    pub webhook_receiver: Option<WebhookReceiver>,

    // --- rp config building ---
    pub cameras: Vec<CameraConfig>,
    pub filter_wheels: Vec<FilterWheelConfig>,
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    pub plugin_configs: Vec<Value>,

    // --- Webhook state ---
    pub received_events: Arc<RwLock<Vec<ReceivedEvent>>>,
    pub webhook_ack_config: Option<(Duration, Duration)>,

    // --- Flat calibration plan ---
    /// Filter name → count for the calibrator-flats service config.
    pub flat_plan: Vec<(String, u32)>,
    /// Filterless (OSC) rig: no filter wheel is rostered and the plan
    /// omits `filter_wheel_id`, so entries are plain capture groups.
    pub no_filter_wheel: bool,

    // --- TLS + auth smoke test (`auth.feature`) ---
    /// State for the shared TLS + auth smoke steps.
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,

    // --- REST API state ---
    pub last_api_status: Option<u16>,
    pub last_api_body: Option<Value>,
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
        // The suite's usual plan; it is never invoked — the smoke
        // scenario only probes `/health`.
        build_calibrator_flats_config(&[("Luminance".to_string(), 1)], Some("main-fw"))
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

    pub fn calibrator_flats_url(&self) -> String {
        self.calibrator_flats
            .as_ref()
            .map(|h| h.base_url.clone())
            .expect("calibrator-flats must be started before accessing its URL")
    }

    /// The service's `/status.phase`, when it answers.
    pub async fn status_phase(&self) -> Option<String> {
        let url = format!("{}/status", self.calibrator_flats_url());
        let resp = reqwest::Client::new().get(&url).send().await.ok()?;
        let body: Value = resp.json().await.ok()?;
        body.get("phase")?.as_str().map(str::to_owned)
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
        for plugin in &self.plugin_configs {
            builder.add_plugin(plugin.clone());
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

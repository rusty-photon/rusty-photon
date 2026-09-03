#![allow(dead_code)]
//! BDD test world for the session-runner service.
//!
//! Holds the three external processes (`OmniSim`, rp, session-runner) plus
//! an in-process webhook receiver. The shared harness types come from
//! `bdd_infra::rp_harness`; everything below is just the per-scenario
//! accumulator state for this service's tests. Runs start at
//! session-runner's own `POST /runs` and are observed on `GET
//! /runs/{id}` — rp keeps no session registry.

use std::sync::Arc;
use std::time::Duration;

use bdd_infra::rp_harness::{
    CameraConfig, CoverCalibratorConfig, FilterWheelConfig, FocuserConfig, GuiderConfig,
    GuiderStub, IcrsCoord, MountConfig, OpticalTrainConfig, PlateSolverConfig, PlateSolverStub,
    ReceivedEvent, RpConfigBuilder, SafetyMonitorConfig, SseClient, WebhookReceiver,
};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Default, World, derive_more::Debug)]
#[debug("SessionRunnerWorld {{ .. }}")]
pub struct SessionRunnerWorld {
    // --- Infrastructure handles ---
    pub omnisim: Option<bdd_infra::rp_harness::OmniSimHandle>,
    pub rp: Option<ServiceHandle>,
    pub session_runner: Option<ServiceHandle>,
    pub webhook_receiver: Option<WebhookReceiver>,
    /// A test-side subscriber to rp's SSE stream, for seq-ordered
    /// assertions on what the engine's triggers did.
    pub sse_client: Option<SseClient>,
    /// Blackboard persistence directory for the spawned session-runner;
    /// held here so it outlives the scenario's service process.
    pub state_dir: Option<tempfile::TempDir>,
    /// The spawned session-runner's workflows directory: the shipped
    /// `workflows/` merged with `tests/fixtures/workflows/`, built per
    /// scenario.
    pub workflows_dir: Option<tempfile::TempDir>,

    // --- rp config building ---
    pub cameras: Vec<CameraConfig>,
    pub filter_wheels: Vec<FilterWheelConfig>,
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    /// Singular mount (`deep_sky.feature`'s scenarios).
    pub mount: Option<MountConfig>,
    pub focusers: Vec<FocuserConfig>,
    /// Optical trains — the deep-sky document is train-addressed, so
    /// its scenarios always carry at least the imaging train.
    pub optical_trains: Vec<OpticalTrainConfig>,
    /// Guider service config pointing at the scenario's stub
    /// (`equipment.mount.guiding`), plus the stub handle itself.
    pub guider: Option<GuiderConfig>,
    pub guider_stub: Option<GuiderStub>,
    /// Safety monitors gating the session (recovery.feature's safety
    /// interruption scenario).
    pub safety_monitors: Vec<SafetyMonitorConfig>,
    /// Override rp's `safety.poll_interval`; pinned short so unsafe/safe
    /// transitions are detected in test time.
    pub safety_poll_interval: Option<Duration>,
    /// Observer site `(latitude, longitude)` — computed per scenario by
    /// `ComputedSky` so the planner sees the sky the scenario needs at test time.
    pub site: Option<(f64, f64)>,
    /// `add_target` argument objects (computed coordinates, optional
    /// altitude floor, optional goals) seeded into rp's redb store
    /// post-boot by `start_rp_service` — the legacy `targets[]` config
    /// array was retired, so planner targets are added via the MCP tool.
    pub pending_store_targets: Vec<Value>,
    /// The computed target coordinates behind `pending_store_targets`,
    /// kept for the mount-sync and plate-solver-echo steps.
    pub night_targets: Vec<IcrsCoord>,
    /// `OmniSim`'s telescope site as it was before a scenario overwrote
    /// it. The site is a profile *setting* the per-scenario device
    /// restart does not reset, and on platforms without
    /// `PR_SET_PDEATHSIG` the `OmniSim` process outlives this test
    /// binary — so the after-hook must put the site back or the next
    /// suite reusing the instance (rp's planner scenarios pin their
    /// config to `OmniSim`'s default site) fails mount-site validation.
    pub original_telescope_site: Option<(f64, f64)>,
    /// The scenario's plate-solver stub (kept alive for its lifetime)
    /// and the rp config block pointing at it.
    pub plate_solver_stub: Option<PlateSolverStub>,
    pub plate_solver: Option<PlateSolverConfig>,
    pub plugin_configs: Vec<Value>,
    /// The `POST /runs` body (`workflow` + `params`), staged by the
    /// Given steps and posted by "a run is started".
    pub run_request: Option<Value>,
    /// The port rp bound on its first start. A restart pins it so the
    /// run's configured `mcp_server_url` finds the new instance where
    /// it left the old one.
    pub rp_port: Option<u16>,

    // --- Recovery scenario state ---
    /// The blackboard's frame counter, read when the run reports paused
    /// — the resumed run must capture exactly
    /// `plan - frames_before_resume` more exposures.
    pub frames_before_resume: Option<u64>,

    // --- Webhook state ---
    pub received_events: Arc<RwLock<Vec<ReceivedEvent>>>,
    pub webhook_ack_config: Option<(Duration, Duration)>,

    // --- TLS + auth smoke test (`auth.feature`) ---
    /// State for the shared TLS + auth smoke steps.
    pub tls_auth: TlsAuthState,

    // --- Flat calibration plan ---
    /// Filter name → count, forwarded as the document's `filters`
    /// parameter in the orchestrator registration's `config`.
    pub flat_plan: Vec<(String, u32)>,
    /// Filterless (OSC) rig: no filter wheel is rostered and the
    /// registration omits `filter_wheel_id`, exercising the document's
    /// `""` default (`set_filter` is skipped).
    pub no_filter_wheel: bool,

    // --- REST API state ---
    pub last_api_status: Option<u16>,
    pub last_api_body: Option<Value>,
}

impl TlsAuthSmokeWorld for SessionRunnerWorld {
    const PROBE_PATH: &'static str = "/health";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // The service only needs directories that exist; the smoke
        // scenario never invokes a workflow, so both stay empty. They are
        // kept (not auto-deleted) under the OS temp dir so they still
        // exist when the service starts, matching the lifetime handling
        // of the harness's staged temp config files.
        let workflows_dir = tempfile::Builder::new()
            .prefix("session-runner-auth-workflows-")
            .tempdir()
            .expect("create workflows dir")
            .keep();
        let state_dir = tempfile::Builder::new()
            .prefix("session-runner-auth-state-")
            .tempdir()
            .expect("create state dir")
            .keep();
        serde_json::json!({
            "workflows_dir": workflows_dir,
            "state_dir": state_dir,
        })
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.session_runner = Some(handle);
    }
}

impl SessionRunnerWorld {
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
        if let Some(mount) = &self.mount {
            builder.with_mount(mount.clone());
        }
        for focuser in &self.focusers {
            builder.add_focuser(focuser.clone());
        }
        for train in &self.optical_trains {
            builder.add_optical_train(train.clone());
        }
        if let Some(guider) = &self.guider {
            builder.with_guider(guider.clone());
        }
        for sm in &self.safety_monitors {
            builder.add_safety_monitor(sm.clone());
        }
        if let Some(interval) = self.safety_poll_interval {
            builder.with_safety_poll_interval(interval);
        }
        if let Some((lat, lon)) = self.site {
            builder.with_site(lat, lon);
        }
        if let Some(ps) = &self.plate_solver {
            builder.with_plate_solver(ps.clone());
        }
        for plugin in &self.plugin_configs {
            builder.add_plugin(plugin.clone());
        }
        if let Some(port) = self.rp_port {
            builder.with_port(port);
        }
        builder.build()
    }

    pub fn session_runner_url(&self) -> String {
        self.session_runner
            .as_ref()
            .map(|h| h.base_url.clone())
            .expect("session-runner must be started before accessing its URL")
    }

    /// Wait for rp's `/health` endpoint to return 200.
    pub async fn wait_for_rp_healthy(&self) -> bool {
        bdd_infra::rp_harness::wait_for_rp_healthy(&self.rp_url()).await
    }

    /// The current run's record from `GET /runs/{id}`, when session-runner
    /// answers.
    pub async fn run_record(&self) -> Option<Value> {
        let url = format!("{}/runs/{}", self.session_runner_url(), self.run_id());
        let resp = reqwest::Client::new().get(&url).send().await.ok()?;
        resp.json::<Value>().await.ok()
    }

    /// The current run's `state`, when session-runner answers.
    pub async fn run_state(&self) -> Option<String> {
        self.run_record()
            .await?
            .get("state")?
            .as_str()
            .map(str::to_owned)
    }

    /// Poll the run until it reaches a terminal state (`complete`,
    /// `failed`, `stopped`) or `budget` elapses; the state reached.
    pub async fn wait_for_run_end(&self, budget: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Some(state) = self.run_state().await {
                if matches!(state.as_str(), "complete" | "failed" | "stopped") {
                    return Some(state);
                }
            }
        }
        None
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

    /// The `session_id` from the last `POST /runs` response.
    pub fn session_id(&self) -> String {
        self.start_response_field("session_id")
    }

    /// The `run_id` from the last `POST /runs` response.
    pub fn run_id(&self) -> String {
        self.start_response_field("run_id")
    }

    fn start_response_field(&self, field: &str) -> String {
        self.last_api_body
            .as_ref()
            .and_then(|body| body.get(field))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("no `{field}` captured — add the 'a run is started' step"))
            .to_owned()
    }

    /// The spawned session-runner's blackboard file for the current session.
    pub fn blackboard_path(&self) -> std::path::PathBuf {
        self.state_dir
            .as_ref()
            .expect("session-runner must be started before reading its state_dir")
            .path()
            .join(format!("{}.json", self.session_id()))
    }

    /// The persisted `session.frames` counter, when the blackboard file
    /// exists and carries one (the recovery fixture's counter).
    pub async fn blackboard_frames(&self) -> Option<u64> {
        self.blackboard_counter("frames").await
    }

    /// A whole-number counter from the persisted blackboard, when the
    /// file exists and carries the key.
    pub async fn blackboard_counter(&self, key: &str) -> Option<u64> {
        let bytes = tokio::fs::read(self.blackboard_path()).await.ok()?;
        let session: Value = serde_json::from_slice(&bytes).ok()?;
        // The engine's expression layer stores numbers as f64 (`2.0`,
        // not `2`), so `as_u64()` would reject every real counter;
        // accept exactly-integral values and fail loud on anything else.
        let value = session.get(key)?.as_f64()?;
        assert!(
            value >= 0.0 && value.fract() == 0.0,
            "the blackboard's `{key}` is not a whole non-negative number: {value}"
        );
        Some(value as u64)
    }
}

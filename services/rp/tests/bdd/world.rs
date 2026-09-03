#![allow(dead_code)]
//! BDD test world for rp service
//!
//! Manages the lifecycle of external processes (`OmniSim`, rp) and
//! in-process test doubles (webhook receiver, test orchestrator)
//! needed for integration testing.
//!
//! The shared types (`OmniSimHandle`, `WebhookReceiver`, `TestOrchestrator`,
//! `McpTestClient`, and the rp config builder) live in the `bdd-infra` crate
//! under the `rp-harness` feature. See `bdd_infra::rp_harness`.

use std::sync::Arc;
use std::time::Duration;

use bdd_infra::rp_harness::{
    AlpacaDeviceStub, CameraConfig, CoverCalibratorConfig, DomeConfig, FilterWheelConfig,
    FocuserConfig, GuiderConfig, GuiderStub, McpTestClient, MountConfig, ObservingConditionsConfig,
    OmniSimHandle, OpticalTrainConfig, OrchestratorInvocation, PlateSolverConfig, PlateSolverStub,
    ReceivedEvent, RotatorConfig, RpConfigBuilder, SafetyMonitorConfig, SseClient, SwitchConfig,
    TestOrchestrator, WebhookReceiver,
};
use bdd_infra::sky_survey_camera_harness::SkyViewStub;
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;
use tokio::sync::RwLock;

/// One raw `/mcp` response, as captured by the transport probes
/// (`steps::mcp_transport_steps`).
#[derive(Debug, Clone)]
pub struct McpProbeResponse {
    pub status: u16,
    /// The `Mcp-Session-Id` header, if the transport handed one out.
    pub session_id: Option<String>,
    /// The JSON-RPC object (parsed from plain JSON or the first SSE
    /// `data:` frame), `None` when the body was not JSON.
    pub body: Option<Value>,
    /// The body verbatim, for failure messages.
    pub raw: String,
}

#[derive(Default, World, derive_more::Debug)]
#[debug("RpWorld {{ .. }}")]
pub struct RpWorld {
    // --- Infrastructure handles ---
    /// Running `OmniSim` process
    pub omnisim: Option<OmniSimHandle>,
    /// Running rp process
    pub rp: Option<ServiceHandle>,
    /// Test webhook receiver (in-process HTTP server acting as an event plugin)
    pub webhook_receiver: Option<WebhookReceiver>,
    /// Test orchestrator (in-process HTTP server acting as an orchestrator plugin)
    pub orchestrator: Option<TestOrchestrator>,
    /// Persistent MCP client for the current scenario
    pub mcp_client: Option<McpTestClient>,
    /// Active SSE subscription to rp's `/api/events/subscribe` stream
    /// (`event_subscribe.feature`). Dropping it aborts the reader task and
    /// closes the connection; the `bdd.rs` `after` hook clears it before
    /// stopping rp (testing.md §5.4).
    pub sse_client: Option<SseClient>,
    /// The highest SSE `id` (`event_seq`) the SSE client had seen at
    /// disconnect, resent as `Last-Event-ID` on reconnect to replay events
    /// missed while disconnected.
    pub sse_reconnect_cursor: Option<u64>,

    // --- Configuration building ---
    /// Camera configs accumulated via Given steps
    pub cameras: Vec<CameraConfig>,
    /// Filter wheel configs accumulated via Given steps
    pub filter_wheels: Vec<FilterWheelConfig>,
    /// `CoverCalibrator` configs accumulated via Given steps
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    /// Focuser configs accumulated via Given steps
    pub focusers: Vec<FocuserConfig>,
    /// Optical trains accumulated via Given steps, emitted as
    /// `equipment.optical_trains`.
    pub optical_trains: Vec<OpticalTrainConfig>,
    /// Singular mount config — at most one per `rp` deployment.
    pub mount: Option<MountConfig>,
    /// Optional plate-solver service config emitted into rp's
    /// `plate_solver` block. Set by the BDD `Given a stub plate
    /// solver returning ...` steps after spawning the stub.
    pub plate_solver: Option<PlateSolverConfig>,
    /// Handle to the in-process stub plate-solver server. Kept on
    /// the world so its request log stays accessible to `Then`
    /// steps and the spawned axum task isn't cancelled mid-scenario.
    pub plate_solver_stub: Option<PlateSolverStub>,
    /// Optional guider service config emitted into rp's
    /// `equipment.mount.guiding` block. Set by the BDD `Given a stub
    /// guider ...` steps after spawning the stub; `start_rp` adds a
    /// simulator mount when none is configured (guiding is
    /// mount-scoped).
    pub guider: Option<GuiderConfig>,
    /// Handle to the in-process stub guider server (same lifecycle
    /// rationale as `plate_solver_stub`).
    pub guider_stub: Option<GuiderStub>,
    /// Optional `(latitude_degrees, longitude_degrees)` site for
    /// ephemeris-driven scenarios; emitted as the `site` block in
    /// the generated rp config. Used by `target_catalog`,
    /// `ephemeris_primitives`, and `planner` BDD features.
    pub site: Option<(f64, f64)>,
    /// Safety monitors accumulated via Given steps (safety.feature).
    pub safety_monitors: Vec<SafetyMonitorConfig>,
    /// Switches accumulated via Given steps (`equipment_connectivity.feature`).
    pub switches: Vec<SwitchConfig>,
    /// Rotators accumulated via Given steps (`equipment_connectivity.feature`).
    pub rotators: Vec<RotatorConfig>,
    /// `ObservingConditions` devices accumulated via Given steps
    /// (`equipment_connectivity.feature`).
    pub observing_conditions: Vec<ObservingConditionsConfig>,
    /// Domes accumulated via Given steps (`equipment_connectivity.feature`).
    pub domes: Vec<DomeConfig>,
    /// Override rp's `safety.poll_interval`; safety scenarios pin this
    /// short so transitions are detected in test time.
    pub safety_poll_interval: Option<Duration>,
    /// Override rp's `safety.gate` as `(gated, ungated)` tool-name lists
    /// (safety.feature's override scenarios).
    pub safety_gate: Option<(Vec<String>, Vec<String>)>,
    /// Override `equipment.reconnect_interval` — session-recovery
    /// scenarios pin it short so the supervisor heals in test time.
    pub reconnect_interval: Option<Duration>,
    /// Restartable Alpaca device stub (session-recovery scenarios): a
    /// downstream device service the scenario can stop and bring back
    /// on the same port with its `Connected` state gone.
    pub alpaca_stub: Option<AlpacaDeviceStub>,
    /// Override rp's `cooling` timing knobs; the camera-cooling
    /// scenarios pin these short so a cooldown pass completes in
    /// test time (`camera_cooling.feature`).
    pub cooling_overrides: Option<bdd_infra::rp_harness::CoolingOverrides>,
    /// Plugin configs accumulated via Given steps
    pub plugin_configs: Vec<Value>,

    // --- Webhook receiver state ---
    /// Events collected by the test webhook receiver
    pub received_events: Arc<RwLock<Vec<ReceivedEvent>>>,
    /// Webhook acknowledgment config (`estimated_duration`, `max_duration`)
    pub webhook_ack_config: Option<(Duration, Duration)>,

    // --- Orchestrator state ---
    /// Invocations received by the test orchestrator
    pub orchestrator_invocations: Arc<RwLock<Vec<OrchestratorInvocation>>>,
    /// Whether the test orchestrator was cancelled
    pub orchestrator_cancelled: Arc<RwLock<bool>>,
    /// The `config` object attached to the orchestrator registration,
    /// for asserting rp's verbatim pass-through at invocation.
    pub orchestrator_registered_config: Option<Value>,

    // --- MCP client state ---
    /// Last captured image path (for `compute_image_stats` chaining)
    pub last_image_path: Option<String>,
    /// Last captured document id (for `compute_image_stats` chaining)
    pub last_document_id: Option<String>,
    /// Last image stats result
    pub last_image_stats: Option<Value>,
    /// Last `measure_basic` result
    pub last_measure_basic_result: Option<Value>,
    /// Last `estimate_background` result
    pub last_estimate_background_result: Option<Value>,
    /// Last `detect_stars` result
    pub last_detect_stars_result: Option<Value>,
    /// Last `measure_stars` result
    pub last_measure_stars_result: Option<Value>,
    /// Last `compute_snr` result
    pub last_compute_snr_result: Option<Value>,
    /// Last `auto_focus` result
    pub last_auto_focus_result: Option<Value>,
    /// Last successful rotator-tool result (`move_rotator`,
    /// `get_rotator_position`)
    pub last_rotator_result: Option<Value>,
    /// Last `plate_solve` result
    pub last_plate_solve_result: Option<Value>,
    /// Last successful guider-tool result (`start_guiding`, dither,
    /// `get_guiding_stats`, ...)
    pub last_guider_result: Option<Value>,
    /// Last `center_on_target` result
    pub last_center_on_target_result: Option<Value>,
    /// Last exposure document fetched via GET /api/documents/{id}
    pub last_exposure_document: Option<Value>,
    /// Last response status from GET /api/images/{id}
    pub last_image_metadata_status: Option<u16>,
    /// Last JSON body from GET /api/images/{id}
    pub last_image_metadata: Option<Value>,
    /// Last response status from GET /api/images/{id}/pixels
    pub last_image_pixels_status: Option<u16>,
    /// Last content-type header from GET /api/images/{id}/pixels
    pub last_image_pixels_content_type: Option<String>,
    /// Last raw body from GET /api/images/{id}/pixels
    pub last_image_pixels_body: Option<Vec<u8>>,
    /// Last tool call result
    pub last_tool_result: Option<Result<Value, String>>,
    /// In-flight tool calls issued on their own MCP session by the
    /// "a second MCP client starts ... in the background" steps
    /// (`motion_gate.feature`), as `(tool_name, handle)` pairs. Every
    /// scenario that spawns one must join it via "the background
    /// {tool} call should succeed" so a stray capture cannot hold
    /// the shared simulator into the next scenario.
    pub background_calls: Vec<(String, tokio::task::JoinHandle<Result<Value, String>>)>,
    /// Last tool list result
    pub last_tool_list: Option<Vec<String>>,
    /// Current filter from `get_filter`
    pub current_filter: Option<String>,
    /// Slug of the most recently added/fetched target (Target Store
    /// scenarios — `target_store`_*.feature, *(planned, P1)*), so a
    /// later step can act on "the target I just added" without the
    /// feature file repeating the slug.
    pub last_target_slug: Option<String>,
    /// Raw JSON array from the most recent `list_targets` call
    /// (Target Store scenarios).
    pub last_target_list: Option<Vec<Value>>,
    /// The exact payload last handed to `validate_plan`, so the
    /// agreement scenarios can replay the identical bytes through
    /// `add_target` instead of restating them.
    pub last_validated_payload: Option<Value>,
    /// Catalog centroid captured by "the MCP client has resolved
    /// catalog target ..." (import scenarios —
    /// `target_store_import.feature`), so later steps can import at or
    /// offset from it without the feature file hard-coding catalog
    /// coordinates.
    pub resolved_coord: Option<(f64, f64)>,
    /// Raw `target_store` config block override (Target Store scenarios —
    /// `db_path`/`default_scheduling`/`default_grading`/`default_goals`,
    /// see rp.md § Target Store → Configuration), merged over whatever
    /// [`RpConfigBuilder::build`] emits so these scenarios can still use
    /// the ordinary OmniSim/mount bootstrap (`tool_steps::start_rp`)
    /// instead of a bespoke launcher. `None` ⇒ no override.
    pub target_store_config: Option<Value>,
    /// `session.file_naming_pattern` override (capture target-linkage
    /// scenarios — `capture_target_linkage.feature`), merged over
    /// [`RpConfigBuilder::build`]'s output the same way
    /// `target_store_config` is. `RpConfigBuilder::build` already bakes
    /// in the documented default pattern unconditionally, so `None`
    /// here means "use that baked-in default", not "omit the field" —
    /// see `clear_file_naming_pattern` for scenarios that need the
    /// field genuinely absent.
    pub file_naming_pattern: Option<String>,
    /// `session.directory_pattern` override, same merge rule as
    /// `file_naming_pattern`. `None` ⇒ field omitted (rp falls back to
    /// the documented default, `"{target}/{night_date}/{frame_type}"`,
    /// whenever `file_naming_pattern` is set).
    pub directory_pattern: Option<String>,
    /// When `true`, forces `session.file_naming_pattern` to `null` in
    /// the emitted config, overriding `RpConfigBuilder::build`'s
    /// unconditional default — for the scenario exercising `capture`'s
    /// "`frame_type` requires `session.file_naming_pattern` to be
    /// configured" error path (`capture_target_linkage.feature`).
    pub clear_file_naming_pattern: bool,
    /// Error from the most recent `ServiceHandle::try_start` call
    /// (Target Store naming-template config-load validation scenarios —
    /// `target_naming_template.feature`, *(planned, P1)*). `None` after
    /// a successful start.
    pub rp_start_error: Option<String>,
    /// `server.advertised_url` override (session-lifecycle advertised-URL
    /// scenario), merged over [`RpConfigBuilder::build`]'s output the
    /// same way `target_store_config` is. `None` ⇒ field omitted, so rp
    /// derives the advertised URL from its listener.
    pub advertised_url: Option<String>,

    // --- REST API state ---
    /// Last REST API response status code
    pub last_api_status: Option<u16>,
    /// Last REST API response body
    pub last_api_body: Option<Value>,
    /// Session status from GET /api/session/status
    pub session_status: Option<String>,

    // --- Test flat-calibration orchestrator config ---
    /// Filter name → count, used by the in-process `TestOrchestrator` when
    /// configured with `OrchestratorBehavior::FlatCalibration(...)`.
    pub flat_plan: Vec<(String, u32)>,

    // --- TLS test state ---
    /// Shared PKI + credentials fixture for the TLS/auth connectivity suites
    /// (`tls.feature`, `auth.feature`).
    pub pki: Option<std::sync::Arc<bdd_infra::tls_auth::PkiFixture>>,
    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,

    // --- Document HTTP API test state (Phase 7 Step 6) ---
    /// Pinned data directory across rp lifecycle. The cross-restart
    /// scenarios need both rp processes pointing at the same on-disk
    /// archive. The `TempDir` is held by `pinned_data_dir_holder` to
    /// keep it alive for the scenario's duration.
    pub pinned_data_directory: Option<String>,
    pub pinned_data_dir_holder: Option<tempfile::TempDir>,
    /// Pinned `session.session_state_file` across rp lifecycle. The
    /// startup-recovery scenarios need the restarted rp to read the
    /// session registry its predecessor persisted; without the pin the
    /// config builder generates a fresh path per build. The `TempDir`
    /// holding the file is kept alive by
    /// `pinned_session_state_holder`.
    pub pinned_session_state_file: Option<String>,
    pub pinned_session_state_holder: Option<tempfile::TempDir>,
    /// Override the imaging cache budgets via `RpConfigBuilder::with_imaging`.
    /// `(cache_max_mib, cache_max_images)`.
    pub pinned_imaging_overrides: Option<(usize, usize)>,
    /// Last response status from `GET /api/documents/{id}`.
    pub last_document_response_status: Option<u16>,
    /// Last JSON body from `GET /api/documents/{id}`.
    pub last_document_response_body: Option<Value>,
    /// Named `document_ids` the test wants to refer back to later (e.g.
    /// "first" → the `document_id` from the first capture). Used by
    /// the eviction and cross-restart scenarios that need to reference
    /// a doc captured several steps ago.
    pub remembered_document_ids: std::collections::HashMap<String, String>,

    // --- Raw MCP probe state (mcp_transport.feature,
    // mcp_host_allowlist.feature) ---
    /// What the last raw `/mcp` POST answered with — the status (403
    /// when the Host allowlist rejected the authority), the session
    /// header that must never appear, and the JSON-RPC object.
    pub last_mcp_probe: Option<McpProbeResponse>,

    // --- Config REST test state (config_rest.feature) ---
    /// `TempDir` holding the scenario's private rp config file (and its data
    /// directory). Held so the file survives until scenario teardown.
    pub config_rest_dir: Option<tempfile::TempDir>,
    /// Path of the config file rp was started from; `PUT /api/config`
    /// persists to it, so the file assertions read this path.
    pub config_rest_path: Option<std::path::PathBuf>,
    /// Byte snapshot of the config file for byte-identical assertions
    /// (invalid / malformed applies must not touch the file).
    pub config_file_snapshot: Option<String>,
    /// The `config` object from the last `GET /api/config` response, edited
    /// and resubmitted by the PUT steps (the BFF's own flow).
    pub fetched_config: Option<Value>,
    /// Status of the last config-endpoint response.
    pub last_config_response_status: Option<u16>,
    /// Raw body of the last config-endpoint response (400s are plain text).
    pub last_config_response_text: Option<String>,
    /// Parsed JSON body of the last config-endpoint response, when JSON.
    pub last_config_response_json: Option<Value>,

    // --- Phase 4 closed-loop centering: sky-survey-camera follow mode ---
    /// Running `sky-survey-camera` process when the centering scenario
    /// uses it as `main-cam`. Held on the world so its child stays
    /// alive for the scenario duration; dropped (which sends SIGTERM
    /// in `ServiceHandle::drop`) at scenario teardown. **Must be
    /// declared above `sky_survey_camera_cache`** — Rust drops struct
    /// fields top-down, so the camera process must die *before* its
    /// cache directory is removed (otherwise an in-flight write would
    /// race the directory removal).
    pub sky_survey_camera: Option<ServiceHandle>,
    /// `TempDir` guard for sky-survey-camera's cache. Removes the
    /// directory tree on drop, preventing accumulation of stale
    /// cache artefacts across scenarios / CI runs.
    pub sky_survey_camera_cache: Option<tempfile::TempDir>,
    /// In-process `SkyView` stub serving cutouts to `sky-survey-camera`.
    /// Held on the world so the axum task isn't cancelled mid-scenario.
    pub sky_view_stub: Option<SkyViewStub>,
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for RpWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        // The minimal shape rp's own `load_config` accepts: `session` and
        // `equipment` are the two required blocks, `server` is the shared
        // core shape (D1). Nothing is started, so the data directory is
        // never created.
        serde_json::json!({
            "session": {
                "data_directory": std::env::temp_dir()
                    .join("rp-doctor-smoke")
                    .to_string_lossy()
            },
            "equipment": {},
            "server": { "port": 0 }
        })
    }
}

impl RpWorld {
    /// The `session.data_directory` the running rp was configured with —
    /// where the progress frame scan looks (rp.md § Progress
    /// derivation), so a scenario can seed frames into it.
    ///
    /// Covers both bootstraps: the target-store suites' bespoke launcher
    /// (`write_target_store_config`, which puts the data directory under
    /// `config_rest_dir`) and the OmniSim/mount path, which needs a
    /// `pinned_data_directory` — without the pin `RpConfigBuilder`
    /// generates a fresh path per build and the seeded frames would land
    /// somewhere rp never reads.
    pub fn data_directory(&self) -> std::path::PathBuf {
        if let Some(dir) = &self.pinned_data_directory {
            return std::path::PathBuf::from(dir);
        }
        if let Some(dir) = &self.config_rest_dir {
            return dir.path().join("data");
        }
        panic!(
            "no known data directory: pin one with `pinned_data_directory` \
             (OmniSim bootstrap) or start rp via `write_target_store_config`"
        )
    }

    /// The base URL for the `OmniSim` Alpaca simulator.
    /// Panics if `OmniSim` has not been started yet.
    pub fn omnisim_url(&self) -> String {
        self.omnisim
            .as_ref()
            .expect("OmniSim must be started before accessing its URL")
            .base_url
            .clone()
    }

    /// The base URL for the rp REST API
    pub fn rp_url(&self) -> String {
        self.rp.as_ref().map_or_else(
            || "http://localhost:11115".to_string(),
            |h| h.base_url.clone(),
        )
    }

    /// The MCP endpoint URL for rp
    pub fn rp_mcp_url(&self) -> String {
        format!("{}/mcp", self.rp_url())
    }

    /// The shared PKI + credentials fixture, panicking if the
    /// `Given generated TLS certificates` step has not run.
    pub fn pki(&self) -> &bdd_infra::tls_auth::PkiFixture {
        self.pki.as_deref().expect("TLS certs not generated")
    }

    /// Get the persistent MCP client, panicking if not connected.
    pub const fn mcp(&self) -> &McpTestClient {
        self.mcp_client
            .as_ref()
            .expect("MCP client not connected — add 'Given an MCP client connected to rp' step")
    }

    /// Build the rp config JSON from accumulated Given steps via [`RpConfigBuilder`].
    pub fn build_config(&self) -> Value {
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
        for foc in &self.focusers {
            builder.add_focuser(foc.clone());
        }
        for train in &self.optical_trains {
            builder.add_optical_train(train.clone());
        }
        if let Some(mount) = &self.mount {
            builder.with_mount(mount.clone());
        }
        if let Some(ps) = &self.plate_solver {
            builder.with_plate_solver(ps.clone());
        }
        if let Some(g) = &self.guider {
            builder.with_guider(g.clone());
        }
        if let Some((lat, lon)) = self.site {
            builder.with_site(lat, lon);
        }
        for sm in &self.safety_monitors {
            builder.add_safety_monitor(sm.clone());
        }
        for sw in &self.switches {
            builder.add_switch(sw.clone());
        }
        for r in &self.rotators {
            builder.add_rotator(r.clone());
        }
        for oc in &self.observing_conditions {
            builder.add_observing_conditions(oc.clone());
        }
        for d in &self.domes {
            builder.add_dome(d.clone());
        }
        if let Some(interval) = self.safety_poll_interval {
            builder.with_safety_poll_interval(interval);
        }
        if let Some((gated, ungated)) = &self.safety_gate {
            builder.with_safety_gate(gated.clone(), ungated.clone());
        }
        if let Some(interval) = self.reconnect_interval {
            builder.with_reconnect_interval(interval);
        }
        if let Some(cooling) = &self.cooling_overrides {
            builder.with_cooling(cooling.clone());
        }
        for plugin in &self.plugin_configs {
            builder.add_plugin(plugin.clone());
        }
        if let Some(dir) = &self.pinned_data_directory {
            builder.with_data_directory(dir.clone());
        }
        if let Some(path) = &self.pinned_session_state_file {
            builder.with_session_state_file(path.clone());
        }
        if let Some((mib, images)) = self.pinned_imaging_overrides {
            builder.with_imaging(mib, images);
        }
        let mut config = builder.build();
        // Target-store settings (`db_path` / `default_scheduling` /
        // `default_goals`, rp.md § Target Store) as the raw
        // `target_store` config object. The shared `RpConfigBuilder` has
        // no typed store-settings builder yet, so a scenario that needs
        // non-default settings sets `target_store_config` and it is
        // spliced in here; targets themselves are seeded post-boot via
        // the `add_target` MCP tool.
        if let Some(target_store) = &self.target_store_config {
            config["target_store"] = target_store.clone();
        }
        if let Some(pattern) = &self.file_naming_pattern {
            config["session"]["file_naming_pattern"] = Value::String(pattern.clone());
        }
        if self.clear_file_naming_pattern {
            config["session"]["file_naming_pattern"] = Value::Null;
        }
        if let Some(pattern) = &self.directory_pattern {
            config["session"]["directory_pattern"] = Value::String(pattern.clone());
        }
        if let Some(url) = &self.advertised_url {
            config["server"]["advertised_url"] = Value::String(url.clone());
        }
        config
    }

    /// Wait for rp to become healthy (retry GET /health).
    /// Timeout: 120 × 250ms = 30s (sanitizer-instrumented binaries start slower).
    pub async fn wait_for_rp_healthy(&self) -> bool {
        bdd_infra::rp_harness::wait_for_rp_healthy(&self.rp_url()).await
    }

    /// Wait for a specific number of events of a given type
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

    /// Wait for the session status to reach an expected value.
    /// Timeout: 40 × 250ms = 10s.
    pub async fn wait_for_session_status(&self, expected: &str) -> bool {
        let client = reqwest::Client::new();
        let url = format!("{}/api/session/status", self.rp_url());
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if body.get("status").and_then(|v| v.as_str()) == Some(expected) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Wait for at least one orchestrator invocation to be recorded.
    /// Timeout: 40 × 250ms = 10s.
    pub async fn wait_for_orchestrator_invocation(&self) -> bool {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let inv = self.orchestrator_invocations.read().await;
            if !inv.is_empty() {
                return true;
            }
        }
        false
    }
}

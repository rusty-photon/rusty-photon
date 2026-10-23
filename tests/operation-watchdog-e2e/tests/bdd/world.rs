//! BDD world for the operation-watchdog end-to-end suite.
//!
//! Holds the real processes (`OmniSim`, rp, sentinel), the in-process
//! plate-solver stub used to wedge a `center_on_target` call, a local
//! Pushover stub so the watchdog's escalations land in sentinel's dashboard
//! history without a network round-trip, and the directory-backed stub
//! service manager (`SENTINEL_SERVICE_MANAGER_DIR`) sentinel discovers the
//! "rp" service from. Everything is per-scenario; teardown runs in the
//! cucumber `after` hook (see `bdd.rs`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bdd_infra::rp_harness::{
    CameraConfig, CannedWcs, McpTestClient, MountConfig, OmniSimHandle, PlateSolverConfig,
    PlateSolverStub, RpConfigBuilder, StubBehavior,
};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;

/// Per-process counter so each scenario gets a distinct harness directory —
/// combined with the PID this stays collision-free across parallel test
/// binaries (the same pattern `write_temp_config_file` uses).
static HARNESS_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// How rp's plate solver behaves for the scenario — selected by a `Given`
/// step before rp starts, since the choice is baked into rp's config.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PlateSolverMode {
    /// No plate solver / no equipment — rp starts bare (the rp-unresponsive
    /// scenario only needs the event stream).
    #[default]
    None,
    /// The solve hangs forever, wedging a `center_on_target` call so it never
    /// completes and the watchdog's centering deadline fires.
    Hang,
    /// The solve returns the target field-center immediately, so centering
    /// converges on iteration 1 and the watchdog does not fire.
    Canned,
}

#[derive(Debug, Default, World)]
pub struct WatchdogE2eWorld {
    pub omnisim: Option<OmniSimHandle>,
    pub rp: Option<ServiceHandle>,
    pub sentinel: Option<ServiceHandle>,
    pub plate_solver_stub: Option<PlateSolverStub>,
    pub plate_solver_mode: PlateSolverMode,

    /// Local Pushover API stub — the watchdog dispatches escalations through
    /// the notifier chain, which records them in the dashboard history. Without
    /// a reachable notifier endpoint the dispatch (and therefore the history
    /// record we assert on) never lands.
    pub pushover_stub: Option<tokio::task::JoinHandle<()>>,
    pub pushover_stub_url: Option<String>,

    /// The detached, never-completing `center_on_target` call for the wedge
    /// scenario. Holds its own MCP client; aborted during teardown.
    pub wedge_task: Option<tokio::task::JoinHandle<()>>,

    /// Outcome of a centering call that is expected to complete (the
    /// no-false-alarm scenario): `Some(Ok)` on success, `Some(Err)` on a tool
    /// error, `None` if never invoked.
    pub centering_result: Option<Result<Value, String>>,

    /// Per-scenario harness directory holding sentinel's config file, the
    /// sibling `rp.json` discovery derives rp's Alpaca base URL from, and the
    /// stub service-manager directory. Removed in teardown.
    pub harness_dir: Option<PathBuf>,

    /// The stub service-manager directory (`<harness_dir>/svcmgr`), handed to
    /// sentinel via `SENTINEL_SERVICE_MANAGER_DIR`; its `restarts.log` is
    /// what the restart-rung assertion reads.
    pub manager_dir: Option<PathBuf>,
}

impl WatchdogE2eWorld {
    pub fn rp_base_url(&self) -> String {
        self.rp
            .as_ref()
            .map(ServiceHandle::base_url)
            .expect("rp not started")
    }

    pub fn sentinel_dashboard_url(&self, path: &str) -> String {
        let s = self.sentinel.as_ref().expect("sentinel not started");
        format!("{}{}", s.base_url(), path)
    }

    pub async fn ensure_omnisim(&mut self) {
        if self.omnisim.is_none() {
            self.omnisim = Some(OmniSimHandle::start().await);
        }
    }

    /// Stand up a local Pushover API stub that 200s any request, so the
    /// watchdog's notifier dispatch (and thus the dashboard history record)
    /// completes without hitting api.pushover.net.
    pub async fn start_pushover_stub(&mut self) {
        use axum::Router;

        let app = Router::new().fallback(|| async {
            axum::Json(serde_json::json!({ "status": 1, "request": "stub" }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pushover stub");
        let addr = listener.local_addr().expect("pushover stub addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.pushover_stub = Some(handle);
        self.pushover_stub_url = Some(format!("http://{addr}/1/messages.json"));
    }

    /// Build rp's JSON config for the selected plate-solver mode.
    fn build_rp_config(&self) -> Value {
        let mut builder = RpConfigBuilder::new();
        if self.plate_solver_mode != PlateSolverMode::None {
            let omnisim = self
                .omnisim
                .as_ref()
                .expect("omnisim started for equipment");
            builder.add_camera(CameraConfig {
                id: "main-cam".to_string(),
                alpaca_url: omnisim.base_url.clone(),
                device_number: 0,
                cooler_targets_c: Vec::new(),
            });
            builder.with_mount(MountConfig {
                alpaca_url: omnisim.base_url.clone(),
                device_number: 0,
                settle_after_slew: None,
            });
            let stub = self
                .plate_solver_stub
                .as_ref()
                .expect("plate solver stub started before rp");
            builder.with_plate_solver(PlateSolverConfig {
                // A generous outer HTTP ceiling so a hung solve stays in flight
                // well past the ~2 s watchdog deadline (the watchdog, not this
                // timeout, is what we want to fire).
                url: stub.url.clone(),
                timeout: Some(Duration::from_mins(1)),
                default_search_radius_deg: None,
            });
        }
        if self.plate_solver_mode == PlateSolverMode::Hang {
            // Shrink the advisory centering deadline to
            // max_attempts(1) × (duration 0.1s + solve 1s + slew 1s) ≈ 2.1s so
            // the watchdog timer fires quickly while the solve is wedged.
            builder.with_centering(Duration::from_secs(1), Duration::from_secs(1));
        }
        builder.build()
    }

    /// Build sentinel's JSON config: the operation watchdog pointed at rp, with
    /// `centering` set to `abort_then_restart` against the *discovered* "rp"
    /// service — there is no `services` map; sentinel's registry comes from the
    /// stub service manager laid out by [`write_sentinel_fs`](Self::write_sentinel_fs),
    /// and the ladder's restart rung hands the unit to that manager. Buffers
    /// are zeroed so the tracked deadline equals rp's `max_duration_ms`
    /// exactly; reconnect is tight so the unresponsive path resolves in a
    /// couple of seconds.
    fn build_sentinel_config(&self) -> Value {
        let rp_url = self.rp_base_url();
        let pushover_url = self
            .pushover_stub_url
            .clone()
            .expect("pushover stub started before sentinel");

        serde_json::json!({
            "monitors": [],
            "notifiers": [{
                "type": "pushover",
                "api_token": "test-token",
                "user_key": "test-user",
                "api_url": pushover_url,
            }],
            "transitions": [],
            "dashboard": { "enabled": true, "history_size": 100 },
            "server": { "port": 0 },
            "operation_watchdog": {
                "rp_url": rp_url,
                "reconnect_max_attempts": 2,
                "reconnect_backoff": "1s",
                "default_buffer": "0s",
                "notifiers": ["pushover"],
                "operations": {
                    "centering": {
                        "buffer": "0s",
                        "on_expiry": "abort_then_restart",
                        "service": "rp",
                    }
                }
            }
        })
    }

    /// Lay out sentinel's on-disk world in a fresh harness directory and
    /// return `(sentinel config path, stub service-manager dir)`:
    ///
    /// - `svcmgr/units.txt` lists `rusty-photon-rp` as `stopped`: discovery
    ///   still resolves it and the ladder's restart rung still restarts it,
    ///   but autonomous health supervision leaves stopped units alone — so it
    ///   can never steal the one-restart-per-service gate slot from the
    ///   ladder mid-scenario.
    /// - `svcmgr/policy.json` overrides only the restart budget (down from
    ///   the 300 s constant to 2 s, bounding the ladder's recovery wait);
    ///   every other supervision constant keeps its slow production default
    ///   so supervision stays quiet for the scenario's lifetime.
    /// - `rp.json`, the sibling file discovery derives rp's Alpaca base URL
    ///   from, claims port 1 — unreachable by construction. `centering` has
    ///   no Alpaca binding, so the ladder's health rung reports unknown and
    ///   falls through to restart, and the recovery wait can never confirm:
    ///   the outcome carries `restart=ran` (the token the test asserts on)
    ///   followed by `recovery=timeout`.
    fn write_sentinel_fs(&mut self, sentinel_config: &Value) -> (String, PathBuf) {
        let seq = HARNESS_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "operation-watchdog-e2e-{}-{}",
            std::process::id(),
            seq
        ));
        // A crashed prior run can leave this deterministic path behind (PIDs
        // recycle); stale units.txt / restarts.log state must never leak into
        // this scenario.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("clear stale harness dir");
        }
        let manager_dir = dir.join("svcmgr");
        std::fs::create_dir_all(&manager_dir).expect("create stub service-manager dir");
        std::fs::write(manager_dir.join("units.txt"), "rusty-photon-rp stopped\n")
            .expect("write stub units.txt");
        std::fs::write(
            manager_dir.join("policy.json"),
            serde_json::json!({ "restart_budget": "2s" }).to_string(),
        )
        .expect("write stub policy.json");
        std::fs::write(
            dir.join("rp.json"),
            serde_json::json!({ "server": { "port": 1 } }).to_string(),
        )
        .expect("write sibling rp.json");
        let config_path = dir.join("sentinel.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(sentinel_config).expect("serialize sentinel config"),
        )
        .expect("write sentinel config");
        self.harness_dir = Some(dir);
        self.manager_dir = Some(manager_dir.clone());
        (config_path.to_string_lossy().to_string(), manager_dir)
    }

    /// Restart-log lines from the stub service manager (one unit name per
    /// recorded restart); empty when no restart ran.
    pub fn restart_log(&self) -> Vec<String> {
        let dir = self.manager_dir.as_ref().expect("sentinel not started");
        std::fs::read_to_string(dir.join("restarts.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Start the full stack: `OmniSim` (when equipment is needed), the Pushover
    /// stub, a real rp, then a real sentinel whose watchdog subscribes to rp.
    pub async fn start_stack(&mut self) {
        if self.plate_solver_mode != PlateSolverMode::None {
            self.ensure_omnisim().await;
        }
        self.start_pushover_stub().await;

        let rp_config = self.build_rp_config();
        let rp = bdd_infra::rp_harness::start_rp(&rp_config).await;
        let rp_url = rp.base_url();
        self.rp = Some(rp);
        assert!(
            bdd_infra::rp_harness::wait_for_rp_healthy(&rp_url).await,
            "rp did not become healthy"
        );

        let sentinel_config = self.build_sentinel_config();
        let (config_path, manager_dir) = self.write_sentinel_fs(&sentinel_config);
        self.sentinel = Some(
            ServiceHandle::start_with_env(
                "sentinel",
                &["--config", &config_path],
                &[(
                    "SENTINEL_SERVICE_MANAGER_DIR",
                    manager_dir.to_str().expect("manager dir path is UTF-8"),
                )],
            )
            .await,
        );

        // Give sentinel's watchdog a moment to open its SSE subscription before
        // any operation fires, so a live `centering_started` is observed. There
        // is no readiness endpoint for an event monitor; a fresh subscribe with
        // no Last-Event-ID tails live, so the operation must start after this.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    /// Spawn a `center_on_target` call on a detached task with its own MCP
    /// client. For the Hang mode it never returns until teardown stops the
    /// stub; the task owns the client so teardown can drop it before rp stops.
    pub async fn spawn_wedge_centering(&mut self) {
        let mcp_url = format!("{}/mcp", self.rp_base_url());
        let args = serde_json::json!({
            "camera_id": "main-cam",
            "ra": 0.7123,
            "dec": 41.269,
            "duration": "100ms",
            "tolerance_arcsec": 60.0,
            "max_attempts": 1,
        });
        let task = tokio::spawn(async move {
            match McpTestClient::connect(&mcp_url).await {
                Ok(client) => {
                    // Result ignored: in the wedge scenario this blocks until
                    // teardown stops the stub, then returns a tool error.
                    let _ = client.call_tool("center_on_target", args).await;
                }
                Err(e) => eprintln!("wedge center_on_target: MCP connect failed: {e}"),
            }
        });
        self.wedge_task = Some(task);
    }

    /// Invoke `center_on_target` and wait for it to finish (the no-false-alarm
    /// scenario, where the canned solve converges immediately). Stores the
    /// outcome and drops the client before returning so teardown is clean.
    pub async fn run_centering_to_completion(&mut self) {
        let mcp_url = format!("{}/mcp", self.rp_base_url());
        let client = McpTestClient::connect(&mcp_url)
            .await
            .expect("connect MCP client for centering");
        // center_on_target syncs the mount on iteration 1, and ASCOM rejects
        // SyncToCoordinates while Tracking is false (OmniSim's reset default).
        client
            .call_tool("set_tracking", serde_json::json!({ "enabled": true }))
            .await
            .expect("enable mount tracking before centering");
        let result = client
            .call_tool(
                "center_on_target",
                serde_json::json!({
                    "camera_id": "main-cam",
                    "ra": 0.7123,
                    "dec": 41.269,
                    "duration": "100ms",
                    "tolerance_arcsec": 60.0,
                    "max_attempts": 1,
                }),
            )
            .await;
        self.centering_result = Some(result);
        // `client` drops here, closing the /mcp connection before teardown.
    }

    /// Stop rp gracefully and forget the handle, simulating rp going away while
    /// sentinel is subscribed (the unresponsive scenario).
    pub async fn stop_rp(&mut self) {
        if let Some(mut rp) = self.rp.take() {
            rp.stop().await;
        }
    }

    /// GET sentinel's `/api/history` as a JSON array. Bounded by a short
    /// request timeout so a stalled dashboard fails this poll fast rather than
    /// hanging the whole suite until the bazel test timeout.
    pub async fn get_history(&self) -> Vec<Value> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build history http client");
        let url = self.sentinel_dashboard_url("/api/history");
        let resp = client.get(&url).send().await.expect("GET /api/history");
        assert_eq!(
            resp.status().as_u16(),
            200,
            "GET /api/history returned {}",
            resp.status()
        );
        resp.json().await.expect("parse history JSON")
    }

    /// Poll `/api/history` until a record matches `predicate` (or ~15 s). Returns
    /// the final snapshot regardless so callers can assert and print on failure.
    pub async fn wait_for_history<F>(&self, predicate: F) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
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

    /// Tear down every process in coverage-safe order: stop the plate-solver
    /// stub first (so rp's in-flight solve errors out and its tool handler
    /// returns), then drop the wedge MCP client, then sentinel (rp's SSE
    /// subscriber), then rp. Leaving any client/handler in flight would block
    /// rp's graceful shutdown and lose its coverage to a SIGKILL.
    pub async fn teardown(&mut self) {
        if let Some(mut stub) = self.plate_solver_stub.take() {
            stub.stop();
        }
        if let Some(task) = self.wedge_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(mut sentinel) = self.sentinel.take() {
            sentinel.stop().await;
        }
        if let Some(mut rp) = self.rp.take() {
            rp.stop().await;
        }
        if let Some(handle) = self.pushover_stub.take() {
            handle.abort();
        }
        // Best-effort: the config, sibling rp.json, and stub manager dir are
        // all under the per-scenario harness directory.
        if let Some(dir) = self.harness_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.manager_dir = None;
    }
}

/// The canned WCS the no-false-alarm scenario solves to: 0.7123 h × 15 =
/// 10.6845°, within tolerance of the requested RA, so centering converges on
/// iteration 1 with no correction slew.
pub fn target_canned_wcs() -> StubBehavior {
    StubBehavior::Canned(CannedWcs {
        ra_center: 10.6845,
        dec_center: 41.269,
        pixel_scale_arcsec: 1.05,
        rotation_deg: 0.0,
        solver: "stub-e2e-1.0".to_string(),
        wcs_matrix: None,
    })
}

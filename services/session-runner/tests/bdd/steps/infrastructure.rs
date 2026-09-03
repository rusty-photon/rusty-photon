//! Shared BDD infrastructure helpers for the session-runner suite: the
//! three-process topology (`OmniSim` + rp + session-runner, in that
//! order — session-runner's config names rp's MCP endpoint) and the run
//! request every scenario posts, reused by every feature's step
//! definitions.

use std::time::Duration;

use bdd_infra::rp_harness::{
    start_rp, write_temp_config_file, McpTestClient, OmniSimHandle, WebhookReceiver,
};
use bdd_infra::ServiceHandle;
use serde_json::Value;

use crate::world::SessionRunnerWorld;

pub async fn ensure_omnisim(world: &mut SessionRunnerWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

// --- Per-device rig primitives -------------------------------------------
//
// One `ensure_*` per device class, each idempotent and pinned to OmniSim
// device 0 with the suite's fixed ids — so every feature's composed
// equipment set (flats, deep-sky, sky-flat) is built from the same
// blocks and cannot drift. Callers `ensure_omnisim` first (these read
// its URL).

pub fn ensure_camera(world: &mut SessionRunnerWorld) {
    if !world.cameras.iter().any(|c| c.id == "main-cam") {
        world.cameras.push(bdd_infra::rp_harness::CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: world.omnisim_url(),
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
    }
}

pub fn ensure_filter_wheel(world: &mut SessionRunnerWorld) {
    if world.filter_wheels.is_empty() {
        world
            .filter_wheels
            .push(bdd_infra::rp_harness::FilterWheelConfig {
                id: "main-fw".to_string(),
                alpaca_url: world.omnisim_url(),
                device_number: 0,
                filters: vec![
                    "Luminance".to_string(),
                    "Red".to_string(),
                    "Green".to_string(),
                    "Blue".to_string(),
                ],
            });
    }
}

pub fn ensure_cover_calibrator(world: &mut SessionRunnerWorld) {
    if world.cover_calibrators.is_empty() {
        world
            .cover_calibrators
            .push(bdd_infra::rp_harness::CoverCalibratorConfig {
                id: "flat-panel".to_string(),
                alpaca_url: world.omnisim_url(),
                device_number: 0,
                poll_interval: Some(std::time::Duration::from_millis(100)),
            });
    }
}

pub fn ensure_mount(world: &mut SessionRunnerWorld) {
    if world.mount.is_none() {
        world.mount = Some(bdd_infra::rp_harness::MountConfig {
            alpaca_url: world.omnisim_url(),
            device_number: 0,
            settle_after_slew: None,
        });
    }
}

pub fn ensure_focuser(world: &mut SessionRunnerWorld) {
    if !world.focusers.iter().any(|f| f.id == "main-focuser") {
        world.focusers.push(bdd_infra::rp_harness::FocuserConfig {
            id: "main-focuser".to_string(),
            alpaca_url: world.omnisim_url(),
            device_number: 0,
            min_position: None,
            max_position: None,
        });
    }
}

/// The default equipment set: one camera, one filter wheel, one cover
/// calibrator, all on `OmniSim` device 0. Scenarios that need less simply
/// don't reference the rest.
pub async fn configure_default_equipment(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
    ensure_camera(world);
    ensure_filter_wheel(world);
    ensure_cover_calibrator(world);
}

/// Start the session-runner service under test: an ephemeral port, a
/// scenario-scoped temp `state_dir`, a temp `workflows_dir` merging the
/// package's shipped `workflows/` with the suite's purpose-built
/// documents from `tests/fixtures/workflows/` (the cucumber runner's cwd
/// is the package dir — `bdd_main!` chdirs to `BDD_PACKAGE_DIR` under
/// Bazel), and — when rp is running — `mcp_server_url` pointing at it,
/// with a fast `safety_poll_interval` so a safety resume lands in test
/// time.
///
/// Both directories are created once per scenario and reused when the
/// service is started again — a recovery scenario that kills and restarts
/// session-runner needs the new process to find the old one's blackboard
/// and run manifest (and resume it by itself).
pub async fn start_session_runner_service(world: &mut SessionRunnerWorld) {
    start_session_runner_service_with(world, Value::Null).await;
}

/// Like [`start_session_runner_service`], merging `extra` top-level fields
/// into the config — the rp client wiring (`mcp_server_url`,
/// `service_auth`, `ca_cert`) for the `mcp_client_auth` scenarios.
pub async fn start_session_runner_service_with(world: &mut SessionRunnerWorld, extra: Value) {
    if world.session_runner.is_some() {
        return;
    }

    if world.workflows_dir.is_none() {
        let cwd = std::env::current_dir().expect("cannot read the cwd");
        let workflows_dir = tempfile::tempdir().expect("cannot create a workflows_dir");
        let mut copied = 0;
        for source in [cwd.join("workflows"), cwd.join("tests/fixtures/workflows")] {
            let entries = std::fs::read_dir(&source)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", source.display()));
            for entry in entries {
                let path = entry.expect("cannot read a workflows entry").path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    let name = path.file_name().expect("a file has a name");
                    std::fs::copy(&path, workflows_dir.path().join(name))
                        .unwrap_or_else(|e| panic!("cannot copy {}: {e}", path.display()));
                    copied += 1;
                }
            }
        }
        assert!(copied > 0, "no workflow documents found to copy");
        world.workflows_dir = Some(workflows_dir);
    }
    if world.state_dir.is_none() {
        world.state_dir = Some(tempfile::tempdir().expect("cannot create a state_dir"));
    }

    let mut config = serde_json::json!({
        "server": { "port": 0 },
        "workflows_dir": world.workflows_dir.as_ref().expect("just ensured").path(),
        "state_dir": world.state_dir.as_ref().expect("just ensured").path(),
        "safety_poll_interval": "250ms",
    });
    if let Some(rp) = world.rp.as_ref() {
        config["mcp_server_url"] = serde_json::json!(format!("{}/mcp", rp.base_url));
    }
    if let Value::Object(extra) = extra {
        let map = config.as_object_mut().expect("config is an object");
        for (key, value) in extra {
            map.insert(key, value);
        }
    }
    let config_path = write_temp_config_file("session-runner-config", &config).await;

    world.session_runner = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);
}

/// Stage the `POST /runs` body — the workflow document and its
/// parameters — that "a run is started" posts to session-runner.
pub fn stage_run(world: &mut SessionRunnerWorld, workflow: &str, parameters: Option<Value>) {
    world.run_request = Some(serde_json::json!({
        "workflow": workflow,
        "params": parameters.unwrap_or_else(|| serde_json::json!({})),
    }));
}

/// Start rp (or restart it after a kill, on the port it first bound —
/// see `SessionRunnerWorld::rp_port`).
pub async fn start_rp_service(world: &mut SessionRunnerWorld) {
    if world
        .rp
        .as_ref()
        .is_some_and(bdd_infra::ServiceHandle::is_running)
    {
        return;
    }

    let config = world.build_rp_config();
    let rp = start_rp(&config).await;
    world.rp_port = Some(rp.port);
    world.rp = Some(rp);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );

    // Seed the planner targets into rp's redb store via the `add_target`
    // MCP tool (the legacy `targets[]` config array was retired). The
    // scenario's Given steps stage the argument objects on
    // `pending_store_targets`; rp must be healthy first.
    if !world.pending_store_targets.is_empty() {
        let mcp = McpTestClient::connect(&format!("{}/mcp", world.rp_url()))
            .await
            .expect("failed to connect MCP client to seed target store");
        for args in &world.pending_store_targets {
            mcp.call_tool("add_target", args.clone())
                .await
                .unwrap_or_else(|e| panic!("seeding add_target failed: {e}"));
        }
    }
}

pub async fn ensure_webhook_receiver(world: &mut SessionRunnerWorld) {
    if world.webhook_receiver.is_some() {
        return;
    }
    let (estimated, max) = world
        .webhook_ack_config
        .unwrap_or((Duration::from_secs(5), Duration::from_secs(10)));
    let events = world.received_events.clone();
    world.webhook_receiver = Some(WebhookReceiver::start(events, estimated, max).await);
}

pub fn add_event_plugin(world: &mut SessionRunnerWorld, events: Vec<String>) {
    let url = world
        .webhook_receiver
        .as_ref()
        .expect("webhook receiver not started")
        .url
        .clone();

    let already_exists = world
        .plugin_configs
        .iter()
        .any(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"));

    if already_exists {
        if let Some(config) = world
            .plugin_configs
            .iter_mut()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"))
        {
            let existing = config
                .get("subscribes_to")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let mut merged = existing;
            for e in events {
                if !merged.contains(&e) {
                    merged.push(e);
                }
            }
            config["subscribes_to"] = serde_json::json!(merged);
        }
    } else {
        world.plugin_configs.push(serde_json::json!({
            "name": "test-event-plugin",
            "type": "event",
            "webhook_url": url,
            "subscribes_to": events
        }));
    }
}

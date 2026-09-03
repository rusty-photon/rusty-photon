//! BDD step definitions for the retired orchestrator surface
//! (`retired_config.feature`, rp.md § Plugin Types, mcp-sessionless D11):
//! a config still carrying a `type: "orchestrator"` registration or a
//! `session.session_state_file` key fails rp's load. The migration
//! message itself is pinned by `config::tests` — the spawned process
//! only reports the refusal.

use cucumber::given;

use crate::world::RpWorld;

fn write_config(world: &mut RpWorld, session: serde_json::Value, plugins: serde_json::Value) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let mut session = session;
    session["data_directory"] =
        serde_json::Value::String(dir.path().join("data").to_string_lossy().into_owned());
    let config = serde_json::json!({
        "session": session,
        "equipment": {},
        "plugins": plugins,
        "server": { "port": 0, "bind_address": "127.0.0.1" }
    });
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

#[given(expr = "an rp config registering an orchestrator plugin at {string}")]
fn config_with_orchestrator(world: &mut RpWorld, invoke_url: String) {
    write_config(
        world,
        serde_json::json!({}),
        serde_json::json!([{
            "name": "session-runner",
            "type": "orchestrator",
            "invoke_url": invoke_url,
            "config": { "workflow": "deep_sky" }
        }]),
    );
}

#[given(expr = "an rp config with session_state_file {string}")]
fn config_with_session_state_file(world: &mut RpWorld, path: String) {
    write_config(
        world,
        serde_json::json!({ "session_state_file": path }),
        serde_json::json!([]),
    );
}

#[given(expr = "an rp config registering an event plugin at {string}")]
fn config_with_event_plugin(world: &mut RpWorld, webhook_url: String) {
    write_config(
        world,
        serde_json::json!({}),
        serde_json::json!([{
            "name": "image-analyzer",
            "type": "event",
            "webhook_url": webhook_url,
            "subscribes_to": ["exposure_complete"]
        }]),
    );
}

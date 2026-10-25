//! Scenario-private rp configs: the temp config file a scenario's rp is
//! spawned from on port 0, and the checks on what that file holds.
//!
//! These scenarios never touch `OmniSim`. Equipment in them points at
//! addresses nothing answers on — rp boots regardless (equipment connects
//! lazily and records the failure).

use std::path::PathBuf;

use cucumber::{given, then};
use serde_json::Value;

use bdd_infra::setup_pages;
use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

/// Write a scenario-private rp config (port 0, temp data directory, the given
/// `equipment` block) and remember its path for later file assertions.
pub fn write_scenario_config(world: &mut RpWorld, equipment: Value) {
    write_scenario_config_with_server(
        world,
        equipment,
        serde_json::json!({ "port": 0, "bind_address": "127.0.0.1" }),
    );
}

/// [`write_scenario_config`] with an explicit `server` block — the
/// Host-allowlist scenarios need the wildcard bind that derives rp's
/// advertised hostname URL.
pub fn write_scenario_config_with_server(world: &mut RpWorld, equipment: Value, server: Value) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": { "data_directory": dir.path().join("data").to_string_lossy() },
        "equipment": equipment,
        "server": server
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

/// The scenario's config file path.
pub fn scenario_config_path(world: &RpWorld) -> PathBuf {
    world
        .config_rest_path
        .clone()
        .expect("no temp rp config — add a 'Given a temp rp config ...' step")
}

/// The scenario's config file as JSON.
pub fn scenario_config_value(world: &RpWorld) -> Value {
    let path = scenario_config_path(world);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("read rp config file"))
        .expect("rp config file is JSON")
}

#[given("a temp rp config with no equipment")]
fn temp_config_no_equipment(world: &mut RpWorld) {
    write_scenario_config(world, serde_json::json!({}));
}

/// Start rp on the scenario's config file, and hand the setup-page steps
/// its address and that file.
#[given("rp is started with that config file")]
async fn rp_started_with_config_file(world: &mut RpWorld) {
    let path = scenario_config_path(world);
    let handle =
        ServiceHandle::start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await;
    setup_pages::adopt_plain(&mut world.setup_pages, handle.bound_port(), path);
    world.rp = Some(handle);
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy"
    );
}

#[then(expr = "the config file JSON at {string} should be {string}")]
fn config_file_json_string_at(world: &mut RpWorld, pointer: String, expected: String) {
    let on_disk = scenario_config_value(world);
    assert_eq!(
        on_disk.pointer(&pointer).and_then(Value::as_str),
        Some(expected.as_str()),
        "unexpected on-disk value at {pointer}; file was: {on_disk}"
    );
}

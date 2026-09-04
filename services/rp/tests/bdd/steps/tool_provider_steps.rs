//! BDD step definitions for tool-provider aggregation (rp.md § Plugin-
//! Provided Tools, § Tool Provider Registration): rp dials a registered
//! provider at startup, merges its tools into the catalog, proxies calls
//! to it, and holds them to the safety contract like built-ins.
//!
//! The provider is [`ToolProviderStub`], an in-process rmcp server the
//! scenario can take down and bring back on the same port; its call and
//! cancellation logs are what the provider-side assertions read.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::ToolProviderStub;

use crate::steps::tool_steps::{ensure_mcp_client, start_rp};
use crate::world::RpWorld;

/// The registration name every scenario uses; the outage scenario's
/// expected error names it.
const PROVIDER_NAME: &str = "stub-provider";

const fn stub(world: &RpWorld) -> &ToolProviderStub {
    world
        .tool_provider_stub
        .as_ref()
        .expect("no stub tool provider — add a 'Given a stub tool provider offering ...' step")
}

const fn stub_mut(world: &mut RpWorld) -> &mut ToolProviderStub {
    world
        .tool_provider_stub
        .as_mut()
        .expect("no stub tool provider — add a 'Given a stub tool provider offering ...' step")
}

/// The `plugins[]` entry registering the stub, with whatever `gate`
/// opt-outs the scenario added.
fn registration(world: &RpWorld) -> Value {
    let mut entry = serde_json::json!({
        "name": PROVIDER_NAME,
        "type": "tool_provider",
        "mcp_server_url": stub(world).url(),
    });
    if !world.tool_provider_ungated.is_empty() {
        let gate: serde_json::Map<String, Value> = world
            .tool_provider_ungated
            .iter()
            .map(|tool| (tool.clone(), Value::String("none".to_owned())))
            .collect();
        entry["gate"] = Value::Object(gate);
    }
    entry
}

// --- Given steps ---

#[given(expr = "a stub tool provider offering {string} and {string}")]
fn stub_provider_offering(world: &mut RpWorld, first: String, second: String) {
    world.tool_provider_stub = Some(ToolProviderStub::start_offering(&[&first, &second]));
}

#[given(expr = "the tool provider registration ungates {string}")]
fn registration_ungates(world: &mut RpWorld, tool: String) {
    world.tool_provider_ungated.push(tool);
}

#[given("rp is running with the tool provider registered")]
async fn rp_running_with_provider(world: &mut RpWorld) {
    let entry = registration(world);
    world.plugin_configs.push(entry);
    start_rp(world).await;
}

/// A minimal config (no equipment) registering the stub, for the
/// startup-validation scenario; consumed by `rp attempts to start`.
#[given("an rp config registering the tool provider")]
fn config_registering_provider(world: &mut RpWorld) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": { "data_directory": dir.path().join("data").to_string_lossy() },
        "equipment": {},
        "plugins": [registration(world)],
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

// --- When steps ---

#[when(expr = "the MCP client calls the provider tool {string} with {}")]
async fn call_provider_tool(world: &mut RpWorld, tool: String, arguments: String) {
    ensure_mcp_client(world).await;
    let args: Value = serde_json::from_str(&arguments).expect("arguments must be JSON");
    world.last_tool_result = Some(world.mcp().call_tool(&tool, args).await);
}

#[when(expr = "a second MCP client starts the provider tool {string} in the background")]
fn start_provider_tool_in_background(world: &mut RpWorld, tool: String) {
    crate::steps::motion_gate_steps::spawn_background_call(world, &tool, serde_json::json!({}));
}

/// Wait until the stub has served a call to `tool` — the background
/// call has to connect and be dispatched before the safety flip lands.
#[when(expr = "the tool provider has received a call to {string}")]
async fn provider_has_received_call(world: &mut RpWorld, tool: String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !stub(world).calls().iter().any(|(name, _)| *name == tool) {
        assert!(
            std::time::Instant::now() < deadline,
            "the tool provider never received a call to '{tool}' (calls: {:?})",
            stub(world).calls()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[when("the tool provider stops")]
async fn provider_stops(world: &mut RpWorld) {
    stub_mut(world).stop().await;
}

#[when("the tool provider comes back")]
async fn provider_comes_back(world: &mut RpWorld) {
    stub_mut(world).restart().await;
}

// --- Then steps ---

#[then(expr = "the provider tool result field {string} should be {string}")]
fn provider_result_field(world: &mut RpWorld, field: String, expected: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result recorded")
        .as_ref()
        .unwrap_or_else(|e| panic!("the provider tool call failed: {e}"));
    assert_eq!(
        result.get(&field).and_then(Value::as_str),
        Some(expected.as_str()),
        "unexpected result: {result}"
    );
}

#[then(expr = "the tool provider should have received a call to {string}")]
fn provider_received_call(world: &mut RpWorld, tool: String) {
    let calls = stub(world).calls();
    assert!(
        calls.iter().any(|(name, _)| *name == tool),
        "the tool provider received no call to '{tool}': {calls:?}"
    );
}

#[then(
    expr = "the tool provider should have seen its {string} request cancelled within {int} seconds"
)]
async fn provider_saw_cancellation(world: &mut RpWorld, tool: String, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while !stub(world).cancelled().contains(&tool) {
        assert!(
            std::time::Instant::now() < deadline,
            "the tool provider saw no cancellation of '{tool}' within {seconds}s (cancelled: {:?})",
            stub(world).cancelled()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the proxied tool until the re-dialed provider answers it — the
/// reconnect supervisor's lane runs on `equipment.reconnect_interval`.
#[then(expr = "the provider tool {string} should answer again within {int} seconds")]
async fn provider_tool_answers_again(world: &mut RpWorld, tool: String, seconds: u64) {
    ensure_mcp_client(world).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let result = world
            .mcp()
            .call_tool(&tool, serde_json::json!({"probe": true}))
            .await;
        match result {
            Ok(value) => {
                assert_eq!(value["probe"], true, "unexpected echo: {value}");
                return;
            }
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the provider tool '{tool}' never answered again within {seconds}s (last error: {e})"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

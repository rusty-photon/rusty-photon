//! BDD step definitions for the stub guider the guided deep-sky
//! scenarios run against: `bdd_infra::rp_harness::GuiderStub`, an
//! in-process double of the guider's HTTP contract
//! (`docs/services/guider.md` § HTTP API), and rp's guiding block that
//! points at it.
//!
//! The stub's device report names the simulator's mount, which is rp's
//! mount in every deep-sky rig, and a camera of its own, which is none
//! of rp's. So rp's check of the guider's devices before a start passes
//! on a rig with no guiding train. A rig step that adds a guiding train
//! names that train's camera in the report itself.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::{GuiderConfig, GuiderStub, McpTestClient};

use crate::steps::infrastructure::ensure_omnisim;
use crate::world::SessionRunnerWorld;

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given(expr = "a stub guider in state {string}")]
async fn stub_guider_in_state(world: &mut SessionRunnerWorld, state: String) {
    ensure_omnisim(world).await;
    let stub = GuiderStub::start(&state).await;
    let mount_id = simulator_unique_id(world, "Telescope").await;
    stub.set_mount_unique_id(Some(&mount_id));
    let mut guider = GuiderConfig::url_only(stub.url.clone());
    // The document's dither call carries no amount: rp falls back to
    // the guiding config's dither_pixels, which is rig geometry and
    // belongs in rp's config, not in workflow parameters.
    guider.dither_pixels = Some(1.5);
    world.guider = Some(guider);
    world.guider_stub = Some(stub);
}

/// The HFD of the frames the stub's metrics serve, one entry per
/// metrics answer, the last repeating. An idle stub serves no frame and
/// takes no entry, so the script starts with the document's start.
#[given(expr = "the stub guider serves the HFD script {string}")]
fn stub_guider_serves_hfd_script(world: &mut SessionRunnerWorld, script: String) {
    let values: Vec<f64> = script
        .split(',')
        .map(|s| s.trim().parse().expect("HFD script entries are numbers"))
        .collect();
    guider_stub(world).set_hfd_script(values);
}

/// rp's Guide Focus Watch config (`equipment.mount.guiding.focus_watch`).
/// Cooldown is left long so a degradation fires exactly once.
#[given(
    expr = "the stub guider has a focus watch of window {int}, poll interval {string}, and escalation deadline {string}"
)]
fn stub_guider_focus_watch(
    world: &mut SessionRunnerWorld,
    window: i64,
    poll_interval: String,
    escalation_deadline: String,
) {
    guider_config(world).focus_watch = Some(serde_json::json!({
        "window": window,
        "degrade_ratio": 1.25,
        "cooldown": "10m",
        "escalation_deadline": escalation_deadline,
        "poll_interval": poll_interval,
    }));
}

/// A key of rp's guiding block given as raw JSON (`"2s"`, `0.8`, ...),
/// written over whatever the rig step emits for it.
#[given(regex = r#"^rp's guiding config sets "([^"]+)" to the JSON (.+)$"#)]
fn guiding_config_sets(world: &mut SessionRunnerWorld, key: String, value: String) {
    let value: Value = serde_json::from_str(&value)
        .unwrap_or_else(|e| panic!("the guiding config value {value:?} is not JSON: {e}"));
    guider_config(world).overrides.push((key, value));
}

#[given(
    expr = "the stub guider answers {string} requests with error code {string} and message {string}"
)]
#[when(
    expr = "the stub guider answers {string} requests with error code {string} and message {string}"
)]
fn stub_guider_fails(world: &mut SessionRunnerWorld, path: String, code: String, message: String) {
    guider_stub(world).fail(&path, &code, &message);
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

#[when("the stub guider loses its guide star")]
fn stub_guider_loses_its_star(world: &mut SessionRunnerWorld) {
    guider_stub(world).set_state("star_lost");
}

#[when(expr = "the stub guider stops the run on its own with reason {string}")]
fn stub_guider_stops_on_its_own(world: &mut SessionRunnerWorld, reason: String) {
    guider_stub(world).stop_on_its_own(&reason);
}

/// A tool call on rp from a client that is not the run's: an operator
/// at another MCP client. The call has to succeed.
#[when(expr = "an operator's MCP client calls {string} on rp")]
async fn operator_calls_tool(world: &mut SessionRunnerWorld, tool: String) {
    let mcp = McpTestClient::connect(&format!("{}/mcp", world.rp_url()))
        .await
        .expect("the operator's MCP client failed to connect to rp");
    mcp.call_tool(&tool, serde_json::json!({}))
        .await
        .unwrap_or_else(|e| panic!("the operator's '{tool}' call failed: {e}"));
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then(expr = "the stub guider should have received exactly {int} {string} request(s)")]
fn stub_guider_received_exactly(world: &mut SessionRunnerWorld, expected: usize, path: String) {
    let received = guider_stub(world).requests_to(&path).len();
    assert_eq!(
        received, expected,
        "expected exactly {expected} request(s) to '{path}', saw {received}"
    );
}

#[then(expr = "the stub guider should have received at least {int} {string} requests")]
fn stub_guider_received_at_least(world: &mut SessionRunnerWorld, minimum: usize, path: String) {
    let received = guider_stub(world).requests_to(&path).len();
    assert!(
        received >= minimum,
        "expected at least {minimum} requests to '{path}', saw {received}"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub const fn guider_stub(world: &SessionRunnerWorld) -> &GuiderStub {
    world
        .guider_stub
        .as_ref()
        .expect("no stub guider: add the 'a stub guider in state ...' step")
}

const fn guider_config(world: &mut SessionRunnerWorld) -> &mut GuiderConfig {
    world
        .guider
        .as_mut()
        .expect("this step expects a prior 'a stub guider in state ...' step")
}

/// The `UniqueID` the simulator lists for its first device of
/// `device_type` (`Telescope`, `Camera`, ...).
async fn simulator_unique_id(world: &SessionRunnerWorld, device_type: &str) -> String {
    let url = format!("{}/management/v1/configureddevices", world.omnisim_url());
    let body: Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("failed to GET the simulator's configureddevices")
        .json()
        .await
        .expect("configureddevices is not JSON");
    body["Value"]
        .as_array()
        .expect("configureddevices has no Value array")
        .iter()
        .find(|d| d["DeviceType"] == device_type)
        .and_then(|d| d["UniqueID"].as_str())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("the simulator lists no {device_type} with a UniqueID: {body}"))
        .to_string()
}

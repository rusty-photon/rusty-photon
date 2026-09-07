//! Step definitions for `server_registration.feature`

use crate::steps::infrastructure::{
    both_disabled_config, default_test_config, oc_only_config, switch_only_config,
};
use crate::world::Upbv2World;
use cucumber::{given, then, when};

// ============================================================================
// Given steps
// ============================================================================

#[given("a UPBv2 server config with switch enabled and OC enabled")]
fn server_config_both_enabled(world: &mut Upbv2World) {
    world.config = default_test_config();
}

#[given("a UPBv2 server config with switch enabled and OC disabled")]
fn server_config_switch_only(world: &mut Upbv2World) {
    world.config = switch_only_config();
}

#[given("a UPBv2 server config with switch disabled and OC enabled")]
fn server_config_oc_only(world: &mut Upbv2World) {
    world.config = oc_only_config();
}

#[given("a UPBv2 server config with switch disabled and OC disabled")]
fn server_config_none(world: &mut Upbv2World) {
    world.config = both_disabled_config();
}

#[given(expr = "a UPBv2 server config with switch name {string}")]
fn server_config_with_switch_name(world: &mut Upbv2World, name: String) {
    world.config = default_test_config();
    world.config["switch"]["name"] = serde_json::json!(name);
}

#[given(expr = "a UPBv2 server config with OC name {string}")]
fn server_config_with_oc_name(world: &mut Upbv2World, name: String) {
    world.config = default_test_config();
    world.config["observingconditions"]["name"] = serde_json::json!(name);
}

// ============================================================================
// When steps
// ============================================================================

#[when("I start the UPBv2 server")]
async fn start_server(world: &mut Upbv2World) {
    world.start_upbv2().await;
}

// ============================================================================
// Then steps
// ============================================================================

#[then("the switch endpoint should respond with 200")]
async fn switch_endpoint_responds_200(world: &mut Upbv2World) {
    let base = world.base_url.as_ref().expect("server not started");
    let url = format!("{base}/api/v1/switch/0/name");
    let resp = reqwest::get(&url).await.expect("GET switch name failed");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "switch name endpoint should respond with 200"
    );
}

#[then("the switch endpoint should not respond with 200")]
async fn switch_endpoint_not_200(world: &mut Upbv2World) {
    let base = world.base_url.as_ref().expect("server not started");
    let url = format!("{base}/api/v1/switch/0/name");
    let resp = reqwest::get(&url).await.expect("GET switch name failed");
    assert_ne!(
        resp.status().as_u16(),
        200,
        "switch should not be registered"
    );
}

#[then("the OC endpoint should respond with 200")]
async fn oc_endpoint_responds_200(world: &mut Upbv2World) {
    let base = world.base_url.as_ref().expect("server not started");
    let url = format!("{base}/api/v1/observingconditions/0/name");
    let resp = reqwest::get(&url).await.expect("GET OC name failed");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "OC name endpoint should respond with 200"
    );
}

#[then("the OC endpoint should not respond with 200")]
async fn oc_endpoint_not_200(world: &mut Upbv2World) {
    let base = world.base_url.as_ref().expect("server not started");
    let url = format!("{base}/api/v1/observingconditions/0/name");
    let resp = reqwest::get(&url).await.expect("GET OC name failed");
    assert_ne!(resp.status().as_u16(), 200, "OC should not be registered");
}

#[then(expr = "the switch name endpoint should return {string}")]
async fn switch_name_endpoint_returns(world: &mut Upbv2World, expected: String) {
    let name = world.switch_ref().name().await.unwrap();
    assert_eq!(name, expected, "switch name mismatch");
}

#[then(expr = "the OC name endpoint should return {string}")]
async fn oc_name_endpoint_returns(world: &mut Upbv2World, expected: String) {
    let name = world.oc_ref().name().await.unwrap();
    assert_eq!(name, expected, "OC name mismatch");
}

#[then("the server should be reachable on the bound port")]
async fn server_reachable_on_bound_port(world: &mut Upbv2World) {
    let handle = world.upbv2.as_ref().expect("server not started");
    let port = handle.port;
    assert_ne!(port, 0, "OS should have assigned a real port");
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await;
    assert!(
        stream.is_ok(),
        "server should be reachable on bound port {port}"
    );
}

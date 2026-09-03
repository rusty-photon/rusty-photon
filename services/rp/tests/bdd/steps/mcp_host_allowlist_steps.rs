//! BDD step definitions for the MCP endpoint's `Host` allowlist
//! (`mcp_host_allowlist.feature`).
//!
//! The scenarios spawn their own rp on port 0 bound to all interfaces —
//! the deployment shape whose advertised hostname URL rp used to reject
//! — and address the loopback listener with an explicit `Host` header,
//! which is exactly what an orchestrator dialing that URL sends. The
//! probe is a self-contained 2026-07-28 `tools/list`
//! (`mcp_transport_steps::post_2026_07_28`); the status assertion
//! lives there too. They never touch `OmniSim`, so the feature is
//! untagged (no `@serial`), and they reuse `rp is started with that
//! config file` from `config_rest_steps`.

use cucumber::{given, when};
use serde_json::Value;

use crate::world::RpWorld;

use super::config_rest_steps::write_scenario_config_with_server;
use super::mcp_transport_steps::post_2026_07_28;

/// A wildcard bind on an ephemeral port, optionally advertising a URL.
fn wildcard_server(advertised_url: Option<&str>) -> Value {
    match advertised_url {
        Some(url) => serde_json::json!({
            "port": 0, "bind_address": "0.0.0.0", "advertised_url": url
        }),
        None => serde_json::json!({ "port": 0, "bind_address": "0.0.0.0" }),
    }
}

/// POST a 2026-07-28 `tools/list` to rp's loopback listener carrying
/// `host` as the `Host` header; the transport's answer lands in
/// `world.last_mcp_probe`.
async fn send_request_with_host(world: &mut RpWorld, host: &str) {
    post_2026_07_28(world, "tools/list", None, serde_json::json!({}), Some(host)).await;
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("a temp rp config bound to all interfaces")]
fn temp_config_wildcard_bind(world: &mut RpWorld) {
    write_scenario_config_with_server(world, serde_json::json!({}), wildcard_server(None));
}

#[given(expr = "a temp rp config bound to all interfaces advertising {string}")]
fn temp_config_wildcard_bind_advertising(world: &mut RpWorld, url: String) {
    write_scenario_config_with_server(world, serde_json::json!({}), wildcard_server(Some(&url)));
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("a 2026-07-28 MCP request is sent with the system hostname as the Host header")]
async fn request_with_system_hostname(world: &mut RpWorld) {
    // The same source rp derives its advertised host from, so the
    // scenario asserts the two agree rather than a hard-coded name.
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .expect("system hostname is not available on this host");
    send_request_with_host(world, &hostname).await;
}

#[when(expr = "a 2026-07-28 MCP request is sent with the Host header {string}")]
async fn request_with_host(world: &mut RpWorld, host: String) {
    send_request_with_host(world, &host).await;
}

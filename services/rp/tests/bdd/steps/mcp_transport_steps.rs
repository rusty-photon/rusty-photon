//! BDD step definitions for the session-less MCP transport
//! (`mcp_transport.feature`; rp.md § MCP Server, ADR-021), plus the raw
//! JSON-RPC probe the Host-allowlist steps share.
//!
//! The probes here bypass the standard client on purpose: the contract
//! under test is the wire shape — which headers a 2026-07-28 request
//! carries, that no response carries an `Mcp-Session-Id`, that an
//! `initialize` from an older client is answered without one — and a
//! client library would hide exactly that. The scenarios spawn their
//! own rp on port 0 with no equipment and never touch `OmniSim`, so
//! the feature is untagged (no `@serial`).

use std::time::Duration;

use cucumber::{then, when};
use serde_json::Value;

use crate::world::{McpProbeResponse, RpWorld};

/// The protocol revision rp serves natively and first-party clients pin.
pub const PROTOCOL_2026_07_28: &str = "2026-07-28";

/// The `_meta` block every 2026-07-28 request carries (SEP-2575): the
/// protocol version and the client's capabilities are what rmcp
/// requires; `clientInfo` is courtesy.
fn request_meta() -> Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_2026_07_28,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "rp-bdd-probe", "version": "0"},
    })
}

/// POST one self-contained 2026-07-28 request to rp's `/mcp`:
/// `MCP-Protocol-Version` and `Mcp-Method` headers (SEP-2243), plus
/// `Mcp-Name` when `tool` names a `tools/call` target, and the `_meta`
/// block inside `params`. `host` overrides the `Host` header for the
/// allowlist scenarios. The answer lands in `world.last_mcp_probe`.
pub async fn post_2026_07_28(
    world: &mut RpWorld,
    method: &str,
    tool: Option<&str>,
    mut params: Value,
    host: Option<&str>,
) {
    let Some(params_object) = params.as_object_mut() else {
        panic!("probe params must be a JSON object, got: {params}");
    };
    params_object.insert("_meta".to_owned(), request_meta());
    let mut request = reqwest::Client::new()
        .post(world.rp_mcp_url())
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_2026_07_28)
        .header("Mcp-Method", method);
    if let Some(tool) = tool {
        request = request.header("Mcp-Name", tool);
    }
    if let Some(host) = host {
        request = request.header(reqwest::header::HOST, host);
    }
    let response = request
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        }))
        .send()
        .await
        .expect("POST /mcp request failed");
    record(world, response).await;
}

/// POST one request the way a pre-2026-07-28 client would: the
/// `MCP-Protocol-Version` header naming `version`, no SEP-2243 headers,
/// no `_meta`, and — deliberately — no `Mcp-Session-Id`.
async fn post_legacy(world: &mut RpWorld, version: &str, body: Value) {
    let response = reqwest::Client::new()
        .post(world.rp_mcp_url())
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", version)
        .json(&body)
        .send()
        .await
        .expect("POST /mcp request failed");
    record(world, response).await;
}

/// Capture the status, the session header (which must never appear)
/// and the JSON-RPC object — rmcp answers as plain JSON or as an SSE
/// stream whose first non-empty `data:` line is the object.
async fn record(world: &mut RpWorld, response: reqwest::Response) {
    let status = response.status().as_u16();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let text = response.text().await.expect("read /mcp response body");
    let json_text = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("data:")
                .map(str::trim)
                .filter(|data| !data.is_empty())
        })
        .unwrap_or(text.as_str());
    let body = serde_json::from_str::<Value>(json_text).ok();
    world.last_mcp_probe = Some(McpProbeResponse {
        status,
        session_id,
        body,
        raw: text,
    });
}

const fn last_probe(world: &RpWorld) -> &McpProbeResponse {
    world
        .last_mcp_probe
        .as_ref()
        .expect("no /mcp request was sent in this scenario")
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(expr = "a 2026-07-28 {string} request for {string} is sent")]
async fn modern_tool_call(world: &mut RpWorld, method: String, tool: String) {
    assert_eq!(method, "tools/call", "only tools/call names a tool");
    post_2026_07_28(
        world,
        &method,
        Some(&tool),
        serde_json::json!({"name": tool, "arguments": {}}),
        None,
    )
    .await;
}

#[when(expr = "a {string} initialize request is sent")]
async fn legacy_initialize(world: &mut RpWorld, version: String) {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": {"name": "rp-bdd-legacy-probe", "version": "0"}
        }
    });
    post_legacy(world, &version, body).await;
}

#[when(expr = "a {string} {string} request is sent with no session header")]
async fn legacy_request(world: &mut RpWorld, version: String, method: String) {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": method});
    post_legacy(world, &version, body).await;
}

#[when(expr = "the MCP client stays idle for {int} seconds")]
async fn client_idles(_world: &mut RpWorld, seconds: u64) {
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the MCP response status should be {int}")]
fn mcp_response_status_is(world: &mut RpWorld, expected: u16) {
    let probe = last_probe(world);
    assert_eq!(
        probe.status, expected,
        "unexpected /mcp status; body: {}",
        probe.raw
    );
}

#[then("the MCP response should carry no Mcp-Session-Id header")]
fn mcp_response_has_no_session(world: &mut RpWorld) {
    let probe = last_probe(world);
    assert_eq!(
        probe.session_id, None,
        "the transport is session-less: no response may hand out a session id"
    );
}

#[then("the MCP response should be a JSON-RPC result")]
fn mcp_response_is_a_result(world: &mut RpWorld) {
    let probe = last_probe(world);
    let body = probe
        .body
        .as_ref()
        .unwrap_or_else(|| panic!("the /mcp body is not JSON-RPC: {}", probe.raw));
    assert!(
        body.get("result").is_some() && body.get("error").is_none(),
        "expected a JSON-RPC result, got: {body}"
    );
}

#[then(expr = "the MCP client should have negotiated protocol version {string}")]
fn client_negotiated_version(world: &mut RpWorld, expected: String) {
    let negotiated = world.mcp().protocol_version();
    assert_eq!(
        negotiated.as_deref(),
        Some(expected.as_str()),
        "the standard client pins the protocol revision it negotiates"
    );
}

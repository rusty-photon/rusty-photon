//! BDD step definitions for MCP over TLS with HTTP Basic auth.
//!
//! These scenarios prove the /mcp endpoint honors the server-wide
//! `server.tls` / `server.auth` (ADR-017): a client presenting the
//! observatory credential over verified HTTPS gets the catalog and can
//! call tools; a client without (or with wrong) credentials cannot
//! establish a session. rp startup reuses the shared TLS/auth steps in
//! `auth_steps.rs` / `tls_steps.rs`.

use bdd_infra::rp_harness::McpTestClient;
use cucumber::{given, then, when};

use crate::world::RpWorld;

use super::tool_steps::{add_camera, ensure_omnisim};

/// The rp MCP URL over TLS. `ServiceHandle.base_url` is http-schemed;
/// the TLS scenarios address the same port over https, like the
/// health-endpoint steps do.
fn mcp_url_https(world: &RpWorld) -> String {
    let port = world.rp.as_ref().expect("rp not started").port();
    format!("https://127.0.0.1:{port}/mcp")
}

#[given("a camera on the simulator is configured for the next rp start")]
async fn camera_configured_for_next_start(world: &mut RpWorld) {
    // Accumulates config only — `When rp is started with auth` (in
    // auth_steps.rs) builds the config from the world, so the camera
    // rides the TLS+auth rp.
    ensure_omnisim(world).await;
    add_camera(world);
}

#[when("an MCP client connects over TLS with valid credentials")]
async fn mcp_client_connects_authed(world: &mut RpWorld) {
    let url = mcp_url_https(world);
    let pki = world.pki();
    let client =
        McpTestClient::connect_authed(&url, pki.username(), pki.password(), &pki.ca_path())
            .await
            .expect("authed MCP connect failed");
    world.mcp_client = Some(client);
}

#[then(expr = "the MCP tool catalog should include {string}")]
async fn mcp_catalog_includes(world: &mut RpWorld, tool: String) {
    let tools = world.mcp().list_tools().await.expect("list_tools failed");
    assert!(
        tools.contains(&tool),
        "tool catalog is missing {tool:?}; got: {tools:?}"
    );
}

#[then(expr = "calling {string} for camera {string} over MCP should succeed")]
async fn mcp_tool_call_succeeds(world: &mut RpWorld, tool: String, camera_id: String) {
    let result = world
        .mcp()
        .call_tool(&tool, serde_json::json!({ "camera_id": camera_id }))
        .await
        .expect("tool call over authed TLS failed");
    assert_eq!(result["camera_id"], camera_id.as_str());
    assert!(
        result.get("max_adu").is_some(),
        "expected camera info fields, got: {result}"
    );
}

#[then("an MCP client without credentials cannot list tools")]
async fn mcp_client_unauthed_rejected(world: &mut RpWorld) {
    let url = mcp_url_https(world);
    // CA-trusting but credential-less: the rejection under test is auth,
    // not TLS trust.
    match McpTestClient::connect_tls(&url, &world.pki().ca_path()).await {
        Err(message) => {
            assert!(
                message.contains("401") || message.to_lowercase().contains("auth"),
                "expected an auth rejection, got: {message}"
            );
        }
        Ok(client) => {
            let err = client
                .list_tools()
                .await
                .expect_err("an unauthenticated session must not list tools");
            assert!(
                err.contains("401") || err.to_lowercase().contains("auth"),
                "expected an auth rejection, got: {err}"
            );
        }
    }
}

#[then("an MCP client with the wrong password cannot list tools")]
async fn mcp_client_wrong_password_rejected(world: &mut RpWorld) {
    let url = mcp_url_https(world);
    let (username, ca_path) = {
        let pki = world.pki();
        (pki.username().to_owned(), pki.ca_path())
    };
    match McpTestClient::connect_authed(&url, &username, "wrong-password", &ca_path).await {
        Err(message) => {
            assert!(
                message.contains("401") || message.to_lowercase().contains("auth"),
                "expected an auth rejection, got: {message}"
            );
        }
        Ok(client) => {
            let err = client
                .list_tools()
                .await
                .expect_err("a wrong-credential session must not list tools");
            assert!(
                err.contains("401") || err.to_lowercase().contains("auth"),
                "expected an auth rejection, got: {err}"
            );
        }
    }
}

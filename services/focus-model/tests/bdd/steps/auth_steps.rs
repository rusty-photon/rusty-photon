//! TLS + HTTP Basic Auth smoke steps. The `/health` half is expanded
//! from the shared macro (the service-specific parts — config
//! template, launch — live in the `TlsAuthSmokeWorld` impl in
//! `world.rs`); the `/mcp` half below proves the same server block
//! guards the MCP endpoint, and that `tools/list` answers with no rp
//! running. The smoke scenarios spawn ONLY focus-model itself, with a
//! temp config — no `OmniSim`, no rp.

use bdd_infra::rp_harness::McpTestClient;
use bdd_infra::tls_auth::TlsAuthSmokeWorld as _;
use cucumber::then;

use crate::world::FocusModelWorld;

bdd_infra::tls_auth_smoke_steps!(FocusModelWorld);

/// The MCP URL of the TLS-and-auth service the smoke started.
fn mcp_url(world: &mut FocusModelWorld) -> String {
    format!("https://localhost:{}/mcp", world.tls_auth().port())
}

#[then("the MCP endpoint rejects an unauthenticated client")]
async fn mcp_rejects_unauthenticated(world: &mut FocusModelWorld) {
    let url = mcp_url(world);
    let health = format!("https://localhost:{}/health", world.tls_auth().port());
    let pki = world.tls_auth().pki();
    // Readiness is probed with credentials on /health first, so the
    // refused discovery below is the auth layer, not the socket: the
    // credential is the only thing the MCP client lacks.
    bdd_infra::tls_auth::wait_until_ready(
        &pki.https_client(),
        &health,
        pki.username(),
        pki.password(),
    )
    .await;

    let refused = McpTestClient::connect_tls(&url, &pki.ca_path()).await;
    assert!(
        refused.is_err(),
        "an unauthenticated MCP client must be refused, but discovery succeeded"
    );

    // And refused with the challenge the contract names: a 403 or a
    // 500 would fail discovery just as well and mean something else.
    let status = pki
        .https_client()
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#)
        .send()
        .await
        .unwrap_or_else(|e| panic!("the unauthenticated request could not be sent: {e}"))
        .status();
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "an unauthenticated /mcp request must be 401"
    );
}

#[then(expr = "the MCP endpoint lists {string} and {string} for the authenticated client")]
async fn mcp_lists_tools_for_authenticated(
    world: &mut FocusModelWorld,
    first: String,
    second: String,
) {
    let url = mcp_url(world);
    let pki = world.tls_auth().pki();
    let client =
        McpTestClient::connect_authed(&url, pki.username(), pki.password(), &pki.ca_path())
            .await
            .unwrap_or_else(|e| panic!("the authenticated MCP client was refused: {e}"));
    let tools = client
        .list_tools()
        .await
        .unwrap_or_else(|e| panic!("tools/list failed: {e}"));
    for expected in [first, second] {
        assert!(
            tools.contains(&expected),
            "tools/list lacks {expected}: {tools:?}"
        );
    }
}

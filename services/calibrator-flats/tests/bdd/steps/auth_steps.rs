//! TLS + HTTP Basic Auth smoke steps. The `/health` half is expanded from
//! the shared macro (the service-specific parts — config template, launch
//! — live in the `TlsAuthSmokeWorld` impl in `world.rs`); the `/mcp` half
//! below proves the same server block guards the MCP endpoint, and that
//! `tools/list` answers with no rp running (plan D10). The smoke
//! scenarios spawn ONLY calibrator-flats itself, with a temp config — no
//! `OmniSim`, no rp.

use bdd_infra::rp_harness::McpTestClient;
use bdd_infra::tls_auth::TlsAuthSmokeWorld as _;
use cucumber::then;

use crate::world::CalibratorFlatsWorld;

bdd_infra::tls_auth_smoke_steps!(CalibratorFlatsWorld);

/// The MCP URL of the TLS-and-auth service the smoke started.
fn mcp_url(world: &mut CalibratorFlatsWorld) -> String {
    format!("https://localhost:{}/mcp", world.tls_auth().port())
}

#[then("the MCP endpoint rejects an unauthenticated client")]
async fn mcp_rejects_unauthenticated(world: &mut CalibratorFlatsWorld) {
    let url = mcp_url(world);
    let pki = world.tls_auth().pki();
    // Readiness is probed with credentials on /health by the shared
    // steps; here the credential is the only thing missing, so a
    // refused discovery is the auth layer, not the socket.
    let ca = pki.ca_path();
    let health = format!("https://localhost:{}/health", world.tls_auth().port());
    let pki = world.tls_auth().pki();
    bdd_infra::tls_auth::wait_until_ready(
        &pki.https_client(),
        &health,
        pki.username(),
        pki.password(),
    )
    .await;

    let refused = McpTestClient::connect_tls(&url, &ca).await;
    assert!(
        refused.is_err(),
        "an unauthenticated MCP client must be refused, but discovery succeeded"
    );
}

#[then(
    expr = "the MCP endpoint lists {string}, {string} and {string} for the authenticated client"
)]
async fn mcp_lists_tools_for_authenticated(
    world: &mut CalibratorFlatsWorld,
    first: String,
    second: String,
    third: String,
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
    for expected in [first, second, third] {
        assert!(
            tools.contains(&expected),
            "tools/list lacks {expected}: {tools:?}"
        );
    }
}

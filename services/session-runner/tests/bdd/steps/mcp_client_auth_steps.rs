//! BDD step definitions for session-runner's authenticated MCP client
//! (`service_auth` / `ca_cert`, ADR-017).
//!
//! The positive scenario proves standalone /validate reaches a TLS- and
//! auth-enabled rp when both fields are wired. The negative scenario
//! pins the credential policy behaviorally: with `service_auth` but no
//! `ca_cert` the credential is NOT sent, so an auth-enabled plain-HTTP rp
//! rejects the catalog fetch — if a regression sent the credential
//! anyway, the fetch would succeed and the scenario would fail.

use bdd_infra::rp_harness::start_rp;
use bdd_infra::tls_auth::{wait_until_ready, PkiFixture};
use cucumber::{given, then, when};
use serde_json::json;

use crate::world::SessionRunnerWorld;

use super::infrastructure::start_session_runner_service_with;

#[given("generated TLS certificates")]
async fn generate_certs(world: &mut SessionRunnerWorld) {
    // The certificate is for the rp these scenarios spawn, not for
    // session-runner's own server (the tls_auth_smoke macro covers that).
    // `shared_pki` keys on that name, so the two fixtures stay distinct.
    world.tls_auth.pki = Some(bdd_infra::tls_auth::shared_pki("rp").await);
}

fn pki(world: &SessionRunnerWorld) -> &PkiFixture {
    world
        .tls_auth
        .pki
        .as_deref()
        .expect("TLS certs not generated")
}

fn rp_port(world: &SessionRunnerWorld) -> u16 {
    world.rp.as_ref().expect("rp not started").port()
}

#[given("rp is started with TLS and auth enabled")]
async fn rp_started_tls_auth(world: &mut SessionRunnerWorld) {
    let mut config = world.build_rp_config();
    config["server"]["tls"] = pki(world).tls_block();
    config["server"]["auth"] = pki(world).auth_block();
    world.rp = Some(start_rp(&config).await);

    let fixture = pki(world);
    let client = fixture.https_client();
    let url = format!("https://127.0.0.1:{}/health", rp_port(world));
    wait_until_ready(&client, &url, fixture.username(), fixture.password()).await;
}

#[given("rp is started without TLS but with auth enabled")]
async fn rp_started_plain_auth(world: &mut SessionRunnerWorld) {
    let mut config = world.build_rp_config();
    config["server"]["auth"] = pki(world).auth_block();
    world.rp = Some(start_rp(&config).await);

    let fixture = pki(world);
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/health", rp_port(world));
    wait_until_ready(&client, &url, fixture.username(), fixture.password()).await;
}

#[given("session-runner is configured with service_auth and ca_cert for rp")]
async fn session_runner_configured_authed(world: &mut SessionRunnerWorld) {
    let mcp_url = format!("https://127.0.0.1:{}/mcp", rp_port(world));
    let fixture = pki(world);
    let extra = json!({
        "mcp_server_url": mcp_url,
        "service_auth": { "username": fixture.username(), "password": fixture.password() },
        "ca_cert": fixture.ca_path().to_string_lossy(),
    });
    start_session_runner_service_with(world, extra).await;
}

#[given("session-runner is configured with service_auth for rp but no ca_cert")]
async fn session_runner_configured_auth_no_ca(world: &mut SessionRunnerWorld) {
    let mcp_url = format!("http://127.0.0.1:{}/mcp", rp_port(world));
    let fixture = pki(world);
    let extra = json!({
        "mcp_server_url": mcp_url,
        "service_auth": { "username": fixture.username(), "password": fixture.password() },
    });
    start_session_runner_service_with(world, extra).await;
}

#[when(expr = "the shipped {string} document is validated standalone")]
async fn validate_standalone(world: &mut SessionRunnerWorld, workflow: String) {
    let base_url = world
        .session_runner
        .as_ref()
        .expect("session-runner not started")
        .base_url()
        .clone();
    let response = reqwest::Client::new()
        .post(format!("{base_url}/validate"))
        .json(&json!({ "workflow": workflow }))
        .send()
        .await
        .expect("POST /validate failed");
    world.last_api_status = Some(response.status().as_u16());
    world.last_api_body = Some(response.json().await.expect("non-JSON /validate response"));
}

#[then(expr = "the validation response reports catalog_validation {string}")]
async fn validation_reports_catalog(world: &mut SessionRunnerWorld, expected: String) {
    assert_eq!(world.last_api_status, Some(200));
    let body = world.last_api_body.as_ref().expect("no /validate response");
    assert_eq!(
        body["catalog_validation"], expected,
        "unexpected catalog_validation in: {body}"
    );
    assert_eq!(body["valid"], true, "document invalid: {body}");
}

#[then("the validation response reports the catalog check skipped as unreachable")]
async fn validation_reports_skipped(world: &mut SessionRunnerWorld) {
    assert_eq!(world.last_api_status, Some(200));
    let body = world.last_api_body.as_ref().expect("no /validate response");
    let catalog = body["catalog_validation"]
        .as_str()
        .expect("catalog_validation is a string");
    assert!(
        catalog.starts_with("skipped: rp unreachable"),
        "expected the catalog fetch to be rejected (credential must not be \
         sent without a CA), got: {catalog}"
    );
}

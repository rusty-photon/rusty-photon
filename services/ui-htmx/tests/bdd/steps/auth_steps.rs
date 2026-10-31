//! BDD step definitions for the ui-htmx TLS + HTTP Basic Auth suites
//! (`auth.feature`, `tls.feature`). The BFF is spawned with the shared
//! bdd-infra PKI + credentials fixture ([`PkiFixture`]) and probed at
//! `/health` — auth wraps the whole router, so the liveness route is enough
//! to prove the credential gate.

use bdd_infra::tls_auth::{wait_until_ready, PkiFixture};
use cucumber::{given, then, when};

use crate::world::UiWorld;

/// The scenario's PKI + credentials fixture.
fn pki(world: &UiWorld) -> &PkiFixture {
    world.pki.as_deref().expect("TLS certs not generated")
}

#[given("generated TLS certificates for ui-htmx")]
async fn generate_tls_certs(world: &mut UiWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("ui-htmx is configured with TLS and auth enabled")]
fn configured_with_tls_and_auth(world: &mut UiWorld) {
    world.pending_config = Some(serde_json::json!({
        "server": pki(world).server_block(0),
        "rp": {}
    }));
}

#[given("ui-htmx is configured with TLS enabled and no auth")]
fn configured_with_tls_only(world: &mut UiWorld) {
    world.pending_config = Some(serde_json::json!({
        "server": { "port": 0, "tls": pki(world).tls_block() },
        "rp": {}
    }));
}

#[given("ui-htmx is configured without TLS or auth")]
fn configured_plain(world: &mut UiWorld) {
    world.pending_config = Some(serde_json::json!({
        "server": { "port": 0, "bind_address": "127.0.0.1" },
        "rp": {}
    }));
}

#[when("ui-htmx is started with TLS and auth")]
async fn started_with_tls_and_auth(world: &mut UiWorld) {
    world.start_bff_from_pending_config().await;
}

#[when("ui-htmx is started with TLS")]
async fn started_with_tls(world: &mut UiWorld) {
    world.start_bff_from_pending_config().await;
}

#[when("ui-htmx is started without TLS or auth")]
async fn started_plain(world: &mut UiWorld) {
    world.start_bff_from_pending_config().await;
}

fn health_url(world: &UiWorld, scheme: &str) -> String {
    let port = world.ui.as_ref().expect("BFF not started").port();
    format!("{scheme}://127.0.0.1:{port}/health")
}

#[then("the health endpoint should respond with valid credentials")]
async fn responds_with_valid_credentials(world: &mut UiWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = health_url(world, "https");
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some(pki.password()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[then("the health endpoint should reject wrong credentials with 401")]
async fn rejects_wrong_credentials(world: &mut UiWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = health_url(world, "https");
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some("wrong-password"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the health endpoint should reject missing credentials with 401")]
async fn rejects_missing_credentials(world: &mut UiWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = health_url(world, "https");
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the 401 response should include a WWW-Authenticate header")]
async fn response_includes_www_authenticate(world: &mut UiWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = health_url(world, "https");
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("missing WWW-Authenticate header")
        .to_str()
        .unwrap();
    assert_eq!(www_auth, "Basic realm=\"Rusty Photon\"");
}

#[then("the health endpoint should respond without credentials")]
async fn responds_without_credentials(world: &mut UiWorld) {
    let client = reqwest::Client::new();
    let port = world.ui.as_ref().expect("BFF not started").port();
    let url = format!("http://127.0.0.1:{port}/health");
    // No auth is configured, so the server ignores the Authorization header
    // the shared readiness probe attaches; readiness still proves liveness.
    wait_until_ready(&client, &url, "", "").await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[then("the health endpoint should answer over HTTPS without credentials")]
async fn answers_over_https_without_credentials(world: &mut UiWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = health_url(world, "https");
    // TLS only, no auth: the fixture credentials the readiness probe sends
    // are ignored; readiness proves the TLS listener is answering.
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

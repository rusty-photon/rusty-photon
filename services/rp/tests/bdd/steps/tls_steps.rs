//! BDD step definitions for rp's TLS connectivity

use cucumber::{given, then, when};

use bdd_infra::tls_auth::wait_until_ready;
use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

// ---------------------------------------------------------------------------
// rp TLS connectivity steps
// ---------------------------------------------------------------------------

#[given("generated TLS certificates")]
async fn generate_tls_certs(world: &mut RpWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("rp is configured with TLS enabled")]
const fn rp_configured_with_tls(_world: &mut RpWorld) {
    // Marker — config is built in the When step
}

#[when("rp is started")]
async fn rp_started_with_tls(world: &mut RpWorld) {
    let mut config = world.build_config();
    config["server"]["tls"] = world.pki().tls_block();

    let config_path = bdd_infra::rp_harness::write_temp_config_file("rp-tls-test", &config).await;

    world.rp = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    // Wait for healthy over HTTPS. rp has no auth configured here, so the
    // shared readiness probe carries no credentials.
    let client = world.pki().https_client();
    let port = world.rp.as_ref().unwrap().port();
    let url = format!("https://127.0.0.1:{port}/health");
    wait_until_ready(&client, &url, "", "").await;
}

#[then("the health endpoint should respond over HTTPS")]
async fn health_responds_https(world: &mut RpWorld) {
    let client = world.pki().https_client();
    let port = world.rp.as_ref().expect("rp not started").port();
    let url = format!("https://127.0.0.1:{port}/health");

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "Hello World, I am healthy!");
}

#[when("rp is started without TLS")]
async fn rp_started_without_tls(world: &mut RpWorld) {
    let config = world.build_config();
    let config_path = bdd_infra::rp_harness::write_temp_config_file("rp-notls-test", &config).await;

    world.rp = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

#[then("the health endpoint should respond over HTTP")]
async fn health_responds_http(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/health", world.rp_url());

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "Hello World, I am healthy!");
}

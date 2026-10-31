//! BDD step definitions for rp HTTP Basic Auth.
//! The auth scenarios build on the shared bdd-infra PKI + credentials
//! fixture (see `world.pki()`) and probe `/health` over HTTPS.

use cucumber::{given, then, when};

use bdd_infra::tls_auth::wait_until_ready;
use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

// ---------------------------------------------------------------------------
// Auth config steps
// ---------------------------------------------------------------------------

#[given("rp is configured with TLS and auth enabled")]
const fn rp_configured_with_tls_and_auth(_world: &mut RpWorld) {
    // Marker — config is built in the When step
}

#[when("rp is started with auth")]
async fn rp_started_with_auth(world: &mut RpWorld) {
    let mut config = world.build_config();
    config["server"]["tls"] = world.pki().tls_block();
    config["server"]["auth"] = world.pki().auth_block();

    let config_path = bdd_infra::rp_harness::write_temp_config_file("rp-auth-test", &config).await;

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;

    world.rp = Some(handle);

    // Wait for healthy over HTTPS with valid credentials.
    let pki = world.pki();
    let client = pki.https_client();
    let port = world.rp.as_ref().unwrap().port();
    let url = format!("https://127.0.0.1:{port}/health");
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;
}

// ---------------------------------------------------------------------------
// Auth validation steps
// ---------------------------------------------------------------------------

#[then("the health endpoint should respond with valid credentials")]
async fn health_responds_with_auth(world: &mut RpWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let port = world.rp.as_ref().expect("rp not started").port();
    let url = format!("https://127.0.0.1:{port}/health");

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some(pki.password()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[then("the health endpoint should reject wrong credentials with 401")]
async fn health_rejects_wrong_credentials(world: &mut RpWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let port = world.rp.as_ref().expect("rp not started").port();
    let url = format!("https://127.0.0.1:{port}/health");

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some("wrong-password"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the health endpoint should reject missing credentials with 401")]
async fn health_rejects_missing_credentials(world: &mut RpWorld) {
    let client = world.pki().https_client();
    let port = world.rp.as_ref().expect("rp not started").port();
    let url = format!("https://127.0.0.1:{port}/health");

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the rp 401 response should include a WWW-Authenticate header")]
async fn rp_401_includes_www_authenticate(world: &mut RpWorld) {
    let client = world.pki().https_client();
    let port = world.rp.as_ref().expect("rp not started").port();
    let url = format!("https://127.0.0.1:{port}/health");

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

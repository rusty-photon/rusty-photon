//! BDD step definitions for pa-scops-oag HTTP Basic Auth

use bdd_infra::tls_auth::{wait_until_ready, PkiFixture};
use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::world::ScopsWorld;
use bdd_infra::ServiceHandle;

fn pki(world: &ScopsWorld) -> &PkiFixture {
    world.pki.as_deref().expect("TLS certs not generated")
}

fn mock_config() -> pa_scops_oag::Config {
    pa_scops_oag::Config {
        serial: pa_scops_oag::SerialConfig {
            port: "/dev/mock".to_string(),
            polling_interval: std::time::Duration::from_mins(1),
            ..Default::default()
        },
        server: pa_scops_oag::AlpacaServerConfig::new(0),
        focuser: pa_scops_oag::FocuserConfig {
            enabled: true,
            ..Default::default()
        },
    }
}

#[given("pa-scops-oag is configured with TLS and auth enabled and mock serial")]
fn configured_with_tls_and_auth(world: &mut ScopsWorld) {
    // The TLS and auth blocks are spliced into the serialized JSON in the
    // When step.
    world.config = Some(mock_config());
}

#[given("pa-scops-oag is configured without auth and with mock serial")]
fn configured_without_auth(world: &mut ScopsWorld) {
    world.config = Some(mock_config());
}

#[when("pa-scops-oag is started with TLS and auth")]
async fn started_with_tls_and_auth(world: &mut ScopsWorld) {
    let config = world.config.as_ref().expect("config not set");
    let mut config_json: serde_json::Value =
        serde_json::to_value(config).expect("failed to serialize config");

    let pki = pki(world);
    config_json["server"]["tls"] = pki.tls_block();
    config_json["server"]["auth"] = pki.auth_block();

    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("scops_auth_config.json");
    std::fs::write(&config_path, config_json.to_string()).unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;
    world.focuser_handle = Some(handle);
}

#[when("pa-scops-oag is started without auth")]
async fn started_without_auth(world: &mut ScopsWorld) {
    let config = world.config.as_ref().expect("config not set");
    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("scops_noauth_config.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(config).unwrap()).unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;
    world.focuser_handle = Some(handle);
}

fn management_url(world: &ScopsWorld) -> String {
    let port = world
        .focuser_handle
        .as_ref()
        .expect("pa-scops-oag not started")
        .port();
    format!("https://localhost:{port}/management/v1/configureddevices")
}

#[then("the Alpaca management endpoint should respond with valid credentials")]
async fn responds_with_auth(world: &mut ScopsWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;
}

#[then("the Alpaca management endpoint should reject wrong credentials with 401")]
async fn rejects_wrong_credentials(world: &mut ScopsWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;
    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some("wrong-password"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the Alpaca management endpoint should reject missing credentials with 401")]
async fn rejects_missing_credentials(world: &mut ScopsWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the 401 response should include a WWW-Authenticate header")]
async fn response_includes_www_authenticate(world: &mut ScopsWorld) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
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

#[then("the Alpaca management endpoint should respond without credentials")]
async fn responds_without_credentials(world: &mut ScopsWorld) {
    let client = reqwest::Client::new();
    let port = world
        .focuser_handle
        .as_ref()
        .expect("pa-scops-oag not started")
        .port();
    let url = format!("http://127.0.0.1:{port}/management/v1/configureddevices");

    for _ in 0..60 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().as_u16() == 200 {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("Alpaca management endpoint did not respond without auth");
}

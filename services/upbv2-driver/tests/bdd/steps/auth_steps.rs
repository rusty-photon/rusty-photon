//! BDD step definitions for upbv2-driver HTTP Basic Auth

use bdd_infra::tls_auth::{wait_until_ready, PkiFixture};
use cucumber::{given, then, when};

use crate::steps::infrastructure::ServiceHandle;
use crate::world::{http_client, wait_for_http_200, Upbv2World};

fn pki(world: &Upbv2World) -> &PkiFixture {
    world.pki.as_deref().expect("TLS certs not generated")
}

fn management_url(world: &Upbv2World) -> String {
    let port = world.upbv2.as_ref().expect("upbv2-driver not started").port;
    format!("https://localhost:{port}/management/v1/configureddevices")
}

#[given("upbv2-driver is configured with TLS and auth enabled and mock serial")]
fn upbv2_configured_with_tls_and_auth(world: &mut Upbv2World) {
    let pki = pki(world);

    world.config = serde_json::json!({
        "serial": { "port": "/dev/mock", "baud_rate": 9600, "polling_interval": "60s", "timeout": "2s" },
        "server": pki.server_block(0),
        "switch": { "name": "Test Switch", "unique_id": "test-switch", "description": "Test", "enabled": true },
        "observingconditions": { "name": "Test OC", "unique_id": "test-oc", "description": "Test", "enabled": false }
    });
}

#[given("upbv2-driver is configured without auth and with mock serial")]
fn upbv2_configured_without_auth(world: &mut Upbv2World) {
    world.config = serde_json::json!({
        "serial": { "port": "/dev/mock", "baud_rate": 9600, "polling_interval": "60s", "timeout": "2s" },
        "server": { "port": 0 },
        "switch": { "name": "Test Switch", "unique_id": "test-switch", "description": "Test", "enabled": true },
        "observingconditions": { "name": "Test OC", "unique_id": "test-oc", "description": "Test", "enabled": false }
    });
}

#[when("upbv2-driver is started with TLS and auth")]
async fn upbv2_started_with_tls_and_auth(world: &mut Upbv2World) {
    let config_path = std::env::temp_dir()
        .join(format!(
            "upbv2-auth-test-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .to_string_lossy()
        .to_string();
    tokio::fs::write(
        &config_path,
        serde_json::to_string_pretty(&world.config).unwrap(),
    )
    .await
    .unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;

    world.base_url = Some(format!("https://localhost:{}", handle.port));
    world.upbv2 = Some(handle);
}

#[when("upbv2-driver is started without auth")]
async fn upbv2_started_without_auth(world: &mut Upbv2World) {
    let config_path = std::env::temp_dir()
        .join(format!(
            "upbv2-noauth-test-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .to_string_lossy()
        .to_string();
    tokio::fs::write(
        &config_path,
        serde_json::to_string_pretty(&world.config).unwrap(),
    )
    .await
    .unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;

    world.base_url = Some(format!("http://127.0.0.1:{}", handle.port));
    world.upbv2 = Some(handle);
}

#[then("the Alpaca management endpoint should respond with valid credentials")]
async fn alpaca_management_responds_with_auth(world: &mut Upbv2World) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;
}

#[then("the Alpaca management endpoint should reject wrong credentials with 401")]
async fn alpaca_rejects_wrong_credentials(world: &mut Upbv2World) {
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
async fn alpaca_rejects_missing_credentials(world: &mut Upbv2World) {
    let pki = pki(world);
    let client = pki.https_client();
    let url = management_url(world);
    wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the 401 response should include a WWW-Authenticate header")]
async fn response_includes_www_authenticate(world: &mut Upbv2World) {
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
async fn alpaca_responds_without_credentials(world: &mut Upbv2World) {
    let port = world.upbv2.as_ref().expect("upbv2-driver not started").port;
    let url = format!("http://127.0.0.1:{port}/management/v1/configureddevices");

    wait_for_http_200(http_client(), &url).await;
}

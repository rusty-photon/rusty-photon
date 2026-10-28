//! BDD step definitions for gemini-powerbox-adv3-driver TLS connectivity

use cucumber::{given, then, when};

use crate::steps::infrastructure::ServiceHandle;
use crate::world::{wait_for_http_200, GeminiWorld};

#[given("generated TLS certificates for gemini-powerbox-adv3-driver")]
async fn generate_tls_certs(world: &mut GeminiWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("gemini-powerbox-adv3-driver is configured with TLS enabled and mock serial")]
fn gemini_configured_with_tls(world: &mut GeminiWorld) {
    let pki = world.pki.as_ref().expect("TLS certs not generated");

    world.config = serde_json::json!({
        "serial": { "port": "/dev/mock", "polling_interval": "60s", "timeout": "2s" },
        "server": {
            "port": 0,
            "tls": pki.tls_block()
        },
        "switch": { "name": "Test Switch", "unique_id": "test-switch", "description": "Test", "enabled": true },
        "observingconditions": { "name": "Test OC", "unique_id": "test-oc", "description": "Test", "enabled": false }
    });
}

#[when("gemini-powerbox-adv3-driver is started with TLS")]
async fn gemini_started_with_tls(world: &mut GeminiWorld) {
    let config_path = std::env::temp_dir()
        .join(format!(
            "gemini-tls-test-{}.json",
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

    world.base_url = Some(format!("https://localhost:{}", handle.port()));
    world.gemini = Some(handle);
}

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn alpaca_management_responds_https(world: &mut GeminiWorld) {
    let client = world
        .pki
        .as_ref()
        .expect("TLS certs not generated")
        .https_client();
    let port = world
        .gemini
        .as_ref()
        .expect("gemini-powerbox-adv3-driver not started")
        .port();
    let url = format!("https://localhost:{port}/management/v1/configureddevices");

    wait_for_http_200(&client, &url).await;
}

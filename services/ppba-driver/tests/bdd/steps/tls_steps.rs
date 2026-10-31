//! BDD step definitions for ppba-driver TLS connectivity

use cucumber::{given, then, when};

use crate::steps::infrastructure::ServiceHandle;
use crate::world::PpbaWorld;

#[given("generated TLS certificates for ppba-driver")]
async fn generate_tls_certs(world: &mut PpbaWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("ppba-driver is configured with TLS enabled and mock serial")]
fn ppba_configured_with_tls(world: &mut PpbaWorld) {
    let pki = world.pki.as_ref().expect("TLS certs not generated");

    world.config = serde_json::json!({
        "serial": { "port": "/dev/mock", "baud_rate": 9600, "polling_interval": "60s", "timeout": "2s" },
        "server": {
            "port": 0,
            "tls": pki.tls_block()
        },
        "switch": { "name": "Test Switch", "unique_id": "test-switch", "description": "Test", "enabled": true },
        "observingconditions": { "name": "Test OC", "unique_id": "test-oc", "description": "Test", "enabled": false }
    });
}

#[when("ppba-driver is started with TLS")]
async fn ppba_started_with_tls(world: &mut PpbaWorld) {
    let config_path = std::env::temp_dir()
        .join(format!(
            "ppba-tls-test-{}.json",
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

    world.base_url = Some(format!("https://127.0.0.1:{}", handle.port()));
    world.ppba = Some(handle);
}

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn alpaca_management_responds_https(world: &mut PpbaWorld) {
    let pki = world.pki.as_ref().expect("TLS certs not generated");
    let client = pki.https_client();
    let port = world.ppba.as_ref().expect("ppba-driver not started").port();
    let url = format!("https://127.0.0.1:{port}/management/v1/configureddevices");

    // No auth is configured in this scenario, so the probe carries no
    // credentials; a 200 proves the endpoint answers over HTTPS.
    bdd_infra::tls_auth::wait_until_ready(&client, &url, "", "").await;
}

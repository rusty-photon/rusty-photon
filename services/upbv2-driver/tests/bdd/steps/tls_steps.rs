//! BDD step definitions for upbv2-driver TLS connectivity

use cucumber::{given, then, when};

use crate::steps::infrastructure::ServiceHandle;
use crate::world::{wait_for_http_200, Upbv2World};

#[given("generated TLS certificates for upbv2-driver")]
async fn generate_tls_certs(world: &mut Upbv2World) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("upbv2-driver is configured with TLS enabled and mock serial")]
fn upbv2_configured_with_tls(world: &mut Upbv2World) {
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

#[when("upbv2-driver is started with TLS")]
async fn upbv2_started_with_tls(world: &mut Upbv2World) {
    let config_path = std::env::temp_dir()
        .join(format!(
            "upbv2-tls-test-{}.json",
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

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn alpaca_management_responds_https(world: &mut Upbv2World) {
    let client = world
        .pki
        .as_ref()
        .expect("TLS certs not generated")
        .https_client();
    let port = world.upbv2.as_ref().expect("upbv2-driver not started").port;
    let url = format!("https://localhost:{port}/management/v1/configureddevices");

    wait_for_http_200(&client, &url).await;
}

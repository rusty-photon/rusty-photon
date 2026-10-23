//! BDD step definitions for pa-scops-oag TLS connectivity

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::world::ScopsWorld;
use bdd_infra::ServiceHandle;

#[given("generated TLS certificates for pa-scops-oag")]
async fn generate_tls_certs(world: &mut ScopsWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("pa-scops-oag is configured with TLS enabled and mock serial")]
fn configured_with_tls(world: &mut ScopsWorld) {
    // The TLS block is spliced into the serialized JSON in the When step.
    world.config = Some(pa_scops_oag::Config {
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
    });
}

#[when("pa-scops-oag is started with TLS")]
async fn started_with_tls(world: &mut ScopsWorld) {
    let config = world.config.as_ref().expect("config not set");
    let mut config_json: serde_json::Value =
        serde_json::to_value(config).expect("failed to serialize config");
    config_json["server"]["tls"] = world
        .pki
        .as_ref()
        .expect("TLS certs not generated")
        .tls_block();

    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("scops_tls_config.json");
    std::fs::write(&config_path, config_json.to_string()).unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;
    world.focuser_handle = Some(handle);
}

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn management_responds_https(world: &mut ScopsWorld) {
    let client = world
        .pki
        .as_ref()
        .expect("TLS certs not generated")
        .https_client();
    let port = world
        .focuser_handle
        .as_ref()
        .expect("pa-scops-oag not started")
        .port();
    let url = format!("https://localhost:{port}/management/v1/configureddevices");

    for _ in 0..60 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().as_u16() == 200 {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("Alpaca management endpoint did not respond over HTTPS");
}

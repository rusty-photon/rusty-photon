//! BDD step definitions for qhy-focuser TLS connectivity

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::world::QhyFocuserWorld;
use bdd_infra::ServiceHandle;

#[given("generated TLS certificates for qhy-focuser")]
async fn generate_tls_certs(world: &mut QhyFocuserWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("qhy-focuser is configured with TLS enabled and mock serial")]
fn qhy_configured_with_tls(world: &mut QhyFocuserWorld) {
    // The TLS block is spliced into the serialized JSON in the When step.
    world.config = Some(qhy_focuser::Config {
        serial: qhy_focuser::SerialConfig {
            port: "/dev/mock".to_string(),
            polling_interval: std::time::Duration::from_mins(1),
            ..Default::default()
        },
        server: qhy_focuser::AlpacaServerConfig::new(0),
        focuser: qhy_focuser::FocuserConfig {
            enabled: true,
            ..Default::default()
        },
    });
}

#[when("qhy-focuser is started with TLS")]
async fn qhy_started_with_tls(world: &mut QhyFocuserWorld) {
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
    let config_path = dir.path().join("qhy_tls_config.json");
    std::fs::write(&config_path, config_json.to_string()).unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;

    world.focuser_handle = Some(handle);
}

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn alpaca_management_responds_https(world: &mut QhyFocuserWorld) {
    let pki = world.pki.as_ref().expect("TLS certs not generated");
    let client = pki.https_client();
    let port = world
        .focuser_handle
        .as_ref()
        .expect("qhy-focuser not started")
        .port();
    let url = format!("https://127.0.0.1:{port}/management/v1/configureddevices");

    // No auth is configured in this scenario, so the probe carries no
    // credentials; a 200 proves the endpoint answers over HTTPS.
    bdd_infra::tls_auth::wait_until_ready(&client, &url, "", "").await;
}

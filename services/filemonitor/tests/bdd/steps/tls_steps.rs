//! BDD step definitions for filemonitor TLS connectivity

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::steps::infrastructure::ServiceHandle;
use crate::world::FilemonitorWorld;

#[given("generated TLS certificates for filemonitor")]
async fn generate_tls_certs(world: &mut FilemonitorWorld) {
    world.pki = Some(bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await);
}

#[given("filemonitor is configured with TLS enabled")]
const fn filemonitor_configured_with_tls(_world: &mut FilemonitorWorld) {
    // Marker — config is built in the When step
}

#[when("filemonitor is started with TLS")]
async fn filemonitor_started_with_tls(world: &mut FilemonitorWorld) {
    let mut config = world.build_config_json();
    config["server"]["tls"] = world.pki().tls_block();

    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("filemonitor_tls_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;

    world.filemonitor = Some(handle);
}

#[then("the Alpaca management endpoint should respond over HTTPS")]
async fn alpaca_management_responds_https(world: &mut FilemonitorWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let port = world
        .filemonitor
        .as_ref()
        .expect("filemonitor not started")
        .port();
    let url = format!("https://127.0.0.1:{port}/management/v1/configureddevices");

    // No auth is configured in this scenario, so the probe carries no
    // credentials; a 200 proves the endpoint answers over HTTPS.
    bdd_infra::tls_auth::wait_until_ready(&client, &url, "", "").await;
}

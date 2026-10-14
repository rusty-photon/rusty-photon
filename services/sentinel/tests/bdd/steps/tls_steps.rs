//! BDD step definitions for sentinel TLS connectivity

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::world::SentinelWorld;

/// Guards the extra filemonitor certificate below. The shared fixture is
/// generated once per suite, so its companion cert is signed once too —
/// without this, concurrent scenarios would rewrite the same file while
/// another scenario's filemonitor is reading it.
static FILEMONITOR_CERT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

#[given("generated TLS certificates for sentinel")]
async fn generate_tls_certs(world: &mut SentinelWorld) {
    let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
    // The cross-service scenarios spawn a TLS-enabled filemonitor whose
    // certificate must chain to the same CA sentinel trusts. The fixture
    // generates one service cert, so sign the filemonitor cert with the
    // fixture's CA directly (ca-key.pem sits next to ca.pem).
    let for_cert = std::sync::Arc::clone(&pki);
    FILEMONITOR_CERT
        .get_or_init(|| async move {
            bdd_infra::off_loop(move || {
                let ca_pem = std::fs::read_to_string(for_cert.ca_path()).unwrap();
                let ca_key =
                    std::fs::read_to_string(for_cert.ca_path().with_file_name("ca-key.pem"))
                        .unwrap();
                let certs_dir = for_cert
                    .cert_path()
                    .parent()
                    .expect("cert path has no parent")
                    .to_path_buf();
                rusty_photon_tls::test_cert::generate_service_cert(
                    &ca_pem,
                    &ca_key,
                    "filemonitor",
                    &certs_dir,
                )
                .unwrap();
            })
            .await;
        })
        .await;
    world.pki = Some(pki);
}

#[given("sentinel is configured with dashboard TLS enabled")]
const fn sentinel_configured_dashboard_tls(_world: &mut SentinelWorld) {
    // Marker — config is built in When step
}

#[when("sentinel is started")]
async fn sentinel_started_with_tls(world: &mut SentinelWorld) {
    let mut config = world.build_sentinel_config();
    config["server"]["tls"] = world.pki().tls_block();

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("sentinel_tls_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap())
            .await;

    world.sentinel = Some(handle);
}

#[then("the dashboard health endpoint should respond over HTTPS")]
async fn dashboard_health_https(world: &mut SentinelWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let port = world.sentinel.as_ref().expect("sentinel not started").port;
    let url = format!("https://localhost:{port}/health");

    // No auth is configured in this scenario, so the credentials the probe
    // sends are ignored; a 200 proves the dashboard answers over HTTPS.
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;
}

// --- Cross-service TLS scenario ---

#[given(expr = "filemonitor is running with TLS enabled and a contains rule {string} as safe")]
async fn filemonitor_running_with_tls(world: &mut SentinelWorld, pattern: String) {
    world.fm_rules.push(serde_json::json!({
        "type": "contains",
        "pattern": pattern,
        "safe": true
    }));

    let mut config = world.build_filemonitor_config();
    config["server"]["tls"] = world.fm_tls_block();

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("filemonitor_tls_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start("filemonitor", config_path.to_str().unwrap()).await;

    world.filemonitor = Some(handle);
}

#[given("sentinel is configured with CA certificate and HTTPS monitor scheme")]
fn sentinel_configured_with_ca(world: &mut SentinelWorld) {
    let fm = world.filemonitor.as_ref().expect("filemonitor not started");

    let monitor = serde_json::json!({
        "type": "alpaca_safety_monitor",
        "name": "Roof Monitor",
        "host": "localhost",
        "port": fm.port,
        "device_number": 0,
        "polling_interval": "1s",
        "scheme": "https"
    });
    world.sentinel_monitors.push(monitor);

    // The CA trust itself (`ca_cert`) is wired into the sentinel config by
    // the "sentinel is running with CA trust" step.
    world.sentinel_monitor_name = "Roof Monitor".to_string();
}

#[given("sentinel is running with CA trust")]
async fn sentinel_running_with_ca(world: &mut SentinelWorld) {
    let mut config = world.build_sentinel_config();
    config["ca_cert"] = serde_json::json!(world.pki().ca_path().to_string_lossy());

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("sentinel_ca_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap())
            .await;

    world.sentinel = Some(handle);
}

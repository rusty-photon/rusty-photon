//! BDD step definitions for sentinel dashboard HTTP Basic Auth

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::world::SentinelWorld;

/// The dashboard health URL of the running sentinel over HTTPS.
fn health_url(world: &SentinelWorld) -> String {
    let port = world
        .sentinel
        .as_ref()
        .expect("sentinel not started")
        .port();
    format!("https://127.0.0.1:{port}/health")
}

#[given("sentinel is configured with dashboard TLS and auth enabled")]
const fn sentinel_configured_with_dashboard_auth(_world: &mut SentinelWorld) {
    // Marker — config is built in the When step
}

#[when("sentinel is started with dashboard auth")]
async fn sentinel_started_with_dashboard_auth(world: &mut SentinelWorld) {
    let mut config = world.build_sentinel_config();
    config["server"] = world.pki().server_block(0);

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("sentinel_auth_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap())
            .await;

    world.sentinel = Some(handle);
}

#[when("sentinel is started without dashboard auth")]
async fn sentinel_started_without_dashboard_auth(world: &mut SentinelWorld) {
    let config = world.build_sentinel_config();

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("sentinel_noauth_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap())
            .await;

    world.sentinel = Some(handle);
}

#[then("the dashboard health endpoint should respond with valid credentials")]
async fn dashboard_health_responds_with_auth(world: &mut SentinelWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = health_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some(pki.password()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[then("the dashboard health endpoint should reject wrong credentials with 401")]
async fn dashboard_rejects_wrong_credentials(world: &mut SentinelWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = health_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some("wrong-password"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the dashboard health endpoint should reject missing credentials with 401")]
async fn dashboard_rejects_missing_credentials(world: &mut SentinelWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = health_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the dashboard 401 response should include a WWW-Authenticate header")]
async fn dashboard_401_includes_www_authenticate(world: &mut SentinelWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = health_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

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

#[then("the dashboard health endpoint should respond without credentials")]
async fn dashboard_responds_without_credentials(world: &mut SentinelWorld) {
    let client = reqwest::Client::new();
    let port = world
        .sentinel
        .as_ref()
        .expect("sentinel not started")
        .port();
    let url = format!("http://127.0.0.1:{port}/health");

    let mut ok = false;
    for _ in 0..60 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().as_u16() == 200 {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(ok, "Dashboard health endpoint did not respond without auth");
}

// --- Cross-service client-side auth scenario ---

#[given(
    expr = "filemonitor is running with TLS and auth enabled and a contains rule {string} as safe"
)]
async fn filemonitor_running_with_tls_and_auth(world: &mut SentinelWorld, pattern: String) {
    world.fm_rules.push(serde_json::json!({
        "type": "contains",
        "pattern": pattern,
        "safe": true
    }));

    // Create a temp file with safe content so filemonitor has something to monitor
    world.create_temp_file(&pattern);

    let mut config = world.build_filemonitor_config();
    config["server"]["tls"] = world.fm_tls_block();
    config["server"]["auth"] = world.pki().auth_block();

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("filemonitor_auth_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start("filemonitor", config_path.to_str().unwrap()).await;

    world.filemonitor = Some(handle);
}

#[given("sentinel is configured to monitor the auth-enabled filemonitor")]
fn sentinel_configured_with_auth_monitor(world: &mut SentinelWorld) {
    let fm = world.filemonitor.as_ref().expect("filemonitor not started");
    let pki = world.pki.as_ref().expect("TLS certs not generated");

    let monitor = serde_json::json!({
        "type": "alpaca_safety_monitor",
        "name": "Auth Monitor",
        "host": "127.0.0.1",
        "port": fm.port(),
        "device_number": 0,
        "polling_interval": "1s",
        "scheme": "https",
        "auth": {
            "username": pki.username(),
            "password": pki.password()
        }
    });
    world.sentinel_monitors.push(monitor);

    world.sentinel_monitor_name = "Auth Monitor".to_string();
}

#[when("sentinel is started with CA trust")]
async fn sentinel_started_with_ca_trust(world: &mut SentinelWorld) {
    let mut config = world.build_sentinel_config();
    config["ca_cert"] = serde_json::json!(world.pki().ca_path().to_string_lossy());

    let temp_dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = temp_dir.path().join("sentinel_auth_monitor_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();

    let handle =
        bdd_infra::ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap())
            .await;

    world.sentinel = Some(handle);
}

#[then("sentinel should successfully poll the filemonitor")]
async fn sentinel_polls_successfully(world: &mut SentinelWorld) {
    world.wait_for_poll().await;

    let statuses = world.get_status().await;
    assert!(!statuses.is_empty(), "no monitor statuses returned");

    let monitor = statuses
        .iter()
        .find(|m| m.get("name").and_then(|n| n.as_str()) == Some("Auth Monitor"))
        .expect("Auth Monitor not found in status");

    let state = monitor
        .get("state")
        .and_then(|s| s.as_str())
        .expect("state field missing");
    assert_eq!(
        state, "Safe",
        "expected Safe state from auth-enabled filemonitor"
    );
}

//! BDD step definitions for filemonitor HTTP Basic Auth

use cucumber::{given, then, when};
use tempfile::TempDir;

use crate::steps::infrastructure::ServiceHandle;
use crate::world::{FilemonitorWorld, ParsingRuleConfig};

/// The Alpaca management URL of the running filemonitor over HTTPS.
fn management_url(world: &FilemonitorWorld) -> String {
    let port = world
        .filemonitor
        .as_ref()
        .expect("filemonitor not started")
        .port();
    format!("https://localhost:{port}/management/v1/configureddevices")
}

#[given(expr = "a monitored file containing {string}")]
fn monitored_file_containing(world: &mut FilemonitorWorld, content: String) {
    world.create_temp_file(&content);
}

#[given(
    expr = "filemonitor is configured with TLS and auth enabled and a contains rule {string} as safe"
)]
fn filemonitor_configured_with_tls_auth_and_rule(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "contains".to_string(),
        pattern,
        safe: true,
    });

    let mut config = world.build_config_json();
    config["server"]["tls"] = world.pki().tls_block();
    config["server"]["auth"] = world.pki().auth_block();

    // Store config in temp dir for the When step to pick up
    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("filemonitor_auth_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();
}

#[given(expr = "filemonitor is configured without auth and a contains rule {string} as safe")]
fn filemonitor_configured_without_auth_and_rule(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "contains".to_string(),
        pattern,
        safe: true,
    });

    let config = world.build_config_json();

    let dir = world
        .temp_dir
        .get_or_insert_with(|| TempDir::new().unwrap());
    let config_path = dir.path().join("filemonitor_noauth_config.json");
    std::fs::write(&config_path, config.to_string()).unwrap();
}

#[when("filemonitor is started with TLS and auth")]
async fn filemonitor_started_with_tls_and_auth(world: &mut FilemonitorWorld) {
    let dir = world.temp_dir.as_ref().expect("temp dir not created");
    let config_path = dir.path().join("filemonitor_auth_config.json");

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;

    world.filemonitor = Some(handle);
}

#[when("filemonitor is started without auth")]
async fn filemonitor_started_without_auth(world: &mut FilemonitorWorld) {
    let dir = world.temp_dir.as_ref().expect("temp dir not created");
    let config_path = dir.path().join("filemonitor_noauth_config.json");

    let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), config_path.to_str().unwrap()).await;

    world.filemonitor = Some(handle);
}

#[then("the Alpaca management endpoint should respond with valid credentials")]
async fn alpaca_management_responds_with_auth(world: &mut FilemonitorWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = management_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some(pki.password()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[then("the Alpaca management endpoint should reject wrong credentials with 401")]
async fn alpaca_rejects_wrong_credentials(world: &mut FilemonitorWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = management_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client
        .get(&url)
        .basic_auth(pki.username(), Some("wrong-password"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the Alpaca management endpoint should reject missing credentials with 401")]
async fn alpaca_rejects_missing_credentials(world: &mut FilemonitorWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = management_url(world);
    bdd_infra::tls_auth::wait_until_ready(&client, &url, pki.username(), pki.password()).await;

    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[then("the 401 response should include a WWW-Authenticate header")]
async fn response_includes_www_authenticate(world: &mut FilemonitorWorld) {
    let pki = world.pki();
    let client = pki.https_client();
    let url = management_url(world);
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

#[then("the Alpaca management endpoint should respond without credentials")]
async fn alpaca_responds_without_credentials(world: &mut FilemonitorWorld) {
    let client = reqwest::Client::new();
    let port = world
        .filemonitor
        .as_ref()
        .expect("filemonitor not started")
        .port();
    let url = format!("http://127.0.0.1:{port}/management/v1/configureddevices");

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
    assert!(
        ok,
        "Alpaca management endpoint did not respond without auth"
    );
}

//! Steps for the --fix provisioning pass: the pki tree, the observatory
//! credential and its distribution, and the never-overwrite contract.

use cucumber::{given, then};
use serde_json::{json, Value};

use crate::world::DoctorWorld;

/// The service name a config file name stands for.
fn service_of(config_file: &str) -> &str {
    config_file
        .strip_suffix(".json")
        .unwrap_or_else(|| panic!("not a config file name: {config_file}"))
}

fn default_port(service: &str) -> u16 {
    doctor::catalog::entry(service)
        .unwrap_or_else(|| panic!("{service} is not in the catalog"))
        .default_port
}

// ---------------------------------------------------------------------------
// Givens
// ---------------------------------------------------------------------------

#[given(expr = "a config file {string} whose auth hash is of the password {string}")]
fn config_with_auth_hash(world: &mut DoctorWorld, name: String, password: String) {
    let service = service_of(&name);
    let hash = rp_auth::credentials::hash_password(&password).expect("hashing succeeds");
    let content = json!({
        "server": {
            "port": default_port(service),
            "auth": { "username": "observatory", "password_hash": hash }
        }
    });
    world.write_config(&name, &serde_json::to_string_pretty(&content).unwrap());
}

#[given(expr = "a config file {string} whose client auth block carries the password {string}")]
fn config_with_client_password(world: &mut DoctorWorld, name: String, password: String) {
    let service = service_of(&name);
    assert_eq!(
        service, "sentinel",
        "the client auth block given is sentinel-shaped"
    );
    let content = json!({
        "server": { "port": default_port(service) },
        "service_auth": { "username": "observatory", "password": password }
    });
    world.write_config(&name, &serde_json::to_string_pretty(&content).unwrap());
}

#[given(expr = "an acme.json for the domain {string}")]
fn acme_config_staged(world: &mut DoctorWorld, domain: String) {
    world.stage_acme_config(&domain);
}

#[given("the acme.json is amended to use the staging endpoint")]
fn acme_config_amended_to_staging(world: &mut DoctorWorld) {
    let path = world.config_dir().join("acme.json");
    let content = std::fs::read_to_string(&path).expect("stage acme.json first");
    let mut value: Value = serde_json::from_str(&content).expect("acme.json is valid JSON");
    value["staging"] = Value::Bool(true);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&value).expect("acme.json serializes"),
    )
    .expect("acme.json");
}

#[given(expr = "an ACME wildcard certificate pair expiring in {int} days")]
fn acme_pair_staged(world: &mut DoctorWorld, days: i64) {
    let acme = std::fs::read_to_string(world.config_dir().join("acme.json"))
        .expect("stage acme.json first — the pair's domain comes from it");
    let domain = serde_json::from_str::<Value>(&acme)
        .ok()
        .and_then(|v| v["domain"].as_str().map(String::from))
        .expect("acme.json carries a domain");
    world.stage_acme_pair(
        &domain,
        time::OffsetDateTime::now_utc() + time::Duration::days(days),
    );
}

#[given("doctor has already run with --fix")]
async fn doctor_ran_with_fix(world: &mut DoctorWorld) {
    world.run_doctor_args(true, true).await;
    let output = world.output.as_ref().expect("doctor ran");
    assert_ne!(
        output.status.code(),
        Some(2),
        "priming --fix run could not execute: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    world.snapshot_pki();
}

// ---------------------------------------------------------------------------
// pki tree assertions
// ---------------------------------------------------------------------------

#[then(expr = "the pki file {string} exists")]
fn pki_file_exists(world: &mut DoctorWorld, name: String) {
    let path = world.pki_dir().join(&name);
    assert!(path.is_file(), "expected pki file at {}", path.display());
}

#[then(expr = "the pki file {string} does not exist")]
fn pki_file_absent(world: &mut DoctorWorld, name: String) {
    let path = world.pki_dir().join(&name);
    assert!(!path.exists(), "unexpected pki file at {}", path.display());
}

#[then("no pki directory exists")]
fn no_pki_dir(world: &mut DoctorWorld) {
    let dir = world.pki_dir();
    assert!(
        !dir.exists(),
        "unexpected pki directory at {}",
        dir.display()
    );
}

#[then(expr = "the pki file {string} is unchanged")]
fn pki_file_unchanged(world: &mut DoctorWorld, name: String) {
    let staged = world
        .pki_staged
        .get(&name)
        .unwrap_or_else(|| panic!("{name} was not snapshotted — missing a priming given?"));
    let current = std::fs::read(world.pki_dir().join(&name)).expect("pki file readable");
    assert_eq!(staged, &current, "{name} changed across runs");
}

#[then(expr = "the pki file {string} has changed")]
fn pki_file_changed(world: &mut DoctorWorld, name: String) {
    let staged = world
        .pki_staged
        .get(&name)
        .unwrap_or_else(|| panic!("{name} was not snapshotted — missing a priming given?"));
    let current = std::fs::read(world.pki_dir().join(&name)).expect("pki file readable");
    assert_ne!(staged, &current, "{name} should differ after this run");
}

// ---------------------------------------------------------------------------
// Config-content assertions
// ---------------------------------------------------------------------------

#[then(expr = "the config file {string} points its tls block at the pki pair for {string}")]
fn config_tls_points_at_pki(world: &mut DoctorWorld, name: String, service: String) {
    let value = world.config_value(&name);
    let cert = value
        .pointer("/server/tls/cert")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{name} has no /server/tls/cert: {value}"));
    let key = value
        .pointer("/server/tls/key")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{name} has no /server/tls/key: {value}"));
    let pki = world.pki_dir();
    assert_eq!(
        std::path::Path::new(cert),
        pki.join(format!("{service}.pem")),
        "cert path in {name}"
    );
    assert_eq!(
        std::path::Path::new(key),
        pki.join(format!("{service}-key.pem")),
        "key path in {name}"
    );
}

#[then(expr = "the auth hash at {string} in {string} verifies against the credential file")]
fn auth_hash_verifies(world: &mut DoctorWorld, pointer: String, name: String) {
    let value = world.config_value(&name);
    let hash = value
        .pointer(&pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{name} has no hash at {pointer}: {value}"));
    let credential = world.credential();
    assert!(
        rp_auth::credentials::verify_password(&credential, hash),
        "credential does not verify against the hash at {pointer} in {name}"
    );
}

// ---------------------------------------------------------------------------
// Sentinel client-block assertions
// ---------------------------------------------------------------------------

#[then(expr = "the sentinel client auth block carries username {string}")]
fn sentinel_client_username(world: &mut DoctorWorld, expected: String) {
    let value = world.config_value("sentinel.json");
    assert_eq!(
        value.pointer("/service_auth/username"),
        Some(&Value::from(expected.as_str())),
        "sentinel.json service_auth: {value}"
    );
}

#[then(expr = "the sentinel client auth password verifies against the auth hash in {string}")]
fn sentinel_client_password_verifies(world: &mut DoctorWorld, name: String) {
    let sentinel = world.config_value("sentinel.json");
    let password = sentinel
        .pointer("/service_auth/password")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("sentinel.json has no service_auth password: {sentinel}"));
    let target = world.config_value(&name);
    let hash = target
        .pointer("/server/auth/password_hash")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{name} has no server.auth hash: {target}"));
    assert!(
        rp_auth::credentials::verify_password(password, hash),
        "sentinel's client password does not verify against {name}'s hash"
    );
}

#[then(expr = "the sentinel client CA path points at the pki file {string}")]
fn sentinel_client_ca_path(world: &mut DoctorWorld, name: String) {
    let value = world.config_value("sentinel.json");
    let ca = value
        .pointer("/ca_cert")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("sentinel.json has no ca_cert: {value}"));
    assert_eq!(std::path::Path::new(ca), world.pki_dir().join(&name));
}

// ---------------------------------------------------------------------------
// Provisioning-action assertions (fixes_applied entries with non-pointer ops)
// ---------------------------------------------------------------------------

fn applied_ops(world: &DoctorWorld) -> Vec<Value> {
    world
        .report()
        .get("fixes_applied")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default()
}

#[then(expr = "the report records an applied {string} provisioning action")]
fn records_provisioning_action(world: &mut DoctorWorld, op: String) {
    let applied = applied_ops(world);
    assert!(
        applied.iter().any(|f| f["op"]["op"] == op.as_str()),
        "no applied {op} in: {applied:?}"
    );
}

#[then(expr = "the report records an applied {string} provisioning action for service {string}")]
fn records_provisioning_action_for(world: &mut DoctorWorld, op: String, service: String) {
    let applied = applied_ops(world);
    assert!(
        applied
            .iter()
            .any(|f| f["op"]["op"] == op.as_str() && f["op"]["service"] == service.as_str()),
        "no applied {op} for {service} in: {applied:?}"
    );
}

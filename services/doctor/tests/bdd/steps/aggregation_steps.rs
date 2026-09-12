//! Steps for the aggregation scenarios (docs/services/doctor.md
//! §Aggregation): stub management endpoints for the active-unit HTTP probe
//! and stub per-service binaries for the inactive-unit shell-out probe.

use std::path::{Path, PathBuf};

use axum::http::{HeaderMap, StatusCode};
use cucumber::{given, then};

use crate::world::DoctorWorld;

/// The plaintext the "staged observatory credential" given writes to
/// `pki/credential`, and the Basic header the authenticated stub demands.
const STUB_PASSWORD: &str = "stub-password";
/// `base64("observatory:stub-password")` — what reqwest's `basic_auth`
/// produces for the staged credential.
const STUB_BASIC_HEADER: &str = "Basic b2JzZXJ2YXRvcnk6c3R1Yi1wYXNzd29yZA==";

/// Whole-request bound on the IPv6 ownership guard below. A loopback
/// listener answers in microseconds, so this only decides how long the
/// suite waits on one that accepts the connection and never replies —
/// a plausible squatter, and the shape doctor's own probe is bounded
/// against (`aggregate::HTTP_TIMEOUT`, sized for a loaded CI host).
/// Unbounded, that wait would be the whole target's: every scenario
/// shares one poll loop (docs/skills/testing.md §5.7).
const IPV6_GUARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

const DEVICES_JSON: &str = r#"{ "Value": [
    { "DeviceName": "Stub Camera", "DeviceType": "Camera", "DeviceNumber": 0 },
    { "DeviceName": "Stub Wheel", "DeviceType": "FilterWheel", "DeviceNumber": 1 }
] }"#;

/// How the stub management endpoint answers.
#[derive(Clone)]
enum StubBehavior {
    Devices,
    RequireAuth,
    ServerError,
    BadPayload,
    /// One Telescope device reporting this `UniqueID` — the fake-mount
    /// probe's subject (`joins.fake-mount`, aggregate.rs).
    UniqueId(String),
}

async fn management_response(behavior: StubBehavior, headers: HeaderMap) -> (StatusCode, String) {
    match behavior {
        StubBehavior::RequireAuth => {
            let authorized = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == STUB_BASIC_HEADER);
            if !authorized {
                return (StatusCode::UNAUTHORIZED, String::new());
            }
            (StatusCode::OK, DEVICES_JSON.to_string())
        }
        StubBehavior::Devices => (StatusCode::OK, DEVICES_JSON.to_string()),
        StubBehavior::ServerError => (StatusCode::INTERNAL_SERVER_ERROR, String::new()),
        StubBehavior::BadPayload => (
            StatusCode::OK,
            "this is not the management JSON".to_string(),
        ),
        StubBehavior::UniqueId(unique_id) => (
            StatusCode::OK,
            serde_json::json!({ "Value": [ {
                "DeviceName": "Stub Telescope", "DeviceType": "Telescope",
                "DeviceNumber": 0, "UniqueID": unique_id } ] })
            .to_string(),
        ),
    }
}

fn stub_router(behavior: StubBehavior) -> axum::Router {
    axum::Router::new().route(
        "/management/v1/configureddevices",
        axum::routing::get(move |headers: HeaderMap| {
            let behavior = behavior.clone();
            management_response(behavior, headers)
        }),
    )
}

/// Start a plain-HTTP stub management endpoint; the bound port lands in
/// `world.stub_port` for the config-staging steps. Served on both
/// loopback address families, because the probe dials `localhost`
/// (`crate::loopback`).
async fn start_http_stub(world: &mut DoctorWorld, behavior: StubBehavior) {
    let (port, listeners) = crate::loopback::bind_loopback_pair().await;
    world.stub_port = Some(port);
    for listener in listeners {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        world.stub_shutdowns.push(shutdown_tx);
        let router = stub_router(behavior.clone());
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    shutdown_rx.await.ok();
                })
                .await
                .expect("stub endpoint serve");
        });
    }
}

#[given("a stub management endpoint serving two configured devices")]
async fn stub_endpoint(world: &mut DoctorWorld) {
    start_http_stub(world, StubBehavior::Devices).await;
}

#[given("a stub management endpoint that requires authentication")]
async fn stub_endpoint_authenticated(world: &mut DoctorWorld) {
    start_http_stub(world, StubBehavior::RequireAuth).await;
}

#[given(expr = "a stub management endpoint serving a device with UniqueID {string}")]
async fn stub_endpoint_unique_id(world: &mut DoctorWorld, unique_id: String) {
    start_http_stub(world, StubBehavior::UniqueId(unique_id)).await;
}

/// rp's mount wired at the stub — the fake-mount probe's subject. The
/// stub's ephemeral port matches no catalog service, so the static
/// loopback join resolves to nothing and the `UniqueID` probe leg runs.
#[given(
    expr = "a config file {string} whose equipment.mount.alpaca_url points at the stub endpoint"
)]
fn config_mount_at_stub(world: &mut DoctorWorld, file: String) {
    let port = world.stub_port.expect("no stub endpoint started yet");
    world.write_config(
        &file,
        &format!(
            r#"{{ "server": {{ "port": 11115 }},
                 "equipment": {{ "mount": {{ "alpaca_url": "http://127.0.0.1:{port}" }} }} }}"#
        ),
    );
}

#[given("a stub management endpoint answering HTTP 500")]
async fn stub_endpoint_server_error(world: &mut DoctorWorld) {
    start_http_stub(world, StubBehavior::ServerError).await;
}

#[given("a stub management endpoint whose payload is not management JSON")]
async fn stub_endpoint_bad_payload(world: &mut DoctorWorld) {
    start_http_stub(world, StubBehavior::BadPayload).await;
}

/// The guard on what the probe's `localhost` reaches: a stub that owns
/// only one address family leaves the other half of its port for any
/// process in the run to take, and the probe then asserts against a
/// stranger's answer (`crate::loopback`).
///
/// The question is asked of the address, not of the port's
/// availability. A failed bind on `[::1]` would prove only that
/// *someone* holds it — true of the foreign listener this guard exists
/// to exclude, so a scenario resting on that could pass in exactly the
/// case it is meant to catch. Reading the stub's own payload back off
/// `[::1]` is what names the holder.
///
/// A host with no IPv6 loopback has nothing to ask: the stub is
/// IPv4-only there by design, and nothing else can hold `[::1]` either.
#[then("the stub itself answers on the IPv6 loopback")]
async fn stub_answers_on_ipv6(world: &mut DoctorWorld) {
    if !crate::loopback::has_ipv6_loopback().await {
        return;
    }
    let port = world.stub_port.expect("no stub endpoint started yet");
    let url = format!("http://[::1]:{port}/management/v1/configureddevices");
    let client = rusty_photon_tls::client::build_reqwest_client(None).expect("probe client");
    let body = client
        .get(&url)
        .timeout(IPV6_GUARD_TIMEOUT)
        .send()
        .await
        .unwrap_or_else(|e| {
            panic!("[::1]:{port} did not answer within {IPV6_GUARD_TIMEOUT:?}: {e}")
        })
        .text()
        .await
        .expect("the stub's body reads");
    assert!(
        body.contains("Stub Camera"),
        "[::1]:{port} answered, but not with the stub's payload — \
         a foreign listener holds the address the probe's localhost reaches: {body}"
    );
}

/// An HTTPS stub serving the pki tree's issued pair for the service — the
/// same trust chain a provisioned rig runs, so the probe must present
/// doctor's CA as its root. Also (re)writes the service's config: the tls
/// block pointing at the issued pair, the port at the stub.
#[given(expr = "an HTTPS stub management endpoint for {string} serving two configured devices")]
async fn stub_endpoint_https(world: &mut DoctorWorld, service: String) {
    let pki = world.pki_dir();
    let cert = pki.join(format!("{service}.pem"));
    let key = pki.join(format!("{service}-key.pem"));
    assert!(
        cert.is_file() && key.is_file(),
        "no issued pair for {service} — missing the `doctor tls issue` given?"
    );
    let tls_config = rusty_photon_tls::config::TlsConfig {
        cert: cert.to_string_lossy().into_owned(),
        key: key.to_string_lossy().into_owned(),
    };

    let (port, listeners) = crate::loopback::bind_loopback_pair().await;
    world.stub_port = Some(port);
    for listener in listeners {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        world.stub_shutdowns.push(shutdown_tx);
        let router = stub_router(StubBehavior::Devices);
        let tls_config = tls_config.clone();
        tokio::spawn(async move {
            rusty_photon_tls::server::serve_tls(listener, router, &tls_config, async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("stub endpoint serve");
        });
    }

    let config = serde_json::json!({ "server": {
        "port": port,
        "tls": { "cert": cert.to_string_lossy(), "key": key.to_string_lossy() },
    } });
    world.write_config(&format!("{service}.json"), &config.to_string());
}

#[given(expr = "a config file {string} pointing at the stub endpoint")]
fn config_at_stub(world: &mut DoctorWorld, file: String) {
    let port = world.stub_port.expect("no stub endpoint started yet");
    world.write_config(&file, &format!(r#"{{ "server": {{ "port": {port} }} }}"#));
}

#[given(expr = "a config file {string} pointing at the stub endpoint with auth enabled")]
fn config_at_stub_with_auth(world: &mut DoctorWorld, file: String) {
    let port = world.stub_port.expect("no stub endpoint started yet");
    // The hash is never verified by the probe (the stub checks the
    // plaintext header) — the block's presence is what routes the credential.
    world.write_config(
        &file,
        &format!(
            r#"{{ "server": {{ "port": {port},
                 "auth": {{ "username": "observatory", "password_hash": "$argon2id$stub" }} }} }}"#
        ),
    );
}

#[given(expr = "a config file {string} declaring a port nothing listens on")]
fn config_at_dead_port(world: &mut DoctorWorld, file: String) {
    // Port 1 (tcpmux) sits outside every OS's dynamic port range and is
    // privileged on Unix, so no test process here can occupy it and
    // nothing on a CI or dev host listens there — the probe reliably gets
    // a refusal. Binding an ephemeral port and dropping it raced the
    // other suites — under high test parallelism another suite's port-0
    // server re-bound the freed port and answered the probe, which is an
    // answer, not "does not answer".
    world.write_config(&file, r#"{ "server": { "port": 1 } }"#);
}

#[given("a staged observatory credential")]
fn staged_credential(world: &mut DoctorWorld) {
    write_credential(world, STUB_PASSWORD);
}

#[given("a staged observatory credential the endpoint does not accept")]
fn staged_rejected_credential(world: &mut DoctorWorld) {
    write_credential(world, "not-the-stub-password");
}

fn write_credential(world: &mut DoctorWorld, plaintext: &str) {
    let pki = world.pki_dir();
    std::fs::create_dir_all(&pki).expect("pki dir");
    std::fs::write(pki.join("credential"), format!("{plaintext}\n")).expect("credential file");
}

/// The minimal `acme.json` that flips the install to ACME for the probes:
/// the domain is what they derive public names from, the rest is the
/// config type's required fields. A reserved-TLD domain keeps the derived
/// names unresolvable, so probe scenarios stay hermetic.
#[given(expr = "an acme.json declaring domain {string}")]
fn acme_json_with_domain(world: &mut DoctorWorld, domain: String) {
    let config = serde_json::json!({
        "email": "op@example.com",
        "domain": domain,
        "dns_provider": "cloudflare",
        "dns_credentials": {},
    });
    world.write_config("acme.json", &config.to_string());
}

/// A config whose `server.tls` is set while no pki tree exists — the probe
/// must warn that it cannot verify, not connect unverified.
#[given(expr = "a config file {string} with a tls block but no pki tree")]
fn config_tls_without_pki(world: &mut DoctorWorld, file: String) {
    world.write_config(
        &file,
        r#"{ "server": { "port": 1,
             "tls": { "cert": "/nope/cert.pem", "key": "/nope/key.pem" } } }"#,
    );
}

// ---------------------------------------------------------------------------
// Stub per-service binaries for the shell-out probe
// ---------------------------------------------------------------------------

/// Write an executable stub script. Windows gets a `.cmd` (the SCM-recorded
/// image is an `.exe`, but the spawn path exercised is the same); elsewhere
/// a `chmod +x` shell script.
fn write_stub_script(dir: &Path, name: &str, unix_body: &str, windows_body: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = unix_body;
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(&path, windows_body).expect("stub script");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        let _ = windows_body;
        let path = dir.join(name);
        std::fs::write(&path, unix_body).expect("stub script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("stub script mode");
        path
    }
}

#[given(expr = "a stub per-service doctor for {string} whose report has a failing {string} check")]
fn stub_doctor_binary(world: &mut DoctorWorld, service: String, check: String) {
    let report = serde_json::json!({
        "schema_version": 1,
        "doctor_version": "9.9.9",
        "mode": "service",
        "service": service,
        "config_dir": format!("/etc/rusty-photon/{service}.json"),
        "checks": [ {
            "name": check,
            "status": "fail",
            "detail": "unknown field `typo_key` at line 3",
        } ],
    });
    let report_file = format!("{service}-doctor-report.json");
    std::fs::write(
        world.temp.path().join(&report_file),
        serde_json::to_string(&report).expect("stub report"),
    )
    .expect("stub report file");
    world.stub_binary = Some(write_stub_script(
        world.temp.path(),
        &format!("stub-{service}"),
        &format!("#!/bin/sh\ncat \"$(dirname \"$0\")/{report_file}\"\n"),
        &format!("@echo off\r\ntype \"%~dp0{report_file}\"\r\n"),
    ));
}

#[given(expr = "a stub per-service binary for {string} that does not know the doctor subcommand")]
fn stub_predates_subcommand(world: &mut DoctorWorld, service: String) {
    world.stub_binary = Some(write_stub_script(
        world.temp.path(),
        &format!("stub-{service}"),
        "#!/bin/sh\necho \"error: unrecognized subcommand 'doctor'\" >&2\nexit 2\n",
        "@echo off\r\necho error: unrecognized subcommand 'doctor' 1>&2\r\nexit /b 2\r\n",
    ));
}

#[given(expr = "a stub per-service doctor for {string} whose report has no checks")]
fn stub_doctor_empty_report(world: &mut DoctorWorld, service: String) {
    let report_file = format!("{service}-doctor-report.json");
    std::fs::write(world.temp.path().join(&report_file), "{}").expect("stub report file");
    world.stub_binary = Some(write_stub_script(
        world.temp.path(),
        &format!("stub-{service}"),
        &format!("#!/bin/sh\ncat \"$(dirname \"$0\")/{report_file}\"\n"),
        &format!("@echo off\r\ntype \"%~dp0{report_file}\"\r\n"),
    ));
}

// ---------------------------------------------------------------------------
// Unit run-state staging
// ---------------------------------------------------------------------------

#[given(expr = "platform facts where unit {string} is installed and active")]
fn unit_active(world: &mut DoctorWorld, unit: String) {
    world.add_unit(&unit);
    world.set_unit_probe_facts(&unit, true, None);
}

#[given(expr = "platform facts where unit {string} is installed but stopped, with the stub binary")]
fn unit_stopped_with_stub(world: &mut DoctorWorld, unit: String) {
    let binary = world.stub_binary.clone().expect("no stub binary staged");
    world.add_unit(&unit);
    world.set_unit_probe_facts(&unit, false, Some(binary));
}

#[given(
    expr = "platform facts where unit {string} is installed but stopped, with no known binary path"
)]
fn unit_stopped_without_binary(world: &mut DoctorWorld, unit: String) {
    world.add_unit(&unit);
    world.set_unit_probe_facts(&unit, false, None);
}

#[given(
    expr = "platform facts where unit {string} is installed but stopped, with a binary that does not exist"
)]
fn unit_stopped_with_missing_binary(world: &mut DoctorWorld, unit: String) {
    let missing = world.temp.path().join("no-such-binary");
    world.add_unit(&unit);
    world.set_unit_probe_facts(&unit, false, Some(missing));
}

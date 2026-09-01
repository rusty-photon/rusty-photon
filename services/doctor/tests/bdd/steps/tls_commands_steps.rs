//! Steps driving the tls/auth subcommands: issuance, ACME setup flags,
//! credential commands, and the TLS roundtrip validation of issued certs.

use cucumber::{given, then, when};

use crate::world::{AcmeFlags, DoctorWorld};

// ---------------------------------------------------------------------------
// tls issue
// ---------------------------------------------------------------------------

#[when("I run doctor tls issue")]
async fn run_tls_issue(world: &mut DoctorWorld) {
    world.run_doctor_subcommand(&["tls", "issue"], None).await;
}

#[given("doctor tls issue has already run")]
async fn tls_issue_already_ran(world: &mut DoctorWorld) {
    world.run_doctor_subcommand(&["tls", "issue"], None).await;
    let output = world.output.as_ref().expect("tls issue ran");
    assert!(
        output.status.success(),
        "priming tls issue failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    world.snapshot_pki();
}

#[when(expr = "I run doctor tls issue limited to the service {string}")]
async fn run_tls_issue_limited(world: &mut DoctorWorld, service: String) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--services", &service], None)
        .await;
}

#[when("I run doctor tls issue with --force")]
async fn run_tls_issue_force(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--force"], None)
        .await;
}

#[when("I run doctor tls issue pointed at a config directory that does not exist")]
async fn run_tls_issue_missing_dir(world: &mut DoctorWorld) {
    world.config_dir_override = Some(world.temp.path().join("no-such-dir"));
    world.run_doctor_subcommand(&["tls", "issue"], None).await;
}

#[then("that config directory was created with a pki tree")]
fn overridden_dir_created(world: &mut DoctorWorld) {
    let dir = world
        .config_dir_override
        .as_ref()
        .expect("a config dir override was staged");
    assert!(
        dir.is_dir(),
        "config directory {} was not created",
        dir.display()
    );
    let ca = dir.join("pki").join("ca.pem");
    assert!(ca.is_file(), "expected {} to exist", ca.display());
}

// ---------------------------------------------------------------------------
// tls issue --acme flag validation
// ---------------------------------------------------------------------------

#[when("I run doctor tls issue with --acme but no --domain")]
async fn run_acme_no_domain(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--acme"], None)
        .await;
}

#[when("I run doctor tls issue with --domain but no --acme")]
async fn run_domain_no_acme(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--domain", "example.org"], None)
        .await;
}

#[when("I run doctor tls issue with --acme and --domain but no --dns-provider")]
async fn run_acme_no_provider(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--acme", "--domain", "example.org"], None)
        .await;
}

#[when("I run doctor tls issue with --acme and --domain and --dns-provider but no --email")]
async fn run_acme_no_email(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                "example.org",
                "--dns-provider",
                "cloudflare",
                "--dns-token",
                "test-token",
            ],
            None,
        )
        .await;
}

#[when(
    "I run doctor tls issue with --acme and --domain and --dns-provider and --email but no token flag"
)]
async fn run_acme_no_token(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                "example.org",
                "--dns-provider",
                "cloudflare",
                "--email",
                "admin@example.org",
            ],
            None,
        )
        .await;
}

#[when(expr = "I run doctor tls issue with --acme and --dns-token-var {string}")]
async fn run_acme_token_var(world: &mut DoctorWorld, var_name: String) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                "example.org",
                "--dns-provider",
                "cloudflare",
                "--email",
                "admin@example.org",
                "--dns-token-var",
                &var_name,
            ],
            None,
        )
        .await;
}

#[when(expr = "I run doctor tls issue with --acme and --dns-token {string}")]
async fn run_acme_token_value(world: &mut DoctorWorld, token: String) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                "example.org",
                "--dns-provider",
                "cloudflare",
                "--email",
                "admin@example.org",
                "--dns-token",
                &token,
            ],
            None,
        )
        .await;
}

#[when("I run doctor tls issue with --acme and both --dns-token and --dns-token-var")]
async fn run_acme_both_token_flags(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                "example.org",
                "--dns-provider",
                "cloudflare",
                "--email",
                "admin@example.org",
                "--dns-token",
                "test-token",
                "--dns-token-var",
                "SOME_VAR",
            ],
            None,
        )
        .await;
}

#[when("I run doctor tls issue with --acme and all required flags pointing to staging")]
async fn run_acme_staging(world: &mut DoctorWorld) {
    let flags = AcmeFlags {
        domain: "example.org".to_string(),
        email: "admin@example.org".to_string(),
        dns_provider: "cloudflare".to_string(),
    };
    // This fails at the DNS provider step (the token is fake), but the
    // acme.json configuration must be persisted first — that is the
    // contract renewal picks up from.
    world
        .run_doctor_subcommand(
            &[
                "tls",
                "issue",
                "--acme",
                "--domain",
                &flags.domain.clone(),
                "--dns-provider",
                &flags.dns_provider.clone(),
                "--dns-token",
                "test-token",
                "--email",
                &flags.email.clone(),
                "--staging",
            ],
            None,
        )
        .await;
    world.acme_flags = Some(flags);
}

// ---------------------------------------------------------------------------
// auth commands
// ---------------------------------------------------------------------------

#[when(expr = "I run doctor auth hash-password with {string} on stdin")]
async fn run_hash_password(world: &mut DoctorWorld, password: String) {
    let mut stdin = password.into_bytes();
    stdin.push(b'\n');
    world
        .run_doctor_subcommand(&["auth", "hash-password", "--stdin"], Some(&stdin))
        .await;
}

#[when("I run doctor auth rotate")]
async fn run_auth_rotate(world: &mut DoctorWorld) {
    world.run_doctor_subcommand(&["auth", "rotate"], None).await;
    let output = world.output.as_ref().expect("auth rotate ran");
    assert!(
        output.status.success(),
        "auth rotate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[when("I run doctor auth rotate with --json")]
async fn run_auth_rotate_json(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["auth", "rotate", "--json"], None)
        .await;
}

#[when("I run doctor tls issue with --json")]
async fn run_tls_issue_json(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "issue", "--json"], None)
        .await;
}

#[when("I run doctor auth hash-password with empty input on stdin")]
async fn run_hash_password_empty(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["auth", "hash-password", "--stdin"], Some(b"\n"))
        .await;
}

// ---------------------------------------------------------------------------
// Process-outcome assertions
// ---------------------------------------------------------------------------

#[then("the command exits with a non-zero status")]
fn exits_non_zero(world: &mut DoctorWorld) {
    let output = world.output.as_ref().expect("run doctor first");
    assert!(
        !output.status.success(),
        "expected failure, got: {:?}",
        output.status
    );
}

#[then(expr = "stderr contains {string}")]
fn stderr_contains(world: &mut DoctorWorld, needle: String) {
    let stderr = world.stderr();
    assert!(
        stderr.contains(&needle),
        "stderr does not mention {needle:?}: {stderr}"
    );
}

#[then(expr = "stderr does not contain {string}")]
fn stderr_does_not_contain(world: &mut DoctorWorld, needle: String) {
    let stderr = world.stderr();
    assert!(
        !stderr.contains(&needle),
        "stderr unexpectedly mentions {needle:?}: {stderr}"
    );
}

#[then(expr = "stdout starts with {string}")]
fn stdout_starts_with(world: &mut DoctorWorld, prefix: String) {
    let stdout = world.stdout();
    assert!(
        stdout.trim_start().starts_with(&prefix),
        "stdout does not start with {prefix:?}: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// acme.json assertions
// ---------------------------------------------------------------------------

fn acme_json(world: &DoctorWorld) -> serde_json::Value {
    let path = world.config_dir().join("acme.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&content).expect("acme.json is valid JSON")
}

#[then(expr = "the config root contains {string}")]
fn config_root_contains(world: &mut DoctorWorld, name: String) {
    let path = world.config_dir().join(&name);
    assert!(path.is_file(), "expected {} to exist", path.display());
}

#[then(expr = "{string} stores the api_token {string}")]
fn acme_stores_api_token(world: &mut DoctorWorld, _name: String, expected: String) {
    let acme = acme_json(world);
    let actual = acme
        .pointer("/dns_credentials/api_token")
        .and_then(serde_json::Value::as_str)
        .expect("acme.json holds dns_credentials.api_token");
    assert_eq!(actual, expected, "acme.json api_token mismatch");
}

#[then(expr = "{string} contains the provided domain")]
fn acme_contains_domain(world: &mut DoctorWorld, _name: String) {
    let expected = world
        .acme_flags
        .as_ref()
        .expect("acme flags staged")
        .domain
        .clone();
    let acme = acme_json(world);
    assert!(
        acme.to_string().contains(&expected),
        "acme.json lacks domain {expected:?}: {acme}"
    );
}

#[then(expr = "{string} contains the provided email")]
fn acme_contains_email(world: &mut DoctorWorld, _name: String) {
    let expected = world
        .acme_flags
        .as_ref()
        .expect("acme flags staged")
        .email
        .clone();
    let acme = acme_json(world);
    assert!(
        acme.to_string().contains(&expected),
        "acme.json lacks email {expected:?}: {acme}"
    );
}

#[then(expr = "{string} contains the DNS provider name")]
fn acme_contains_provider(world: &mut DoctorWorld, _name: String) {
    let expected = world
        .acme_flags
        .as_ref()
        .expect("acme flags staged")
        .dns_provider
        .clone();
    let acme = acme_json(world);
    assert!(
        acme.to_string().contains(&expected),
        "acme.json lacks provider {expected:?}: {acme}"
    );
}

#[then(expr = "{string} has staging set to true")]
fn acme_staging_true(world: &mut DoctorWorld, _name: String) {
    let acme = acme_json(world);
    assert_eq!(
        acme.get("staging"),
        Some(&serde_json::Value::Bool(true)),
        "acme.json staging: {acme}"
    );
}

// ---------------------------------------------------------------------------
// TLS roundtrip validation: doctor-issued certs actually serve HTTPS
// ---------------------------------------------------------------------------

#[when(expr = "a test HTTPS server is started with the {string} certificate")]
fn start_test_https_server(world: &mut DoctorWorld, service: String) {
    // Records the service whose cert pair the connect step below serves —
    // server and client live in one async scope there.
    world.tls_roundtrip_service = Some(service);
}

#[when("a client connects using the generated CA certificate")]
async fn client_connects_with_ca(world: &mut DoctorWorld) {
    // The hot-reload scenario has its server already running: connect to
    // it and capture the peer certificate for the swapped-pair assertion.
    if let Some(addr) = world.hot_reload_addr {
        let ca_path = world.pki_dir().join("ca.pem");
        let (status, peer) =
            crate::steps::tls_renew_steps::https_get_capturing_peer(addr, &ca_path).await;
        world.tls_https_status = Some(status);
        world.peer_cert_after = Some(peer);
        return;
    }

    let service = world
        .tls_roundtrip_service
        .clone()
        .expect("no service recorded — missing the HTTPS-server step?");
    let pki = world.pki_dir();
    let tls_config = rusty_photon_tls::config::TlsConfig {
        cert: pki
            .join(format!("{service}.pem"))
            .to_string_lossy()
            .into_owned(),
        key: pki
            .join(format!("{service}-key.pem"))
            .to_string_lossy()
            .into_owned(),
    };

    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
        .await
        .unwrap();
    let bound_addr = listener.local_addr().unwrap();

    let router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        rusty_photon_tls::server::serve_tls(listener, router, &tls_config, async {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap();
    });

    let ca_path = pki.join("ca.pem");
    let client = rusty_photon_tls::client::build_reqwest_client(Some(&ca_path)).unwrap();
    let url = format!("https://localhost:{}/health", bound_addr.port());

    let response = client.get(&url).send().await.unwrap();
    world.tls_https_status = Some(response.status().as_u16());

    shutdown_tx.send(()).ok();
}

#[then("the HTTPS connection succeeds")]
fn https_connection_succeeds(world: &mut DoctorWorld) {
    let status = world.tls_https_status.expect("no HTTPS response captured");
    assert_eq!(status, 200, "HTTPS request should return 200 OK");
}

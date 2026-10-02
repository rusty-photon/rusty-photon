//! Steps for `doctor tls renew`: staging pki trees with arbitrary validity
//! windows, running the renew subcommand, asserting the re-issues, and the
//! in-process hot-reload proof.

use std::path::Path;

use cucumber::{given, then, when};

use crate::world::DoctorWorld;

fn days_from_now(days: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() + time::Duration::days(days)
}

fn pki_pem(world: &DoctorWorld, name: &str) -> String {
    let path = world.pki_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Givens: staged pki material
// ---------------------------------------------------------------------------

#[given("a pki tree with a CA")]
fn pki_tree_with_ca(world: &mut DoctorWorld) {
    world.stage_ca(days_from_now(3650));
}

#[given(expr = "a pki tree with a CA expiring in {int} days")]
fn pki_tree_with_expiring_ca(world: &mut DoctorWorld, days: i64) {
    world.stage_ca(days_from_now(days));
}

#[given(expr = "a certificate pair for {string} expiring in {int} days")]
fn pair_expiring(world: &mut DoctorWorld, service: String, days: i64) {
    world.stage_service_pair(&service, days_from_now(days), &[]);
}

#[given(expr = "a certificate pair for {string} that expired {int} days ago")]
fn pair_expired(world: &mut DoctorWorld, service: String, days: i64) {
    world.stage_service_pair(&service, days_from_now(-days), &[]);
}

#[given(
    expr = "a certificate pair for {string} expiring in {int} days with the extra SAN {string}"
)]
fn pair_with_extra_san(world: &mut DoctorWorld, service: String, days: i64, san: String) {
    world.stage_service_pair(&service, days_from_now(days), &[san.as_str()]);
}

#[given(expr = "the pki file {string} has been deleted")]
fn pki_file_deleted(world: &mut DoctorWorld, name: String) {
    let path = world.pki_dir().join(&name);
    std::fs::remove_file(&path).unwrap_or_else(|e| panic!("deleting {}: {e}", path.display()));
}

// ---------------------------------------------------------------------------
// Whens: the renew subcommand
// ---------------------------------------------------------------------------

#[when("I run doctor tls renew")]
async fn run_renew(world: &mut DoctorWorld) {
    world.run_doctor_subcommand(&["tls", "renew"], None).await;
}

#[when("I run doctor tls renew with --json")]
async fn run_renew_json(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "renew", "--json"], None)
        .await;
}

#[when("I run doctor tls renew with --force")]
async fn run_renew_force(world: &mut DoctorWorld) {
    world
        .run_doctor_subcommand(&["tls", "renew", "--force"], None)
        .await;
}

// ---------------------------------------------------------------------------
// Thens
// ---------------------------------------------------------------------------

#[then(expr = "the command exits with status {int}")]
fn exits_with_status(world: &mut DoctorWorld, expected: i32) {
    let output = world.output.as_ref().expect("run doctor first");
    let code = output.status.code().expect("doctor was signal-killed");
    assert_eq!(
        code,
        expected,
        "expected exit {expected}, got {code}; stderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[then(expr = "the certificate {string} is not within its renewal window")]
fn cert_outside_renewal_window(world: &mut DoctorWorld, name: String) {
    let not_after = doctor::provision::expiry::not_after(&pki_pem(world, &name))
        .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
    let margin = not_after - time::OffsetDateTime::now_utc();
    assert!(
        margin > time::Duration::days(30),
        "{name} expires {not_after} — still inside the 30-day window"
    );
}

#[then(expr = "the certificate {string} carries the SAN {string}")]
fn cert_carries_san(world: &mut DoctorWorld, name: String, san: String) {
    let sans = doctor::provision::expiry::sans(&pki_pem(world, &name));
    assert!(sans.contains(&san), "{name} SANs {sans:?} lack {san:?}");
}

// ---------------------------------------------------------------------------
// The in-process hot-reload proof
// ---------------------------------------------------------------------------

/// How many times [`https_get_capturing_peer`] re-runs the whole probe
/// before failing the scenario, and how long it waits between tries.
///
/// The server bounds how long one connection may make no progress — a wait
/// for the first byte, and the handshake itself — and drops it when the bound
/// passes, which reaches the client as a bare transport abort with nothing
/// wrong on either side. Scheduling delay alone can consume that bound on a
/// loaded runner, and a fresh connection starts a fresh one, so retrying the
/// *transport* costs nothing the scenario cares about. A TLS or HTTP verdict
/// — the certificate the server actually serves, which is this scenario's
/// whole subject — is never retried: those fail on the first attempt.
const PROBE_ATTEMPTS: usize = 3;
const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// The most one probe attempt may take, end to end. The server's own bounds
/// cover a connection that stalls *on its side*, but nothing covered one
/// that stalled on the client's: an unanswered connect, a handshake, or a
/// response read with no end. A CI run once sat silent in this probe until
/// Bazel killed the suite at 300 s, leaving no clue which stage it was in.
/// With the deadline that becomes a `TimedOut` naming the stage — retried
/// like any other transport abort, and reported if every attempt stalls.
///
/// 15 s is past the server's 10 s handshake bound, so a server-side drop
/// still reaches the client as the abort it is rather than as our timeout.
const PROBE_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Handshake against `addr` trusting `ca_path`, returning the HTTP status
/// of a `/health` GET plus the peer's leaf certificate DER — reqwest hides
/// the peer certificate, so this speaks rustls directly.
pub async fn https_get_capturing_peer(
    addr: std::net::SocketAddr,
    ca_path: &Path,
) -> (u16, Vec<u8>) {
    rusty_photon_tls::install_default_crypto_provider();
    let ca_pem = std::fs::read_to_string(ca_path).expect("CA pem");
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(cert.expect("CA cert parses")).expect("CA added");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));

    let started = std::time::Instant::now();
    let mut aborts = Vec::new();
    for attempt in 1..=PROBE_ATTEMPTS {
        match probe_once(&connector, addr).await {
            Ok(captured) => return captured,
            Err(e) if is_transport_abort(&e) => {
                aborts.push(format!(
                    "attempt {attempt} after {:?}: {e}",
                    started.elapsed()
                ));
                // Nothing follows the last attempt, so backing off there
                // would only delay the panic and pad its elapsed time.
                if attempt < PROBE_ATTEMPTS {
                    tokio::time::sleep(PROBE_BACKOFF).await;
                }
            }
            // A TLS alert, an untrusted certificate, a refused connect: the
            // server is answering, just not the way the scenario expects.
            Err(e) => panic!("HTTPS probe of {addr} failed: {e}"),
        }
    }
    panic!(
        "HTTPS probe of {addr} was aborted on all {PROBE_ATTEMPTS} attempts over {:?} — {}",
        started.elapsed(),
        aborts.join("; ")
    );
}

/// One connect + handshake + `GET /health`. Every transport failure comes
/// back as an error for [`https_get_capturing_peer`] to grade; only broken
/// test material (an unusable CA, a server with no leaf certificate) panics.
async fn probe_once(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let deadline = tokio::time::Instant::now() + PROBE_ATTEMPT_TIMEOUT;
    let tcp = within(
        deadline,
        "TCP connect",
        tokio::net::TcpStream::connect(addr),
    )
    .await?;
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut tls = within(deadline, "TLS handshake", connector.connect(name, tcp)).await?;
    let peer_der = tls
        .get_ref()
        .1
        .peer_certificates()
        .expect("peer certificates")[0]
        .to_vec();

    within(
        deadline,
        "request write",
        tls.write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n"),
    )
    .await?;
    let mut response = Vec::new();
    // The server may close without a TLS close_notify; the bytes read so
    // far still carry the status line. A read that never ends is still an
    // error, though: that is the stall the deadline exists to name.
    if let Err(e) = within(deadline, "response read", tls.read_to_end(&mut response)).await {
        if e.kind() == std::io::ErrorKind::TimedOut {
            return Err(e);
        }
    }
    std::str::from_utf8(&response)
        .ok()
        .and_then(|text| text.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .map(|status| (status, peer_der))
        .ok_or_else(|| {
            // A response with no status line is a connection that died
            // mid-answer — same grading as any other truncated read.
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "no HTTP status line in: {}",
                    String::from_utf8_lossy(&response)
                ),
            )
        })
}

/// Run one stage of [`probe_once`] against the attempt's shared deadline,
/// turning an expiry into a `TimedOut` error that says which stage stalled.
async fn within<T>(
    deadline: tokio::time::Instant,
    stage: &str,
    fut: impl std::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    tokio::time::timeout_at(deadline, fut)
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{stage} did not finish within the {PROBE_ATTEMPT_TIMEOUT:?} attempt deadline"
                ),
            ))
        })
}

/// Whether an error means the connection died under the probe rather than
/// the server refusing what the probe asked for. rustls surfaces protocol
/// and certificate failures as `InvalidData`, which is deliberately absent.
fn is_transport_abort(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
    )
}

#[given(expr = "a hot-reloading test HTTPS server is started with the {string} certificate")]
async fn start_hot_reload_server(world: &mut DoctorWorld, service: String) {
    let pki = world.pki_dir();
    let resolver = rusty_photon_tls::resolver::ReloadableCertResolver::load(
        pki.join(format!("{service}.pem")),
        pki.join(format!("{service}-key.pem")),
    )
    .expect("the staged pair loads")
    // Zero interval: every handshake re-stats the pair, so the scenario
    // needs no sleeps around the renewal.
    .with_check_interval(std::time::Duration::ZERO);
    let acceptor = rusty_photon_tls::server::acceptor_from_resolver(resolver);

    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
        .await
        .expect("bind");
    let bound = listener.local_addr().expect("bound addr");
    let router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        rusty_photon_tls::server::serve_tls_with_acceptor(listener, router, acceptor, async {
            shutdown_rx.await.ok();
        })
        .await
        .expect("hot-reload server serves");
    });
    world.stub_shutdowns.push(shutdown_tx);
    world.hot_reload_addr = Some(bound);

    let (status, peer) = https_get_capturing_peer(bound, &pki.join("ca.pem")).await;
    assert_eq!(status, 200, "the pre-renewal connection must serve");
    world.peer_cert_before = Some(peer);
}

#[then("the server is now serving a different certificate than before the renewal")]
fn server_cert_changed(world: &mut DoctorWorld) {
    let before = world
        .peer_cert_before
        .as_ref()
        .expect("no pre-renewal peer certificate captured");
    let after = world
        .peer_cert_after
        .as_ref()
        .expect("no post-renewal peer certificate captured");
    assert_ne!(
        before, after,
        "the server still serves the pre-renewal certificate"
    );
}

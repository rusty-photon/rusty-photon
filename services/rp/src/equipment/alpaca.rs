//! Generic Alpaca client glue: HTTP basic-auth header construction and
//! the retry/backoff helper that every per-device `connect_*` shares.

use std::future::Future;
use std::time::Duration;

use ascom_alpaca::Client;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rp_auth::config::ClientAuthConfig;
use tracing::{debug, info};

/// Maximum attempts each `connect_*` makes for the get-devices +
/// set-connected pair. Backoff between attempts is 1 s, then 2 s
/// (see [`retry_connect_attempt`]), so the worst-case wait per device
/// before giving up is 3 s of sleep plus up to 3 ×
/// [`GET_DEVICES_TIMEOUT`] of in-flight HTTP. Three attempts is
/// enough to ride out a transient `OmniSim` stall (e.g. a long .NET
/// full GC) without making BDD scenarios that intentionally point at
/// unreachable URLs pay too high a wall-clock cost.
pub(super) const CONNECT_ATTEMPTS: u32 = 3;

/// Per-call timeout on `client.get_devices()`. Short enough that a stuck
/// Alpaca server falls into the retry loop instead of hanging the whole
/// rp startup.
pub(super) const GET_DEVICES_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of a single attempt inside [`retry_connect_attempt`]. The
/// `Permanent` / `Transient` split is what makes this a retry helper
/// rather than a sleep-and-hope wrapper: the connect closures map each
/// failure mode to its own variant. Today only "device-not-found at
/// the requested index in the Alpaca server's reply" is `Permanent`
/// inside the closure — every other failure inside the retry loop
/// (HTTP transport errors including connection-refused on an
/// unreachable host, `get_devices` timeouts, `set_connected` errors,
/// `OmniSim` under GC pressure) is `Transient` and goes through the
/// retry/backoff path. Note: `Client::new` failures are filtered
/// out *before* the retry loop, so that case never produces an
/// `AttemptOutcome` at all.
pub(super) enum AttemptOutcome<T> {
    Ok(T),
    Permanent(String),
    Transient(String),
}

/// Drive `operation` up to [`CONNECT_ATTEMPTS`] times with exponential
/// backoff between attempts: 1 s, then 2 s. Returns the first `Ok`
/// result, or the last transient error wrapped with attempt count if
/// every attempt returned `Transient`. Returns immediately on
/// `Permanent`.
///
/// `label` is used purely for log lines so the operator can see which
/// device is retrying — e.g. `"camera main-cam"`.
pub(super) async fn retry_connect_attempt<T, F, Fut>(label: &str, operation: F) -> Result<T, String>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = AttemptOutcome<T>>,
{
    let mut last_transient = String::from("no attempts made");
    for attempt in 1..=CONNECT_ATTEMPTS {
        match operation(attempt).await {
            AttemptOutcome::Ok(value) => {
                if attempt > 1 {
                    // Worth surfacing at info: the user will want to
                    // know the system had to retry but did recover.
                    info!(label, attempt, "connect succeeded after retry");
                }
                return Ok(value);
            }
            AttemptOutcome::Permanent(msg) => return Err(msg),
            AttemptOutcome::Transient(msg) => {
                last_transient = msg;
                if attempt < CONNECT_ATTEMPTS {
                    // 1 s, then 2 s — bounded total backoff of 3 s per
                    // device keeps unreachable-URL BDD scenarios fast
                    // while still smoothing over a transient stall.
                    let delay = Duration::from_secs(1u64 << attempt.saturating_sub(1));
                    // Each retry is at info level: a transient
                    // connect failure that the system is recovering
                    // from is something the operator should see in
                    // default-verbosity logs without having to enable
                    // debug.
                    info!(
                        label,
                        attempt,
                        max = CONNECT_ATTEMPTS,
                        ?delay,
                        error = %last_transient,
                        "transient connect failure, retrying after backoff"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(format!(
        "gave up after {CONNECT_ATTEMPTS} attempts (last error: {last_transient})"
    ))
}

/// Connect-phase timeout for every Alpaca HTTP request rp issues. A
/// localhost / LAN Alpaca server completes the TCP connect in well under
/// this; the bound keeps a refused or black-holed host from stalling the
/// connect indefinitely.
pub(super) const ALPACA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-read inactivity timeout for every Alpaca HTTP request.
///
/// reqwest has **no** default request timeout, and `ascom-alpaca`'s
/// client adds none (it only bounds UDP discovery). Without this, a
/// device that accepts the TCP connection but never answers — an `OmniSim`
/// wedged under heavy parallel CI load, or a real mount / USB-serial
/// bridge that stalls mid-request during an unattended night — hangs the
/// awaiting tool handler **forever**.
///
/// That unbounded `await` is the root cause of issue #319's
/// `center_on_target` timeout: a per-iteration mount read stalled, the
/// loop never returned, rmcp tore the MCP transport down at its 300 s
/// keep-alive, and the BDD `MCP_CALL_TIMEOUT` tripped at 360 s. The
/// blocking-op deadlines in `mcp::internals` (120 s capture, 300 s slew)
/// guard *poll loops*, not a single in-flight request, so they cannot
/// interrupt this — only a client-level timeout can.
///
/// A *read* timeout (resets on every chunk received, vs. a total request
/// deadline) bounds a genuine stall without capping a legitimately large
/// `image_array` download on a slow link. 10 s is far above any healthy
/// single Alpaca round-trip yet well below rp's 120 s / 300 s
/// blocking-op deadlines and the 360 s BDD backstop, so a stall now
/// surfaces as a fast, legible equipment error instead of a hang.
pub(super) const ALPACA_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Build an Alpaca client with per-request timeouts, optional HTTP Basic
/// Auth credentials, and optional CA certificate trust.
///
/// Both the auth and no-auth paths go through `new_with_client` so the
/// timeouts apply uniformly — the no-auth path must **not** fall back to
/// `Client::new`, whose default reqwest client has no timeout (the #319
/// hang). See [`ALPACA_READ_TIMEOUT`].
///
/// `ca_cert_path` is the observatory CA (`Config::ca_cert_path`, rp.md
/// §Configuration): without it, an `https://` `alpaca_url` signed by that
/// CA fails certificate verification regardless of `auth` (issue #609).
pub(super) fn build_alpaca_client(
    url: &str,
    auth: Option<&ClientAuthConfig>,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    let mut builder = rusty_photon_tls::client::client_builder(ca_cert_path)?
        .user_agent("rusty-photon-rp")
        .connect_timeout(ALPACA_CONNECT_TIMEOUT)
        .read_timeout(ALPACA_READ_TIMEOUT);
    if let Some(a) = auth {
        let encoded = BASE64.encode(format!("{}:{}", a.username, a.password));
        // `set_sensitive` keeps the credential out of `Client`'s `Debug`
        // impl, which prints `default_headers` unconditionally — without
        // it, an incidental `{:?}`/`debug!()` of this client leaks the
        // password (base64 is trivially reversible).
        let mut header_value: reqwest::header::HeaderValue = format!("Basic {encoded}").parse()?;
        header_value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("authorization", header_value);
        builder = builder.default_headers(headers);
    }
    Ok(Client::new_with_client(url, builder.build()?)?)
}

/// Attempts an idempotent runtime Alpaca read makes before giving up.
pub(crate) const READ_RETRY_ATTEMPTS: u32 = 3;

/// Retry an idempotent Alpaca read up to [`READ_RETRY_ATTEMPTS`] times
/// with short backoff (100 ms, then 200 ms).
///
/// The runtime analogue of [`retry_connect_attempt`]: the connect path
/// already rides out a transient `OmniSim` stall (e.g. a long .NET GC),
/// but the *runtime* mount reads that `center_on_target` issues every
/// iteration did not — so a single read failure aborted the whole
/// compound tool. Now that [`ALPACA_READ_TIMEOUT`] bounds a stalled read
/// (turning the #319 hang into a fast error), retrying lets a brief
/// device hiccup recover instead of failing the operation.
///
/// **Idempotent reads only** (`right_ascension`, `declination`,
/// `slewing`) — never a command that mutates device state, where a
/// duplicate could double-apply.
pub(crate) async fn retry_idempotent_read<T, F, Fut>(label: &str, op: F) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut last = String::from("no attempts made");
    for attempt in 1..=READ_RETRY_ATTEMPTS {
        match op().await {
            Ok(value) => {
                if attempt > 1 {
                    debug!(label, attempt, "Alpaca read recovered after retry");
                }
                return Ok(value);
            }
            Err(e) => {
                last = e;
                if attempt < READ_RETRY_ATTEMPTS {
                    // 100 ms, then 200 ms — bounded total backoff well
                    // under the per-request read timeout above.
                    let delay = Duration::from_millis(100u64 << attempt.saturating_sub(1));
                    debug!(
                        label,
                        attempt,
                        max = READ_RETRY_ATTEMPTS,
                        ?delay,
                        error = %last,
                        "transient Alpaca read failure, retrying after backoff"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(format!("{last} (after {READ_RETRY_ATTEMPTS} attempts)"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn build_alpaca_client_without_auth() {
        build_alpaca_client("http://localhost:11111", None, None).unwrap();
    }

    #[test]
    fn build_alpaca_client_with_auth() {
        let auth = ClientAuthConfig {
            username: "observatory".to_string(),
            password: "secret".to_string(),
        };
        build_alpaca_client("http://localhost:11111", Some(&auth), None).unwrap();
    }

    #[test]
    fn build_alpaca_client_with_invalid_url_fails() {
        let result = build_alpaca_client("not-a-url", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn build_alpaca_client_with_missing_ca_cert_fails() {
        let result = build_alpaca_client(
            "http://localhost:11111",
            None,
            Some(std::path::Path::new("/nonexistent/ca.pem")),
        );
        assert!(result.is_err());
    }

    // ----- retry_connect_attempt direct unit tests --------------------
    //
    // These exercise the retry helper independently of any of the
    // `connect_*` callers, so a regression to the Permanent /
    // Transient split or the backoff schedule shows up here rather
    // than as a slow / flaky integration test. `start_paused = true`
    // makes `tokio::time::sleep` advance virtual time only when
    // awaited, so the 1 s / 2 s backoff doesn't slow the suite.

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn retry_connect_attempt_returns_immediately_on_permanent() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = calls.clone();
        let result: Result<u32, String> = retry_connect_attempt("test", |_attempt| {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                AttemptOutcome::Permanent("config bad".to_string())
            }
        })
        .await;
        let err = result.expect_err("Permanent should propagate as Err");
        assert_eq!(err, "config bad");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Permanent must NOT trigger a retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_connect_attempt_succeeds_after_transient_failures() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_closure = attempts.clone();
        let result: Result<&'static str, String> = retry_connect_attempt("test", |attempt| {
            let attempts = attempts_for_closure.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < CONNECT_ATTEMPTS {
                    AttemptOutcome::Transient(format!("attempt {attempt} flake"))
                } else {
                    AttemptOutcome::Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), CONNECT_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_connect_attempt_gives_up_after_all_transient() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_closure = attempts.clone();
        let result: Result<u32, String> = retry_connect_attempt("test", |_| {
            let attempts = attempts_for_closure.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                AttemptOutcome::Transient("always fails".to_string())
            }
        })
        .await;
        let err = result.expect_err("all-Transient must give up with Err");
        assert!(
            err.contains(&format!("gave up after {CONNECT_ATTEMPTS} attempts")),
            "error should report attempt count, got: {err}"
        );
        assert!(
            err.contains("always fails"),
            "error should include last transient message, got: {err}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            CONNECT_ATTEMPTS,
            "every attempt should have been tried"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_connect_attempt_succeeds_first_try_does_not_sleep() {
        // Belt-and-braces: the helper shouldn't even enter the
        // backoff branch when the first attempt succeeds.
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_closure = attempts.clone();
        let result: Result<u32, String> = retry_connect_attempt("test", |_| {
            let attempts = attempts_for_closure.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                AttemptOutcome::Ok(42)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    // ----- retry_idempotent_read direct unit tests --------------------

    #[tokio::test(start_paused = true)]
    async fn retry_idempotent_read_recovers_after_transient_failures() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = calls.clone();
        let result: Result<i32, String> = retry_idempotent_read("test", || {
            let calls = calls_for_closure.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < READ_RETRY_ATTEMPTS {
                    Err(format!("transient {n}"))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), READ_RETRY_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_idempotent_read_gives_up_after_all_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = calls.clone();
        let result: Result<i32, String> = retry_idempotent_read("test", || {
            let calls = calls_for_closure.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("always fails".to_string())
            }
        })
        .await;
        let err = result.expect_err("all-failing read must give up with Err");
        assert!(err.contains("always fails"), "got: {err}");
        assert!(
            err.contains(&format!("after {READ_RETRY_ATTEMPTS} attempts")),
            "error should report attempt count, got: {err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), READ_RETRY_ATTEMPTS);
    }

    // ----- per-request timeout (the #319 root-cause regression) -------
    //
    // A device that accepts the TCP connection but never sends a
    // response must surface as an error, not hang forever. `start_paused`
    // advances virtual time so reqwest's read timeout fires in real-time
    // milliseconds; the outer `tokio::time::timeout` only trips if the
    // fix regresses (no client-level timeout), turning a silent infinite
    // hang into a loud test failure.
    #[tokio::test(start_paused = true)]
    async fn alpaca_client_times_out_on_silently_stalled_device() {
        use crate::equipment::test_support::spawn_stub;
        use axum::{routing::get, Json, Router};

        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async { std::future::pending::<Json<serde_json::Value>>().await }),
        );
        let stub = spawn_stub(app).await;
        let client = build_alpaca_client(&stub.url(), None, None).unwrap();

        let outcome = tokio::time::timeout(Duration::from_mins(2), client.get_devices()).await;
        let inner = outcome.expect("get_devices must return via the read timeout, not hang");
        // `get_devices`'s Ok type is `impl Iterator` (not Debug), so match
        // rather than `expect_err`.
        assert!(
            inner.is_err(),
            "a silently-stalled device must surface as an error, not a value"
        );
    }

    // ----- CA trust wiring (issue #609) --------------------------------
    //
    // Proves `build_alpaca_client` actually plumbs `ca_cert_path` into the
    // underlying reqwest client, not just that the parameter parses: a
    // client trusting the observatory CA connects to a device serving a
    // CA-signed certificate; a client without that trust rejects it. This
    // is the end-to-end proof for the gap issue #609 describes — the
    // config field alone (`Config::ca_cert`) doesn't guarantee the client
    // it feeds actually verifies against it.
    #[tokio::test]
    async fn ca_trusting_client_connects_to_ca_signed_alpaca_server() {
        use axum::{routing::get, Json, Router};

        let pki_dir = tempfile::tempdir().unwrap();
        rusty_photon_tls::test_cert::generate_ca(pki_dir.path()).unwrap();
        let ca_cert_pem = std::fs::read_to_string(pki_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = std::fs::read_to_string(pki_dir.path().join("ca-key.pem")).unwrap();
        let certs_dir = pki_dir.path().join("certs");
        rusty_photon_tls::test_cert::generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-alpaca",
            &certs_dir,
        )
        .unwrap();
        let tls_config = rusty_photon_tls::config::TlsConfig {
            cert: certs_dir
                .join("test-alpaca.pem")
                .to_string_lossy()
                .into_owned(),
            key: certs_dir
                .join("test-alpaca-key.pem")
                .to_string_lossy()
                .into_owned(),
        };

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
            .await
            .unwrap();
        let bound_addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async {
                Json(serde_json::json!({
                    "Value": [],
                    "ErrorNumber": 0,
                    "ErrorMessage": ""
                }))
            }),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_handle = tokio::spawn(async move {
            rusty_photon_tls::server::serve_tls(listener, router, &tls_config, async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
        });

        // The bound IPv4 loopback, not "localhost" — the listener is
        // IPv4-only (bind_dual_stack only widens an IPv6 bind), and on a
        // host where "localhost" resolves to ::1 first the connection
        // could miss it. The generated cert's SANs include 127.0.0.1
        // (see `generate_service_cert`), so this still exercises CA trust.
        let url = format!("https://{bound_addr}");
        let ca_path = pki_dir.path().join("ca.pem");

        let trusting_client = build_alpaca_client(&url, None, Some(&ca_path)).unwrap();
        let devices = trusting_client
            .get_devices()
            .await
            .expect("client trusting the CA must connect");
        assert_eq!(devices.count(), 0);

        let untrusting_client = build_alpaca_client(&url, None, None).unwrap();
        assert!(
            untrusting_client.get_devices().await.is_err(),
            "client without the CA must reject the CA-signed certificate as untrusted"
        );

        shutdown_tx.send(()).ok();
        server_handle.await.ok();
    }
}
